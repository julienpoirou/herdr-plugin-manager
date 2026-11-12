//! The `C` settings screen: the editable parts of `config.toml`, changed in
//! place so the file keeps its comments and layout.

use anyhow::{anyhow, bail, Context, Result};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::config::{builtin_markets, Config, HistoryFormat, Strategy, Trigger};

const HISTORY: [HistoryFormat; 3] = [HistoryFormat::Jsonl, HistoryFormat::Ecs, HistoryFormat::Off];

const STRATEGIES: [Strategy; 4] = [
    Strategy::Latest,
    Strategy::Release,
    Strategy::Minor,
    Strategy::Patch,
];

const TRIGGERS: [Trigger; 4] = [
    Trigger::Startup,
    Trigger::Popup,
    Trigger::MinInterval,
    Trigger::Manual,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    KeepInstalls,
    MarketTtl,
    CheckConcurrency,
    UseGhCli,
    GithubViaGit,
    Icons,
    /// Format of the change log.
    History,
    LogDir,
    BackupEnabled,
    BackupRemote,
    BackupBranch,
    BackupFile(String),
    BackupError,
    /// What keeps automatic backup from working; shown only then.
    BackupProblem,
    Editor,
    Strategy,
    Trigger,
    MinInterval,
    /// `herdr-pm` on the user's PATH.
    CliOnPath,
    /// The `herdr-pm` agent skill.
    AgentSkill,
    /// Strategy of one plugin (`[auto_update.plugins]`).
    PluginStrategy(String),
    /// A marketplace source, turned on or off.
    Market(String),
    Alias(String),
    AddAlias,
}

/// How a field is changed, for the key hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Number,
    Toggle,
    Choice,
    Text,
    Alias,
    AddAlias,
    ReadOnly,
}

impl Field {
    pub fn kind(&self) -> Kind {
        match self {
            Field::KeepInstalls | Field::MarketTtl | Field::CheckConcurrency => Kind::Number,
            Field::UseGhCli
            | Field::GithubViaGit
            | Field::Icons
            | Field::BackupEnabled
            | Field::BackupFile(_)
            | Field::Market(_)
            | Field::CliOnPath
            | Field::AgentSkill => Kind::Toggle,
            Field::Strategy | Field::Trigger | Field::History | Field::PluginStrategy(_) => {
                Kind::Choice
            }
            Field::Editor
            | Field::MinInterval
            | Field::LogDir
            | Field::BackupRemote
            | Field::BackupBranch => Kind::Text,
            Field::BackupError | Field::BackupProblem => Kind::ReadOnly,
            Field::Alias(_) => Kind::Alias,
            Field::AddAlias => Kind::AddAlias,
        }
    }

    /// The prompt label and the text it starts with, for fields edited by
    /// typing.
    pub fn prompt(&self, config: &Config) -> Option<(String, String)> {
        Some(match self {
            Field::MinInterval => (
                "minimum time between auto-updates (30m, 6h, 1d): ".into(),
                config.auto_update.min_interval.clone(),
            ),
            Field::KeepInstalls => (
                "versions kept for rollback: ".into(),
                config.keep_installs.to_string(),
            ),
            Field::MarketTtl => (
                "seconds before the marketplace is refreshed: ".into(),
                config.market_ttl.to_string(),
            ),
            Field::CheckConcurrency => (
                "parallel update checks: ".into(),
                config.check_concurrency.to_string(),
            ),
            Field::Editor => (
                "editor command (empty: VISUAL, EDITOR, then vi/notepad): ".into(),
                config.editor.clone().unwrap_or_default(),
            ),
            Field::LogDir => (
                "absolute log directory (Delete resets to default): ".into(),
                effective_log_dir(config).display().to_string(),
            ),
            Field::BackupRemote => (
                "Git remote URL (empty: local capture only): ".into(),
                config.backup.remote.clone().unwrap_or_default(),
            ),
            Field::BackupBranch => (
                "Git branch (Delete resets to main): ".into(),
                config.backup.branch.clone(),
            ),
            Field::Alias(name) => (
                format!("{name} = "),
                config.aliases.get(name).cloned().unwrap_or_default(),
            ),
            Field::AddAlias => ("new alias (name url): ".into(), String::new()),
            _ => return None,
        })
    }
}

