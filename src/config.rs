//! `config.toml` in the manager's Herdr config directory
//! (`herdr plugin config-dir julienpoirou.herdr-plugin-manager`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MarketKind {
    /// GitHub repository search by topic (GitHub Enterprise, custom topics).
    #[default]
    Github,
    /// The official herdr.dev index behind herdr.dev/plugins.
    Herdr,
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
            MarketKind::Herdr => "herdr",
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
    /// (index, and herdr to override the herdr.dev index), or git remote
    /// (git-index).
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
    /// Upper bound on listed entries per source.
    #[serde(default = "default_max")]
    pub max: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_topic() -> String {
    "herdr-plugin".to_string()
}

fn default_max() -> usize {
    10_000
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

/// Where an auto-update moves a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Newest commit of the branch or tag the plugin follows.
    #[default]
    Latest,
    /// Newest version tag.
    Release,
    /// Newest version tag with the same major version.
    Minor,
    /// Newest version tag with the same major and minor version.
    Patch,
}

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Strategy::Latest => "latest",
            Strategy::Release => "release",
            Strategy::Minor => "minor",
            Strategy::Patch => "patch",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Strategy::Latest => "newest commit",
            Strategy::Release => "newest version tag",
            Strategy::Minor => "newest tag of the same major version",
            Strategy::Patch => "newest tag of the same minor version",
        }
    }

    /// How many leading version numbers must stay the same (tag strategies).
    pub fn same(self) -> Option<usize> {
        match self {
            Strategy::Latest => None,
            Strategy::Release => Some(0),
            Strategy::Minor => Some(1),
            Strategy::Patch => Some(2),
        }
    }
}

/// What starts an auto-update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// When the Herdr server starts.
    #[default]
    Startup,
    /// When the popup opens.
    Popup,
    /// At most once per `min_interval`, checked at server start, when the
    /// popup opens and when a tab or pane is created.
    MinInterval,
    /// Only `herdr-pm auto-update`.
    Manual,
}

impl Trigger {
    pub fn label(self) -> &'static str {
        match self {
            Trigger::Startup => "startup",
            Trigger::Popup => "popup",
            Trigger::MinInterval => "min-interval",
            Trigger::Manual => "manual",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Trigger::Startup => "when the Herdr server starts",
            Trigger::Popup => "when the popup opens",
            Trigger::MinInterval => "at most once per min_interval",
            Trigger::Manual => "only with herdr-pm auto-update",
        }
    }
}

/// The format of the change log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HistoryFormat {
    /// `history.jsonl`, this manager's own fields.
    #[default]
    Jsonl,
    /// `history.ecs.ndjson`, Elastic Common Schema (Elastic, Wazuh…).
    Ecs,
    /// Nothing is logged.
    Off,
}

impl HistoryFormat {
    pub fn label(self) -> &'static str {
        match self {
            HistoryFormat::Jsonl => "jsonl",
            HistoryFormat::Ecs => "ecs",
            HistoryFormat::Off => "off",
        }
    }
}

/// What asks whether an auto-update is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// The `[[startup]]` hook.
    Startup,
    /// The `[[events]]` hooks (a tab or pane was created).
    Event,
    /// The popup opened.
    Popup,
}

fn default_min_interval() -> String {
    "6h".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoUpdate {
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub trigger: Trigger,
    /// For `trigger = "min-interval"`: `30m`, `6h`, `1d`, `1h30m`.
    #[serde(default = "default_min_interval")]
    pub min_interval: String,
    /// Per-plugin strategies, by plugin id.
    #[serde(default)]
    pub plugins: BTreeMap<String, Strategy>,
}

impl Default for AutoUpdate {
    fn default() -> Self {
        AutoUpdate {
            strategy: Strategy::default(),
            trigger: Trigger::default(),
            min_interval: default_min_interval(),
            plugins: BTreeMap::new(),
        }
    }
}

impl AutoUpdate {
    pub fn strategy_for(&self, plugin_id: &str) -> Strategy {
        self.plugins
            .get(plugin_id)
            .copied()
            .unwrap_or(self.strategy)
    }

    /// `min_interval` in seconds (checked by `Config::check`).
    pub fn min_interval_secs(&self) -> u64 {
        crate::util::parse_duration(&self.min_interval).unwrap_or(6 * 3600)
    }

    /// Whether `hook` starts an auto-update now; `last` is when one last
    /// started.
    pub fn due(&self, hook: Hook, now: u64, last: u64) -> bool {
        match (self.trigger, hook) {
            (Trigger::Startup, Hook::Startup) | (Trigger::Popup, Hook::Popup) => true,
            (Trigger::MinInterval, _) => now.saturating_sub(last) >= self.min_interval_secs(),
            _ => false,
        }
    }
}

/// Machine-local opt-in plaintext snapshots. Network sync is always explicit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Backup {
    pub enabled: bool,
    pub remote: Option<String>,
    pub branch: String,
    pub files: Vec<String>,
}

