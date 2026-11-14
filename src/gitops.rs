//! Plaintext, explicitly selected configuration snapshots and reviewed Git sync.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::git::{self, Git};
use crate::herdr::Herdr;
use crate::manifest::{self, Manifest};
use crate::ops::{self, Ctx, GitRequest, Mode, Origin};
use crate::{paths, util};

const SCHEMA: u32 = 1;
const MAX_CONFIG: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginState {
    id: String,
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subdir: Option<String>,
    commit: String,
    version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    follow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin: Option<String>,
    enabled: bool,
    auto_update: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalOnly {
    id: String,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema: u32,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    plugins: Vec<PluginState>,
    #[serde(default)]
    local_only: Vec<LocalOnly>,
}

struct Snapshot {
    metadata: Metadata,
    files: BTreeMap<String, Vec<u8>>,
    /// Whether `metadata.plugins`/`local_only` hold this machine's plugins.
    /// False when `plugins.toml` is off here: the snapshot's entries are kept.
    plugins_included: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    plugins: BTreeMap<String, PluginState>,
    local_only: BTreeMap<String, String>,
    files: BTreeMap<String, Option<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    remote: String,
    branch: String,
    commit: String,
    summary: Summary,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    #[serde(default)]
    captured: BTreeMap<String, String>,
    #[serde(default)]
    baseline: Option<Baseline>,
    #[serde(default)]
    preserved_files: BTreeSet<String>,
    #[serde(default)]
    removed: BTreeSet<String>,
    /// Plugins were left out of a capture/pull since the last successful
    /// plugin sync, so the baseline's plugin part no longer describes this
    /// machine: once back on, plugins follow first-sync rules until a pull.
    #[serde(default)]
    plugins_off: bool,
}

struct Session {
    _lock: Option<File>,
    directory: PathBuf,
    state_path: PathBuf,
    state: State,
    preview: Option<PreviewRepo>,
}

#[derive(Clone)]
struct Blob {
    oid: String,
    mode: String,
}

type Tree = BTreeMap<String, Blob>;

struct PreviewRepo {
    directory: PathBuf,
}

impl PreviewRepo {
    fn new(git: Git, branch: &str) -> Result<Self> {
        validate_branch(git, branch)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let temporary_root = trusted_root(&std::env::temp_dir())?;
        for attempt in 0..100 {
            let directory = temporary_root.join(format!(
                "herdr-pm-gitops-preview-{}-{nonce}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let preview = Self { directory };
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&preview.directory, fs::Permissions::from_mode(0o700))?;
                    }
                    git.run(
                        Some(&preview.directory),
                        &["init", "--quiet", "--initial-branch", branch],
                        false,
                    )?;
                    return Ok(preview);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not create a private GitOps preview directory")
    }
}

impl Drop for PreviewRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hash = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(hash.len() * 2);
    for &byte in hash.iter() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}

/// The most recent state file named after a snapshot-directory digest, from
/// before the snapshot repository had a fixed location.
fn legacy_state(state_dir: &Path) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries {
        let path = entry?.path();
        let digest_named = path.extension().is_some_and(|e| e == "json")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        let meta = fs::symlink_metadata(&path)?;
        if !digest_named || !meta.is_file() {
            continue;
        }
        let modified = meta.modified()?;
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, path));
        }
    }
    Ok(latest.map(|(_, path)| path))
}

/// Resolve only a machine-local directory authority, before adding any
/// selector-derived children. New roots inherit their nearest existing
/// parent's physical location without requiring directories to be created.
fn trusted_root(path: &Path) -> Result<PathBuf> {
    let path = absolute(path)?;
    match fs::canonicalize(&path) {
        Ok(resolved) => {
            if !fs::metadata(&resolved)?.is_dir() {
                bail!(
                    "configured local root is not a directory: {}",
                    path.display()
                );
            }
            Ok(without_verbatim_prefix(resolved))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(error).with_context(|| {
                        format!("could not resolve configured local root {}", path.display())
                    })
                }
                Err(inspect) if inspect.kind() == std::io::ErrorKind::NotFound => {}
                Err(inspect) => {
                    return Err(inspect).with_context(|| {
                        format!("could not inspect configured local root {}", path.display())
                    })
                }
            }
            let parent = path.parent().ok_or_else(|| {
                anyhow!(
                    "configured local root has no existing parent: {}",
                    path.display()
                )
            })?;
            let name = path.file_name().ok_or_else(|| {
                anyhow!(
                    "configured local root has no directory name: {}",
                    path.display()
                )
            })?;
            Ok(trusted_root(parent)?.join(name))
        }
        Err(error) => Err(error)
            .with_context(|| format!("could not resolve configured local root {}", path.display())),
    }
}

/// `canonicalize` returns `\\?\C:\…` on Windows, which Git reads as a
/// remote host when such a path is passed as an argument (`git fetch`).
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        PathBuf::from(local)
    } else {
        path
    }
}

/// A configured filename authorizes its parent directory, not a symlink at
/// the file itself. Never call this with a selector-derived child path.
fn authority_file(path: &Path) -> Result<PathBuf> {
    let path = absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("configured file has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("configured file has no filename: {}", path.display()))?;
    Ok(trusted_root(parent)?.join(name))
}

fn plugin_config_root() -> Result<PathBuf> {
    trusted_root(&paths::herdr_config_dir().join("plugins").join("config"))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Reject links at every existing component below already-resolved authority
/// roots, including the final file. Selector-derived paths are never resolved.
fn no_links(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) if is_link(&meta) => {
                bail!("symlink traversal is not allowed: {}", ancestor.display())
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("could not inspect {}", ancestor.display()))
            }
        }
    }
    Ok(())
}

fn component(value: &str) -> Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.ends_with(['.', ' '])
        || value.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        bail!("unsafe or non-portable path component {value:?}");
    }
    let stem = value.split('.').next().unwrap_or("").to_ascii_lowercase();
    if matches!(
        stem.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || (stem.len() == 4
        && (stem.starts_with("com") || stem.starts_with("lpt"))
        && stem.as_bytes()[3].is_ascii_digit())
        || value.eq_ignore_ascii_case(".git")
    {
        bail!("reserved path component {value:?}");
    }
    Ok(())
}

fn relative(value: &str) -> Result<()> {
    if value.len() > 4096 {
        bail!("snapshot path is too long");
    }
    for part in value.split('/') {
        component(part)?;
    }
    Ok(())
}

fn plugin_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
    {
        bail!("invalid Herdr plugin id {value:?}");
    }
    Ok(())
}

fn config_file(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.split('/').any(|part| {
        matches!(
            part,
            "logs"
                | "log"
                | "state"
                | "cache"
                | "caches"
                | "bin"
                | "target"
                | "node_modules"
                | ".git"
        )
    }) {
        return false;
    }
    let name = lower.rsplit('/').next().unwrap_or("");
    !matches!(
        name,
        "history"
            | ".history"
            | "history.json"
            | "history.jsonl"
            | "history.ndjson"
            | "history.txt"
            | "state.json"
            | "cache.json"
    ) && !name.ends_with(".log")
        && !name.contains(".log.")
        && ![".pid", ".lock", ".sock"]
            .iter()
            .any(|suffix| name.ends_with(*suffix))
}

fn selector(value: &str) -> Result<()> {
    relative(value)?;
    match value {
        "herdr/config.toml" | "manager/config.toml" => Ok(()),
        _ => {
            let mut parts = value.splitn(3, '/');
            if parts.next() != Some("plugins") {
                bail!("unknown configuration selector {value:?}");
            }
            let id = parts.next().unwrap_or("");
            component(id)?;
            if id.eq_ignore_ascii_case(paths::PLUGIN_ID)
                || id.eq_ignore_ascii_case(paths::OLD_PLUGIN_ID)
            {
                bail!("select manager/config.toml for manager configuration, not {value:?}");
            }
            let file = parts.next().unwrap_or("");
            relative(file)?;
            if !config_file(file) {
                bail!("{value:?} is not a supported plaintext configuration file (logs, state, caches and binaries are excluded)");
            }
            Ok(())
        }
    }
}

fn live_path(value: &str) -> Result<PathBuf> {
    selector(value)?;
    Ok(match value {
        "herdr/config.toml" => authority_file(&paths::herdr_config_toml())?,
        "manager/config.toml" => authority_file(&paths::pm_config_file())?,
        _ => {
            let mut parts = value.splitn(3, '/');
            parts.next();
            plugin_config_root()?
                .join(parts.next().unwrap())
                .join(parts.next().unwrap())
        }
    })
}

fn plaintext(bytes: &[u8], name: &str) -> Result<()> {
    if bytes.len() as u64 > MAX_CONFIG || bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
    {
        bail!("{name} is not a UTF-8 plaintext configuration of at most 4 MiB");
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    no_links(path)?;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("could not inspect {}", path.display()))
        }
    };
    if !meta.is_file() {
        bail!("not a regular file: {}", path.display());
    }
    if meta.len() > MAX_CONFIG {
        bail!("configuration exceeds 4 MiB: {}", path.display());
    }
    Ok(Some(fs::read(path).with_context(|| {
        format!("could not read {}", path.display())
    })?))
}

fn portable_manager(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut doc: toml_edit::DocumentMut = std::str::from_utf8(bytes)?.parse()?;
    doc.remove("backup");
    doc.remove("log_dir");
    if doc.as_table().is_empty() {
        return Ok(Vec::new());
    }
    let text = doc.to_string();
    Config::parse(&text)?;
    Ok(text.into_bytes())
}

fn restored_manager(bytes: Option<&[u8]>, local: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut remote: toml_edit::DocumentMut =
        std::str::from_utf8(bytes.unwrap_or_default())?.parse()?;
    if remote.contains_key("backup") || remote.contains_key("log_dir") {
        bail!("manager/config.toml contains machine-local backup or log_dir settings; remove them from the remote snapshot");
    }
    if let Some(local) = local {
        let doc: toml_edit::DocumentMut = std::str::from_utf8(local)?.parse()?;
        for key in ["backup", "log_dir"] {
            if let Some(item) = doc.get(key) {
                remote.insert(key, item.clone());
            }
        }
    }
    let text = remote.to_string();
    Config::parse(&text)?;
    Ok(text.into_bytes())
}

impl Session {
    fn open(config: &Config) -> Result<Self> {
        Self::open_inner(config, None)
    }

    fn operation(ctx: &Ctx, git: Git) -> Result<Self> {
        Self::open_inner(&ctx.config, if ctx.dry_run { Some(git) } else { None })
    }

