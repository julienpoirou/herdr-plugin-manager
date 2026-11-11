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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        for attempt in 0..100 {
            let directory = std::env::temp_dir().join(format!(
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
    format!("{:x}", Sha256::digest(bytes))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
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

/// Reject links at every existing component, not just at the final file.
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
        "herdr/config.toml" => paths::herdr_config_toml(),
        "manager/config.toml" => paths::pm_config_file(),
        _ => {
            let mut parts = value.splitn(3, '/');
            parts.next();
            paths::herdr_config_dir()
                .join("plugins")
                .join("config")
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
        let lock_path = paths::pm_home().join(".gitops.lock");
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
        if config
            .backup
            .directory
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            bail!("backup.directory must be absolute");
        }
        let directory = config
            .backup
            .directory
            .clone()
            .unwrap_or_else(|| paths::pm_home().join("backup"));
        let directory = absolute(&directory)?;
        if directory
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!(
                "backup directory may not contain '..': {}",
                directory.display()
            );
        }
        no_links(&directory)?;
        for protected in [
            paths::herdr_config_dir(),
            paths::herdr_config_toml(),
            paths::pm_config_dir(),
            paths::installs_dir(),
            paths::repo_cache_dir(),
        ] {
            let protected = absolute(&protected)?;
            if protected.starts_with(&directory) || directory.starts_with(&protected) {
                bail!(
                    "backup directory overlaps live configuration or installation files: {}",
                    directory.display()
                );
            }
        }
        if absolute(&paths::pm_home())?.starts_with(&directory) {
            bail!("backup directory must be dedicated, not the manager data directory or its ancestor");
        }
        let state_path = paths::pm_home().join("gitops").join(format!(
            "{}.json",
            digest(directory.to_string_lossy().as_bytes())
        ));
        no_links(&state_path)?;
        let state: State = match fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("invalid GitOps state {}; move it aside only after resolving local snapshot changes", state_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(err) => return Err(err.into()),
        };
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

    fn capture(&mut self, snapshot: &Snapshot) -> Result<Metadata> {
        let mut metadata = snapshot.metadata.clone();
        let mut files: BTreeSet<String> = metadata.files.iter().cloned().collect();
        files.extend(self.state.preserved_files.iter().cloned());
        let mut portable = BTreeSet::new();
        for path in &files {
            if !portable.insert(path.to_ascii_lowercase()) {
                bail!("configuration selections collide on a case-insensitive filesystem: {path}");
            }
        }
        metadata.files = files.into_iter().collect();
        let encoded = toml::to_string_pretty(&metadata)?.into_bytes();
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
    for row in ops::rows(herdr)? {
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
    })
}

fn summary(snapshot: &Snapshot) -> Summary {
    Summary {
        plugins: snapshot
            .metadata
            .plugins
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.clone()))
            .collect(),
        local_only: snapshot
            .metadata
            .local_only
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.reason.clone()))
            .collect(),
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
    }
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
    session.capture(&snapshot).map(|_| ())
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

