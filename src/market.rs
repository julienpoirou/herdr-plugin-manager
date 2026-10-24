//! Marketplace: one listing assembled from several sources.
//!
//! * `github`    — repository search by topic (the herdr.dev/plugins index)
//! * `gitea`     — Gitea / Forgejo topic search
//! * `gitlab`    — GitLab projects by topic
//! * `index`     — a static index JSON over http(s) or from a file
//! * `git-index` — a static index JSON inside any git repository, so a plain
//!   git server with no web API at all can still publish a catalog
//!
//! Listings are cached on disk for `market_ttl` seconds.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{CloneProtocol, Config, MarketKind, MarketSource};
use crate::git::Git;
use crate::index::Index;
use crate::paths;
use crate::util;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Marketplace source name.
    pub market: String,
    /// What to show: `owner/repo` or the plugin name.
    pub title: String,
    #[serde(default)]
    pub plugin_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: String,
    /// What `install` takes: `owner/repo` (GitHub) or a git URL.
    pub install: String,
    #[serde(default)]
    pub subdir: Option<String>,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub stars: Option<u64>,
    #[serde(default)]
    pub updated_unix: u64,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub min_herdr_version: Option<String>,
}

impl Entry {
    /// The argument for `herdr-pm install`.
    pub fn install_spec(&self) -> String {
        match &self.subdir {
            Some(s)
                if self.install.contains("://")
                    || self.install.contains('@')
                    || self.install.starts_with('/') =>
            {
                format!("{}//{s}", self.install)
            }
            Some(s) => format!("{}/{s}", self.install),
            None => self.install.clone(),
        }
    }

    pub fn is_github(&self) -> bool {
        !self.install.contains(':') && !self.install.starts_with('/') && self.install.contains('/')
    }

    fn matches(&self, terms: &[String]) -> bool {
        let haystack = format!(
            "{} {} {} {}",
            self.title,
            self.plugin_id.as_deref().unwrap_or(""),
            self.description,
            self.install
        )
        .to_lowercase();
        terms.iter().all(|t| haystack.contains(t))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// What the source reported as its total, if more than were fetched.
    pub total: Option<u64>,
    pub fetched_unix: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Stars,
    Updated,
    Name,
}

impl Sort {
    pub fn next(self) -> Sort {
        match self {
            Sort::Stars => Sort::Updated,
            Sort::Updated => Sort::Name,
            Sort::Name => Sort::Stars,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Stars => "stars",
            Sort::Updated => "updated",
            Sort::Name => "name",
        }
    }

    pub fn parse(value: &str) -> Option<Sort> {
        match value {
            "stars" => Some(Sort::Stars),
            "updated" | "recent" => Some(Sort::Updated),
            "name" => Some(Sort::Name),
            _ => None,
        }
    }
}

pub fn sort_entries(entries: &mut [Entry], sort: Sort) {
    match sort {
        Sort::Stars => entries.sort_by(|a, b| {
            b.stars
                .unwrap_or(0)
                .cmp(&a.stars.unwrap_or(0))
                .then(b.updated_unix.cmp(&a.updated_unix))
        }),
        Sort::Updated => entries.sort_by_key(|e| std::cmp::Reverse(e.updated_unix)),
        Sort::Name => entries.sort_by_key(|a| a.title.to_lowercase()),
    }
}

// ---------------------------------------------------------------- http

fn env_any(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

fn host_of(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    Some((host.to_lowercase(), scheme))
}

/// `NO_PROXY` semantics as curl and git implement them: `*`, exact hosts,
/// and domain suffixes (`corp.local` and `.corp.local` both match
/// `git.corp.local`). Ports in entries are ignored.
pub fn bypasses_proxy(host: &str, no_proxy: &str) -> bool {
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let entry = entry.trim_start_matches("*.");
            let entry = entry
                .split(':')
                .next()
                .unwrap_or(entry)
                .trim_start_matches('.')
                .to_lowercase();
            host == entry || host.ends_with(&format!(".{entry}"))
        })
}

