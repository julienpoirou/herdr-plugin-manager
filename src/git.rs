//! Plain `git`, nothing forge-specific: every transport git speaks works
//! (https, ssh, scp-like `user@host:path`, `git://`, dumb http, `file://`,
//! local paths). Remote repositories are mirrored into a bare cache so
//! installs, updates, version lookups and the index builder never need a
//! forge API or a raw-file endpoint.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::paths;
use crate::util;

/// Ref under which the cache keeps the remote's HEAD (its default branch).
const REMOTE_HEAD: &str = "refs/hpm/remote-head";

#[derive(Debug, Clone, Copy)]
pub struct Git {
    /// Interactive calls may prompt for credentials or an SSH passphrase;
    /// background calls must fail fast instead.
    pub interactive: bool,
}

#[derive(Debug, Clone)]
pub struct Mirror {
    pub path: PathBuf,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub time: u64,
}

impl Git {
    pub const FOREGROUND: Git = Git { interactive: true };
    pub const BACKGROUND: Git = Git { interactive: false };

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command.env("LC_ALL", "C");
        command.env("GIT_ADVICE", "0");
        if !self.interactive {
            command.env("GIT_TERMINAL_PROMPT", "0");
            if std::env::var_os("GIT_SSH_COMMAND").is_none()
                && std::env::var_os("GIT_SSH").is_none()
            {
                command.env(
                    "GIT_SSH_COMMAND",
                    "ssh -o BatchMode=yes -o ConnectTimeout=10",
                );
            }
            command.stdin(Stdio::null());
        }
        command
    }

    fn timeout(&self, network: bool) -> Option<Duration> {
        match (self.interactive, network) {
            (true, _) => None,
            (false, true) => Some(Duration::from_secs(45)),
            (false, false) => Some(Duration::from_secs(20)),
        }
    }

    /// Runs git and returns trimmed stdout.
    pub fn run(&self, cwd: Option<&Path>, args: &[&str], network: bool) -> Result<String> {
        let mut command = self.command();
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = if self.interactive && network {
            // Let git talk to the terminal (credential and passphrase
            // prompts) while still capturing stdout for parsing.
            command
                .stdin(Stdio::inherit())
                .stderr(Stdio::inherit())
                .stdout(Stdio::piped());
            let child = command
                .spawn()
                .context("could not run git (is it installed?)")?;
            child.wait_with_output()?
        } else {
            util::run_captured(command, self.timeout(network))
                .context("could not run git (is it installed?)")?
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    l.trim_start_matches("fatal: ")
                        .trim_start_matches("error: ")
                })
                .collect::<Vec<_>>()
                .join("; ");
            if message.is_empty() {
                bail!(
                    "git {} failed ({})",
                    args.first().copied().unwrap_or(""),
                    output.status
                );
            }
            bail!("git {}: {message}", args.first().copied().unwrap_or(""));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub fn version(&self) -> Result<String> {
        let out = self.run(None, &["--version"], false)?;
        Ok(out.trim_start_matches("git version ").to_string())
    }

    /// Resolves what a ref currently points to on the remote, without
    /// fetching anything. `None` ref means the remote's default branch.
    /// Exact shas resolve to themselves (they cannot move).
    pub fn ls_remote(&self, url: &str, reference: Option<&str>) -> Result<String> {
        if let Some(reference) = reference {
            if util::is_full_sha(reference) {
                return Ok(reference.to_lowercase());
            }
        }
        let name = reference
            .map(|r| {
                r.trim_start_matches("refs/heads/")
                    .trim_start_matches("refs/tags/")
            })
            .unwrap_or("");
        let patterns: Vec<String> = if name.is_empty() {
            vec!["HEAD".into()]
        } else {
            vec![
                format!("refs/tags/{name}"),
                format!("refs/tags/{name}^{{}}"),
                format!("refs/heads/{name}"),
            ]
        };
        let mut args = vec!["ls-remote", url];
        args.extend(patterns.iter().map(String::as_str));
        let out = self.run(None, &args, true)?;
        parse_ls_remote(&out, name).ok_or_else(|| match reference {
            Some(r) if util::looks_like_sha(r) => {
                anyhow!("{r} looks like an abbreviated commit; use the full sha")
            }
            Some(r) => anyhow!("ref {r:?} not found on {url}"),
            None => anyhow!("{url} has no HEAD (empty repository?)"),
        })
    }

    /// Mirrors (or refreshes) a remote into the cache and returns it.
    pub fn sync(&self, url: &str) -> Result<Mirror> {
        let dir = cache_path_for(url);
        std::fs::create_dir_all(dir.parent().unwrap_or(&dir))?;
        let lock_path = dir.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("could not open {}", lock_path.display()))?;
        lock.lock()?;
        if !dir.join("HEAD").exists() {
            std::fs::create_dir_all(&dir)?;
            self.run(Some(&dir), &["init", "--bare", "--quiet"], false)?;
            // Objects only: never run hooks or follow config from remotes.
            let _ = self.run(Some(&dir), &["config", "gc.auto", "0"], false);
        }
        let head_spec = format!("+HEAD:{REMOTE_HEAD}");
        let result = self.run(
            Some(&dir),
            &[
                "fetch",
                "--quiet",
                "--prune",
                "--force",
                "--no-write-fetch-head",
                url,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
                &head_spec,
            ],
            true,
        );
        drop(lock);
        match result {
            Ok(_) => Ok(Mirror {
                path: dir,
                url: url.to_string(),
            }),
            Err(err) => {
                let text = err.to_string();
                if text.contains("couldn't find remote ref HEAD") {
                    bail!("{url} is an empty repository (no commits yet)");
                }
                Err(err.context(format!("could not fetch {url}")))
            }
        }
    }

    /// Resolves a ref inside a mirror. Branches win over tags of the same
    /// name only when no tag exists, matching how `ls_remote` reports.
    pub fn resolve(&self, mirror: &Mirror, reference: Option<&str>) -> Result<String> {
        let candidates: Vec<String> = match reference {
            None => vec![format!("{REMOTE_HEAD}^{{commit}}")],
            Some(r) => {
                let name = r
                    .trim_start_matches("refs/heads/")
                    .trim_start_matches("refs/tags/");
                let mut c = vec![
                    format!("refs/tags/{name}^{{commit}}"),
                    format!("refs/heads/{name}^{{commit}}"),
                ];
                if util::looks_like_sha(r) {
                    c.insert(0, format!("{r}^{{commit}}"));
                }
                c
            }
        };
        for candidate in &candidates {
            if let Ok(sha) = self.run(
                Some(&mirror.path),
                &["rev-parse", "--verify", "--quiet", candidate],
                false,
            ) {
                if !sha.is_empty() {
                    return Ok(sha);
                }
            }
        }
        match reference {
            Some(r) => bail!("ref {r:?} not found in {}", mirror.url),
            None => bail!("could not resolve the default branch of {}", mirror.url),
        }
    }

    /// Classifies a ref for display: "branch", "tag", "commit" or "default".
    pub fn ref_kind(&self, mirror: &Mirror, reference: Option<&str>) -> &'static str {
        let Some(r) = reference else {
            return "default branch";
        };
        if util::is_full_sha(r) {
            return "commit";
        }
        let name = r
            .trim_start_matches("refs/heads/")
            .trim_start_matches("refs/tags/");
        let has = |full: String| {
            self.run(
                Some(&mirror.path),
                &["rev-parse", "--verify", "--quiet", &full],
                false,
            )
            .is_ok()
        };
        if has(format!("refs/tags/{name}")) {
            "tag"
        } else if has(format!("refs/heads/{name}")) {
            "branch"
        } else {
            "commit"
        }
    }

    pub fn show_file(&self, mirror: &Mirror, sha: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{sha}:{path}");
        match self.run(Some(&mirror.path), &["cat-file", "-e", &spec], false) {
            Ok(_) => {}
            Err(_) => return Ok(None),
        }
        let mut command = self.command();
        command
            .args(["cat-file", "blob", &spec])
            .current_dir(&mirror.path);
        let output = util::run_captured(command, Some(Duration::from_secs(20)))?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
    }

    /// Every `herdr-plugin.toml` in a commit's tree.
    pub fn find_manifests(&self, mirror: &Mirror, sha: &str) -> Result<Vec<String>> {
        let out = self.run(
            Some(&mirror.path),
            &["ls-tree", "-r", "--name-only", sha],
            false,
        )?;
        Ok(out
            .lines()
            .filter(|l| {
                *l == crate::manifest::FILE_NAME
                    || l.ends_with(&format!("/{}", crate::manifest::FILE_NAME))
            })
            .map(str::to_string)
            .collect())
    }

    pub fn commit_info(&self, mirror: &Mirror, sha: &str) -> Result<CommitInfo> {
        let out = self.run(
            Some(&mirror.path),
            &["log", "-1", "--format=%H%x1f%s%x1f%an%x1f%ct", sha],
            false,
        )?;
        let mut parts = out.split('\u{1f}');
        Ok(CommitInfo {
            sha: parts.next().unwrap_or(sha).to_string(),
            subject: util::sanitize(parts.next().unwrap_or("")),
            author: util::sanitize(parts.next().unwrap_or("")),
            time: parts
                .next()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0),
        })
    }

    /// One line per commit in `old..new`, newest first. Empty when `old` is
    /// unknown to the mirror (force-push, different history).
    pub fn log_range(&self, mirror: &Mirror, old: &str, new: &str, limit: usize) -> Vec<String> {
        let range = format!("{old}..{new}");
        let limit = format!("-n{limit}");
        self.run(
            Some(&mirror.path),
            &["log", &limit, "--format=%h %s", &range],
            false,
        )
        .map(|out| out.lines().map(util::sanitize).collect())
        .unwrap_or_default()
    }

    pub fn count_range(&self, mirror: &Mirror, old: &str, new: &str) -> Option<usize> {
        let range = format!("{old}..{new}");
        self.run(Some(&mirror.path), &["rev-list", "--count", &range], false)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// Materialises `sha` into a fresh working tree at `dest`.
    pub fn checkout(&self, mirror: &Mirror, sha: &str, dest: &Path) -> Result<()> {
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        let mirror_path = mirror.path.display().to_string();
        let dest_str = dest.display().to_string();
        self.run(
            None,
            &["clone", "--quiet", "--no-checkout", &mirror_path, &dest_str],
            false,
        )?;
        // The default-branch ref lives outside refs/heads, so bring it along
        // in case the commit is only reachable from there.
        let spec = format!("+{REMOTE_HEAD}:{REMOTE_HEAD}");
        let _ = self.run(
            Some(dest),
            &["fetch", "--quiet", &mirror_path, &spec],
            false,
        );
        self.run(Some(dest), &["checkout", "--quiet", "--detach", sha], false)
            .with_context(|| format!("could not check out {sha}"))?;
        let _ = self.run(
            Some(dest),
            &["remote", "set-url", "origin", &mirror.url],
            false,
        );
        if dest.join(".gitmodules").exists() {
            // Submodule URLs relative to the original remote resolve
            // correctly now that origin points back at it.
            let _ = Git::FOREGROUND.run(
                Some(dest),
                &["submodule", "update", "--init", "--recursive", "--quiet"],
                true,
            );
        }
        Ok(())
    }
}

