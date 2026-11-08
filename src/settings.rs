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
    Logos,
    /// Format of the change log.
    History,
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
}

impl Field {
    pub fn kind(&self) -> Kind {
        match self {
            Field::KeepInstalls | Field::MarketTtl | Field::CheckConcurrency => Kind::Number,
            Field::UseGhCli
            | Field::GithubViaGit
            | Field::Logos
            | Field::Market(_)
            | Field::CliOnPath
            | Field::AgentSkill => Kind::Toggle,
            Field::Strategy | Field::Trigger | Field::History | Field::PluginStrategy(_) => {
                Kind::Choice
            }
            Field::Editor | Field::MinInterval => Kind::Text,
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

/// Every editable setting, by section. `plugins` are the ids that can get
/// their own strategy.
pub fn rows(config: &Config, plugins: &[String]) -> Vec<Row> {
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
            "logos",
            if config.logos {
                "on: pictures where the terminal draws images".to_string()
            } else {
                "off".to_string()
            },
            Field::Logos,
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
    let config = Config::parse(text)?;
    let mut doc: DocumentMut = text.parse()?;
    match field {
        Field::CliOnPath => put(&mut doc["cli_on_path"], flag(action, config.cli_on_path)),
        Field::AgentSkill => put(&mut doc["agent_skill"], flag(action, config.agent_skill)),
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
        Field::Logos => put(&mut doc["logos"], flag(action, config.logos)),
        Field::History => put(
            &mut doc["history"],
            cycle(&HISTORY, config.history, action).label(),
        ),
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

    fn apply(text: &str, field: Field, action: Action) -> String {
        change(text, &field, &action).unwrap()
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