/// The proxy to use for `url` from the usual environment variables, or
/// None for a direct connection.
pub fn proxy_for(url: &str) -> Option<String> {
    let (host, scheme) = host_of(url)?;
    if let Some(no_proxy) = env_any(&["NO_PROXY", "no_proxy"]) {
        if bypasses_proxy(&host, &no_proxy) {
            return None;
        }
    }
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return None;
    }
    let specific = if scheme == "https" {
        env_any(&["HTTPS_PROXY", "https_proxy"])
    } else {
        env_any(&["http_proxy", "HTTP_PROXY"])
    };
    specific.or_else(|| env_any(&["ALL_PROXY", "all_proxy"]))
}

fn agent_for(url: &str) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("herdr-plugin-manager/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = proxy_for(url).and_then(|p| ureq::Proxy::new(p).ok()) {
        builder = builder.proxy(proxy);
    }
    builder.build()
}

fn describe_http_error(url: &str, err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| util::truncate(&util::sanitize(&body), 120));
            let hint = match code {
                401 => " (token missing or rejected)",
                403 => " (forbidden or rate-limited; set a token)",
                404 => " (not found: check the url)",
                _ => "",
            };
            anyhow!("HTTP {code}{hint}: {message}")
        }
        ureq::Error::Transport(t) => anyhow!("could not reach {url}: {t}"),
    }
}

fn http_get(url: &str, headers: &[(&str, String)], timeout: Duration) -> Result<ureq::Response> {
    let mut request = agent_for(url).get(url).timeout(timeout);
    for (name, value) in headers {
        request = request.set(name, value);
    }
    request.call().map_err(|e| describe_http_error(url, e))
}

pub fn http_get_text(url: &str, token: Option<&str>, timeout: Duration) -> Result<String> {
    let headers: Vec<(&str, String)> = token
        .map(|t| vec![("Authorization", format!("Bearer {t}"))])
        .unwrap_or_default();
    let response = http_get(url, &headers, timeout)?;
    let mut text = String::new();
    response
        .into_reader()
        .take(8 * 1024 * 1024)
        .read_to_string(&mut text)?;
    Ok(text)
}

fn http_get_json(
    url: &str,
    headers: &[(&str, String)],
) -> Result<(serde_json::Value, Option<String>)> {
    let response = http_get(url, headers, Duration::from_secs(20))?;
    let total = response
        .header("x-total")
        .or_else(|| response.header("x-total-count"))
        .map(str::to_string);
    let mut text = String::new();
    response
        .into_reader()
        .take(16 * 1024 * 1024)
        .read_to_string(&mut text)?;
    let value =
        serde_json::from_str(&text).with_context(|| format!("{url} did not return JSON"))?;
    Ok((value, total))
}

fn enc(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn gh_token(config: &Config, source: &MarketSource) -> Option<String> {
    if let Some(t) = source.resolve_token() {
        return Some(t);
    }
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Some(t.trim().to_string());
            }
        }
    }
    if std::env::var_os("HERDR_PM_NO_TOKEN").is_some() || !config.use_gh_cli {
        return None;
    }
    let mut command = std::process::Command::new("gh");
    command.args(["auth", "token"]);
    let out = util::run_captured(command, Some(Duration::from_secs(5))).ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

// ---------------------------------------------------------------- sources

fn fetch_github(source: &MarketSource, query: &str, config: &Config) -> Result<Listing> {
    let api = source
        .url
        .as_deref()
        .unwrap_or("https://api.github.com")
        .trim_end_matches('/');
    let token = gh_token(config, source);
    let mut headers = vec![
        ("Accept", "application/vnd.github+json".to_string()),
        ("X-GitHub-Api-Version", "2022-11-28".to_string()),
    ];
    if let Some(t) = &token {
        headers.push(("Authorization", format!("Bearer {t}")));
    }
    let mut q = format!("topic:{}", source.topic);
    if !query.trim().is_empty() {
        q.push(' ');
        q.push_str(query.trim());
    }
    let per_page = 100usize;
    let mut entries = Vec::new();
    let mut total = None;
    let mut page = 1;
    while entries.len() < source.max {
        let url = format!(
            "{api}/search/repositories?q={}&sort=stars&order=desc&per_page={per_page}&page={page}",
            enc(&q)
        );
        let (value, _) = http_get_json(&url, &headers)?;
        total = value["total_count"].as_u64();
        let items = value["items"].as_array().cloned().unwrap_or_default();
        let count = items.len();
        for repo in items {
            if repo["fork"].as_bool() == Some(true) || repo["archived"].as_bool() == Some(true) {
                continue;
            }
            let Some(full_name) = repo["full_name"].as_str() else {
                continue;
            };
            entries.push(Entry {
                market: source.name.clone(),
                title: util::sanitize(full_name),
                plugin_id: None,
                version: None,
                description: util::sanitize(repo["description"].as_str().unwrap_or("")),
                install: if source.url.is_some() {
                    // GitHub Enterprise: herdr's installer only knows github.com.
                    clone_url(&repo, source.clone, "clone_url", "ssh_url")
                } else {
                    full_name.to_string()
                },
                subdir: None,
                git_ref: None,
                stars: repo["stargazers_count"].as_u64(),
                updated_unix: repo["pushed_at"]
                    .as_str()
                    .and_then(util::parse_iso8601)
                    .unwrap_or(0),
                web_url: repo["html_url"].as_str().map(str::to_string),
                platforms: None,
                min_herdr_version: None,
            });
        }
        // The search API stops at 1000 results.
        if count < per_page || page * per_page >= 1000 {
            break;
        }
        page += 1;
    }
    entries.truncate(source.max);
    Ok(Listing {
        entries,
        total,
        fetched_unix: util::now_unix(),
    })
}

