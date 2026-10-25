//! What the user typed → where a plugin comes from.
//!
//! Accepted forms:
//!   owner/repo[/subdir]            GitHub, installed natively by Herdr
//!   github:owner/repo[/subdir]     same, explicit
//!   <alias>:path[//subdir]         an alias from config.toml, e.g. corp:team/tool
//!   https://host/team/repo.git     any git URL (http, https, ssh, git, file)
//!   git@host:team/repo.git         scp-like ssh
//!   /srv/git/repo.git, ./repo      a local repository
//! A `//` after the repository path selects a subdirectory, as in
//! `ssh://git@host/team/monorepo.git//plugins/foo`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Handed to `herdr plugin install`, so Herdr owns the checkout.
    Github {
        owner: String,
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subdir: Option<String>,
    },
    /// Cloned, built and linked by this manager.
    Git {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subdir: Option<String>,
    },
}

impl Source {
    pub fn display(&self) -> String {
        match self {
            Source::Github {
                owner,
                repo,
                subdir,
            } => match subdir {
                Some(s) => format!("{owner}/{repo}/{s}"),
                None => format!("{owner}/{repo}"),
            },
            Source::Git { url, subdir } => match subdir {
                Some(s) => format!("{url}//{s}"),
                None => url.clone(),
            },
        }
    }

    pub fn with_subdir(self, new: Option<String>) -> Source {
        let new = new.map(|s| normalize_subdir(&s)).filter(|s| !s.is_empty());
        match self {
            Source::Github {
                owner,
                repo,
                subdir,
            } => Source::Github {
                owner,
                repo,
                subdir: new.or(subdir),
            },
            Source::Git { url, subdir } => Source::Git {
                url,
                subdir: new.or(subdir),
            },
        }
    }

    /// The same plugin, fetched through git instead of Herdr's installer
    /// (`--git`): useful for private GitHub repositories over SSH.
    pub fn into_git(self) -> Source {
        match self {
            Source::Github {
                owner,
                repo,
                subdir,
            } => Source::Git {
                url: format!("https://github.com/{owner}/{repo}.git"),
                subdir,
            },
            other => other,
        }
    }
}

pub fn normalize_subdir(value: &str) -> String {
    value
        .replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

pub fn validate_subdir(subdir: &str) -> Result<()> {
    if subdir.split('/').any(|s| s == "..") {
        bail!("subdirectory may not contain '..': {subdir}");
    }
    Ok(())
}

fn github_segment_ok(value: &str, repo: bool) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || (repo && matches!(b, b'.' | b'_')))
}

fn parse_github(spec: &str) -> Result<Source> {
    let spec = spec.trim().trim_matches('/');
    let mut parts = spec.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default().trim_end_matches(".git");
    if !github_segment_ok(owner, false) || !github_segment_ok(repo, true) {
        bail!("not a GitHub owner/repo: {spec}");
    }
    let rest: Vec<&str> = parts.collect();
    let subdir = normalize_subdir(&rest.join("/"));
    validate_subdir(&subdir)?;
    Ok(Source::Github {
        owner: owner.to_string(),
        repo: repo.to_string(),
        subdir: (!subdir.is_empty()).then_some(subdir),
    })
}

/// Splits `repo//subdir` (the `//` that is not part of `scheme://`).
fn split_double_slash(value: &str) -> (String, Option<String>) {
    let start = value.find("://").map(|i| i + 3).unwrap_or(0);
    if let Some(pos) = value[start..].find("//") {
        let at = start + pos;
        let repo = value[..at].to_string();
        let subdir = normalize_subdir(&value[at + 2..]);
        return (repo, (!subdir.is_empty()).then_some(subdir));
    }
    (value.to_string(), None)
}

fn is_url(value: &str) -> bool {
    [
        "https://",
        "http://",
        "ssh://",
        "git://",
        "file://",
        "git+ssh://",
        "ssh+git://",
    ]
    .iter()
    .any(|scheme| value.starts_with(scheme))
}

fn is_scp_like(value: &str) -> bool {
    // user@host:path or host.with.dots:path — but never C:\ or C:/ (Windows).
    let Some((before, after)) = value.split_once(':') else {
        return false;
    };
    if before.contains('/') || after.is_empty() || after.starts_with("//") {
        return false;
    }
    if before.len() == 1 && before.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    before.contains('@') || before.contains('.') || before == "localhost"
}

fn is_local_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || value == "."
        || (value.len() > 2
            && value.as_bytes()[1] == b':'
            && matches!(value.as_bytes()[2], b'\\' | b'/'))
        || value.starts_with(".\\")
        || value.starts_with("..\\")
}

/// A relative path that is clearly a repository on disk (so `team/tool`
/// typed inside a folder of checkouts is not mistaken for GitHub).
fn looks_like_repo_dir(value: &str) -> bool {
    let (path, _) = split_double_slash(value);
    let path = Path::new(&path);
    path.join(".git").exists() || (path.join("HEAD").is_file() && path.join("objects").is_dir())
}

fn expand_alias(template: &str, path: &str) -> String {
    if template.contains("{path}") {
        return template.replace("{path}", path);
    }
    if template.ends_with('/') || template.ends_with(':') {
        format!("{template}{path}")
    } else {
        format!("{template}/{path}")
    }
}