    fn open_inner(config: &Config, preview_git: Option<Git>) -> Result<Self> {
        let home = trusted_root(&paths::pm_home())?;
        let lock_path = home.join(".gitops.lock");
        no_links(&lock_path)?;
        let lock = if preview_git.is_some() {
            match OpenOptions::new().read(true).write(true).open(&lock_path) {
                Ok(lock) => {
                    lock.lock()?;
                    Some(lock)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        } else {
            fs::create_dir_all(lock_path.parent().unwrap())?;
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            lock.lock()?;
            Some(lock)
        };
        let directory = trusted_root(&config.backup.directory())?;
        no_links(&directory)?;
        for protected in [
            trusted_root(&paths::herdr_config_dir())?,
            authority_file(&paths::herdr_config_toml())?,
            trusted_root(&paths::pm_config_dir())?,
            trusted_root(&paths::installs_dir())?,
            trusted_root(&paths::repo_cache_dir())?,
        ] {
            if protected.starts_with(&directory) || directory.starts_with(&protected) {
                bail!(
                    "backup directory overlaps live configuration or installation files: {}",
                    directory.display()
                );
            }
        }
        let state_dir = home.join("gitops");
        let state_path = state_dir.join("state.json");
        no_links(&state_path)?;
        // The state used to be named after a digest of a configurable
        // snapshot directory; adopt the latest such file once.
        let legacy = if state_path.exists() {
            None
        } else {
            legacy_state(&state_dir)?
        };
        let source = legacy.as_deref().unwrap_or(&state_path);
        let state: State = match fs::read(source) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("invalid GitOps state {}; move it aside only after resolving local snapshot changes", source.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(err) => return Err(err.into()),
        };
        if let (Some(legacy), None) = (&legacy, &preview_git) {
            fs::rename(legacy, &state_path)
                .with_context(|| format!("could not adopt GitOps state {}", legacy.display()))?;
        }
        for path in state
            .captured
            .keys()
            .chain(state.preserved_files.iter())
            .chain(state.removed.iter())
        {
            if path != "plugins.toml" {
                selector(path).context("invalid owned path in local GitOps state")?;
            }
        }
        let preview = if let Some(git) = preview_git {
            let preview = PreviewRepo::new(git, &config.backup.branch)?;
            no_links(&directory.join(".git"))?;
            if directory.join(".git").exists() && !directory.join(".git").is_dir() {
                bail!("backup must be a dedicated Git worktree with its own .git directory");
            }
            if directory.join(".git").is_dir() {
                if let Ok(head) = git.run(
                    Some(&directory),
                    &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
                    false,
                ) {
                    let source = directory.to_string_lossy();
                    git.run(
                        Some(&preview.directory),
                        &[
                            "fetch",
                            "--quiet",
                            "--no-tags",
                            "--no-recurse-submodules",
                            "--",
                            &source,
                            "HEAD:refs/herdr-preview/local-head",
                        ],
                        false,
                    )?;
                    git.run(
                        Some(&preview.directory),
                        &[
                            "update-ref",
                            &format!("refs/heads/{}", config.backup.branch),
                            &head,
                        ],
                        false,
                    )?;
                }
            }
            Some(preview)
        } else {
            None
        };
        Ok(Self {
            _lock: lock,
            directory,
            state_path,
            state,
            preview,
        })
    }

    fn git_directory(&self) -> &Path {
        self.preview
            .as_ref()
            .map(|preview| preview.directory.as_path())
            .unwrap_or(&self.directory)
    }

    fn save(&self) -> Result<()> {
        write_changed(&self.state_path, &serde_json::to_vec_pretty(&self.state)?)
    }

    fn initialize(&self, git: Git, branch: &str) -> Result<()> {
        let git_dir = self.git_directory().join(".git");
        no_links(&git_dir)?;
        if git_dir.exists() {
            if !git_dir.is_dir() {
                bail!("backup must be a dedicated Git worktree with its own .git directory");
            }
            return Ok(());
        }
        validate_branch(git, branch)?;
        fs::create_dir_all(self.git_directory())?;
        git.run(
            Some(self.git_directory()),
            &["init", "--quiet", "--initial-branch", branch],
            false,
        )?;
        Ok(())
    }

    fn branch(&self, git: Git, branch: &str) -> Result<()> {
        validate_branch(git, branch)?;
        let expected = format!("refs/heads/{branch}");
        let actual = git.run(
            Some(if self.directory.join(".git").is_dir() {
                &self.directory
            } else {
                self.git_directory()
            }),
            &["symbolic-ref", "--quiet", "HEAD"],
            false,
        )?;
        if actual != expected {
            bail!("backup worktree is on {actual}, but backup.branch is {branch:?}; select the intended branch explicitly before syncing");
        }
        Ok(())
    }

    fn head(&self, git: Git) -> Option<String> {
        git.run(
            Some(self.git_directory()),
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            false,
        )
        .ok()
    }

    /// The committed `plugins.toml` at HEAD: the snapshot last pulled or pushed.
    fn head_metadata(&self, git: Git) -> Result<Option<Vec<u8>>> {
        let Some(head) = self.head(git) else {
            return Ok(None);
        };
        let tree = tree(git, self.git_directory(), &head)?;
        let Some(value) = tree.get("plugins.toml") else {
            return Ok(None);
        };
        configuration_blob(git, self.git_directory(), value, "plugins.toml").map(Some)
    }

    fn capture(&mut self, git: Git, snapshot: &Snapshot) -> Result<Metadata> {
        let previous = if snapshot.plugins_included {
            None
        } else {
            self.head_metadata(git)?
        };
        self.write_snapshot(snapshot, previous.as_deref())
    }

    /// `previous` is the committed `plugins.toml`, consulted only when this
    /// machine's plugins are not included: its plugin entries are kept, and
    /// its exact bytes when nothing else changed. Without one, no entries
    /// are written; push refuses to publish that over an existing remote.
    fn write_snapshot(&mut self, snapshot: &Snapshot, previous: Option<&[u8]>) -> Result<Metadata> {
        let mut metadata = snapshot.metadata.clone();
        let previous = match previous {
            Some(bytes) if !snapshot.plugins_included => {
                let parsed = parse_metadata(bytes)?;
                metadata.plugins = parsed.plugins.clone();
                metadata.local_only = parsed.local_only.clone();
                Some((bytes, parsed))
            }
            _ => {
                if !snapshot.plugins_included {
                    // Never publish this machine's plugins while off.
                    metadata.plugins.clear();
                    metadata.local_only.clear();
                }
                None
            }
        };
        let mut files: BTreeSet<String> = metadata.files.iter().cloned().collect();
        files.extend(self.state.preserved_files.iter().cloned());
        let mut portable = BTreeSet::new();
        for path in &files {
            if !portable.insert(path.to_ascii_lowercase()) {
                bail!("configuration selections collide on a case-insensitive filesystem: {path}");
            }
        }
        metadata.files = files.into_iter().collect();
        let encoded = match previous {
            Some((bytes, parsed)) if parsed == metadata => bytes.to_vec(),
            _ => toml::to_string_pretty(&metadata)?.into_bytes(),
        };
        let retained: BTreeSet<&str> = self
            .state
            .preserved_files
            .iter()
            .filter(|path| !snapshot.metadata.files.contains(path))
            .map(String::as_str)
            .collect();
        let mut desired: BTreeMap<&str, &[u8]> = snapshot
            .files
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
            .collect();
        desired.insert("plugins.toml", &encoded);
        let mut conflicts = Vec::new();
        let paths: BTreeSet<&str> = desired
            .keys()
            .copied()
            .chain(self.state.captured.keys().map(String::as_str))
            .collect();
        for path in paths {
            let current = read_file(&self.directory.join(path))?;
            let previous = self.state.captured.get(path);
            let wanted = desired.get(path);
            match current {
                Some(current) => {
                    if wanted.is_some_and(|bytes| *bytes == current) {
                        continue;
                    }
                    if previous.is_none_or(|hash| *hash != digest(&current)) {
                        conflicts.push(path.to_owned());
                    }
                }
                None if previous.is_some() && (wanted.is_some() || retained.contains(path)) => {
                    conflicts.push(path.to_owned())
                }
                None => {}
            }
        }
        stop_conflicts("snapshot worktree was edited outside capture", conflicts)?;
        for (path, bytes) in &desired {
            write_changed(&self.directory.join(path), bytes)?;
            snapshot_mode(&self.directory.join(path), false)?;
        }
        for path in self.state.captured.keys() {
            if !desired.contains_key(path.as_str()) && !retained.contains(path.as_str()) {
                remove_file(&self.directory.join(path))?;
                self.state.removed.insert(path.clone());
            }
        }
        self.state
            .captured
            .retain(|path, _| retained.contains(path.as_str()));
        self.state.captured.extend(
            desired
                .iter()
                .map(|(path, bytes)| ((*path).to_owned(), digest(bytes))),
        );
        self.state
            .removed
            .retain(|path| !desired.contains_key(path.as_str()));
        self.state.plugins_off |= !snapshot.plugins_included;
        self.save()?;
        Ok(metadata)
    }
}

fn validate_branch(git: Git, branch: &str) -> Result<()> {
    if branch.is_empty() || branch.starts_with('-') || branch.chars().any(char::is_control) {
        bail!("unsafe backup branch {branch:?}");
    }
    let checked = git
        .run(None, &["check-ref-format", "--branch", branch], false)
        .with_context(|| format!("invalid backup.branch {branch:?}"))?;
    if checked != branch {
        bail!("backup.branch must be a literal branch name, not a checkout expression");
    }
    Ok(())
}

fn write_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    no_links(path)?;
    match fs::read(path) {
        Ok(current) if current == bytes => return Ok(()),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // The cross-process GitOps lock covers this write and its temporary file.
    let temp = path.with_file_name(format!(
        ".{}.gitops-tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    no_links(&temp)?;
    if temp.exists() {
        bail!(
            "leftover GitOps temporary file {}; inspect and remove it before retrying",
            temp.display()
        );
    }
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("could not write {}", path.display()))
}

fn remove_file(path: &Path) -> Result<()> {
    no_links(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("could not remove {}", path.display())),
    }
}

fn collect(herdr: &Herdr, config: &Config) -> Result<Snapshot> {
    let mut plugins = Vec::new();
    let mut local_only = Vec::new();
    // With `plugins.toml` off, this machine's plugins are not even read.
    let rows = if config.backup.plugins {
        ops::rows(herdr)?
    } else {
        Vec::new()
    };
    for row in rows {
        let id = row.plugin.plugin_id.clone();
        plugin_id(&id)?;
        let Some((url, _)) = row.remote() else {
            local_only.push(LocalOnly {
                id,
                reason: "local link: no reproducible Git source; plugin code is NOT backed up"
                    .into(),
            });
            continue;
        };
        let Some(commit) = row.commit().filter(|sha| util::is_full_sha(sha)) else {
            local_only.push(LocalOnly {
                id,
                reason: "source has no exact full Git commit; plugin code is NOT backed up".into(),
            });
            continue;
        };
        if row
            .pin
            .as_deref()
            .is_some_and(|pin| !pin.eq_ignore_ascii_case(commit))
        {
            bail!("{} is running a different commit from its recorded pin; restore or explicitly repin it before capture", row.plugin.plugin_id);
        }
        let pin = if row.pinned() {
            Some(commit.to_ascii_lowercase())
        } else {
            None
        };
        let follow_ref = pin
            .clone()
            .or_else(|| row.requested_ref().map(str::to_owned));
        let subdir = match row.origin {
            Origin::Git => row.record.as_ref().and_then(|record| record.subdir.clone()),
            _ => row.plugin.source.subdir.clone(),
        };
        let entry = PluginState {
            id,
            url,
            subdir,
            commit: commit.to_ascii_lowercase(),
            version: row.plugin.version.clone(),
            follow_ref,
            pin,
            enabled: row.plugin.enabled,
            auto_update: row.auto_update,
        };
        validate_plugin(&entry)?;
        plugins.push(entry);
    }
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    local_only.sort_by(|a, b| a.id.cmp(&b.id));
    let mut selections = BTreeSet::new();
    let mut files = BTreeMap::new();
    for value in &config.backup.files {
        selector(value)?;
        if !selections.insert(value.clone()) {
            bail!("configuration selected twice: {value}");
        }
        if let Some(mut bytes) = read_file(&live_path(value)?)? {
            plaintext(&bytes, value)?;
            if value == "manager/config.toml" {
                bytes = portable_manager(&bytes)?;
            }
            files.insert(value.clone(), bytes);
        }
    }
    Ok(Snapshot {
        metadata: Metadata {
            schema: SCHEMA,
            files: selections.into_iter().collect(),
            plugins,
            local_only,
        },
        files,
        plugins_included: config.backup.plugins,
    })
}

fn summary(snapshot: &Snapshot) -> Summary {
    let mut result = Summary {
        files: snapshot
            .metadata
            .files
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    snapshot
                        .files
                        .get(path)
                        .filter(|bytes| path != "manager/config.toml" || !bytes.is_empty())
                        .map(|bytes| digest(bytes)),
                )
            })
            .collect(),
        ..Summary::default()
    };
    plugin_summary(&mut result, &snapshot.metadata);
    result
}

