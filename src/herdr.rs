//! Thin, typed wrapper over the `herdr plugin` CLI.
//!
//! Every call goes through the real `herdr` binary so its validation, locking
//! and live-server notification stay authoritative. The only exception is
//! enable/disable while no server is running: Herdr's CLI refuses those
//! offline, so we flip the flag in `plugins.json` under Herdr's own lock.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::paths;
use crate::util::{self, Version};

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct CommandSpec {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct ActionInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub contexts: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct EventInfo {
    #[serde(default)]
    pub on: String,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct PaneInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct LinkHandlerInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub action: String,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Local,
    Github,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SourceInfo {
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub subdir: Option<String>,
    #[serde(default)]
    pub requested_ref: Option<String>,
    #[serde(default)]
    pub resolved_commit: Option<String>,
    #[serde(default)]
    pub managed_path: Option<String>,
    #[serde(default)]
    pub installed_unix_ms: Option<u64>,
}

impl SourceInfo {
    /// `owner/repo[/subdir]` for GitHub-managed plugins.
    pub fn github_spec(&self) -> Option<String> {
        let (owner, repo) = (self.owner.as_deref()?, self.repo.as_deref()?);
        Some(match self.subdir.as_deref().filter(|s| !s.is_empty()) {
            Some(subdir) => format!("{owner}/{repo}/{subdir}"),
            None => format!("{owner}/{repo}"),
        })
    }

    pub fn github_slug(&self) -> Option<String> {
        Some(format!(
            "{}/{}",
            self.owner.as_deref()?,
            self.repo.as_deref()?
        ))
    }
}

/// One entry of `herdr plugin list --json`.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct Plugin {
    pub plugin_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub min_herdr_version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub manifest_path: String,
    #[serde(default)]
    pub plugin_root: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub build: Vec<CommandSpec>,
    #[serde(default)]
    pub startup: Vec<CommandSpec>,
    #[serde(default)]
    pub actions: Vec<ActionInfo>,
    #[serde(default)]
    pub events: Vec<EventInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub link_handlers: Vec<LinkHandlerInfo>,
    #[serde(default)]
    pub source: SourceInfo,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Plugin {
    pub fn is_github(&self) -> bool {
        self.source.kind == SourceKind::Github
    }
}

#[derive(Debug, Clone)]
pub struct Herdr {
    pub bin: PathBuf,
    pub dry_run: bool,
}

#[derive(Deserialize)]
struct ListEnvelope {
    result: ListResult,
}

#[derive(Deserialize)]
struct ListResult {
    #[serde(default)]
    plugins: Vec<Plugin>,
}

/// Pulls the human message out of Herdr's JSON error envelope
/// (`{"error":{"code":..,"message":..}}`), or returns the text as-is.
pub fn error_message(stderr: &str, stdout: &str) -> String {
    for text in [stderr, stdout] {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('{') {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(message) = value["error"]["message"].as_str() {
                        return message.to_string();
                    }
                }
            }
        }
    }
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("herdr failed")
        .to_string()
}

fn error_code(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|v| v["error"]["code"].as_str().map(str::to_string))
    })
}