/// Parses a source. `aliases` maps short names (from config) to URL
/// prefixes or `{path}` templates.
pub fn parse(input: &str, aliases: &BTreeMap<String, String>) -> Result<Source> {
    let input = input.trim();
    if input.is_empty() {
        bail!("empty plugin source");
    }
    if let Some(rest) = input.strip_prefix("github:") {
        return parse_github(rest);
    }
    if let Some((name, rest)) = input.split_once(':') {
        if let Some(template) = aliases.get(name) {
            let (path, subdir) = split_double_slash(rest);
            let url = expand_alias(template, path.trim_start_matches('/'));
            if let Some(s) = &subdir {
                validate_subdir(s)?;
            }
            return Ok(Source::Git { url, subdir });
        }
    }
    if is_url(input) || is_scp_like(input) {
        let (url, subdir) = split_double_slash(input);
        if let Some(s) = &subdir {
            validate_subdir(s)?;
        }
        let url = url
            .strip_prefix("git+ssh://")
            .map(|r| format!("ssh://{r}"))
            .unwrap_or(url);
        let url = url
            .strip_prefix("ssh+git://")
            .map(|r| format!("ssh://{r}"))
            .unwrap_or(url);
        return Ok(Source::Git { url, subdir });
    }
    if is_local_path(input) || looks_like_repo_dir(input) {
        let (path, subdir) = split_double_slash(input);
        let expanded = if let Some(rest) = path.strip_prefix('~') {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_default();
            format!("{home}{rest}")
        } else {
            path
        };
        let absolute = std::fs::canonicalize(&expanded)
            .map(|p| p.display().to_string())
            .unwrap_or(expanded);
        let absolute = absolute
            .strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or(absolute);
        if let Some(s) = &subdir {
            validate_subdir(s)?;
        }
        return Ok(Source::Git {
            url: absolute,
            subdir,
        });
    }
    if input.contains('/') && !input.contains(':') {
        return parse_github(input);
    }
    bail!(
        "don't know how to install {input:?}: use owner/repo for GitHub, a git URL \
         (https://, ssh://, git@host:path, git://, file://), a local path, or an alias"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Source {
        let mut aliases = BTreeMap::new();
        aliases.insert("corp".to_string(), "ssh://git@git.corp.local/".to_string());
        aliases.insert(
            "lab".to_string(),
            "https://lab.example/{path}.git".to_string(),
        );
        parse(s, &aliases).unwrap()
    }

    #[test]
    fn github_shorthand() {
        assert_eq!(
            p("owner/repo/sub/dir"),
            Source::Github {
                owner: "owner".into(),
                repo: "repo".into(),
                subdir: Some("sub/dir".into())
            }
        );
        assert_eq!(p("github:o/r").display(), "o/r");
        assert!(parse("o/../x", &BTreeMap::new()).is_err());
        assert!(parse("o/r/../x", &BTreeMap::new()).is_err());
    }

    #[test]
    fn git_urls_and_subdirs() {
        assert_eq!(
            p("ssh://git@host:2222/team/mono.git//plugins/foo"),
            Source::Git {
                url: "ssh://git@host:2222/team/mono.git".into(),
                subdir: Some("plugins/foo".into())
            }
        );
        assert_eq!(
            p("git@git.corp.local:team/tool.git"),
            Source::Git {
                url: "git@git.corp.local:team/tool.git".into(),
                subdir: None
            }
        );
        assert_eq!(
            p("git://host/x.git"),
            Source::Git {
                url: "git://host/x.git".into(),
                subdir: None
            }
        );
        assert_eq!(
            p("git+ssh://h/x"),
            Source::Git {
                url: "ssh://h/x".into(),
                subdir: None
            }
        );
        assert_eq!(
            p("file:///srv/git/x.git"),
            Source::Git {
                url: "file:///srv/git/x.git".into(),
                subdir: None
            }
        );
    }

    #[test]
    fn aliases_expand() {
        assert_eq!(
            p("corp:team/tool.git//pkg"),
            Source::Git {
                url: "ssh://git@git.corp.local/team/tool.git".into(),
                subdir: Some("pkg".into())
            }
        );
        assert_eq!(
            p("lab:a/b"),
            Source::Git {
                url: "https://lab.example/a/b.git".into(),
                subdir: None
            }
        );
    }

    #[test]
    fn local_paths() {
        let Source::Git { url, .. } = p("/definitely/not/here/repo.git") else {
            panic!()
        };
        assert_eq!(url, "/definitely/not/here/repo.git");
        assert!(matches!(p("./x"), Source::Git { .. }));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("", &BTreeMap::new()).is_err());
        assert!(parse("nonsense", &BTreeMap::new()).is_err());
        assert!(parse("ssh://h/x.git//../etc", &BTreeMap::new()).is_err());
    }

    #[test]
    fn force_git_for_github() {
        assert_eq!(
            p("o/r/s").into_git(),
            Source::Git {
                url: "https://github.com/o/r.git".into(),
                subdir: Some("s".into())
            }
        );
    }
}