fn plugin_summary(summary: &mut Summary, metadata: &Metadata) {
    summary.plugins = metadata
        .plugins
        .iter()
        .map(|plugin| (plugin.id.clone(), plugin.clone()))
        .collect();
    summary.local_only = metadata
        .local_only
        .iter()
        .map(|plugin| (plugin.id.clone(), plugin.reason.clone()))
        .collect();
}

fn validate_plugin(plugin: &PluginState) -> Result<()> {
    plugin_id(&plugin.id)?;
    git::validate_remote(&plugin.url)
        .with_context(|| format!("unsafe source for {}", plugin.id))?;
    if !util::is_full_sha(&plugin.commit) || plugin.commit != plugin.commit.to_ascii_lowercase() {
        bail!(
            "{} must specify an exact lowercase full Git commit",
            plugin.id
        );
    }
    if let Some(subdir) = &plugin.subdir {
        relative(subdir)?;
    }
    if let Some(reference) = &plugin.follow_ref {
        if reference.is_empty()
            || reference.starts_with('-')
            || reference.chars().any(char::is_control)
        {
            bail!("unsafe followed ref for {}", plugin.id);
        }
    }
    if let Some(pin) = &plugin.pin {
        if pin != &plugin.commit || plugin.follow_ref.as_ref() != Some(pin) {
            bail!(
                "{} has an incoherent pin: pin and followed SHA must equal its exact commit",
                plugin.id
            );
        }
    } else if plugin.follow_ref.as_deref().is_some_and(util::is_full_sha) {
        bail!(
            "{} follows a full SHA but does not declare the matching pin",
            plugin.id
        );
    }
    Ok(())
}

/// Local only: no fetch, commit, staging or push. Unchanged bytes are not copied.
pub fn capture(herdr: &Herdr, config: &Config) -> Result<()> {
    let mut session = Session::open(config)?;
    let snapshot = collect(herdr, config)?;
    session.initialize(Git::BACKGROUND, &config.backup.branch)?;
    session.capture(Git::BACKGROUND, &snapshot).map(|_| ())
}

fn operation_git(ctx: &Ctx) -> Git {
    if ctx.report.interactive() {
        Git::FOREGROUND
    } else {
        Git::UNATTENDED
    }
}

fn remote(config: &Config) -> Result<&str> {
    let remote = config.backup.remote.as_deref().ok_or_else(|| {
        anyhow!("set backup.remote to your Git repository before pushing or pulling")
    })?;
    git::validate_remote(remote)?;
    Ok(remote)
}

fn stop_conflicts(reason: &str, mut paths: Vec<String>) -> Result<()> {
    paths.sort();
    paths.dedup();
    if !paths.is_empty() {
        bail!("GitOps conflict: {reason}\n  {}\nStopped without overwriting the conflicting paths. Any earlier successfully reviewed plugin installations remain installed. Commit/push the intended local state, or restore these paths to the last sync before pulling; no automatic merge or overwrite is performed.", paths.join("\n  "));
    }
    Ok(())
}

fn approval(ctx: &Ctx, title: &str, lines: &[String], question: &str) -> Result<bool> {
    ctx.report.review(
        title,
        lines,
        if ctx.yes || ctx.dry_run {
            None
        } else {
            Some(question)
        },
    )
}

fn unsupported_lines(snapshot: &Snapshot, lines: &mut Vec<String>) {
    for plugin in &snapshot.metadata.local_only {
        lines.push(format!(
            "  LOCAL ONLY {}: {}",
            util::sanitize(&plugin.id),
            util::sanitize(&plugin.reason)
        ));
    }
}

fn plugin_scope_line(snapshot: &Snapshot) -> String {
    if snapshot.plugins_included {
        "  plugin state: included (plugins.toml on: sources, versions, enabled, auto-update)".into()
    } else {
        "  plugin state: NOT included (plugins.toml off on this machine); the snapshot's plugin entries are kept unchanged".into()
    }
}

/// Capture and review a real Git commit, then perform a normal fast-forward push.
pub fn push(ctx: &Ctx) -> Result<()> {
    let remote = remote(&ctx.config)?;
    let git = operation_git(ctx);
    let mut session = Session::operation(ctx, git)?;
    session.initialize(git, &ctx.config.backup.branch)?;
    session.branch(git, &ctx.config.backup.branch)?;
    let snapshot = collect(&ctx.herdr, &ctx.config)?;
    push_preflight(git, &session, remote, &ctx.config.backup.branch)?;
    if snapshot.plugins_included && session.state.plugins_off && session.head(git).is_some() {
        bail!("plugins.toml was turned back on after this machine skipped plugin sync, so its plugins were never reconciled with the snapshot. Run `herdr-pm pull` first (as on a first sync: differing plugins stop the pull, missing ones are installed after review, extra local ones are kept), then run `herdr-pm push` again. No local commit was created");
    }
    if ctx.dry_run {
        let mut lines = vec![
            if snapshot.plugins_included {
                format!(
                    "  capture {} reproducible plugins and {} selected files",
                    snapshot.metadata.plugins.len(),
                    snapshot.files.len()
                )
            } else {
                format!("  capture {} selected files", snapshot.files.len())
            },
            plugin_scope_line(&snapshot),
            format!(
                "  push to {} ({})",
                util::sanitize(remote),
                ctx.config.backup.branch
            ),
        ];
        unsupported_lines(&snapshot, &mut lines);
        approval(ctx, "GitOps push (dry run)", &lines, "Push this snapshot?")?;
        return Ok(());
    }
    let captured_metadata = session.capture(git, &snapshot)?;
    // Never absorb a user's staged files into a snapshot commit.
    let staged = git.run(
        Some(&session.directory),
        &["diff", "--cached", "--name-only"],
        false,
    )?;
    if !staged.is_empty() {
        bail!("backup repository already has staged changes; commit or unstage them before push:\n{staged}");
    }
    let mut owned: BTreeSet<String> = session.state.captured.keys().cloned().collect();
    if let Some(head) = session.head(git) {
        let previous_tree = tree(git, &session.directory, &head)?;
        if previous_tree.contains_key("plugins.toml") {
            let previous = load_snapshot(git, &session.directory, &previous_tree)?;
            owned.extend(previous.metadata.files);
        }
    }
    let owned: Vec<String> = owned
        .into_iter()
        .filter(|path| {
            session.directory.join(path).exists() || is_tracked(git, &session.directory, path)
        })
        .collect();
    for path in &owned {
        if let Err(error) = git.run(Some(&session.directory), &["add", "--", path], false) {
            unstage(
                git,
                &session.directory,
                session.head(git).as_deref(),
                &owned,
            )?;
            return Err(error);
        }
    }
    let diff = git.run(
        Some(&session.directory),
        &[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
        ],
        false,
    )?;
    let patch = git.run(
        Some(&session.directory),
        &["diff", "--cached", "--no-ext-diff", "--no-textconv", "--"],
        false,
    )?;
    let mut lines = vec![
        format!("  repository {}", session.directory.display()),
        format!("  remote {}", util::sanitize(remote)),
        format!("  branch {}", ctx.config.backup.branch),
        plugin_scope_line(&snapshot),
    ];
    lines.extend(diff.lines().map(util::sanitize));
    lines.extend(patch.lines().map(util::sanitize));
    if diff.is_empty() {
        lines.push("  no snapshot changes to commit; push existing commits only".into());
    }
    unsupported_lines(&snapshot, &mut lines);
    let approved = match approval(
        ctx,
        "GitOps push — selected configuration is PLAINTEXT",
        &lines,
        "Commit and push this snapshot?",
    ) {
        Ok(approved) => approved,
        Err(error) => {
            unstage(
                git,
                &session.directory,
                session.head(git).as_deref(),
                &owned,
            )?;
            return Err(error);
        }
    };
    if !approved {
        unstage(
            git,
            &session.directory,
            session.head(git).as_deref(),
            &owned,
        )?;
        return Ok(());
    }
    if !diff.is_empty() {
        if let Err(error) = git.run(
            Some(&session.directory),
            &[
                "commit",
                "-m",
                "Update Herdr plugin and configuration snapshot",
            ],
            false,
        ) {
            unstage(
                git,
                &session.directory,
                session.head(git).as_deref(),
                &owned,
            )?;
            return Err(error);
        }
    }
    let commit = session
        .head(git)
        .ok_or_else(|| anyhow!("snapshot has no commit to push"))?;
    let refspec = format!("HEAD:refs/heads/{}", ctx.config.backup.branch);
    ctx.report.step("pushing snapshot (fast-forward only)");
    git.run(Some(&session.directory), &["push", "--", remote, &refspec], true)
        .with_context(|| format!("snapshot push failed; the local commit is retained in {}. If the remote advanced during review, resolve the Git history manually in this backup repository before retrying; pull will not merge divergent commits. No force push was attempted", session.directory.display()))?;
    let mut synced = summary(&snapshot);
    if !snapshot.plugins_included {
        // Not published from here: the baseline holds the snapshot's plugins.
        plugin_summary(&mut synced, &captured_metadata);
    }
    session.state.baseline = Some(Baseline {
        remote: remote.into(),
        branch: ctx.config.backup.branch.clone(),
        commit,
        summary: synced,
    });
    session
        .state
        .preserved_files
        .extend(captured_metadata.files);
    session.state.removed.clear();
    // Refused above while re-enabled with history, so a plugin push here
    // published this machine's plugins as the first to sync them.
    session.state.plugins_off = !snapshot.plugins_included;
    session.save()?;
    ctx.report.line("Snapshot pushed. Only explicitly selected configuration files were included; local-only plugin code was not backed up.");
    Ok(())
}