impl Herdr {
    pub fn detect(dry_run: bool) -> Herdr {
        let bin = std::env::var_os("HERDR_PM_HERDR_BIN")
            .or_else(|| std::env::var_os("HERDR_BIN_PATH"))
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("herdr"));
        Herdr { bin, dry_run }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        // Keep Herdr from treating these calls as coming from a plugin pane's
        // own environment when that would change behavior.
        command.env_remove("HERDR_PLUGIN_ACTION_ID");
        command
    }

    fn display(&self, args: &[&str]) -> String {
        let mut out = self.bin.display().to_string();
        for arg in args {
            if arg.contains(' ') || arg.is_empty() {
                out.push_str(&format!(" '{arg}'"));
            } else {
                out.push(' ');
                out.push_str(arg);
            }
        }
        out
    }

    /// Runs a quick herdr command and returns stdout, or herdr's message.
    fn run(&self, args: &[&str]) -> Result<String> {
        let mut command = self.command();
        command.args(args);
        let output = util::run_captured(command, Some(Duration::from_secs(30)))
            .with_context(|| format!("could not run {}", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            Ok(stdout)
        } else {
            let code = error_code(&stderr);
            let message = error_message(&stderr, &stdout);
            Err(anyhow!(HerdrError { code, message }))
        }
    }

    fn mutate(&self, args: &[&str]) -> Result<String> {
        if self.dry_run {
            let shown = self.display(args);
            eprintln!("[dry-run] {shown}");
            return Ok(String::new());
        }
        self.run(args)
    }

    pub fn version(&self) -> Result<Version> {
        let out = self.run(&["--version"])?;
        let text = out.trim();
        let raw = text.strip_prefix("herdr").unwrap_or(text).trim();
        Version::parse(raw).ok_or_else(|| anyhow!("unexpected `herdr --version` output: {text}"))
    }

    pub fn list(&self) -> Result<Vec<Plugin>> {
        let out = self.run(&["plugin", "list", "--json"])?;
        let line = out
            .lines()
            .find(|l| l.trim_start().starts_with('{'))
            .ok_or_else(|| anyhow!("herdr plugin list printed no JSON"))?;
        let envelope: ListEnvelope =
            serde_json::from_str(line).context("could not parse `herdr plugin list --json`")?;
        Ok(envelope.result.plugins)
    }

    /// Registers (or re-registers) a plugin directory. Re-linking an id that
    /// is already registered replaces it in place, which is how updates and
    /// rollbacks of git-managed plugins switch installations atomically.
    pub fn link(&self, path: &Path, enabled: bool) -> Result<()> {
        let path = path.display().to_string();
        let mut args = vec!["plugin", "link", path.as_str()];
        if !enabled {
            args.push("--disabled");
        }
        self.mutate(&args).map(|_| ())
    }

    /// Removes a registration. `uninstall` (unlike `unlink`) also works when
    /// no server is running, for local and GitHub plugins alike. Herdr only
    /// ever deletes checkouts it cloned itself.
    pub fn unregister(&self, plugin_id: &str) -> Result<()> {
        self.mutate(&["plugin", "uninstall", plugin_id]).map(|_| ())
    }

    pub fn set_enabled(&self, plugin_id: &str, enabled: bool) -> Result<()> {
        let verb = if enabled { "enable" } else { "disable" };
        match self.mutate(&["plugin", verb, plugin_id]) {
            Ok(_) => Ok(()),
            Err(err) if is_server_not_running(&err) => set_enabled_offline(plugin_id, enabled),
            Err(err) => Err(err),
        }
    }

    pub fn invoke(&self, action_id: &str) -> Result<()> {
        self.mutate(&["plugin", "action", "invoke", action_id])
            .map(|_| ())
    }

    /// Opens one of a plugin's panes as a focused popup.
    pub fn open_popup(&self, plugin_id: &str, entrypoint: &str) -> Result<()> {
        self.mutate(&[
            "plugin",
            "pane",
            "open",
            "--plugin",
            plugin_id,
            "--entrypoint",
            entrypoint,
            // The manifest declares placement = "popup"; the CLI's
            // --placement flag has no popup value.
            "--focus",
        ])
        .map(|_| ())
    }

    /// Runs an interactive herdr command on the terminal (install/update
    /// previews need the tty). Returns its exit status.
    pub fn interactive(&self, args: &[&str]) -> Result<ExitStatus> {
        if self.dry_run {
            eprintln!("[dry-run] {}", self.display(args));
            return Ok(success_status());
        }
        let mut command = self.command();
        command
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command
            .status()
            .with_context(|| format!("could not run {}", self.bin.display()))
    }

    pub fn install_github(
        &self,
        spec: &str,
        reference: Option<&str>,
        yes: bool,
    ) -> Result<ExitStatus> {
        let mut args = vec!["plugin", "install", spec];
        if let Some(reference) = reference {
            args.push("--ref");
            args.push(reference);
        }
        if yes {
            args.push("--yes");
        }
        self.interactive(&args)
    }

    pub fn update_github(&self, plugin_id: &str, yes: bool) -> Result<ExitStatus> {
        let mut args = vec!["plugin", "update", plugin_id];
        if yes {
            args.push("--yes");
        }
        self.interactive(&args)
    }
}

#[cfg(unix)]
fn success_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn success_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[derive(Debug)]
pub struct HerdrError {
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HerdrError {}

pub fn is_server_not_running(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HerdrError>()
        .is_some_and(|e| e.code.as_deref() == Some("server_not_running"))
}

/// Resolves a registry symlink even when its final target does not exist
/// yet, matching Herdr's stow-friendly write behavior.
fn resolve_write_target(path: &Path) -> Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..40 {
        let metadata = match std::fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(err) => return Err(err.into()),
        };
        if !metadata.file_type().is_symlink() {
            return Ok(target);
        }
        let link = std::fs::read_link(&target)?;
        target = if link.is_absolute() {
            link
        } else {
            target.parent().unwrap_or_else(|| Path::new(".")).join(link)
        };
    }
    bail!("too many plugin registry symlinks")
}

