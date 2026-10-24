//! A static plugin index: the catalog format for marketplaces that have no
//! forge API (plain git servers, a shared folder, an intranet web server),
//! and `herdr-pm index build`, which produces it by reading manifests
//! straight out of git repositories.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::git::Git;
use crate::manifest::{self, Manifest};
use crate::util;

pub const FORMAT: &str = "herdr-plugin-index";
pub const DEFAULT_FILE: &str = "herdr-index.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexPlugin {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_herdr_version: Option<String>,
    /// Git URL (or GitHub owner/repo) to install from.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// Ref the index was built from; installs follow it.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default)]
    pub updated_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    #[serde(default = "format_name")]
    pub format: String,
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub generated_unix: u64,
    #[serde(default)]
    pub plugins: Vec<IndexPlugin>,
}

fn format_name() -> String {
    FORMAT.to_string()
}

fn one() -> u32 {
    1
}

impl Index {
    pub fn parse(text: &str) -> Result<Index> {
        let index: Index = serde_json::from_str(text).context("plugin index is not valid JSON")?;
        if index.format != FORMAT {
            bail!("not a herdr plugin index (format {:?})", index.format);
        }
        if index.version > 1 {
            bail!(
                "plugin index version {} is newer than this manager understands",
                index.version
            );
        }
        Ok(index)
    }
}

/// One repository to index.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoSpec {
    pub url: String,
    pub git_ref: Option<String>,
}

/// `repos.txt`: one `url [ref]` per line, `#` comments.
pub fn parse_repo_list(text: &str) -> Vec<RepoSpec> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut parts = l.split_whitespace();
            RepoSpec {
                url: parts.next().unwrap_or_default().to_string(),
                git_ref: parts.next().map(str::to_string),
            }
        })
        .collect()
}

fn is_bare_repo(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

/// Finds repositories under a directory of bare repos (as a plain git
/// server keeps them) or working copies, up to a few levels deep.
pub fn scan_dir(root: &Path, url_base: Option<&str>) -> Result<Vec<RepoSpec>> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 5 {
            return;
        }
        if is_bare_repo(dir) || dir.join(".git").exists() {
            out.push(dir.to_path_buf());
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect();
        dirs.sort();
        for d in dirs {
            walk(&d, depth + 1, out);
        }
    }
    let root =
        std::fs::canonicalize(root).with_context(|| format!("cannot read {}", root.display()))?;
    let mut found = Vec::new();
    walk(&root, 0, &mut found);
    Ok(found
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let url = match url_base {
                Some(base) if base.contains("{path}") => base.replace("{path}", &rel),
                Some(base) => format!("{}/{}", base.trim_end_matches('/'), rel),
                None => path.display().to_string(),
            };
            RepoSpec { url, git_ref: None }
        })
        .collect())
}

pub struct BuildReport {
    pub index: Index,
    pub warnings: Vec<String>,
}

/// Reads every manifest in each repository at its ref and builds the index.
/// `fetch_from` lets a server index its bare repos from disk while
/// publishing the URLs clients will clone from.
pub fn build(name: Option<String>, repos: &[(RepoSpec, Option<String>)]) -> BuildReport {
    let git = Git::BACKGROUND;
    let mut plugins = Vec::new();
    let mut warnings = Vec::new();
    for (repo, fetch_from) in repos {
        let fetch_url = fetch_from.as_deref().unwrap_or(&repo.url);
        let result = (|| -> Result<Vec<IndexPlugin>> {
            let mirror = git.sync(fetch_url)?;
            let sha = git.resolve(&mirror, repo.git_ref.as_deref())?;
            let info = git.commit_info(&mirror, &sha)?;
            let mut out = Vec::new();
            for path in git.find_manifests(&mirror, &sha)? {
                let Some(text) = git.show_file(&mirror, &sha, &path)? else {
                    continue;
                };
                let subdir = path
                    .strip_suffix(manifest::FILE_NAME)
                    .unwrap_or("")
                    .trim_end_matches('/')
                    .to_string();
                let manifest = match Manifest::parse(&text) {
                    Ok(m) => m,
                    Err(err) => {
                        warnings.push(format!("{} {path}: {err:#}", repo.url));
                        continue;
                    }
                };
                if let Err(err) = manifest.validate(None) {
                    warnings.push(format!("{} {path}: {err:#}", repo.url));
                    continue;
                }
                out.push(IndexPlugin {
                    id: manifest.id.trim().to_string(),
                    name: manifest.name.clone(),
                    version: manifest.version.clone(),
                    description: manifest.description.clone(),
                    platforms: manifest.platforms.clone(),
                    min_herdr_version: manifest.min_herdr_version.clone(),
                    source: repo.url.clone(),
                    subdir: (!subdir.is_empty()).then_some(subdir),
                    git_ref: repo.git_ref.clone(),
                    commit: Some(sha.clone()),
                    homepage: util::web_url_for_remote(&repo.url),
                    updated_unix: info.time,
                });
            }
            Ok(out)
        })();
        match result {
            Ok(found) if found.is_empty() => {
                warnings.push(format!("{}: no {} found", repo.url, manifest::FILE_NAME))
            }
            Ok(found) => plugins.extend(found),
            Err(err) => warnings.push(format!("{}: {err:#}", repo.url)),
        }
    }
    plugins.sort_by(|a, b| a.id.cmp(&b.id).then(a.source.cmp(&b.source)));
    let mut seen = std::collections::BTreeMap::new();
    for p in &plugins {
        if let Some(other) = seen.insert(p.id.clone(), p.source.clone()) {
            warnings.push(format!(
                "plugin id {} appears in both {other} and {}",
                p.id, p.source
            ));
        }
    }
    BuildReport {
        index: Index {
            format: FORMAT.into(),
            version: 1,
            name,
            generated_unix: util::now_unix(),
            plugins,
        },
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_lists_parse() {
        let list = parse_repo_list("# comment\nssh://h/a.git main\n\n  /srv/b.git   # local\n");
        assert_eq!(
            list,
            vec![
                RepoSpec {
                    url: "ssh://h/a.git".into(),
                    git_ref: Some("main".into())
                },
                RepoSpec {
                    url: "/srv/b.git".into(),
                    git_ref: None
                },
            ]
        );
    }

    #[test]
    fn index_roundtrip_and_format_check() {
        let index = Index {
            format: FORMAT.into(),
            version: 1,
            name: Some("corp".into()),
            generated_unix: 1,
            plugins: vec![IndexPlugin {
                id: "a.b".into(),
                name: "B".into(),
                version: "1.0.0".into(),
                description: None,
                platforms: Some(vec!["linux".into()]),
                min_herdr_version: Some("0.7.0".into()),
                source: "ssh://h/a.git".into(),
                subdir: None,
                git_ref: None,
                commit: None,
                homepage: None,
                updated_unix: 2,
            }],
        };
        let text = serde_json::to_string(&index).unwrap();
        assert_eq!(Index::parse(&text).unwrap().plugins, index.plugins);
        assert!(Index::parse(r#"{"format":"other","plugins":[]}"#).is_err());
        assert!(Index::parse(r#"{"plugins":[]}"#).is_ok());
    }
}