fn push_preflight(git: Git, session: &Session, remote: &str, branch: &str) -> Result<()> {
    let branch_ref = format!("refs/heads/{branch}");
    let listing = git.run(
        None,
        &["ls-remote", "--refs", "--", remote, &branch_ref],
        true,
    )?;
    if listing.trim().is_empty() {
        return Ok(());
    }
    let reference = format!("refs/remotes/herdr-backup/{branch}");
    let refspec = format!("{branch_ref}:{reference}");
    git.run(
        Some(session.git_directory()),
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            remote,
            &refspec,
        ],
        true,
    )?;
    let Some(head) = session.head(git) else {
        bail!("the remote backup branch already exists, but this backup repository has no local commit. Run `herdr-pm pull` to adopt its history, then run `herdr-pm push` again; no local commit was created");
    };
    let ancestor = |older: &str, newer: &str| {
        git.run(
            Some(session.git_directory()),
            &["merge-base", "--is-ancestor", older, newer],
            false,
        )
        .is_ok()
    };
    if ancestor(&reference, &head) {
        return Ok(());
    }
    if ancestor(&head, &reference) {
        bail!("the remote backup branch has changes this machine has not pulled yet. Run `herdr-pm pull` (it keeps your local changes unless the same item also changed remotely), then run `herdr-pm push` again. No local commit was created");
    }
    bail!("the remote backup branch and the local backup history have diverged; resolve the Git history manually in {}. No local commit was created", session.directory.display());
}

fn capture_generated(
    state: &State,
    path: &str,
    index: u8,
    worktree: u8,
    current: Option<&[u8]>,
) -> bool {
    if index != b' ' {
        return false; // Capture never stages anything.
    }
    match (worktree, current) {
        (b'M', Some(bytes)) => state
            .captured
            .get(path)
            .is_some_and(|hash| *hash == digest(bytes)),
        (b'D', None) => state.removed.contains(path),
        _ => false,
    }
}

fn worktree_conflicts(git: Git, session: &Session) -> Result<Vec<String>> {
    if !session.directory.join(".git").is_dir() {
        return Ok(Vec::new());
    }
    // A preview may inspect the real worktree, but must not refresh its index.
    let status = git.run_bytes(
        Some(&session.directory),
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=no",
        ],
        false,
    )?;
    let mut records = status
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut conflicts = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            bail!("invalid Git worktree status");
        }
        let path = std::str::from_utf8(&record[3..]).context("non-UTF-8 backup worktree path")?;
        let current = read_repo_file(&session.directory.join(path))?;
        if !capture_generated(
            &session.state,
            path,
            record[0],
            record[1],
            current.as_deref(),
        ) {
            conflicts.push(format!("{}{} {path}", record[0] as char, record[1] as char));
        }
        if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
            records
                .next()
                .ok_or_else(|| anyhow!("incomplete renamed Git worktree path"))?;
        }
    }
    Ok(conflicts)
}

fn review_mirror(git: Git, session: &Session, url: &str) -> Result<git::Mirror> {
    let Some(preview) = &session.preview else {
        return git.sync(url);
    };
    git::validate_remote(url)?;
    let path = preview
        .directory
        .join("sources")
        .join(digest(url.as_bytes()));
    if !path.exists() {
        fs::create_dir_all(&path)?;
        git.run(Some(&path), &["init", "--bare", "--quiet"], false)?;
        git.run(
            Some(&path),
            &[
                "fetch",
                "--quiet",
                "--no-recurse-submodules",
                "--",
                url,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
            true,
        )?;
    }
    Ok(git::Mirror {
        path,
        url: url.into(),
    })
}

fn is_tracked(git: Git, directory: &Path, path: &str) -> bool {
    git.run(
        Some(directory),
        &["ls-files", "--error-unmatch", "--", path],
        false,
    )
    .is_ok()
}

fn unstage(git: Git, directory: &Path, head: Option<&str>, paths: &[String]) -> Result<()> {
    for path in paths {
        if let Some(head) = head {
            git.run(
                Some(directory),
                &["reset", "--quiet", head, "--", path],
                false,
            )?;
        } else {
            git.run(
                Some(directory),
                &["rm", "--quiet", "--cached", "--ignore-unmatch", "--", path],
                false,
            )?;
        }
    }
    Ok(())
}

fn tree(git: Git, directory: &Path, commit: &str) -> Result<Tree> {
    let bytes = git.run_bytes(
        Some(directory),
        &["ls-tree", "-r", "-z", "--full-tree", commit],
        false,
    )?;
    let mut result = BTreeMap::new();
    let mut casefold = BTreeSet::new();
    for item in bytes
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        let line =
            std::str::from_utf8(item).context("snapshot tree contains a non-UTF-8 filename")?;
        let (header, path) = line
            .split_once('\t')
            .ok_or_else(|| anyhow!("invalid Git tree entry"))?;
        relative(path)?;
        let mut fields = header.split_whitespace();
        let mode = fields.next().unwrap_or("");
        let kind = fields.next().unwrap_or("");
        let oid = fields.next().unwrap_or("");
        if kind != "blob" || !matches!(mode, "100644" | "100755") || !util::is_full_sha(oid) {
            bail!("unsafe snapshot entry {path}: symlinks, submodules and special files are not accepted");
        }
        if !casefold.insert(path.to_lowercase()) {
            bail!("snapshot has case-colliding portable paths: {path}");
        }
        result.insert(
            path.into(),
            Blob {
                oid: oid.into(),
                mode: mode.into(),
            },
        );
    }
    Ok(result)
}

fn blob(git: Git, directory: &Path, value: &Blob) -> Result<Vec<u8>> {
    git.run_bytes(Some(directory), &["cat-file", "blob", &value.oid], false)
}

fn configuration_blob(git: Git, directory: &Path, value: &Blob, name: &str) -> Result<Vec<u8>> {
    let size: u64 = git
        .run(Some(directory), &["cat-file", "-s", &value.oid], false)?
        .parse()?;
    if size > MAX_CONFIG {
        bail!("{name} exceeds 4 MiB");
    }
    blob(git, directory, value)
}

fn load_snapshot(git: Git, directory: &Path, tree: &Tree) -> Result<Snapshot> {
    let metadata_blob = tree
        .get("plugins.toml")
        .ok_or_else(|| anyhow!("remote branch has no plugins.toml GitOps snapshot"))?;
    if metadata_blob.mode != "100644" {
        bail!("plugins.toml must be a non-executable regular file");
    }
    let bytes = configuration_blob(git, directory, metadata_blob, "plugins.toml")?;
    let metadata = parse_metadata(&bytes)?;
    let mut selections = BTreeSet::new();
    let mut casefold = BTreeSet::new();
    let mut files = BTreeMap::new();
    for path in &metadata.files {
        selector(path)?;
        if !selections.insert(path.as_str()) || !casefold.insert(path.to_ascii_lowercase()) {
            bail!("duplicate selected path {path}");
        }
        if let Some(value) = tree.get(path) {
            if value.mode != "100644" {
                bail!("configuration must not be executable: {path}");
            }
            let bytes = configuration_blob(git, directory, value, path)?;
            plaintext(&bytes, path)?;
            if path == "manager/config.toml" {
                restored_manager(Some(&bytes), None)?;
            } else if path == "herdr/config.toml" {
                toml::from_str::<toml::Table>(std::str::from_utf8(&bytes)?)?;
            }
            files.insert(path.clone(), bytes);
        }
    }
    for path in tree.keys() {
        if path.starts_with("herdr/")
            || path.starts_with("manager/")
            || path.starts_with("plugins/")
        {
            selector(path)?;
            if !selections.contains(path.as_str()) {
                bail!("remote configuration path {path} is not declared in plugins.toml");
            }
        }
    }
    Ok(Snapshot {
        metadata,
        files,
        plugins_included: true,
    })
}

/// Parse and validate `plugins.toml` metadata (files are checked by callers).
fn parse_metadata(bytes: &[u8]) -> Result<Metadata> {
    let metadata: Metadata = toml::from_str(std::str::from_utf8(bytes)?)?;
    if metadata.schema != SCHEMA {
        bail!(
            "unsupported plugins.toml schema {}; expected {SCHEMA}",
            metadata.schema
        );
    }
    let mut ids = BTreeSet::new();
    for plugin in &metadata.plugins {
        validate_plugin(plugin)?;
        if !ids.insert(plugin.id.clone()) {
            bail!("duplicate plugin id {}", plugin.id);
        }
    }
    for plugin in &metadata.local_only {
        plugin_id(&plugin.id)?;
        if !ids.insert(plugin.id.clone()) {
            bail!("duplicate local-only plugin id {}", plugin.id);
        }
    }
    Ok(metadata)
}

/// How one plugin or selected file is reconciled from its state at the last
/// sync (`base`), on this machine (`local`) and in the fetched snapshot
/// (`remote`). `None` means absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Both sides agree: nothing to do.
    Same,
    /// Only the remote changed it: apply the remote state, removal included.
    TakeRemote,
    /// Only this machine changed it: keep it; the next push publishes it.
    KeepLocal,
    /// Both changed it differently, or both added different versions.
    Conflict,
}

fn decide<T: PartialEq + ?Sized>(
    base: Option<&T>,
    local: Option<&T>,
    remote: Option<&T>,
) -> Decision {
    if local == remote {
        Decision::Same
    } else if local == base {
        Decision::TakeRemote
    } else if remote == base {
        Decision::KeepLocal
    } else {
        Decision::Conflict
    }
}

/// A plugin as reconciled. Local-only plugins (links, sources without an
/// exact commit) carry no sync instruction, so only this machine's count.
#[derive(PartialEq)]
enum Installed<'a> {
    Exact(&'a PluginState),
    LocalOnly,
}

/// Everything a pull would do, decided before any mutation: all changes are
/// listed for review, and any conflict stops the pull first.
#[derive(Default)]
struct Plan {
    conflicts: Vec<String>,
    /// Remote plugin states to take: install/update to the exact commit.
    install: Vec<PluginState>,
    /// Plugins removed remotely and unchanged here: uninstall, keep config.
    uninstall: Vec<String>,
    /// Selected files set to their remote state (written, or deleted).
    files: Vec<String>,
    lines: Vec<String>,
}

fn plugin_line(plugin: &PluginState) -> String {
    format!(
        "{} {} @ {} (enabled={}, auto-update={}, follows={})",
        plugin.id,
        util::sanitize(&plugin.version),
        util::short_sha(&plugin.commit),
        plugin.enabled,
        plugin.auto_update,
        util::sanitize(plugin.follow_ref.as_deref().unwrap_or("default branch"))
    )
}

fn kept(base: bool, local: bool) -> &'static str {
    match (base, local) {
        (false, _) => "addition",
        (true, false) => "removal",
        (true, true) => "change",
    }
}

/// How plugins take part in a pull on this machine.
#[derive(Clone, Copy)]
enum Plugins<'a> {
    /// `plugins.toml` off here: nothing installed, updated or uninstalled.
    Off,
    /// Three-way against this plugin baseline; `None` means first-sync rules.
    Sync(Option<&'a Summary>),
}