/// Flips `enabled` in `plugins.json` under Herdr's registry lock. Only used
/// when no server is running (the CLI refuses enable/disable offline), so
/// there is no live state to notify. Unknown fields are preserved verbatim.
fn set_enabled_offline(plugin_id: &str, enabled: bool) -> Result<()> {
    let registry = paths::herdr_plugins_json();
    let lock_path = paths::herdr_config_dir().join(".plugins.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("could not open {}", lock_path.display()))?;
    lock.lock()?;
    let target = resolve_write_target(&registry)?;
    let text = std::fs::read_to_string(&target)
        .with_context(|| format!("could not read {}", target.display()))?;
    let mut entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).context("plugins.json is not valid JSON")?;
    let mut found = false;
    for entry in &mut entries {
        if entry["plugin_id"].as_str() == Some(plugin_id) {
            entry["enabled"] = serde_json::Value::Bool(enabled);
            found = true;
        }
    }
    if !found {
        bail!("plugin not installed: {plugin_id}");
    }
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&entries)?)?;
    std::fs::rename(&tmp, &target)?;
    drop(lock);
    Ok(())
}

/// Plugin-action key bindings from Herdr's `config.toml`:
/// `[[keys.command]]` entries with `type = "plugin_action"`.
pub fn action_keybindings() -> BTreeMap<String, Vec<String>> {
    let path = paths::herdr_config_toml();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    parse_action_keybindings(&text)
}

pub fn parse_action_keybindings(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(value) = text.parse::<toml::Table>() else {
        return out;
    };
    let commands = value
        .get("keys")
        .and_then(|k| k.get("command"))
        .and_then(|c| c.as_array());
    for entry in commands.into_iter().flatten() {
        let kind = entry.get("type").and_then(|v| v.as_str());
        let command = entry.get("command").and_then(|v| v.as_str());
        let key = entry.get("key").and_then(|v| v.as_str());
        if let (Some("plugin_action"), Some(command), Some(key)) = (kind, command, key) {
            out.entry(command.to_string())
                .or_default()
                .push(key.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_output() {
        let json = r#"{"id":"cli:plugin","result":{"plugins":[{"actions":[{"command":["echo","hi"],"id":"hello","title":"Say hello"}],"enabled":false,"manifest_path":"/p/herdr-plugin.toml","min_herdr_version":"0.7.0","name":"Demo","platforms":["linux"],"plugin_id":"t.demo","plugin_root":"/p","source":{"kind":"local"},"version":"0.2.0"},{"plugin_id":"t.gh","name":"Gh","version":"1.0.0","enabled":true,"source":{"kind":"github","owner":"o","repo":"r","subdir":"x","requested_ref":"main","resolved_commit":"abc"}}],"type":"plugin_list"}}"#;
        let envelope: ListEnvelope = serde_json::from_str(json).unwrap();
        let plugins = envelope.result.plugins;
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].actions[0].id, "hello");
        assert!(!plugins[0].enabled);
        assert!(plugins[1].is_github());
        assert_eq!(plugins[1].source.github_spec().as_deref(), Some("o/r/x"));
    }

    #[test]
    fn extracts_error_messages() {
        let stderr = r#"{"id":"cli:plugin","error":{"code":"server_not_running","message":"no herdr server is running"}}"#;
        assert_eq!(error_message(stderr, ""), "no herdr server is running");
        assert_eq!(error_code(stderr).as_deref(), Some("server_not_running"));
        assert_eq!(error_message("plain failure\nmore", ""), "plain failure");
    }

    #[test]
    fn parses_keybindings() {
        let toml = r#"
[ui]
mouse_capture = true

[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "gitbridge.open"

[[keys.command]]
key = "prefix+g"
type = "shell"
command = "lazygit"

[[keys.command]]
key = "alt+p"
type = "plugin_action"
command = "gitbridge.open"
"#;
        let keys = parse_action_keybindings(toml);
        assert_eq!(keys["gitbridge.open"], vec!["prefix+p", "alt+p"]);
        assert_eq!(keys.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn registry_write_target_follows_dangling_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "hpm-registry-link-{}-{}",
            std::process::id(),
            util::now_unix()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let link = root.join("plugins.json");
        std::os::unix::fs::symlink("actual/plugins.json", &link).unwrap();
        assert_eq!(
            resolve_write_target(&link).unwrap(),
            root.join("actual/plugins.json")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