fn clone_url(
    repo: &serde_json::Value,
    protocol: CloneProtocol,
    https_key: &str,
    ssh_key: &str,
) -> String {
    let https = repo[https_key].as_str().unwrap_or_default();
    let ssh = repo[ssh_key].as_str().unwrap_or_default();
    match protocol {
        CloneProtocol::Ssh if !ssh.is_empty() => ssh.to_string(),
        _ if !https.is_empty() => https.to_string(),
        _ => ssh.to_string(),
    }
}

fn fetch_gitea(source: &MarketSource, query: &str) -> Result<Listing> {
    let base = source
        .url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let mut headers = vec![("Accept", "application/json".to_string())];
    if let Some(t) = source.resolve_token() {
        headers.push(("Authorization", format!("token {t}")));
    }
    let limit = 50usize;
    let mut entries = Vec::new();
    let mut total = None;
    let mut page = 1;
    while entries.len() < source.max {
        let url = format!(
            "{base}/api/v1/repos/search?q={}&topic=true&limit={limit}&page={page}&sort=stars&order=desc",
            enc(&source.topic)
        );
        let (value, total_header) = http_get_json(&url, &headers)?;
        total = total_header.and_then(|t| t.parse().ok()).or(total);
        if value["ok"].as_bool() == Some(false) {
            bail!(
                "gitea search failed: {}",
                value["message"].as_str().unwrap_or("?")
            );
        }
        let items = value["data"].as_array().cloned().unwrap_or_default();
        let count = items.len();
        for repo in items {
            if repo["archived"].as_bool() == Some(true) || repo["fork"].as_bool() == Some(true) {
                continue;
            }
            let Some(full_name) = repo["full_name"].as_str() else {
                continue;
            };
            entries.push(Entry {
                market: source.name.clone(),
                title: util::sanitize(full_name),
                plugin_id: None,
                version: None,
                description: util::sanitize(repo["description"].as_str().unwrap_or("")),
                install: clone_url(&repo, source.clone, "clone_url", "ssh_url"),
                subdir: None,
                git_ref: None,
                stars: repo["stars_count"].as_u64(),
                updated_unix: repo["updated_at"]
                    .as_str()
                    .and_then(util::parse_iso8601)
                    .unwrap_or(0),
                web_url: repo["html_url"].as_str().map(str::to_string),
                platforms: None,
                min_herdr_version: None,
            });
        }
        if count < limit {
            break;
        }
        page += 1;
    }
    filter_local(&mut entries, query);
    entries.truncate(source.max);
    Ok(Listing {
        entries,
        total,
        fetched_unix: util::now_unix(),
    })
}