/// The plugin part of `reconcile`.
fn reconcile_plugins(
    plan: &mut Plan,
    local: &Summary,
    baseline: Option<&Summary>,
    remote: &Summary,
) {
    let empty = Summary::default();
    let base = baseline.unwrap_or(&empty);
    let ids: BTreeSet<&String> = base
        .plugins
        .keys()
        .chain(local.plugins.keys())
        .chain(local.local_only.keys())
        .chain(remote.plugins.keys())
        .collect();
    for id in ids {
        let incoming = remote.plugins.get(id);
        if incoming.is_some() && local.local_only.contains_key(id) {
            plan.conflicts.push(format!("plugins.toml: incoming plugin {id} collides with a local-only link; remove or explicitly take over that link before pulling"));
            continue;
        }
        let current = local.plugins.get(id).map(Installed::Exact).or_else(|| {
            local
                .local_only
                .contains_key(id)
                .then_some(Installed::LocalOnly)
        });
        let previous = base.plugins.get(id).map(Installed::Exact);
        let theirs = incoming.map(Installed::Exact);
        match decide(previous.as_ref(), current.as_ref(), theirs.as_ref()) {
            Decision::Same => {
                if let Some(plugin) = incoming {
                    plan.lines
                        .push(format!("  unchanged {}", plugin_line(plugin)));
                }
            }
            Decision::TakeRemote => match incoming {
                Some(plugin) => {
                    plan.lines.push(format!(
                        "  take remote: {} {}",
                        if current.is_some() {
                            "update to exact"
                        } else {
                            "install exact"
                        },
                        plugin_line(plugin)
                    ));
                    plan.install.push(plugin.clone());
                }
                None if remote.local_only.contains_key(id) => plan.lines.push(format!(
                    "  keep plugin {id}: the remote snapshot lists it as local-only (no reproducible source to take)"
                )),
                None => {
                    plan.lines.push(format!("  take remote: uninstall plugin {id} (removed in the remote snapshot; its configuration files are kept)"));
                    plan.uninstall.push(id.clone());
                }
            },
            Decision::KeepLocal => plan.lines.push(match (&current, baseline) {
                (Some(Installed::LocalOnly), _) => format!(
                    "  keep local-only plugin {id}: {}",
                    util::sanitize(&local.local_only[id])
                ),
                (Some(Installed::Exact(_)), None) => {
                    format!("  keep unrelated local plugin {id} (not removed)")
                }
                _ => format!(
                    "  keep local {} of plugin {id} (will be pushed)",
                    kept(previous.is_some(), current.is_some())
                ),
            }),
            Decision::Conflict => plan.conflicts.push(if baseline.is_some() {
                format!("plugins.toml: plugin {id} changed both on this machine and in the remote snapshot since the last sync")
            } else {
                format!(
                    "plugins.toml: existing plugin {id} differs from remote; no previous sync baseline"
                )
            }),
        }
    }
}

/// Three-way reconciliation of this machine (`local`) with the fetched
/// snapshot (`remote`) against the last sync. Without a baseline (first
/// pull) everything counts as new on both sides, and a selected local file
/// that differs from the remote conflicts instead of being kept. Plugins
/// use their own baseline, which is absent after `plugins.toml` was off.
fn reconcile(
    local: &Summary,
    baseline: Option<&Summary>,
    remote: &Summary,
    plugins: Plugins,
) -> Plan {
    let mut plan = Plan::default();
    match plugins {
        Plugins::Off => plan.lines.push(
            "  plugins not synchronized on this machine (plugins.toml off): none installed, updated or uninstalled".into(),
        ),
        Plugins::Sync(plugin_baseline) => {
            reconcile_plugins(&mut plan, local, plugin_baseline, remote)
        }
    }
    let empty = Summary::default();
    let base = baseline.unwrap_or(&empty);
    for (path, current) in &local.files {
        let Some(incoming) = remote.files.get(path) else {
            // An unselected remote path is not a deletion instruction.
            plan.lines
                .push(format!("  keep {path}: not selected in remote snapshot"));
            continue;
        };
        let previous = base.files.get(path).and_then(Option::as_ref);
        match decide(previous, current.as_ref(), incoming.as_ref()) {
            Decision::Same => {}
            Decision::TakeRemote => {
                plan.lines
                    .push(if incoming.is_some() || path == "manager/config.toml" {
                        format!("  take remote: restore {path}")
                    } else {
                        format!(
                            "  take remote: delete selected file {path} (removed in the remote snapshot)"
                        )
                    });
                plan.files.push(path.clone());
            }
            Decision::KeepLocal if baseline.is_some() => plan.lines.push(format!(
                "  keep local {} of {path} (will be pushed)",
                kept(previous.is_some(), current.is_some())
            )),
            Decision::KeepLocal | Decision::Conflict => {
                plan.conflicts.push(if baseline.is_some() {
                    format!("{path}: selected configuration changed both on this machine and in the remote snapshot since the last sync")
                } else {
                    format!("{path}: existing local configuration differs from remote; no previous sync baseline")
                })
            }
        }
    }
    for path in remote.files.keys() {
        if !local.files.contains_key(path) {
            plan.lines.push(format!(
                "  skip {path}: not explicitly selected on this machine"
            ));
        }
    }
    plan
}

/// Plugins and selected files whose local state differs between two
/// collections, e.g. edits made while a pull was under review.
fn local_changes(before: &Summary, after: &Summary) -> Vec<String> {
    let mut result = Vec::new();
    let ids: BTreeSet<&String> = before
        .plugins
        .keys()
        .chain(after.plugins.keys())
        .chain(before.local_only.keys())
        .chain(after.local_only.keys())
        .collect();
    for id in ids {
        if before.plugins.get(id) != after.plugins.get(id)
            || before.local_only.get(id) != after.local_only.get(id)
        {
            result.push(format!("plugins.toml: plugin {id}"));
        }
    }
    let paths: BTreeSet<&String> = before.files.keys().chain(after.files.keys()).collect();
    for path in paths {
        if before.files.get(path) != after.files.get(path) {
            result.push(path.clone());
        }
    }
    result
}

