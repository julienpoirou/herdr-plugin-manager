//! `config.toml` in the manager's Herdr config directory
//! (`herdr plugin config-dir herdr-plugin-manager`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MarketKind {
    /// GitHub repository search by topic (what herdr.dev/plugins indexes).
    #[default]
    Github,
    /// Gitea or Forgejo `/api/v1/repos/search?topic=true`.
    Gitea,
    /// GitLab `/api/v4/projects?topic=`.
    Gitlab,
    /// A static index JSON served over http(s) or read from a file.
    Index,
    /// A static index JSON stored in a git repository (works with plain git).
    GitIndex,
}

impl MarketKind {
    pub fn label(self) -> &'static str {
        match self {
            MarketKind::Github => "github",
            MarketKind::Gitea => "gitea",
            MarketKind::Gitlab => "gitlab",
            MarketKind::Index => "index",
            MarketKind::GitIndex => "git-index",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CloneProtocol {
    #[default]
    Https,
    Ssh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketSource {
    pub name: String,
    #[serde(default)]
    pub kind: MarketKind,
    /// Forge base URL (gitea/gitlab, and github for GitHub Enterprise: the
    /// API root such as `https://ghe.example/api/v3`), index URL or path
    /// (index), or git remote (git-index).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_topic")]
    pub topic: String,
    /// git-index: path of the index file inside the repository.
    #[serde(default)]
    pub path: Option<String>,
    /// git-index: branch, tag or commit to read (default branch otherwise).
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    /// Literal token. Prefer `token_env` or `token_cmd`.
    #[serde(default)]
    pub token: Option<String>,
    /// Environment variable holding a token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Command printing a token, e.g. `["pass", "show", "gitea/token"]`.
    #[serde(default)]
    pub token_cmd: Option<Vec<String>>,
    /// Which clone URL forge listings install from.
    #[serde(default)]
    pub clone: CloneProtocol,
    /// Upper bound on listed repositories per source.
    #[serde(default = "default_max")]
    pub max: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_topic() -> String {
    "herdr-plugin".to_string()
}

fn default_max() -> usize {
    300
}

fn default_true() -> bool {
    true
}

fn default_keep() -> usize {
    3
}

fn default_ttl() -> u64 {
    600
}

fn default_concurrency() -> usize {
    6
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Short names for git servers: `corp = "ssh://git@git.corp.local/"`
    /// makes `herdr-pm install corp:team/tool` work. A value containing
    /// `{path}` is used as a template instead of a prefix.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Previous installations kept per plugin for `rollback`.
    #[serde(default = "default_keep")]
    pub keep_installs: usize,
    /// Seconds marketplace listings stay cached.
    #[serde(default = "default_ttl")]
    pub market_ttl: u64,
    /// Parallel update checks in the popup.
    #[serde(default = "default_concurrency")]
    pub check_concurrency: usize,
    /// Use `gh auth token` for GitHub when no token is in the environment.
    #[serde(default = "default_true")]
    pub use_gh_cli: bool,
    /// Editor for the `c` key (falls back to VISUAL, EDITOR, then vi/notepad).
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default = "default_markets")]
    pub marketplace: Vec<MarketSource>,
}

fn default_markets() -> Vec<MarketSource> {
    vec![MarketSource {
        name: "github".into(),
        kind: MarketKind::Github,
        url: None,
        topic: default_topic(),
        path: None,
        git_ref: None,
        token: None,
        token_env: None,
        token_cmd: None,
        clone: CloneProtocol::Https,
        max: default_max(),
        enabled: true,
    }]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            aliases: BTreeMap::new(),
            keep_installs: default_keep(),
            market_ttl: default_ttl(),
            check_concurrency: default_concurrency(),
            use_gh_cli: true,
            editor: None,
            marketplace: default_markets(),
        }
    }
}

pub const EXAMPLE: &str = r#"# herdr-plugin-manager configuration.
# Location: `herdr plugin config-dir herdr-plugin-manager`/config.toml

# Previous installations kept per plugin, for `herdr-pm rollback`.
keep_installs = 3

# Seconds marketplace listings are cached (press r in the popup to refresh).
market_ttl = 600

# Short names for your git servers. With these,
#   herdr-pm install corp:tools/herdr-notify
# clones ssh://git@git.corp.local/tools/herdr-notify
[aliases]
# corp = "ssh://git@git.corp.local/"
# lab  = "https://gitlab.lab.local/{path}.git"

# Marketplace sources, shown together in the popup (m) and `herdr-pm search`.
# Remove the GitHub block to browse only your own servers.