pub struct Row {
    pub section: &'static str,
    pub label: String,
    pub value: String,
    pub field: Field,
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.to_string()
}

fn effective_log_dir(config: &Config) -> std::path::PathBuf {
    config.log_dir.clone().unwrap_or_else(crate::paths::pm_home)
}

/// Proves the selected directory can receive logs before changing the config.
fn prepare_log_dir(path: &std::path::Path) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_PROBE: AtomicU64 = AtomicU64::new(0);
    if !path.is_absolute() {
        bail!("log_dir must be an absolute directory: {}", path.display());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("could not create log directory {}", path.display()))?;
    loop {
        let probe = path.join(format!(
            ".herdr-pm-write-probe-{}-{}",
            std::process::id(),
            NEXT_PROBE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(mut file) => {
                use std::io::Write;
                let result = file.write_all(b"log directory write check\n");
                drop(file);
                let cleanup = std::fs::remove_file(&probe);
                result.with_context(|| {
                    format!("log directory is not writable: {}", path.display())
                })?;
                cleanup
                    .with_context(|| format!("could not remove write probe {}", probe.display()))?;
                return Ok(());
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("log directory is not writable: {}", path.display()))
            }
        }
    }
}

fn change_log_dir(doc: &mut DocumentMut, action: &Action, default: &std::path::Path) -> Result<()> {
    match action {
        Action::Set(text) => {
            prepare_log_dir(std::path::Path::new(text.trim()))?;
            put(&mut doc["log_dir"], text.trim());
        }
        Action::Delete => {
            prepare_log_dir(default)?;
            doc.remove("log_dir");
        }
        _ => {}
    }
    Ok(())
}