/// Validate Git history and all remote inputs, then reconcile three ways
/// against the last sync before installing anything: remote-only changes are
/// taken, local-only changes kept (for the next push), and items changed on
/// both sides stop the pull. No merge, force update, or remote-expanded
/// config allowlist.
pub fn pull(ctx: &Ctx) -> Result<()> {
    let remote = remote(&ctx.config)?;
    let git = operation_git(ctx);
    let mut session = Session::operation(ctx, git)?;
    session.initialize(git, &ctx.config.backup.branch)?;
    session.branch(git, &ctx.config.backup.branch)?;
    let head = session.head(git);
    let reference = format!("refs/remotes/herdr-backup/{}", ctx.config.backup.branch);
    let refspec = format!("refs/heads/{}:{reference}", ctx.config.backup.branch);
    ctx.report.step("fetching snapshot (no merge or overwrite)");
    git.run(
        Some(session.git_directory()),
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            remote,
            &refspec,
        ],
        true,
    )?;
    let commit = git.run(
        Some(session.git_directory()),
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        false,
    )?;
    let incoming_tree = tree(git, session.git_directory(), &commit)?;
    let incoming = load_snapshot(git, session.git_directory(), &incoming_tree)?;
    let local_summary = summary(&collect(&ctx.herdr, &ctx.config)?);
    let baseline = session.state.baseline.as_ref();
    if baseline.is_some_and(|baseline| {
        baseline.remote != remote || baseline.branch != ctx.config.backup.branch
    }) {
        bail!("backup remote or branch changed since the last sync. The local snapshot history in {} and its sync state {} belong to the previous remote/branch; move both aside to start over with this one", session.directory.display(), session.state_path.display());
    }
    if let Some(head) = &head {
        if git
            .run(
                Some(session.git_directory()),
                &["merge-base", "--is-ancestor", head, &commit],
                false,
            )
            .is_err()
        {
            bail!("GitOps conflict: local Git history is ahead of or divergent from the fetched branch. Push local commits or resolve the history explicitly; no merge, reset or force update was performed");
        }
    }
    if let Some(baseline) = baseline {
        if git
            .run(
                Some(session.git_directory()),
                &["merge-base", "--is-ancestor", &baseline.commit, &commit],
                false,
            )
            .is_err()
        {
            bail!("GitOps conflict: remote history no longer contains the last synchronized commit {}; refusing rewritten history", baseline.commit);
        }
    }
    stop_conflicts(
        "backup Git worktree/index has changes not generated by capture",
        worktree_conflicts(git, &session)?,
    )?;
    let baseline_summary = baseline.map(|baseline| &baseline.summary);
    let plugins = if !ctx.config.backup.plugins {
        Plugins::Off
    } else if session.state.plugins_off {
        // Plugins skipped since the baseline: reconcile them as a first sync.
        Plugins::Sync(None)
    } else {
        Plugins::Sync(baseline_summary)
    };
    let plan = reconcile(
        &local_summary,
        baseline_summary,
        &summary(&incoming),
        plugins,
    );
    if !plan.conflicts.is_empty() {
        bail!("GitOps conflict: changed differently on this machine and in the remote snapshot\n  {}\nNothing was installed, removed or written. For each item, make this machine match the remote version, or restore it to its state at the last sync so the remote version is taken, then pull again. To publish your own version instead, pull that way, re-apply your change, then push. No automatic merge or overwrite is performed.", plan.conflicts.join("\n  "));
    }
    let old_tree = match &head {
        Some(head) => tree(git, session.git_directory(), head)?,
        None => Tree::new(),
    };
    // Preload every blob and prove all write/delete targets safe before approval.
    let mut repo_files = BTreeMap::new();
    let mut repo_before = BTreeMap::new();
    let mut collisions = Vec::new();
    for (path, value) in &incoming_tree {
        let bytes = blob(git, session.git_directory(), value)?;
        let destination = session.directory.join(path);
        let current = read_repo_file(&destination)?;
        repo_before.insert(path.clone(), current.as_ref().map(|bytes| digest(bytes)));
        if let Some(current) = current {
            if current != bytes && !old_tree.contains_key(path) {
                let generated = session
                    .state
                    .captured
                    .get(path)
                    .is_some_and(|hash| *hash == digest(&current));
                if !generated {
                    collisions.push(format!(
                        "backup/{path}: untracked file would be overwritten"
                    ));
                }
            }
        }
        repo_files.insert(path.clone(), bytes);
    }
    for path in old_tree
        .keys()
        .filter(|path| !incoming_tree.contains_key(*path))
    {
        no_links(&session.directory.join(path))?;
        if path == "plugins.toml" || selector(path).is_ok() {
            repo_before.insert(
                path.clone(),
                read_repo_file(&session.directory.join(path))?
                    .as_ref()
                    .map(|bytes| digest(bytes)),
            );
        }
    }
    stop_conflicts("untracked backup files collide with the remote", collisions)?;
    let mut apply = BTreeMap::new();
    for path in &plan.files {
        let current = read_file(&live_path(path)?)?;
        let incoming_bytes = incoming.files.get(path);
        let desired = if path == "manager/config.toml" {
            Some(restored_manager(
                incoming_bytes.map(Vec::as_slice),
                current.as_deref(),
            )?)
        } else {
            incoming_bytes.cloned()
        };
        if current != desired {
            apply.insert(path.clone(), (current, desired));
        }
    }
    let mut lines = vec![
        format!(
            "  branch {} at {}",
            ctx.config.backup.branch,
            util::short_sha(&commit)
        ),
        "  only this machine's explicit file selections may be applied".into(),
    ];
    lines.extend(plan.lines.iter().cloned());
    if plan.install.is_empty() && plan.uninstall.is_empty() && apply.is_empty() {
        lines.push("  nothing to take from the remote snapshot".into());
    }
    if ctx.config.backup.plugins {
        unsupported_lines(&incoming, &mut lines);
    }
    // All manifests are reviewed up front, not only after preceding plugins
    // have already changed. install_git retains its own authoritative review.
    for plugin in &plan.install {
        ctx.report
            .step(&format!("validating exact source for {}", plugin.id));
        let mirror = review_mirror(git, &session, &plugin.url)?;
        let resolved = git.resolve(&mirror, Some(&plugin.commit))?;
        if resolved != plugin.commit {
            bail!("{} did not resolve to its recorded exact commit", plugin.id);
        }
        let manifest_path = match &plugin.subdir {
            Some(subdir) => format!("{subdir}/{}", manifest::FILE_NAME),
            None => manifest::FILE_NAME.into(),
        };
        let text = git
            .show_file(&mirror, &resolved, &manifest_path)?
            .ok_or_else(|| anyhow!("{} has no {manifest_path} at {}", plugin.id, plugin.commit))?;
        let manifest = Manifest::parse(&text)?;
        manifest.validate(ctx.herdr.version().ok().as_ref())?;
        if manifest.id.trim() != plugin.id || manifest.version != plugin.version {
            bail!(
                "{} exact commit manifest id/version differs from plugins.toml",
                plugin.id
            );
        }
        lines.extend(manifest.preview_lines());
    }
    if !approval(
        ctx,
        "GitOps pull — review all changes",
        &lines,
        "Apply this snapshot?",
    )? || ctx.dry_run
    {
        return Ok(());
    }
    // Users may edit configuration while reviewing. Recheck it before the
    // first install, while the watcher remains excluded by our lock.
    stop_conflicts(
        "local files/plugins changed during review",
        local_changes(&local_summary, &summary(&collect(&ctx.herdr, &ctx.config)?)),
    )?;
    check_repo_unchanged(&session.directory, &repo_before)?;
    for plugin in &plan.install {
        let applied = ops::install_git(
            ctx,
            GitRequest {
                url: plugin.url.clone(),
                subdir: plugin.subdir.clone(),
                checkout_ref: Some(plugin.commit.clone()),
                follow_ref: Some(plugin.follow_ref.clone()),
                mode: Mode::Sync,
                enabled: Some(plugin.enabled),
                replace: true,
                expect_id: Some(plugin.id.clone()),
            },
        )?;
        if !applied {
            // A declined per-plugin review must not mark the sync successful.
            let current = ops::rows(&ctx.herdr)?
                .into_iter()
                .find(|row| row.plugin.plugin_id == plugin.id);
            if current
                .as_ref()
                .is_none_or(|row| row.commit() != Some(plugin.commit.as_str()))
            {
                bail!("sync stopped: installation of {} was declined; previous successful installations are retained, and Git/configuration were not advanced", plugin.id);
            }
        }
        let current = ops::rows(&ctx.herdr)?
            .into_iter()
            .find(|row| row.plugin.plugin_id == plugin.id)
            .ok_or_else(|| anyhow!("{} missing after install", plugin.id))?;
        if current.plugin.enabled != plugin.enabled {
            ops::set_enabled(ctx, &plugin.id, plugin.enabled)?;
        }
        ops::set_auto_update(ctx, &plugin.id, plugin.auto_update)?;
        let installed = collect(&ctx.herdr, &ctx.config)?;
        if installed
            .metadata
            .plugins
            .iter()
            .find(|entry| entry.id == plugin.id)
            != Some(plugin)
        {
            bail!("sync stopped: {} does not match its reviewed exact source/version/ref/settings; Git/configuration were not advanced", plugin.id);
        }
    }
    for id in &plan.uninstall {
        // Not a purge: the plugin's configuration files stay on disk.
        ops::uninstall(ctx, id, false)?;
        if ops::rows(&ctx.herdr)?
            .iter()
            .any(|row| row.plugin.plugin_id == *id)
        {
            bail!("sync stopped: uninstall of {id} was declined; previous successful changes are retained, and Git/configuration were not advanced");
        }
    }
    // Check again after builds: selected files may have changed while a build ran.
    let mut changed = Vec::new();
    for (path, (before, _)) in &apply {
        if read_file(&live_path(path)?)? != *before {
            changed.push(path.clone());
        }
    }
    stop_conflicts(
        "configuration changed during plugin installation; installed plugins are retained",
        changed,
    )?;
    check_repo_unchanged(&session.directory, &repo_before)?;
    for (path, (_, bytes)) in &apply {
        let destination = live_path(path)?;
        if let Some(bytes) = bytes {
            write_changed(&destination, bytes)?;
        } else {
            remove_file(&destination)?;
        }
    }
    if apply.contains_key("herdr/config.toml") {
        ctx.herdr.reload_config()?;
    }
    let refreshed_config = Config::load_read_only()?;
    if ctx.config.agent_skill && !refreshed_config.agent_skill {
        ops::set_agent_skill(ctx, false)?;
    }
    if ctx.config.cli_on_path && !refreshed_config.cli_on_path {
        ops::set_cli_on_path(ctx, false)?;
    }
    crate::integrate::refresh(
        &ctx.herdr,
        refreshed_config.cli_on_path,
        refreshed_config.agent_skill,
    )?;
    // Materialize the validated tree without checkout/reset --hard, so no Git
    // filter, remote symlink, or blind worktree overwrite participates in apply.
    for (path, bytes) in &repo_files {
        write_changed(&session.directory.join(path), bytes)?;
        snapshot_mode(
            &session.directory.join(path),
            incoming_tree[path].mode == "100755",
        )?;
    }
    for path in old_tree
        .keys()
        .filter(|path| !incoming_tree.contains_key(*path))
    {
        if path == "plugins.toml" || selector(path).is_ok() {
            remove_file(&session.directory.join(path))?;
        }
        // Arbitrary repository user files removed remotely remain untracked.
    }
    for path in session
        .state
        .captured
        .keys()
        .filter(|path| !incoming_tree.contains_key(*path))
    {
        if repo_before.contains_key(path) || old_tree.contains_key(path) {
            continue;
        }
        // Only files created by capture may be removed from an unborn worktree.
        let current = read_repo_file(&session.directory.join(path))?;
        if current
            .as_ref()
            .is_some_and(|bytes| session.state.captured[path] == digest(bytes))
        {
            remove_file(&session.directory.join(path))?;
        }
    }
    git.run(Some(&session.directory), &["read-tree", &commit], false)?;
    let branch_ref = format!("refs/heads/{}", ctx.config.backup.branch);
    let previous = head
        .as_deref()
        .unwrap_or("0000000000000000000000000000000000000000");
    git.run(
        Some(&session.directory),
        &["update-ref", &branch_ref, &commit, previous],
        false,
    )?;
    session.state.captured.clear();
    session.state.captured.insert(
        "plugins.toml".into(),
        digest(repo_files.get("plugins.toml").unwrap()),
    );
    for path in &incoming.metadata.files {
        if let Some(bytes) = repo_files.get(path) {
            session.state.captured.insert(path.clone(), digest(bytes));
        }
    }
    session.state.preserved_files = incoming.metadata.files.iter().cloned().collect();
    session.state.removed.clear();
    // The baseline is what the remote holds now, so changes kept locally
    // still count as local changes on the next pull.
    session.state.baseline = Some(Baseline {
        remote: remote.into(),
        branch: ctx.config.backup.branch.clone(),
        commit,
        summary: summary(&incoming),
    });
    // Plugins were reconciled by this pull unless plugins.toml is off here.
    session.state.plugins_off = !ctx.config.backup.plugins;
    session.save()?;
    // Re-capture so local changes kept by this pull become capture-generated
    // snapshot changes, which the next push publishes as a fast-forward.
    session.capture(git, &collect(&ctx.herdr, &refreshed_config)?)?;
    ctx.report.line("Snapshot applied. Unrelated plugins were kept; backup settings and log_dir remained machine-local.");
    ctx.report.line("CLI/skill links were refreshed for new invocations. Existing plugin processes, open shell PATH values and already-loaded agent contexts are not restarted or rewritten.");
    Ok(())
}