/// Picks the commit for `name` out of `git ls-remote` output: a peeled
/// annotated tag, then a tag, then a branch; `HEAD` when no name is given.
pub fn parse_ls_remote(out: &str, name: &str) -> Option<String> {
    let mut peeled = None;
    let mut tag = None;
    let mut branch = None;
    let mut head = None;
    for line in out.lines() {
        let mut fields = line.split_whitespace();
        let (Some(sha), Some(reference)) = (fields.next(), fields.next()) else {
            continue;
        };
        let sha = Some(sha.to_string());
        if name.is_empty() {
            if reference == "HEAD" {
                head = sha;
            }
        } else if reference == format!("refs/tags/{name}^{{}}") {
            peeled = sha;
        } else if reference == format!("refs/tags/{name}") {
            tag = sha;
        } else if reference == format!("refs/heads/{name}") {
            branch = sha;
        }
    }
    peeled.or(tag).or(branch).or(head)
}

/// `<cache>/repos/<readable>-<hash>.git` so the same remote always maps to
/// the same mirror while staying recognisable on disk.
pub fn cache_path_for(url: &str) -> PathBuf {
    let readable = util::slug(
        url.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("ssh://")
            .trim_start_matches("git://")
            .trim_start_matches("file://"),
        48,
    );
    paths::repo_cache_dir().join(format!("{readable}-{}.git", &util::fnv1a64(url)[..12]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_remote_prefers_peeled_tags() {
        let out = "\
1111111111111111111111111111111111111111\trefs/tags/v1
2222222222222222222222222222222222222222\trefs/tags/v1^{}
3333333333333333333333333333333333333333\trefs/heads/v1
";
        assert_eq!(parse_ls_remote(out, "v1").unwrap(), "2".repeat(40));
        let out = "3333333333333333333333333333333333333333\trefs/heads/main\n";
        assert_eq!(parse_ls_remote(out, "main").unwrap(), "3".repeat(40));
        let out = "4444444444444444444444444444444444444444\tHEAD\n";
        assert_eq!(parse_ls_remote(out, "").unwrap(), "4".repeat(40));
        assert!(parse_ls_remote("", "main").is_none());
    }

    #[test]
    fn cache_paths_are_stable_and_distinct() {
        let a = cache_path_for("ssh://git@host/a.git");
        assert_eq!(a, cache_path_for("ssh://git@host/a.git"));
        assert_ne!(a, cache_path_for("ssh://git@host/b.git"));
        assert!(a.to_string_lossy().ends_with(".git"));
    }
}