impl Default for Backup {
    fn default() -> Self {
        Self {
            enabled: false,
            remote: None,
            branch: "main".to_string(),
            files: Vec::new(),
        }
    }
}

impl Backup {
    /// The local snapshot repository: an internal detail of the manager.
    pub fn directory(&self) -> PathBuf {
        paths::pm_home().join("backup")
    }

    fn check(&self) -> Result<()> {
        if let Some(remote) = &self.remote {
            crate::git::validate_remote(remote).context("invalid backup.remote")?;
        }
        if self.branch.trim().is_empty()
            || self.branch.starts_with('-')
            || self.branch.chars().any(char::is_control)
        {
            bail!("backup.branch must name a Git branch");
        }
        let mut selected = std::collections::BTreeSet::new();
        for file in &self.files {
            let parts: Vec<_> = file.split('/').collect();
            let supported = matches!(file.as_str(), "herdr/config.toml" | "manager/config.toml")
                || (parts.len() >= 3 && parts[0] == "plugins");
            if !supported
                || parts.iter().any(|part| {
                    part.is_empty()
                        || matches!(*part, "." | "..")
                        || part
                            .chars()
                            .any(|c| c.is_control() || "\\:*?[]".contains(c))
                })
            {
                bail!("backup.files requires exact portable file selectors, not paths or globs: {file:?}");
            }
            if !selected.insert(file) {
                bail!("backup.files contains duplicate selector {file:?}");
            }
        }
        Ok(())
    }
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
    /// Editor for the `E` key (falls back to VISUAL, EDITOR, then vi/notepad).
    #[serde(default)]
    pub editor: Option<String>,
    /// Install GitHub shorthands through this manager's git pipeline rather
    /// than Herdr's installer, so `herdr plugin update` leaves them alone.
    #[serde(default = "default_true")]
    pub github_via_git: bool,
    /// Keep a `herdr-pm` link to the current installation on the user's
    /// PATH (`integrate`).
    #[serde(default)]
    pub cli_on_path: bool,
    /// Keep the `herdr-pm` agent skill linked for the coding agents found.
    #[serde(default)]
    pub agent_skill: bool,
    /// Project and owner pictures in the popup, where the terminal draws
    /// images.
    #[serde(default = "default_true")]
    pub icons: bool,
    /// How changes are logged for `herdr-pm logs` (and log shippers).
    #[serde(default)]
    pub history: HistoryFormat,
    /// Directory for change logs; defaults to the manager's data directory.
    #[serde(default)]
    pub log_dir: Option<PathBuf>,
    /// Local backup controls and exact plaintext file approvals; never imported.
    #[serde(default)]
    pub backup: Backup,
    #[serde(default)]
    pub auto_update: AutoUpdate,
    /// Sources added to the built-in ones (`builtin_markets`); one with the
    /// name of a built-in source overrides the fields it sets.
    #[serde(default)]
    pub marketplace: Vec<MarketSource>,
}

fn market(name: &str, kind: MarketKind, url: Option<&str>) -> MarketSource {
    MarketSource {
        name: name.into(),
        kind,
        url: url.map(str::to_string),
        topic: default_topic(),
        path: None,
        git_ref: None,
        token: None,
        token_env: None,
        token_cmd: None,
        clone: CloneProtocol::Https,
        max: default_max(),
        enabled: true,
    }
}