fn read_repo_file(path: &Path) -> Result<Option<Vec<u8>>> {
    no_links(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => bail!(
            "snapshot destination is not a regular file: {}",
            path.display()
        ),
        Ok(_) => Ok(Some(fs::read(path)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn check_repo_unchanged(
    directory: &Path,
    expected: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    let mut conflicts = Vec::new();
    for (path, hash) in expected {
        if read_repo_file(&directory.join(path))?
            .as_ref()
            .map(|bytes| digest(bytes))
            != *hash
        {
            conflicts.push(format!("backup/{path}"));
        }
    }
    stop_conflicts(
        "backup worktree changed during review or installation",
        conflicts,
    )
}

fn snapshot_mode(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        let mode = permissions.mode();
        let desired = if executable {
            mode | 0o111
        } else {
            mode & !0o111
        };
        if mode != desired {
            permissions.set_mode(desired);
            fs::set_permissions(path, permissions)?;
        }
    }
    #[cfg(not(unix))]
    let _ = (path, executable);
    Ok(())
}

/// Actual portable plaintext file selectors; discovery never grants approval.
pub fn candidates() -> Result<Vec<String>> {
    let mut result = Vec::new();
    for selected in ["herdr/config.toml", "manager/config.toml"] {
        let path = live_path(selected)?;
        if let Ok(Some(bytes)) = read_file(&path) {
            if plaintext(&bytes, selected).is_ok() {
                result.push(selected.into());
            }
        }
    }
    let plugin_root = plugin_config_root()?;
    for plugin in Herdr::detect(false).list()? {
        if plugin.plugin_id == paths::PLUGIN_ID
            || plugin.plugin_id == paths::OLD_PLUGIN_ID
            || plugin_id(&plugin.plugin_id).is_err()
        {
            continue;
        }
        let root = plugin_root.join(paths::plugin_path_component(&plugin.plugin_id));
        discover(
            &root,
            &root,
            &format!(
                "plugins/{}",
                paths::plugin_path_component(&plugin.plugin_id)
            ),
            &mut result,
        )?;
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn discover(root: &Path, directory: &Path, prefix: &str, result: &mut Vec<String>) -> Result<()> {
    if no_links(directory).is_err() {
        return Ok(());
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        let relative_path = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if relative(&relative_path).is_err() {
            continue;
        }
        if kind.is_dir() {
            if !relative_path.to_ascii_lowercase().split('/').any(|part| {
                matches!(
                    part,
                    "logs"
                        | "log"
                        | "state"
                        | "cache"
                        | "caches"
                        | "bin"
                        | "target"
                        | "node_modules"
                        | ".git"
                )
            }) {
                discover(root, &path, prefix, result)?;
            }
        } else if kind.is_file() && config_file(&relative_path) {
            let selected = format!("{prefix}/{relative_path}");
            if let Some(bytes) = read_file(&path)? {
                if plaintext(&bytes, &selected).is_ok() {
                    result.push(selected);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_preserves_persisted_sha256_format() {
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest(b"s"),
            "043a718774c572bd8a25adbeb1bfcd5c0256ae11cecf9f9c3f925d0e52beaf89"
        );
    }

    fn plugin() -> PluginState {
        PluginState {
            id: "test.plugin".into(),
            url: "https://example.org/plugin.git".into(),
            subdir: None,
            commit: "a".repeat(40),
            version: "1.0.0".into(),
            follow_ref: Some("main".into()),
            pin: None,
            enabled: true,
            auto_update: false,
        }
    }

    #[test]
    fn selections_reject_traversal_devices_wildcards_and_runtime_data() {
        for path in [
            "plugins/test.plugin/../secret.toml",
            "plugins/test.plugin/C:/secret.toml",
            "plugins/test.plugin/config\\secret.toml",
            "plugins/test.plugin/logs/config.toml",
            "plugins/test.plugin/cache/a.json",
            "plugins/test.plugin/CON.toml",
            "plugins/test.plugin/*.toml",
            "plugins/test.plugin/history.json",
            "plugins/julienpoirou.herdr-plugin-manager/config.toml",
            "manager/other.toml",
        ] {
            assert!(selector(path).is_err(), "{path}");
        }
        for path in [
            "herdr/config.toml",
            "manager/config.toml",
            "plugins/test.plugin/nested/settings.toml",
            "plugins/test.plugin/.env",
            "plugins/test.plugin/config.lua",
            "plugins/test.plugin/config.js",
            "plugins/test.plugin/settings",
            "plugins/owner%3Aplugin/settings",
        ] {
            assert!(selector(path).is_ok(), "{path}");
        }
    }

    #[test]
    fn sources_and_pins_are_checked_before_installation() {
        let mut value = plugin();
        for url in [
            "--upload-pack=evil",
            "ext::sh -c evil",
            "evil::anything",
            "https://example.org/\nrepo",
        ] {
            value.url = url.into();
            assert!(validate_plugin(&value).is_err());
        }
        value = plugin();
        value.pin = Some(value.commit.clone());
        assert!(validate_plugin(&value).is_err());
        value.follow_ref = value.pin.clone();
        assert!(validate_plugin(&value).is_ok());
        value.subdir = Some("../outside".into());
        assert!(validate_plugin(&value).is_err());
    }

    fn plugins(values: &[PluginState]) -> BTreeMap<String, PluginState> {
        values
            .iter()
            .map(|value| (value.id.clone(), value.clone()))
            .collect()
    }

    fn files(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        entries
            .iter()
            .map(|(path, text)| ((*path).to_owned(), text.map(|text| digest(text.as_bytes()))))
            .collect()
    }

    #[test]
    fn three_way_decision_keeps_local_takes_remote_or_conflicts() {
        use Decision::*;
        let (a, b, c) = (&1, &2, &3);
        // Only the remote changed it: take it, removals included.
        assert_eq!(decide(Some(a), Some(a), Some(b)), TakeRemote);
        assert_eq!(decide(Some(a), Some(a), None), TakeRemote);
        assert_eq!(decide(None, None, Some(b)), TakeRemote);
        // Only this machine changed it: keep it for the next push.
        assert_eq!(decide(Some(a), Some(b), Some(a)), KeepLocal);
        assert_eq!(decide(Some(a), None, Some(a)), KeepLocal);
        assert_eq!(decide(None, Some(b), None), KeepLocal);
        // Both sides agree, whatever the baseline was.
        assert_eq!(decide(Some(a), Some(a), Some(a)), Same);
        assert_eq!(decide(Some(a), Some(b), Some(b)), Same);
        assert_eq!(decide(None, Some(b), Some(b)), Same);
        // Changed differently on both sides, or added differently on both.
        assert_eq!(decide(Some(a), Some(b), Some(c)), Conflict);
        assert_eq!(decide(Some(a), Some(b), None), Conflict);
        assert_eq!(decide(None, Some(b), Some(c)), Conflict);
    }

    #[test]
    fn pull_keeps_unpushed_local_file_while_taking_remote_change() {
        let custom = "plugins/fixture.sync/custom.toml";
        let value = plugin();
        let baseline = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            files: files(&[
                ("herdr/config.toml", Some("theme = 'old'\n")),
                (custom, Some("color = 'old'\n")),
            ]),
            ..Summary::default()
        };
        let mut local = baseline.clone();
        local
            .files
            .insert(custom.into(), Some(digest(b"color = 'local'\n")));
        let mut remote = baseline.clone();
        remote.files.insert(
            "herdr/config.toml".into(),
            Some(digest(b"theme = 'remote'\n")),
        );
        let plan = reconcile(
            &local,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        assert_eq!(plan.files, vec!["herdr/config.toml".to_owned()]);
        assert!(plan.install.is_empty() && plan.uninstall.is_empty());
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("keep local change of") && line.contains(custom)));
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("take remote") && line.contains("herdr/config.toml")));

        // A plugin changed only remotely is taken; one changed only locally is kept.
        let mut remote_plugin = value.clone();
        remote_plugin.commit = "b".repeat(40);
        remote.plugins = plugins(&[remote_plugin.clone()]);
        assert_eq!(
            reconcile(
                &local,
                Some(&baseline),
                &remote,
                Plugins::Sync(Some(&baseline))
            )
            .install,
            vec![remote_plugin]
        );
        let mut local_plugin = value.clone();
        local_plugin.enabled = false;
        local.plugins = plugins(&[local_plugin]);
        remote.plugins = baseline.plugins.clone();
        let plan = reconcile(
            &local,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.conflicts.is_empty() && plan.install.is_empty());
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("keep local change of plugin test.plugin")));
    }

    #[test]
    fn same_item_changed_differently_on_both_sides_conflicts() {
        let value = plugin();
        let baseline = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            files: files(&[("herdr/config.toml", Some("theme = 'old'\n"))]),
            ..Summary::default()
        };
        let mut local = baseline.clone();
        local.files.insert(
            "herdr/config.toml".into(),
            Some(digest(b"theme = 'local'\n")),
        );
        local.plugins.get_mut(&value.id).unwrap().auto_update = true;
        let mut remote = baseline.clone();
        remote.files.insert(
            "herdr/config.toml".into(),
            Some(digest(b"theme = 'remote'\n")),
        );
        remote.plugins.get_mut(&value.id).unwrap().enabled = false;
        let plan = reconcile(
            &local,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert_eq!(plan.conflicts.len(), 2);
        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("herdr/config.toml")));
        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.contains("test.plugin")));
        // The same change made on both sides is no conflict.
        let plan = reconcile(
            &local,
            Some(&baseline),
            &local,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.conflicts.is_empty() && plan.files.is_empty() && plan.install.is_empty());
    }

    #[test]
    fn remote_removals_are_proposed_for_review() {
        let value = plugin();
        let path = "plugins/test.plugin/settings";
        let baseline = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            files: files(&[(path, Some("a = 1\n"))]),
            ..Summary::default()
        };
        let remote = Summary {
            files: files(&[(path, None)]),
            ..Summary::default()
        };
        let plan = reconcile(
            &baseline,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.uninstall, vec![value.id.clone()]);
        assert_eq!(plan.files, vec![path.to_owned()]);
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("uninstall plugin test.plugin")));
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("delete selected file") && line.contains(path)));
        // Removed remotely but changed here: conflict, nothing removed.
        let mut local = baseline.clone();
        local.plugins.get_mut(&value.id).unwrap().auto_update = true;
        local.files.insert(path.into(), Some(digest(b"a = 2\n")));
        let plan = reconcile(
            &local,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert_eq!(plan.conflicts.len(), 2);
        // A remote local-only entry is not an instruction to uninstall.
        let remote = Summary {
            local_only: BTreeMap::from([(value.id.clone(), "local developer link".into())]),
            ..remote
        };
        assert!(reconcile(
            &baseline,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline))
        )
        .uninstall
        .is_empty());
        // A path no longer selected remotely is not a deletion instruction.
        let plan = reconcile(
            &baseline,
            Some(&baseline),
            &Summary::default(),
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.files.is_empty());
    }

    #[test]
    fn items_added_on_both_sides_conflict_only_when_different() {
        let value = plugin();
        let path = "herdr/config.toml";
        let baseline = Summary::default();
        let added = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            files: files(&[(path, Some("theme = 'same'\n"))]),
            ..Summary::default()
        };
        let plan = reconcile(
            &added,
            Some(&baseline),
            &added,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.conflicts.is_empty() && plan.install.is_empty() && plan.files.is_empty());
        let mut other = added.clone();
        other.plugins.get_mut(&value.id).unwrap().commit = "b".repeat(40);
        other
            .files
            .insert(path.into(), Some(digest(b"theme = 'other'\n")));
        assert_eq!(
            reconcile(
                &added,
                Some(&baseline),
                &other,
                Plugins::Sync(Some(&baseline))
            )
            .conflicts
            .len(),
            2
        );
        // Added on one side only: kept locally, or taken from the remote.
        let remote = Summary {
            files: files(&[(path, None)]),
            ..Summary::default()
        };
        let plan = reconcile(
            &added,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(plan.conflicts.is_empty() && plan.install.is_empty() && plan.files.is_empty());
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("keep local addition of plugin test.plugin")));
        let local = Summary {
            files: files(&[(path, None)]),
            ..Summary::default()
        };
        let plan = reconcile(
            &local,
            Some(&baseline),
            &added,
            Plugins::Sync(Some(&baseline)),
        );
        assert_eq!(plan.install, vec![value]);
        assert_eq!(plan.files, vec![path.to_owned()]);
    }

    #[test]
    fn edits_during_review_are_detected() {
        let value = plugin();
        let before = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            files: files(&[("herdr/config.toml", Some("theme = 'a'\n"))]),
            ..Summary::default()
        };
        assert!(local_changes(&before, &before).is_empty());
        let mut after = before.clone();
        after
            .files
            .insert("herdr/config.toml".into(), Some(digest(b"theme = 'b'\n")));
        after.plugins.get_mut(&value.id).unwrap().auto_update = true;
        assert_eq!(local_changes(&before, &after).len(), 2);
    }

    #[test]
    fn local_only_collision_stops_pull_even_with_unchanged_baseline() {
        let value = plugin();
        let remote = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            ..Summary::default()
        };
        let local = Summary {
            local_only: BTreeMap::from([(value.id.clone(), "local developer link".into())]),
            ..Summary::default()
        };
        for baseline in [None, Some(&local), Some(&remote)] {
            let plan = reconcile(&local, baseline, &remote, Plugins::Sync(baseline));
            assert!(plan.install.is_empty());
            assert!(plan.conflicts.iter().any(
                |conflict| conflict.contains("local-only link") && conflict.contains(&value.id)
            ));
        }
    }

    #[test]
    fn capture_generated_changes_do_not_hide_manual_or_staged_changes() {
        let mut state = State::default();
        state
            .captured
            .insert("plugins.toml".into(), digest(b"generated snapshot"));
        assert!(capture_generated(
            &state,
            "plugins.toml",
            b' ',
            b'M',
            Some(b"generated snapshot")
        ));
        assert!(!capture_generated(
            &state,
            "plugins.toml",
            b' ',
            b'M',
            Some(b"manual edit")
        ));
        assert!(!capture_generated(
            &state,
            "plugins.toml",
            b'M',
            b' ',
            Some(b"generated snapshot")
        ));
        assert!(!capture_generated(
            &state,
            "notes.txt",
            b' ',
            b'M',
            Some(b"generated snapshot")
        ));
        assert!(!capture_generated(
            &state,
            "plugins/extra/settings",
            b' ',
            b'D',
            None
        ));
        state.removed.insert("plugins/extra/settings".into());
        assert!(capture_generated(
            &state,
            "plugins/extra/settings",
            b' ',
            b'D',
            None
        ));
        assert!(!capture_generated(
            &state,
            "plugins/extra/settings",
            b'D',
            b' ',
            None
        ));
    }

    #[test]
    fn capture_preserves_remote_files_unselected_on_this_machine() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = trusted_root(&std::env::temp_dir()).unwrap().join(format!(
            "herdr-pm-capture-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let scratch = PreviewRepo { directory };
        let path = "plugins/test.plugin/config.lua";
        let bytes = b"return { theme = 'remote' }\n";
        let mut session = Session {
            _lock: None,
            directory: scratch.directory.join("snapshot"),
            state_path: scratch.directory.join("state.json"),
            state: State::default(),
            preview: None,
        };
        write_changed(&session.directory.join(path), bytes).unwrap();
        session.state.captured.insert(path.into(), digest(bytes));
        session.state.preserved_files.insert(path.into());
        let mut snapshot = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: vec![],
                plugins: vec![],
                local_only: vec![],
            },
            files: BTreeMap::new(),
            plugins_included: true,
        };
        let metadata = session.write_snapshot(&snapshot, None).unwrap();
        assert!(metadata.files.iter().any(|selected| selected == path));
        assert_eq!(
            fs::read(session.directory.join(path)).unwrap().as_slice(),
            bytes.as_slice()
        );
        assert_eq!(session.state.captured[path], digest(bytes));
        // Selecting an absent live file explicitly authorizes its deletion;
        // merely omitting it from this machine's allowlist did not.
        snapshot.metadata.files.push(path.into());
        session.write_snapshot(&snapshot, None).unwrap();
        assert!(!session.directory.join(path).exists());
        assert!(session.state.removed.contains(path));
    }

    #[test]
    fn capture_with_plugins_off_keeps_the_snapshot_plugin_entries() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = trusted_root(&std::env::temp_dir()).unwrap().join(format!(
            "herdr-pm-plugins-off-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let scratch = PreviewRepo { directory };
        let mut session = Session {
            _lock: None,
            directory: scratch.directory.join("snapshot"),
            state_path: scratch.directory.join("state.json"),
            state: State::default(),
            preview: None,
        };
        // Committed by another machine, in a layout this one would not write.
        let committed = format!(
            "schema = 1\n# published elsewhere\n\n[[plugins]]\nid = \"test.plugin\"\nurl = \"https://example.org/plugin.git\"\ncommit = \"{}\"\nversion = \"1.0.0\"\nfollow_ref = \"main\"\nenabled = true\nauto_update = false\n\n[[local_only]]\nid = \"dev.link\"\nreason = \"local developer link\"\n",
            "a".repeat(40)
        );
        let remote = parse_metadata(committed.as_bytes()).unwrap();
        // This machine's own plugins are never published while off.
        let mut own = plugin();
        own.id = "own.plugin".into();
        let mut snapshot = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: vec![],
                plugins: vec![own],
                local_only: vec![],
            },
            files: BTreeMap::new(),
            plugins_included: false,
        };
        let metadata = session
            .write_snapshot(&snapshot, Some(committed.as_bytes()))
            .unwrap();
        assert_eq!(metadata.plugins, remote.plugins);
        assert_eq!(metadata.local_only, remote.local_only);
        assert_eq!(
            fs::read(session.directory.join("plugins.toml")).unwrap(),
            committed.as_bytes()
        );
        assert!(session.state.plugins_off);
        // A changed file selection rewrites plugins.toml with the same entries.
        let path = "herdr/config.toml";
        snapshot.metadata.files.push(path.into());
        snapshot
            .files
            .insert(path.into(), b"theme = 'local'\n".to_vec());
        session
            .write_snapshot(&snapshot, Some(committed.as_bytes()))
            .unwrap();
        let written =
            parse_metadata(&fs::read(session.directory.join("plugins.toml")).unwrap()).unwrap();
        assert_eq!(written.plugins, remote.plugins);
        assert_eq!(written.local_only, remote.local_only);
        assert_eq!(written.files, vec![path.to_owned()]);
        // Without a previous snapshot no plugin entries are written.
        let metadata = session.write_snapshot(&snapshot, None).unwrap();
        assert!(metadata.plugins.is_empty() && metadata.local_only.is_empty());
    }

    #[test]
    fn pull_with_plugins_off_plans_no_plugin_changes() {
        let value = plugin();
        let path = "herdr/config.toml";
        let baseline = Summary {
            plugins: plugins(std::slice::from_ref(&value)),
            files: files(&[(path, Some("theme = 'old'\n"))]),
            ..Summary::default()
        };
        // Off: this machine's plugins are not collected at all.
        let local = Summary {
            files: baseline.files.clone(),
            ..Summary::default()
        };
        let mut added = plugin();
        added.id = "added.plugin".into();
        let mut changed = value.clone();
        changed.commit = "b".repeat(40);
        for remote_plugins in [vec![], vec![added.clone()], vec![changed, added]] {
            let remote = Summary {
                plugins: plugins(&remote_plugins),
                files: files(&[(path, Some("theme = 'remote'\n"))]),
                ..Summary::default()
            };
            let plan = reconcile(&local, Some(&baseline), &remote, Plugins::Off);
            assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
            assert!(plan.install.is_empty() && plan.uninstall.is_empty());
            assert_eq!(plan.files, vec![path.to_owned()]);
            assert_eq!(
                plan.lines
                    .iter()
                    .filter(|line| line.contains("plugin"))
                    .collect::<Vec<_>>(),
                vec!["  plugins not synchronized on this machine (plugins.toml off): none installed, updated or uninstalled"]
            );
        }
    }

    #[test]
    fn plugins_turned_back_on_follow_first_sync_rules() {
        let value = plugin();
        let mut missing = plugin();
        missing.id = "missing.plugin".into();
        let mut extra = plugin();
        extra.id = "extra.plugin".into();
        // The baseline holds the remote plugins seen while off.
        let remote = Summary {
            plugins: plugins(&[value.clone(), missing.clone()]),
            ..Summary::default()
        };
        let baseline = remote.clone();
        let mut differing = value.clone();
        differing.enabled = false;
        let local = Summary {
            plugins: plugins(&[differing, extra]),
            ..Summary::default()
        };
        // A stale baseline would silently keep the difference and publish
        // the missing plugin's removal on the next push.
        let stale = reconcile(
            &local,
            Some(&baseline),
            &remote,
            Plugins::Sync(Some(&baseline)),
        );
        assert!(stale.conflicts.is_empty() && stale.install.is_empty());
        let plan = reconcile(&local, Some(&baseline), &remote, Plugins::Sync(None));
        assert_eq!(plan.conflicts.len(), 1);
        assert!(plan.conflicts[0].contains("existing plugin test.plugin differs from remote"));
        assert_eq!(plan.install, vec![missing]);
        assert!(plan.uninstall.is_empty());
        assert!(plan
            .lines
            .iter()
            .any(|line| line.contains("keep unrelated local plugin extra.plugin")));
    }

    #[cfg(unix)]
    #[test]
    fn trusted_root_aliases_resolve_without_following_selector_children() {
        use std::os::unix::fs::symlink;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = trusted_root(&std::env::temp_dir()).unwrap().join(format!(
            "herdr-pm-authority-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let scratch = PreviewRepo { directory };
        let physical = scratch.directory.join("physical");
        fs::create_dir(&physical).unwrap();
        let alias = scratch.directory.join("configured-root");
        symlink(&physical, &alias).unwrap();

        // The configured authority may be an OS/user alias, and its
        // descendant root need not have been created yet.
        let root = trusted_root(&alias.join("new").join("config")).unwrap();
        assert_eq!(root, physical.join("new").join("config"));
        write_changed(&root.join("settings.lua"), b"return { enabled = true }\n").unwrap();

        let outside = scratch.directory.join("outside");
        let secret = outside.join("secrets.toml");
        write_changed(&secret, b"secret = 'keep'\n").unwrap();
        symlink(&outside, root.join("linked-directory")).unwrap();
        let selected = root.join("linked-directory").join("secrets.toml");
        assert!(read_file(&selected).is_err());
        assert!(write_changed(&selected, b"secret = 'replace'\n").is_err());
        assert!(remove_file(&selected).is_err());

        symlink(&secret, root.join("linked.toml")).unwrap();
        let configured_file =
            authority_file(&alias.join("new").join("config").join("linked.toml")).unwrap();
        assert_eq!(configured_file, root.join("linked.toml"));
        assert!(read_file(&configured_file).is_err());
        assert!(write_changed(&configured_file, b"secret = 'replace'\n").is_err());
        assert_eq!(
            fs::read(&secret).unwrap().as_slice(),
            b"secret = 'keep'\n".as_slice()
        );

        let mut found = Vec::new();
        discover(&root, &root, "plugins/test.plugin", &mut found).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], "plugins/test.plugin/settings.lua");
    }

    #[test]
    fn metadata_ids_and_encoded_configuration_directories_are_distinct() {
        let mut value = plugin();
        value.id = "Owner:Plugin".into();
        assert!(validate_plugin(&value).is_ok());
        let encoded = paths::plugin_path_component(&value.id);
        let selected = format!("plugins/{encoded}/settings");
        assert_eq!(
            live_path(&selected).unwrap(),
            plugin_config_root()
                .unwrap()
                .join(&encoded)
                .join("settings")
        );
        assert!(selector("plugins/owner:plugin/settings").is_err());
        assert!(plaintext(b"return { theme = 'dark' }\n", "config.lua").is_ok());
        assert!(plaintext(b"MZ\0binary", "program.exe").is_err());
    }

    #[test]
    fn first_pull_accepts_machine_local_only_manager_config_but_not_file_deletion() {
        let local = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: vec!["manager/config.toml".into(), "herdr/config.toml".into()],
                plugins: vec![],
                local_only: vec![],
            },
            files: BTreeMap::from([(
                "manager/config.toml".into(),
                portable_manager(b"[backup]\nenabled = false\n").unwrap(),
            )]),
            plugins_included: true,
        };
        let incoming = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: local.metadata.files.clone(),
                plugins: vec![],
                local_only: vec![],
            },
            files: BTreeMap::from([("manager/config.toml".into(), b"icons = false\n".to_vec())]),
            plugins_included: true,
        };
        let plan = reconcile(
            &summary(&local),
            None,
            &summary(&incoming),
            Plugins::Sync(None),
        );
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.files, vec!["manager/config.toml".to_owned()]);
        let mut local = local;
        local
            .files
            .insert("herdr/config.toml".into(), b"theme = 'local'\n".to_vec());
        let conflicts = reconcile(
            &summary(&local),
            None,
            &summary(&incoming),
            Plugins::Sync(None),
        )
        .conflicts;
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("herdr/config.toml"));
    }

    #[test]
    fn manager_local_settings_are_neither_exported_nor_remote_controlled() {
        let log_dir = std::env::temp_dir().join("herdr-gitops-test-logs");
        let mut doc: toml::Table = toml::from_str("icons = false\n[backup]\nenabled = true\nremote = 'https://example.org/backup.git'\nfiles = ['manager/config.toml']\n").unwrap();
        doc.insert(
            "log_dir".into(),
            toml::Value::String(log_dir.to_string_lossy().into_owned()),
        );
        let source = toml::to_string(&doc).unwrap().into_bytes();
        let portable = portable_manager(&source).unwrap();
        let text = std::str::from_utf8(&portable).unwrap();
        assert!(!text.contains("backup"));
        assert!(!text.contains("log_dir"));
        let restored = restored_manager(Some(b"icons = true\n"), Some(&source)).unwrap();
        let doc: toml::Table = toml::from_str(std::str::from_utf8(&restored).unwrap()).unwrap();
        assert_eq!(doc["icons"].as_bool(), Some(true));
        assert_eq!(doc["backup"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["log_dir"].as_str(), log_dir.to_str());
        assert!(restored_manager(Some(&source), None).is_err());
    }
}