/// Every editable setting, by section. `plugins` are the ids that can get
/// their own strategy; `problem` is the automatic backup's current failure.
pub fn rows(
    config: &Config,
    plugins: &[String],
    candidates: Result<&[String], &str>,
    problem: Option<&str>,
) -> Vec<Row> {
    let row = |section, label: &str, value: String, field| Row {
        section,
        label: label.to_string(),
        value,
        field,
    };
    let mut rows = vec![
        row(
            "General",
            "keep_installs",
            config.keep_installs.to_string(),
            Field::KeepInstalls,
        ),
        row(
            "General",
            "market_ttl",
            format!("{} s", config.market_ttl),
            Field::MarketTtl,
        ),
        row(
            "General",
            "check_concurrency",
            config.check_concurrency.to_string(),
            Field::CheckConcurrency,
        ),
        row(
            "General",
            "use_gh_cli",
            on_off(config.use_gh_cli),
            Field::UseGhCli,
        ),
        row(
            "General",
            "github_via_git",
            if config.github_via_git {
                "on: owner/repo installs through git".to_string()
            } else {
                "off: owner/repo installs through herdr".to_string()
            },
            Field::GithubViaGit,
        ),
        row(
            "General",
            "icons",
            if config.icons {
                "on: pictures where the terminal draws images".to_string()
            } else {
                "off".to_string()
            },
            Field::Icons,
        ),
        row(
            "General",
            "history",
            match config.history {
                HistoryFormat::Jsonl => "jsonl: history.jsonl".to_string(),
                HistoryFormat::Ecs => "ecs: history.ecs.ndjson (Elastic Common Schema)".to_string(),
                HistoryFormat::Off => "off: changes are not logged".to_string(),
            },
            Field::History,
        ),
        row(
            "General",
            "log_dir",
            effective_log_dir(config).display().to_string(),
            Field::LogDir,
        ),
        row(
            "General",
            "editor",
            config
                .editor
                .clone()
                .unwrap_or_else(|| "default (VISUAL, EDITOR)".into()),
            Field::Editor,
        ),
        row(
            "Auto-update",
            "strategy",
            format!(
                "{}: {}",
                config.auto_update.strategy.label(),
                config.auto_update.strategy.describe()
            ),
            Field::Strategy,
        ),
        row(
            "Auto-update",
            "trigger",
            format!(
                "{}: {}",
                config.auto_update.trigger.label(),
                config.auto_update.trigger.describe()
            ),
            Field::Trigger,
        ),
        row(
            "Auto-update",
            "min_interval",
            if config.auto_update.trigger == Trigger::MinInterval {
                config.auto_update.min_interval.clone()
            } else {
                format!("{} (used by min-interval)", config.auto_update.min_interval)
            },
            Field::MinInterval,
        ),
    ];
    let mut ids: Vec<&String> = plugins
        .iter()
        .chain(config.auto_update.plugins.keys())
        .collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        let value = match config.auto_update.plugins.get(id) {
            Some(s) => s.label().to_string(),
            None => format!("default ({})", config.auto_update.strategy.label()),
        };
        rows.push(row(
            "Auto-update",
            id,
            value,
            Field::PluginStrategy(id.clone()),
        ));
    }
    for source in &config.marketplace {
        rows.push(row(
            "Marketplaces",
            &source.name,
            format!(
                "{} · {} {}",
                on_off(source.enabled),
                source.kind.label(),
                source.url.as_deref().unwrap_or("")
            )
            .trim_end()
            .to_string(),
            Field::Market(source.name.clone()),
        ));
    }
    for (name, url) in &config.aliases {
        rows.push(row(
            "Aliases",
            name,
            url.clone(),
            Field::Alias(name.clone()),
        ));
    }
    rows.push(row(
        "Aliases",
        "+ add an alias",
        String::new(),
        Field::AddAlias,
    ));
    rows.extend([
        row(
            "Backup (plaintext, local only)",
            "enabled",
            on_off(config.backup.enabled),
            Field::BackupEnabled,
        ),
        row(
            "Backup (plaintext, local only)",
            "remote",
            config
                .backup
                .remote
                .clone()
                .unwrap_or_else(|| "(not set; push/pull unavailable)".into()),
            Field::BackupRemote,
        ),
        row(
            "Backup (plaintext, local only)",
            "branch",
            config.backup.branch.clone(),
            Field::BackupBranch,
        ),
    ]);
    if let (true, Some(problem)) = (config.backup.enabled, problem) {
        rows.push(row(
            "Backup (plaintext, local only)",
            "problem",
            problem.to_string(),
            Field::BackupProblem,
        ));
    }
    match candidates {
        Ok(candidates) => {
            let mut files: Vec<&String> = candidates.iter().chain(&config.backup.files).collect();
            files.sort();
            files.dedup();
            for file in files {
                let selected = config.backup.files.contains(file);
                let available = candidates.contains(file);
                rows.push(row(
                    "Plaintext file approvals (empty by default)",
                    file,
                    format!(
                        "{}{}",
                        on_off(selected),
                        if available {
                            ""
                        } else {
                            " (currently missing)"
                        }
                    ),
                    Field::BackupFile(file.clone()),
                ));
            }
        }
        Err(err) => {
            rows.push(row(
                "Plaintext file approvals (empty by default)",
                "files unavailable",
                err.to_string(),
                Field::BackupError,
            ));
            for file in &config.backup.files {
                rows.push(row(
                    "Plaintext file approvals (empty by default)",
                    file,
                    "on (listing unavailable; may revoke)".into(),
                    Field::BackupFile(file.clone()),
                ));
            }
        }
    }
    rows.push(row(
        "Integrations",
        "cli_on_path",
        if config.cli_on_path {
            "on: herdr-pm is on your PATH, following updates".to_string()
        } else {
            "off".to_string()
        },
        Field::CliOnPath,
    ));
    rows.push(row(
        "Integrations",
        "agent_skill",
        if config.agent_skill {
            "on: coding agents know herdr-pm".to_string()
        } else {
            "off".to_string()
        },
        Field::AgentSkill,
    ));
    rows
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Next,
    Prev,
    Set(String),
    Delete,
}

/// The table `key` at the top of the document, created when missing.
fn table<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table> {
    doc.entry(key)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("{key} is not a table"))
}

fn number(text: &str, min: u64) -> Result<i64> {
    let n: u64 = text
        .trim()
        .parse()
        .with_context(|| format!("{text:?} is not a whole number"))?;
    if n < min {
        bail!("{n} is below the minimum of {min}");
    }
    i64::try_from(n).context("number too large")
}

fn step(current: u64, action: &Action, by: u64, min: u64) -> Result<i64> {
    match action {
        Action::Next => number(&current.saturating_add(by).to_string(), min),
        Action::Prev => number(&current.saturating_sub(by).max(min).to_string(), min),
        Action::Set(text) => number(text, min),
        Action::Delete => bail!("this setting cannot be deleted"),
    }
}