/// herdr.dev, gitlab.com and codeberg.org, searched by the `herdr-plugin`
/// topic.
pub fn builtin_markets() -> Vec<MarketSource> {
    vec![
        market("herdr.dev", MarketKind::Herdr, None),
        market("gitlab.com", MarketKind::Gitlab, Some("https://gitlab.com")),
        market(
            "codeberg.org",
            MarketKind::Gitea,
            Some("https://codeberg.org"),
        ),
    ]
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
            github_via_git: true,
            cli_on_path: false,
            agent_skill: false,
            icons: true,
            history: HistoryFormat::default(),
            log_dir: None,
            backup: Backup::default(),
            auto_update: AutoUpdate::default(),
            marketplace: Vec::new(),
        }
        .with_builtin_markets()
    }
}

pub const EXAMPLE: &str = r#"# C in the popup edits this file. herdr.dev, gitlab.com and codeberg.org
# are built-in marketplaces; the README lists every key.

keep_installs = 3   # versions kept for rollback
market_ttl = 600    # seconds

[auto_update]
strategy = "latest" # latest, release, minor or patch
trigger = "startup" # startup, popup, min-interval or manual
"#;

impl Config {
    /// Built-in sources first, replaced by user sources of the same name,
    /// then the user's other sources.
    fn with_builtin_markets(mut self) -> Self {
        let mut user = std::mem::take(&mut self.marketplace);
        for builtin in builtin_markets() {
            match user.iter().position(|s| s.name == builtin.name) {
                Some(i) => self.marketplace.push(user.remove(i)),
                None => self.marketplace.push(builtin),
            }
        }
        self.marketplace.append(&mut user);
        self
    }

    /// A `[[marketplace]]` block named like a built-in source takes the
    /// built-in's `kind` and `url` unless it sets them, so
    /// `name = "codeberg.org"` with `enabled = false` only hides it.
    fn inherit_builtin_fields(table: &mut toml::Table) {
        let Some(toml::Value::Array(sources)) = table.get_mut("marketplace") else {
            return;
        };
        let builtins = builtin_markets();
        for source in sources.iter_mut().filter_map(toml::Value::as_table_mut) {
            let name = source.get("name").and_then(toml::Value::as_str);
            let Some(builtin) = builtins.iter().find(|b| Some(b.name.as_str()) == name) else {
                continue;
            };
            source
                .entry("kind")
                .or_insert_with(|| builtin.kind.label().into());
            if let Some(url) = &builtin.url {
                source.entry("url").or_insert_with(|| url.as_str().into());
            }
        }
    }

    pub fn path() -> PathBuf {
        paths::pm_config_file()
    }

    pub fn load() -> Result<Config> {
        Self::load_path(&Config::path(), true)
    }

    /// Loads legacy preferences in memory without modifying the config file.
    pub fn load_read_only() -> Result<Config> {
        Self::load_path(&Config::path(), false)
    }