/// Capture and review a real Git commit, then perform a normal fast-forward push.
pub fn push(ctx: &Ctx) -> Result<()> {
    let remote = remote(&ctx.config)?;
    let git = operation_git(ctx);
    let mut session = Session::operation(ctx, git)?;
    session.initialize(git, &ctx.config.backup.branch)?;
    session.branch(git, &ctx.config.backup.branch)?;
    let snapshot = collect(&ctx.herdr, &ctx.config)?;
    push_preflight(git, &session, remote, &ctx.config.backup.branch)?;
    if ctx.dry_run {
        let mut lines = vec![
            format!(
                "  capture {} reproducible plugins and {} selected files",
                snapshot.metadata.plugins.len(),
                snapshot.files.len()
            ),
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
    let captured_metadata = session.capture(&snapshot)?;
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
    session.state.baseline = Some(Baseline {
        remote: remote.into(),
        branch: ctx.config.backup.branch.clone(),
        commit,
        summary: summary(&snapshot),
    });
    session
        .state
        .preserved_files
        .extend(captured_metadata.files);
    session.state.removed.clear();
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
        bail!("the remote backup branch already exists, but this backup repository has no local commit. Pull first to adopt its history; no local commit was created");
    };
    if git
        .run(
            Some(session.git_directory()),
            &["merge-base", "--is-ancestor", &reference, &head],
            false,
        )
        .is_err()
    {
        bail!("the remote backup branch is ahead of or divergent from the local backup history. Pull first if it is a fast-forward; otherwise resolve the Git history manually in {}. No local commit was created", session.directory.display());
    }
    Ok(())
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
    let metadata: Metadata = toml::from_str(std::str::from_utf8(&bytes)?)?;
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
    Ok(Snapshot { metadata, files })
}

fn drift(local: &Summary, baseline: Option<&Summary>, incoming: &Snapshot) -> Vec<String> {
    let mut result = Vec::new();
    for plugin in &incoming.metadata.plugins {
        if local.local_only.contains_key(&plugin.id) {
            result.push(format!("plugins.toml: incoming plugin {} collides with a local-only link; remove or explicitly take over that link before pulling", plugin.id));
        }
    }
    if let Some(baseline) = baseline {
        let ids: BTreeSet<&String> = baseline
            .plugins
            .keys()
            .chain(local.plugins.keys())
            .chain(baseline.local_only.keys())
            .chain(local.local_only.keys())
            .collect();
        for id in ids {
            if baseline.plugins.get(id) != local.plugins.get(id)
                || baseline.local_only.get(id) != local.local_only.get(id)
            {
                result.push(format!(
                    "plugins.toml: plugin {id} changed locally since the last sync"
                ));
            }
        }
        for (path, value) in &local.files {
            if let Some(previous) = baseline.files.get(path) {
                if previous != value {
                    result.push(format!(
                        "{path}: selected local configuration changed since the last sync"
                    ));
                }
            } else if incoming.metadata.files.contains(path)
                && value.is_some()
                && *value != incoming.files.get(path).map(|bytes| digest(bytes))
            {
                result.push(format!(
                    "{path}: newly selected local file differs from the remote"
                ));
            }
        }
    } else {
        for plugin in &incoming.metadata.plugins {
            if local
                .plugins
                .get(&plugin.id)
                .is_some_and(|current| current != plugin)
            {
                result.push(format!("plugins.toml: existing plugin {} differs from remote; no previous sync baseline", plugin.id));
            }
        }
        for (path, current) in &local.files {
            if incoming.metadata.files.contains(path)
                && current.is_some()
                && *current != incoming.files.get(path).map(|bytes| digest(bytes))
            {
                result.push(format!("{path}: existing local configuration differs from remote; no previous sync baseline"));
            }
        }
    }
    result
}

/// Validate Git history, all remote inputs, and local drift before installing
/// anything. No merge, force update, or remote-expanded config allowlist.
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
    let local = collect(&ctx.herdr, &ctx.config)?;
    let local_summary = summary(&local);
    let baseline = session.state.baseline.as_ref();
    if baseline.is_some_and(|baseline| {
        baseline.remote != remote || baseline.branch != ctx.config.backup.branch
    }) {
        bail!("backup remote or branch changed since the last sync; use a separate dedicated backup.directory for a different history");
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
    stop_conflicts(
        "local installation/configuration drift",
        drift(
            &local_summary,
            baseline.map(|baseline| &baseline.summary),
            &incoming,
        ),
    )?;
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
    let mut lines = vec![
        format!(
            "  branch {} at {}",
            ctx.config.backup.branch,
            util::short_sha(&commit)
        ),
        "  only this machine's explicit file selections may be applied".into(),
    ];
    for path in &ctx.config.backup.files {
        let current = read_file(&live_path(path)?)?;
        let incoming_bytes = incoming.files.get(path);
        // An unselected remote path is not a deletion instruction.
        if !incoming.metadata.files.contains(path) {
            lines.push(format!("  keep {path}: not selected in remote snapshot"));
            continue;
        }
        let desired = if path == "manager/config.toml" {
            Some(restored_manager(
                incoming_bytes.map(Vec::as_slice),
                current.as_deref(),
            )?)
        } else {
            incoming_bytes.cloned()
        };
        if current != desired {
            lines.push(format!(
                "  {} {path}",
                if desired.is_some() {
                    "restore"
                } else {
                    "remove selected file"
                }
            ));
            apply.insert(path.clone(), (current, desired));
        }
    }
    for path in &incoming.metadata.files {
        if !ctx.config.backup.files.contains(path) {
            lines.push(format!(
                "  skip {path}: not explicitly selected on this machine"
            ));
        }
    }
    for plugin in &local.metadata.plugins {
        if !incoming
            .metadata
            .plugins
            .iter()
            .any(|incoming| incoming.id == plugin.id)
        {
            lines.push(format!(
                "  keep unrelated local plugin {} (not removed)",
                plugin.id
            ));
        }
    }
    for plugin in &local.metadata.local_only {
        lines.push(format!(
            "  keep local-only plugin {}: {}",
            plugin.id,
            util::sanitize(&plugin.reason)
        ));
    }
    unsupported_lines(&incoming, &mut lines);
    // All manifests are reviewed up front, not only after preceding plugins
    // have already changed. install_git retains its own authoritative review.
    for plugin in &incoming.metadata.plugins {
        lines.push(format!(
            "  {} {} {} @ {} (enabled={}, auto-update={}, follows={})",
            if local_summary.plugins.get(&plugin.id) == Some(plugin) {
                "keep"
            } else {
                "install exact"
            },
            plugin.id,
            util::sanitize(&plugin.version),
            util::short_sha(&plugin.commit),
            plugin.enabled,
            plugin.auto_update,
            util::sanitize(plugin.follow_ref.as_deref().unwrap_or("default branch"))
        ));
        if local_summary.plugins.get(&plugin.id) != Some(plugin) {
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
                .ok_or_else(|| {
                    anyhow!("{} has no {manifest_path} at {}", plugin.id, plugin.commit)
                })?;
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
        drift(
            &summary(&collect(&ctx.herdr, &ctx.config)?),
            Some(&local_summary),
            &incoming,
        ),
    )?;
    check_repo_unchanged(&session.directory, &repo_before)?;
    for plugin in &incoming.metadata.plugins {
        if local_summary.plugins.get(&plugin.id) != Some(plugin) {
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
    let actual = collect(&ctx.herdr, &refreshed_config)?;
    session.state.baseline = Some(Baseline {
        remote: remote.into(),
        branch: ctx.config.backup.branch.clone(),
        commit,
        summary: summary(&actual),
    });
    session.save()?;
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
    for (selector, path) in [
        ("herdr/config.toml", paths::herdr_config_toml()),
        ("manager/config.toml", paths::pm_config_file()),
    ] {
        if let Ok(Some(bytes)) = read_file(&path) {
            if plaintext(&bytes, selector).is_ok() {
                result.push(selector.into());
            }
        }
    }
    for plugin in Herdr::detect(false).list()? {
        if plugin.plugin_id == paths::PLUGIN_ID
            || plugin.plugin_id == paths::OLD_PLUGIN_ID
            || plugin_id(&plugin.plugin_id).is_err()
        {
            continue;
        }
        let root = paths::herdr_plugin_config_dir(&plugin.plugin_id);
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

    #[test]
    fn selected_local_edits_and_plugin_drift_stop_pull() {
        let value = plugin();
        let incoming = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: vec!["herdr/config.toml".into()],
                plugins: vec![value.clone()],
                local_only: vec![],
            },
            files: BTreeMap::from([("herdr/config.toml".into(), b"theme = 'remote'\n".to_vec())]),
        };
        let baseline = summary(&incoming);
        assert!(drift(&baseline, Some(&baseline), &incoming).is_empty());
        let mut local = baseline.clone();
        local.files.insert(
            "herdr/config.toml".into(),
            Some(digest(b"theme = 'local'\n")),
        );
        local.plugins.get_mut(&value.id).unwrap().auto_update = true;
        let conflicts = drift(&local, Some(&baseline), &incoming);
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts
            .iter()
            .any(|path| path.contains("herdr/config.toml")));
        assert!(conflicts.iter().any(|path| path.contains("test.plugin")));
    }

    #[test]
    fn local_only_collision_stops_pull_even_with_unchanged_baseline() {
        let value = plugin();
        let incoming = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: vec![],
                plugins: vec![value.clone()],
                local_only: vec![],
            },
            files: BTreeMap::new(),
        };
        let local = Summary {
            local_only: BTreeMap::from([(value.id.clone(), "local developer link".into())]),
            ..Summary::default()
        };
        for baseline in [None, Some(&local)] {
            let conflicts = drift(&local, baseline, &incoming);
            assert!(conflicts.iter().any(
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
        let directory = std::env::temp_dir().join(format!(
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
        };
        let metadata = session.capture(&snapshot).unwrap();
        assert!(metadata.files.iter().any(|selected| selected == path));
        assert_eq!(
            fs::read(session.directory.join(path)).unwrap().as_slice(),
            bytes.as_slice()
        );
        assert_eq!(session.state.captured[path], digest(bytes));
        // Selecting an absent live file explicitly authorizes its deletion;
        // merely omitting it from this machine's allowlist did not.
        snapshot.metadata.files.push(path.into());
        session.capture(&snapshot).unwrap();
        assert!(!session.directory.join(path).exists());
        assert!(session.state.removed.contains(path));
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
            paths::herdr_plugin_config_dir(&value.id).join("settings")
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
        };
        let incoming = Snapshot {
            metadata: Metadata {
                schema: SCHEMA,
                files: local.metadata.files.clone(),
                plugins: vec![],
                local_only: vec![],
            },
            files: BTreeMap::from([("manager/config.toml".into(), b"icons = false\n".to_vec())]),
        };
        assert!(drift(&summary(&local), None, &incoming).is_empty());
        let mut local = local;
        local
            .files
            .insert("herdr/config.toml".into(), b"theme = 'local'\n".to_vec());
        let conflicts = drift(&summary(&local), None, &incoming);
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