fn cycle<T: Copy + PartialEq>(options: &[T], current: T, action: &Action) -> T {
    let i = options.iter().position(|o| *o == current).unwrap_or(0);
    let n = options.len();
    match action {
        Action::Prev => options[(i + n - 1) % n],
        _ => options[(i + 1) % n],
    }
}

fn set_market(doc: &mut DocumentMut, name: &str, enabled: bool) -> Result<()> {
    let builtin = builtin_markets().iter().any(|b| b.name == name);
    let sources = doc
        .entry("marketplace")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("marketplace is not a list of [[marketplace]] blocks"))?;
    let position = sources
        .iter()
        .position(|t| t.get("name").and_then(Item::as_str) == Some(name));
    match (position, enabled) {
        (Some(i), true) => {
            let block = sources.get_mut(i).unwrap();
            let only_toggle = block.iter().all(|(k, _)| k == "name" || k == "enabled");
            if builtin && only_toggle {
                sources.remove(i);
            } else {
                block.remove("enabled");
            }
        }
        (Some(i), false) => {
            put(&mut sources.get_mut(i).unwrap()["enabled"], false);
        }
        (None, false) if builtin => {
            let mut block = Table::new();
            block["name"] = value(name);
            block["enabled"] = value(false);
            sources.push(block);
        }
        (None, true) => {}
        (None, false) => bail!("no marketplace source named {name}"),
    }
    if sources.is_empty() {
        doc.remove("marketplace");
    }
    Ok(())
}

/// Replaces a value, keeping the comment around the old one.
fn put(slot: &mut Item, new: impl Into<toml_edit::Value>) {
    let mut new = new.into();
    if let Some(old) = slot.as_value() {
        *new.decor_mut() = old.decor().clone();
    }
    *slot = Item::Value(new);
}

/// `Set("true"|"false")` sets a switch; any other action flips it.
fn flag(action: &Action, current: bool) -> bool {
    match action {
        Action::Set(text) => text == "true",
        _ => !current,
    }
}

/// Applies one change to `config.toml` on disk (the example when it does
/// not exist yet) and returns the resulting configuration.
pub fn write(field: &Field, action: &Action) -> Result<Config> {
    let path = crate::paths::pm_config_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            crate::config::EXAMPLE.to_string()
        }
        Err(err) => return Err(err.into()),
    };
    let updated = change(&text, field, action)?;
    let config = Config::parse(&updated)?;
    if updated != text {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, updated)
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(config)
}