    fn load_path(path: &std::path::Path, persist_migration: bool) -> Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("could not read {}", path.display()))
            }
        };
        let migrated = Self::migrate_text(&text)
            .with_context(|| format!("could not migrate config {}", path.display()))?;
        let config = Config::parse(migrated.as_deref().unwrap_or(&text))
            .with_context(|| format!("invalid config {}", path.display()))?;
        if let Some(updated) = migrated.filter(|_| persist_migration) {
            std::fs::write(path, updated)
                .with_context(|| format!("could not migrate config {}", path.display()))?;
        }
        Ok(config)
    }

    /// One-time persisted-key migrations, preserving values and comments:
    /// `logos` became `icons`, and `backup.directory` is no longer a setting
    /// (the snapshot repository always lives in the manager's data
    /// directory). Parsing itself deliberately accepts only the current schema.
    pub fn migrate_text(text: &str) -> Result<Option<String>> {
        let mut doc: toml_edit::DocumentMut = text.parse()?;
        let mut changed = false;
        if doc.contains_key("logos") {
            if doc.contains_key("icons") {
                bail!(
                    "config contains both logos and icons; remove logos after choosing the icons value"
                );
            }
            let (old_key, item) = doc.as_table_mut().remove_entry("logos").unwrap();
            let mut key = toml_edit::Key::new("icons");
            *key.leaf_decor_mut() = old_key.leaf_decor().clone();
            *key.dotted_decor_mut() = old_key.dotted_decor().clone();
            doc.as_table_mut().insert_formatted(&key, item);
            changed = true;
        }
        if let Some(backup) = doc.get_mut("backup").and_then(|b| b.as_table_like_mut()) {
            changed |= backup.remove("directory").is_some();
        }
        if !changed {
            return Ok(None);
        }
        let updated = doc.to_string();
        Self::parse(&updated)?;
        Ok(Some(updated))
    }

    /// Parses and checks a config file's text, built-in sources included.
    pub fn parse(text: &str) -> Result<Config> {
        let mut table: toml::Table = toml::from_str(text)?;
        Config::inherit_builtin_fields(&mut table);
        let config = Config::deserialize(table)?.with_builtin_markets();
        config.check()?;
        Ok(config)
    }

    pub fn check(&self) -> Result<()> {
        self.backup.check()?;
        if let Some(path) = &self.log_dir {
            if !path.is_absolute() {
                bail!("log_dir must be an absolute directory: {}", path.display());
            }
            match path.metadata() {
                Ok(metadata) if !metadata.is_dir() => {
                    bail!("log_dir is not a directory: {}", path.display());
                }
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(err)
                        .with_context(|| format!("could not inspect log_dir {}", path.display()));
                }
                _ => {}
            }
        }
        match crate::util::parse_duration(&self.auto_update.min_interval) {
            Some(secs) if secs >= 60 => {}
            Some(_) => bail!("auto_update.min_interval must be at least 1m"),
            None => bail!(
                "auto_update.min_interval {:?} is not a duration such as 30m, 6h or 1d",
                self.auto_update.min_interval
            ),
        }
        let mut names = std::collections::BTreeSet::new();
        for source in &self.marketplace {
            if source.name.trim().is_empty() {
                bail!("a [[marketplace]] entry has no name");
            }
            if !names.insert(source.name.as_str()) {
                bail!("two [[marketplace]] entries are named {:?}", source.name);
            }
            let needs_url = !matches!(source.kind, MarketKind::Github | MarketKind::Herdr);
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
    fn backup_defaults_do_not_grant_plaintext_files_or_require_remote() {
        let defaults = Config::parse("").unwrap();
        assert_eq!(defaults.backup, Backup::default());
        let enabled = Config::parse("[backup]\nenabled = true\n").unwrap();
        assert!(enabled.backup.enabled);
        assert!(enabled.backup.remote.is_none());
        assert!(enabled.backup.files.is_empty());
        assert_eq!(enabled.backup.branch, "main");
        assert!(Config::parse("[backup]\nunknown = true\n").is_err());
    }

    #[test]
    fn backup_file_approvals_are_exact_and_transport_is_checked() {
        for file in [
            "../secrets",
            "plugins/id/../secret",
            "plugins/id/*.toml",
            "plugins/id/file?",
            "plugins/id/C:\\\\secret",
            "/manager/config.toml",
        ] {
            let text = format!("[backup]\nfiles = [{}]\n", toml::Value::String(file.into()));
            assert!(Config::parse(&text).is_err(), "{file}");
        }
        assert!(Config::parse(
            "[backup]\nfiles = ['herdr/config.toml', 'plugins/a.b/nested/settings.toml']\n"
        )
        .is_ok());
        assert!(Config::parse(
            "[backup]\nfiles = ['manager/config.toml', 'manager/config.toml']\n"
        )
        .is_err());
        let legacy = "[backup]\nenabled = true # keep\ndirectory = '/old/snapshots'\nremote = 'https://git.example/herdr.git'\n";
        assert!(
            Config::parse(legacy).is_err(),
            "directory is no longer a setting"
        );
        let migrated = Config::migrate_text(legacy).unwrap().unwrap();
        assert!(!migrated.contains("directory"));
        assert!(migrated.contains("enabled = true # keep"));
        let config = Config::parse(&migrated).unwrap();
        assert!(config.backup.enabled);
        assert_eq!(config.backup.directory(), paths::pm_home().join("backup"));
        assert!(Config::parse("[backup]\nremote = 'ext::unsafe'\n").is_err());
        assert!(Config::parse("[backup]\nbranch = ''\n").is_err());
    }

    #[test]
    fn icons_migration_preserves_comments_and_runs_once_on_load() {
        let root = std::env::temp_dir().join(format!(
            "hpm-config-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.toml");
        let original = "# pictures preference\nlogos = false # keep this comment\n";
        std::fs::write(&path, original).unwrap();
        assert!(!Config::load_path(&path, false).unwrap().icons);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!Config::load_path(&path, true).unwrap().icons);
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("icons = false # keep this comment"));
        assert!(migrated.contains("# pictures preference"));
        assert!(!migrated.contains("logos"));
        assert!(Config::migrate_text(&migrated).unwrap().is_none());
        assert!(!Config::load_path(&path, true).unwrap().icons);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
        assert!(Config::parse(original).is_err());
        assert!(Config::migrate_text("logos = false\nicons = true\n").is_err());
        std::fs::write(&path, "logos = false\nunknown = true\n").unwrap();
        assert!(Config::load_path(&path, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "logos = false\nunknown = true\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn icons_and_log_directory_have_current_schema_defaults() {
        let defaults = Config::parse("").unwrap();
        assert!(defaults.icons);
        assert!(defaults.log_dir.is_none());
        assert!(!Config::parse("icons = false\n").unwrap().icons);
        assert!(Config::parse("log_dir = 'relative/logs'\n").is_err());
        let directory = std::env::temp_dir();
        let text = format!("log_dir = {:?}\n", directory.to_str().unwrap());
        assert_eq!(Config::parse(&text).unwrap().log_dir, Some(directory));
        assert!(!toml::to_string(&defaults).unwrap().contains("logos"));
    }

    #[test]
    fn example_config_parses() {
        let config = Config::parse(EXAMPLE).unwrap();
        let names: Vec<&str> = config.marketplace.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["herdr.dev", "gitlab.com", "codeberg.org"]);
        assert_eq!(config.keep_installs, 3);
        assert_eq!(config.auto_update.strategy, Strategy::Latest);
    }

    #[test]
    fn user_sources_override_builtins_by_name() {
        let config = Config::parse(
            "[[marketplace]]\nname = \"corp\"\nkind = \"gitea\"\nurl = \"https://git.corp\"\n\n\
             [[marketplace]]\nname = \"codeberg.org\"\nenabled = false\n\n\
             [[marketplace]]\nname = \"gitlab.com\"\nurl = \"https://gitlab.example\"\n",
        )
        .unwrap();
        let names: Vec<&str> = config.marketplace.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["herdr.dev", "gitlab.com", "codeberg.org", "corp"]);
        let codeberg = &config.marketplace[2];
        assert!(!codeberg.enabled);
        assert_eq!(codeberg.kind, MarketKind::Gitea);
        assert_eq!(codeberg.url.as_deref(), Some("https://codeberg.org"));
        let gitlab = &config.marketplace[1];
        assert_eq!(gitlab.kind, MarketKind::Gitlab);
        assert_eq!(gitlab.url.as_deref(), Some("https://gitlab.example"));
    }

    #[test]
    fn triggers_decide_which_hooks_update() {
        let auto = |trigger: &str| {
            Config::parse(&format!(
                "[auto_update]\ntrigger = \"{trigger}\"\nmin_interval = \"1h\"\n"
            ))
            .unwrap()
            .auto_update
        };
        let now = 100_000;
        let startup = auto("startup");
        assert!(startup.due(Hook::Startup, now, now));
        assert!(!startup.due(Hook::Event, now, 0));
        assert!(!startup.due(Hook::Popup, now, 0));
        let popup = auto("popup");
        assert!(popup.due(Hook::Popup, now, now));
        assert!(!popup.due(Hook::Startup, now, 0));
        let interval = auto("min-interval");
        assert!(interval.due(Hook::Event, now, now - 3600));
        assert!(!interval.due(Hook::Startup, now, now - 3599));
        let manual = auto("manual");
        assert!([Hook::Startup, Hook::Event, Hook::Popup]
            .iter()
            .all(|h| !manual.due(*h, now, 0)));
        assert!(Config::parse("[auto_update]\nmin_interval = \"30s\"\n").is_err());
        assert!(Config::parse("[auto_update]\nmin_interval = \"soon\"\n").is_err());
    }

    #[test]
    fn per_plugin_strategy_overrides_the_default() {
        let config = Config::parse(
            "[auto_update]\nstrategy = \"release\"\n[auto_update.plugins]\n\"a.b\" = \"patch\"\n",
        )
        .unwrap();
        assert_eq!(config.auto_update.strategy_for("a.b"), Strategy::Patch);
        assert_eq!(config.auto_update.strategy_for("c.d"), Strategy::Release);
        assert!(Config::parse("[auto_update]\nstrategy = \"nightly\"\n").is_err());
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