fn fetch_gitlab(source: &MarketSource, query: &str) -> Result<Listing> {
    let base = source
        .url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let mut headers = vec![("Accept", "application/json".to_string())];
    if let Some(t) = source.resolve_token() {
        headers.push(("PRIVATE-TOKEN", t));
    }
    let per_page = 100usize;
    let mut entries = Vec::new();
    let mut page = 1;
    let mut total = None;
    while entries.len() < source.max {
        let url = format!(
            "{base}/api/v4/projects?topic={}&archived=false&per_page={per_page}&page={page}&order_by=last_activity_at",
            enc(&source.topic)
        );
        let (value, total_header) = http_get_json(&url, &headers)?;
        total = total_header.and_then(|t| t.parse().ok()).or(total);
        let items = value.as_array().cloned().unwrap_or_default();
        let count = items.len();
        for project in items {
            if !project["forked_from_project"].is_null()
                && project.get("forked_from_project").is_some()
            {
                continue;
            }
            let Some(path) = project["path_with_namespace"].as_str() else {
                continue;
            };
            entries.push(Entry {
                market: source.name.clone(),
                title: util::sanitize(path),
                plugin_id: None,
                version: None,
                description: util::sanitize(project["description"].as_str().unwrap_or("")),
                install: clone_url(
                    &project,
                    source.clone,
                    "http_url_to_repo",
                    "ssh_url_to_repo",
                ),
                subdir: None,
                git_ref: None,
                stars: project["star_count"].as_u64(),
                updated_unix: project["last_activity_at"]
                    .as_str()
                    .and_then(util::parse_iso8601)
                    .unwrap_or(0),
                web_url: project["web_url"].as_str().map(str::to_string),
                platforms: None,
                min_herdr_version: None,
            });
        }
        if count < per_page {
            break;
        }
        page += 1;
    }
    filter_local(&mut entries, query);
    entries.truncate(source.max);
    Ok(Listing {
        entries,
        total,
        fetched_unix: util::now_unix(),
    })
}

fn index_entries(source: &MarketSource, index: Index, query: &str) -> Listing {
    let mut entries: Vec<Entry> = index
        .plugins
        .into_iter()
        .map(|p| Entry {
            market: source.name.clone(),
            title: util::sanitize(if p.name.is_empty() { &p.id } else { &p.name }),
            plugin_id: Some(util::sanitize(&p.id)),
            version: Some(util::sanitize(&p.version)).filter(|v| !v.is_empty()),
            description: util::sanitize(p.description.as_deref().unwrap_or("")),
            web_url: p.homepage.or_else(|| util::web_url_for_remote(&p.source)),
            install: p.source,
            subdir: p.subdir,
            git_ref: p.git_ref,
            stars: None,
            updated_unix: p.updated_unix,
            platforms: p.platforms,
            min_herdr_version: p.min_herdr_version,
        })
        .collect();
    filter_local(&mut entries, query);
    entries.truncate(source.max);
    Listing {
        entries,
        total: None,
        fetched_unix: util::now_unix(),
    }
}

fn fetch_index(source: &MarketSource, query: &str) -> Result<Listing> {
    let url = source.url.as_deref().unwrap_or_default();
    let text = if url.starts_with("http://") || url.starts_with("https://") {
        http_get_text(
            url,
            source.resolve_token().as_deref(),
            Duration::from_secs(20),
        )?
    } else {
        let path = url.strip_prefix("file://").unwrap_or(url);
        std::fs::read_to_string(Path::new(path))
            .with_context(|| format!("could not read index {path}"))?
    };
    let index = Index::parse(&text)?;
    Ok(index_entries(source, index, query))
}

fn fetch_git_index(source: &MarketSource, query: &str) -> Result<Listing> {
    let url = source.url.as_deref().unwrap_or_default();
    let git = Git::BACKGROUND;
    let mirror = git.sync(url)?;
    let sha = git.resolve(&mirror, source.git_ref.as_deref())?;
    let path = source.path.as_deref().unwrap_or(crate::index::DEFAULT_FILE);
    let text = git
        .show_file(&mirror, &sha, path)?
        .ok_or_else(|| anyhow!("{url} has no {path} (generate one with `herdr-pm index build`)"))?;
    let index = Index::parse(&text)?;
    Ok(index_entries(source, index, query))
}

fn filter_local(entries: &mut Vec<Entry>, query: &str) {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if !terms.is_empty() {
        entries.retain(|e| e.matches(&terms));
    }
}

fn cache_file(source: &MarketSource, query: &str) -> std::path::PathBuf {
    let key = format!(
        "{}|{}|{}|{}|{}|{}",
        source.name,
        source.kind.label(),
        source.url.as_deref().unwrap_or(""),
        source.topic,
        source.path.as_deref().unwrap_or(""),
        query.trim().to_lowercase()
    );
    paths::market_cache_dir().join(format!("{}.json", util::fnv1a64(&key)))
}