/// Applies one change to the text of `config.toml` and returns the new
/// text, refused unless `Config::parse` accepts it.
pub fn change(text: &str, field: &Field, action: &Action) -> Result<String> {
    let migrated = Config::migrate_text(text)?;
    let text = migrated.as_deref().unwrap_or(text);
    let config = Config::parse(text)?;
    let mut doc: DocumentMut = text.parse()?;
    match field {
        Field::CliOnPath => put(&mut doc["cli_on_path"], flag(action, config.cli_on_path)),
        Field::AgentSkill => put(&mut doc["agent_skill"], flag(action, config.agent_skill)),
        Field::BackupEnabled => put(
            &mut table(&mut doc, "backup")?["enabled"],
            flag(action, config.backup.enabled),
        ),
        Field::BackupRemote | Field::BackupBranch => {
            let key = match field {
                Field::BackupRemote => "remote",
                _ => "branch",
            };
            match action {
                Action::Set(text) if !text.trim().is_empty() => {
                    put(&mut table(&mut doc, "backup")?[key], text.trim());
                }
                Action::Set(_) | Action::Delete => {
                    table(&mut doc, "backup")?.remove(key);
                }
                _ => {}
            }
        }
        Field::BackupFile(file) => {
            let mut files = config.backup.files.clone();
            let selected = files.contains(file);
            let approve = !matches!(action, Action::Delete) && flag(action, selected);
            if approve && !selected {
                // Recheck at approval time, not only when the dialog was opened.
                if !crate::gitops::candidates()?.contains(file) {
                    bail!("backup file is no longer an available candidate: {file}");
                }
                files.push(file.clone());
            } else if !approve {
                files.retain(|selected| selected != file);
            }
            files.sort();
            let array: toml_edit::Array = files.iter().map(String::as_str).collect();
            put(&mut table(&mut doc, "backup")?["files"], array);
        }
        Field::BackupError => {
            bail!("file candidates could not be listed; close and reopen Settings to retry")
        }
        Field::BackupProblem => {
            bail!("automatic backup retries by itself; fix the cause shown and it recovers")
        }
        Field::KeepInstalls => put(
            &mut doc["keep_installs"],
            step(config.keep_installs as u64, action, 1, 0)?,
        ),
        Field::MarketTtl => put(
            &mut doc["market_ttl"],
            step(config.market_ttl, action, 60, 0)?,
        ),
        Field::CheckConcurrency => put(
            &mut doc["check_concurrency"],
            step(config.check_concurrency as u64, action, 1, 1)?,
        ),
        Field::UseGhCli => put(&mut doc["use_gh_cli"], !config.use_gh_cli),
        Field::GithubViaGit => put(&mut doc["github_via_git"], !config.github_via_git),
        Field::Icons => put(&mut doc["icons"], flag(action, config.icons)),
        Field::History => put(
            &mut doc["history"],
            cycle(&HISTORY, config.history, action).label(),
        ),
        Field::LogDir => change_log_dir(&mut doc, action, &crate::paths::pm_home())?,
        Field::Trigger => {
            let next = cycle(&TRIGGERS, config.auto_update.trigger, action);
            put(
                &mut table(&mut doc, "auto_update")?["trigger"],
                next.label(),
            );
        }
        Field::MinInterval => match action {
            Action::Set(text) if !text.trim().is_empty() => put(
                &mut table(&mut doc, "auto_update")?["min_interval"],
                text.trim(),
            ),
            Action::Set(_) | Action::Delete => {
                table(&mut doc, "auto_update")?.remove("min_interval");
            }
            _ => {}
        },
        Field::Editor => match action {
            Action::Set(text) if !text.trim().is_empty() => put(&mut doc["editor"], text.trim()),
            Action::Set(_) | Action::Delete => {
                doc.remove("editor");
            }
            _ => {}
        },
        Field::Strategy => {
            let next = cycle(&STRATEGIES, config.auto_update.strategy, action);
            put(
                &mut table(&mut doc, "auto_update")?["strategy"],
                next.label(),
            );
        }
        Field::PluginStrategy(id) => {
            let options: [Option<Strategy>; 5] = [
                None,
                Some(Strategy::Latest),
                Some(Strategy::Release),
                Some(Strategy::Minor),
                Some(Strategy::Patch),
            ];
            let current = config.auto_update.plugins.get(id).copied();
            let next = match action {
                Action::Delete => None,
                _ => cycle(&options, current, action),
            };
            let auto = table(&mut doc, "auto_update")?;
            let plugins = auto
                .entry("plugins")
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(|| anyhow!("auto_update.plugins is not a table"))?;
            match next {
                Some(s) => put(&mut plugins[id.as_str()], s.label()),
                None => {
                    plugins.remove(id);
                }
            }
        }
        Field::Market(name) => {
            let enabled = config
                .marketplace
                .iter()
                .find(|s| &s.name == name)
                .is_some_and(|s| s.enabled);
            set_market(&mut doc, name, !enabled)?;
        }
        Field::Alias(name) => match action {
            Action::Set(url) if !url.trim().is_empty() => {
                put(&mut table(&mut doc, "aliases")?[name.as_str()], url.trim())
            }
            Action::Set(_) | Action::Delete => {
                table(&mut doc, "aliases")?.remove(name);
            }
            _ => {}
        },
        Field::AddAlias => {
            let Action::Set(text) = action else {
                return Ok(text.to_string());
            };
            let text = text.trim();
            let (name, url) = text
                .split_once('=')
                .or_else(|| text.split_once(char::is_whitespace))
                .map(|(n, u)| (n.trim(), u.trim()))
                .filter(|(n, u)| !n.is_empty() && !u.is_empty())
                .ok_or_else(|| anyhow!("type a name and a URL, e.g. corp ssh://git@git.corp/"))?;
            if config.aliases.contains_key(name) {
                bail!("alias {name} already exists");
            }
            put(&mut table(&mut doc, "aliases")?[name], url);
        }
    }
    let out = doc.to_string();
    Config::parse(&out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    #[test]
    fn backup_can_capture_locally_without_remote_or_plaintext_file_grants() {
        let text = apply("", Field::BackupEnabled, Action::Set("true".into()));
        let config = Config::parse(&text).unwrap();
        assert!(config.backup.enabled);
        assert!(config.backup.files.is_empty());
        assert!(config.backup.remote.is_none());
        let text = apply(
            &text,
            Field::BackupRemote,
            Action::Set("ssh://git@example.org/config.git".into()),
        );
        assert!(Config::parse(&text).unwrap().backup.remote.is_some());
        let text = apply(&text, Field::BackupRemote, Action::Delete);
        assert!(Config::parse(&text).unwrap().backup.remote.is_none());
        assert!(change(
            &text,
            &Field::BackupRemote,
            &Action::Set("ext::unsafe".into())
        )
        .is_err());
    }

    #[test]
    fn failed_candidate_listing_surfaces_error_and_allows_revocation_only() {
        let config = Config::parse("[backup]\nfiles = ['manager/config.toml']\n").unwrap();
        let rows = rows(&config, &[], Err("permission denied"), None);
        assert!(rows
            .iter()
            .any(|row| row.field == Field::BackupError && row.value == "permission denied"));
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.field, Field::BackupFile(_)))
                .count(),
            1
        );
        let text = apply(
            "[backup]\nfiles = ['manager/config.toml']\n",
            Field::BackupFile("manager/config.toml".into()),
            Action::Delete,
        );
        assert!(Config::parse(&text).unwrap().backup.files.is_empty());
    }

    #[test]
    fn backup_problem_is_shown_only_while_backup_is_on_and_failing() {
        let off = Config::parse("").unwrap();
        let on = Config::parse("[backup]\nenabled = true\n").unwrap();
        let problem = |config: &Config, problem| {
            rows(config, &[], Ok(&[]), problem)
                .into_iter()
                .find(|row| row.field == Field::BackupProblem)
                .map(|row| row.value)
        };
        assert_eq!(problem(&on, None), None);
        assert_eq!(problem(&off, Some("git not found")), None);
        assert_eq!(
            problem(&on, Some("git not found")).as_deref(),
            Some("git not found")
        );
        assert!(rows(&on, &[], Ok(&[]), None)
            .iter()
            .all(|row| row.label != "directory" && row.label != "worker errors"));
    }

    fn apply(text: &str, field: Field, action: Action) -> String {
        change(text, &field, &action).unwrap()
    }

    #[test]
    fn icons_setting_migrates_legacy_preferences_without_a_schema_alias() {
        let text = apply(
            "# preference\nlogos = false # terminal pictures\n",
            Field::KeepInstalls,
            Action::Set("4".into()),
        );
        assert!(!text.contains("logos"));
        assert!(text.contains("# preference"));
        assert!(text.contains("icons = false # terminal pictures"));
        let text = apply(&text, Field::Icons, Action::Next);
        assert!(Config::parse(&text).unwrap().icons);
        assert!(text.contains("# terminal pictures"));
    }

    #[test]
    fn log_directory_is_created_checked_and_reset() {
        let root = std::env::temp_dir().join(format!(
            "hpm-log-setting-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let directory = root.join("new").join("logs");
        let text = apply(
            "",
            Field::LogDir,
            Action::Set(directory.display().to_string()),
        );
        assert!(directory.is_dir());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.log_dir.as_deref(), Some(directory.as_path()));
        assert!(change("", &Field::LogDir, &Action::Set("relative/logs".into())).is_err());
        let file = root.join("file");
        std::fs::write(&file, "do not overwrite").unwrap();
        assert!(change("", &Field::LogDir, &Action::Set(file.display().to_string())).is_err());
        assert!(change(
            "",
            &Field::LogDir,
            &Action::Set(file.join("child").display().to_string())
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "do not overwrite");
        let manual = format!("log_dir = {:?}\n", file.to_str().unwrap());
        assert!(Config::parse(&manual).is_err());
        let default = root.join("default");
        let mut doc: DocumentMut = text.parse().unwrap();
        change_log_dir(&mut doc, &Action::Delete, &default).unwrap();
        assert!(default.is_dir());
        assert!(Config::parse(&doc.to_string()).unwrap().log_dir.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_cycles_formats_and_can_be_disabled() {
        let text = apply("", Field::History, Action::Next);
        assert_eq!(Config::parse(&text).unwrap().history, HistoryFormat::Ecs);
        let text = apply(&text, Field::History, Action::Next);
        assert_eq!(Config::parse(&text).unwrap().history, HistoryFormat::Off);
        let text = apply(&text, Field::History, Action::Next);
        assert_eq!(Config::parse(&text).unwrap().history, HistoryFormat::Jsonl);
        let text = apply(&text, Field::History, Action::Prev);
        assert_eq!(Config::parse(&text).unwrap().history, HistoryFormat::Off);
        assert!(Config::parse("history = \"unknown\"").is_err());
    }

    #[test]
    fn edits_keep_comments_and_are_checked() {
        let text = apply(EXAMPLE, Field::KeepInstalls, Action::Set("5".into()));
        assert!(text.contains("keep_installs = 5"));
        assert!(text.contains("# versions kept for rollback"));
        assert!(text.contains("# C in the popup edits this file."));
        assert!(change(EXAMPLE, &Field::CheckConcurrency, &Action::Set("0".into())).is_err());
        assert!(change(EXAMPLE, &Field::KeepInstalls, &Action::Set("x".into())).is_err());
        assert!(change(EXAMPLE, &Field::AddAlias, &Action::Set("https x".into())).is_err());
    }

    #[test]
    fn trigger_cycles_and_min_interval_is_checked() {
        let text = apply(EXAMPLE, Field::Trigger, Action::Prev);
        assert_eq!(
            Config::parse(&text).unwrap().auto_update.trigger,
            Trigger::Manual
        );
        assert!(text.contains("# startup, popup, min-interval or manual"));
        let text = apply(&text, Field::MinInterval, Action::Set("2h".into()));
        assert_eq!(
            Config::parse(&text)
                .unwrap()
                .auto_update
                .min_interval_secs(),
            7200
        );
        assert!(change(&text, &Field::MinInterval, &Action::Set("10s".into())).is_err());
        let text = apply(&text, Field::MinInterval, Action::Delete);
        assert_eq!(Config::parse(&text).unwrap().auto_update.min_interval, "6h");
    }

    #[test]
    fn strategies_cycle_and_plugin_overrides_return_to_default() {
        let text = apply(EXAMPLE, Field::Strategy, Action::Next);
        assert_eq!(
            Config::parse(&text).unwrap().auto_update.strategy,
            Strategy::Release
        );
        let text = apply(&text, Field::Strategy, Action::Prev);
        assert_eq!(
            Config::parse(&text).unwrap().auto_update.strategy,
            Strategy::Latest
        );
        let id = Field::PluginStrategy("a.b".into());
        let text = apply(EXAMPLE, id.clone(), Action::Prev);
        assert_eq!(
            Config::parse(&text)
                .unwrap()
                .auto_update
                .strategy_for("a.b"),
            Strategy::Patch
        );
        let text = apply(&text, id, Action::Next);
        let config = Config::parse(&text).unwrap();
        assert!(config.auto_update.plugins.is_empty());
    }

    #[test]
    fn builtin_sources_toggle_without_leaving_a_block() {
        let field = Field::Market("codeberg.org".into());
        let off = apply(EXAMPLE, field.clone(), Action::Next);
        let config = Config::parse(&off).unwrap();
        let codeberg = config
            .marketplace
            .iter()
            .find(|s| s.name == "codeberg.org")
            .unwrap();
        assert!(!codeberg.enabled);
        let on = apply(&off, field, Action::Next);
        assert!(!on.contains("codeberg.org\""));
        assert!(Config::parse(&on)
            .unwrap()
            .marketplace
            .iter()
            .all(|s| s.enabled));
    }

    #[test]
    fn aliases_add_edit_delete() {
        let text = apply(
            EXAMPLE,
            Field::AddAlias,
            Action::Set("corp = ssh://git@git.corp/".into()),
        );
        assert_eq!(
            Config::parse(&text).unwrap().aliases["corp"],
            "ssh://git@git.corp/"
        );
        let text = apply(
            &text,
            Field::Alias("corp".into()),
            Action::Set("https://git.corp/{path}.git".into()),
        );
        assert_eq!(
            Config::parse(&text).unwrap().aliases["corp"],
            "https://git.corp/{path}.git"
        );
        let text = apply(&text, Field::Alias("corp".into()), Action::Delete);
        assert!(Config::parse(&text).unwrap().aliases.is_empty());
    }
}