[[marketplace]]
name = "github"
kind = "github"            # topic search, the same index as herdr.dev/plugins
topic = "herdr-plugin"

# Gitea / Forgejo: repositories tagged with the topic.
# [[marketplace]]
# name = "corp"
# kind = "gitea"
# url = "https://git.corp.local"
# token_env = "CORP_GITEA_TOKEN"   # only needed for private repositories
# clone = "ssh"                    # install over ssh instead of https

# GitLab (self-managed or gitlab.com).
# [[marketplace]]
# name = "lab"
# kind = "gitlab"
# url = "https://gitlab.lab.local"
# token_cmd = ["pass", "show", "gitlab/token"]

# Plain git server, no web API: point at a repository holding an index file
# produced by `herdr-pm index build` (run it from cron or CI).
# [[marketplace]]
# name = "internal"
# kind = "git-index"
# url = "ssh://git@git.internal/herdr/index.git"
# path = "herdr-index.json"

# Static index over http(s) or from a shared folder.
# [[marketplace]]
# name = "team"
# kind = "index"
# url = "https://intranet.example/herdr-index.json"
"#;

impl Config {
    pub fn path() -> PathBuf {
        paths::pm_config_file()
    }

    pub fn load() -> Result<Config> {
        let path = Config::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("could not read {}", path.display()))
            }
        };
        let config: Config =
            toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
        config.check()?;
        Ok(config)
    }

    pub fn check(&self) -> Result<()> {
        let mut names = std::collections::BTreeSet::new();
        for source in &self.marketplace {
            if source.name.trim().is_empty() {
                bail!("a [[marketplace]] entry has no name");
            }
            if !names.insert(source.name.as_str()) {
                bail!("two [[marketplace]] entries are named {:?}", source.name);
            }
            let needs_url = !matches!(source.kind, MarketKind::Github);
            if needs_url && source.url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                bail!(
                    "marketplace {:?} ({}) needs a url",
                    source.name,
                    source.kind.label()
                );
            }
        }
        for name in self.aliases.keys() {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                bail!("alias {name:?} must be letters, digits, '-' or '_'");
            }
            if matches!(
                name.as_str(),
                "github" | "http" | "https" | "ssh" | "git" | "file"
            ) {
                bail!("alias {name:?} is reserved");
            }
        }
        Ok(())
    }

    /// Writes the commented example config if none exists yet.
    pub fn init() -> Result<(PathBuf, bool)> {
        let path = Config::path();
        if path.exists() {
            return Ok((path, false));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, EXAMPLE)?;
        Ok((path, true))
    }
}

impl MarketSource {
    /// Resolves the token from `token`, `token_env` or `token_cmd`.
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(token) = self.token.as_deref().filter(|t| !t.is_empty()) {
            return Some(token.to_string());
        }
        if let Some(var) = &self.token_env {
            if let Ok(token) = std::env::var(var) {
                if !token.trim().is_empty() {
                    return Some(token.trim().to_string());
                }
            }
        }
        if let Some(cmd) = self.token_cmd.as_ref().filter(|c| !c.is_empty()) {
            let mut command = std::process::Command::new(&cmd[0]);
            command.args(&cmd[1..]);
            if let Ok(out) =
                crate::util::run_captured(command, Some(std::time::Duration::from_secs(10)))
            {
                if out.status.success() {
                    let token = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !token.is_empty() {
                        return Some(token);
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        config.check().unwrap();
        assert_eq!(config.marketplace.len(), 1);
        assert_eq!(config.keep_installs, 3);
    }

    #[test]
    fn full_config_parses() {
        let text = r#"
[aliases]
corp = "ssh://git@git.corp.local/"

[[marketplace]]
name = "corp"
kind = "gitea"
url = "https://git.corp.local"
clone = "ssh"

[[marketplace]]
name = "internal"
kind = "git-index"
url = "/srv/git/index.git"
ref = "main"
"#;
        let config: Config = toml::from_str(text).unwrap();
        config.check().unwrap();
        assert_eq!(config.marketplace[0].kind, MarketKind::Gitea);
        assert_eq!(config.marketplace[0].clone, CloneProtocol::Ssh);
        assert_eq!(config.marketplace[1].git_ref.as_deref(), Some("main"));
        assert_eq!(config.aliases["corp"], "ssh://git@git.corp.local/");
    }

    #[test]
    fn rejects_bad_configs() {
        let config: Config =
            toml::from_str("[[marketplace]]\nname = \"x\"\nkind = \"gitea\"\n").unwrap();
        assert!(config.check().is_err());
        let config: Config = toml::from_str("[aliases]\nhttps = \"x\"\n").unwrap();
        assert!(config.check().is_err());
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }
}