/// Fetches one source, using the cache unless `refresh`.
pub fn fetch(
    source: &MarketSource,
    query: &str,
    refresh: bool,
    config: &Config,
) -> Result<Listing> {
    let cache = cache_file(source, query);
    if !refresh {
        if let Ok(text) = std::fs::read_to_string(&cache) {
            if let Ok(listing) = serde_json::from_str::<Listing>(&text) {
                if util::now_unix().saturating_sub(listing.fetched_unix) < config.market_ttl {
                    return Ok(listing);
                }
            }
        }
    }
    let listing = match source.kind {
        MarketKind::Github => fetch_github(source, query, config),
        MarketKind::Gitea => fetch_gitea(source, query),
        MarketKind::Gitlab => fetch_gitlab(source, query),
        MarketKind::Index => fetch_index(source, query),
        MarketKind::GitIndex => fetch_git_index(source, query),
    }
    .with_context(|| format!("marketplace {:?}", source.name))?;
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&listing) {
        let _ = std::fs::write(&cache, text);
    }
    Ok(listing)
}

pub struct SourceResult {
    pub name: String,
    pub result: Result<Listing>,
}

/// Queries every enabled source in parallel.
pub fn search_all(
    config: &Config,
    query: &str,
    refresh: bool,
    only: Option<&str>,
) -> Vec<SourceResult> {
    let sources: Vec<MarketSource> = config
        .marketplace
        .iter()
        .filter(|s| s.enabled && only.is_none_or(|o| o == s.name))
        .cloned()
        .collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = sources
            .iter()
            .map(|source| {
                let query = query.to_string();
                scope.spawn(move || SourceResult {
                    name: source.name.clone(),
                    result: fetch(source, &query, refresh, config),
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| SourceResult {
                    name: "?".into(),
                    result: Err(anyhow!("source crashed")),
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, stars: u64, updated: u64) -> Entry {
        Entry {
            market: "m".into(),
            title: title.into(),
            plugin_id: None,
            version: None,
            description: format!("{title} plugin"),
            install: format!("o/{title}"),
            subdir: None,
            git_ref: None,
            stars: Some(stars),
            updated_unix: updated,
            web_url: None,
            platforms: None,
            min_herdr_version: None,
        }
    }

    #[test]
    fn sorting_and_filtering() {
        let mut list = vec![entry("b", 1, 30), entry("a", 5, 10), entry("c", 3, 20)];
        sort_entries(&mut list, Sort::Stars);
        assert_eq!(
            list.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        sort_entries(&mut list, Sort::Updated);
        assert_eq!(list[0].title, "b");
        sort_entries(&mut list, Sort::Name);
        assert_eq!(list[0].title, "a");
        filter_local(&mut list, "C PLUG");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn install_specs() {
        let mut e = entry("x", 0, 0);
        e.subdir = Some("p/q".into());
        assert_eq!(e.install_spec(), "o/x/p/q");
        assert!(e.is_github());
        e.install = "ssh://git@h/t/x.git".into();
        assert_eq!(e.install_spec(), "ssh://git@h/t/x.git//p/q");
        assert!(!e.is_github());
        e.install = "git@h:t/x.git".into();
        assert!(!e.is_github());
    }

    #[test]
    fn no_proxy_matching() {
        assert!(bypasses_proxy("git.corp.local", "corp.local"));
        assert!(bypasses_proxy("git.corp.local", ".corp.local:443, other"));
        assert!(bypasses_proxy("corp.local", "corp.local"));
        assert!(!bypasses_proxy("evilcorp.local", "corp.local"));
        assert!(bypasses_proxy("anything", "*"));
        assert!(bypasses_proxy("10.0.0.5", "10.0.0.5"));
        assert_eq!(
            host_of("https://user:pw@Git.Corp.Local:8443/api").map(|(h, s)| (h, s.to_string())),
            Some(("git.corp.local".to_string(), "https".to_string()))
        );
    }

    #[test]
    fn url_encoding() {
        assert_eq!(enc("topic:herdr-plugin foo"), "topic%3Aherdr-plugin%20foo");
    }
}
