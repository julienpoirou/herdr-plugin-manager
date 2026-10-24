//! `herdr-plugin.toml`: parsing, the checks Herdr applies when linking, and
//! a human-readable account of everything a plugin will run.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::util::{self, Version};

pub const FILE_NAME: &str = "herdr-plugin.toml";
const KNOWN_PLATFORMS: [&str; 3] = ["linux", "macos", "windows"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub contexts: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub on: String,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub placement: Option<String>,
    #[serde(default)]
    pub width: Option<toml::Value>,
    #[serde(default)]
    pub height: Option<toml::Value>,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkHandler {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub min_herdr_version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub build: Vec<Entry>,
    #[serde(default)]
    pub startup: Vec<Entry>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub panes: Vec<Pane>,
    #[serde(default)]
    pub link_handlers: Vec<LinkHandler>,
}

/// One thing a plugin can execute, flattened for previews and diffs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runnable {
    /// `build #1`, `startup #1`, `action open`, `event worktree.created`, `pane board`
    pub label: String,
    pub command: String,
    /// False when the entry is declared for other platforms only.
    pub runs_here: bool,
}

fn plugin_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
}

fn local_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'))
}

pub fn applies_here(item: &Option<Vec<String>>, top: &Option<Vec<String>>) -> bool {
    item.as_ref()
        .or(top.as_ref())
        .is_none_or(|platforms| platforms.iter().any(|p| p == util::current_platform()))
}

/// Shell-ish rendering of an argv array for display only.
pub fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.is_empty()
                || arg
                    .chars()
                    .any(|c| c.is_whitespace() || c == '"' || c == '\'')
            {
                format!("{arg:?}")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest> {
        toml::from_str(text).context("herdr-plugin.toml is not valid TOML for a Herdr manifest")
    }

    /// Mirrors the checks Herdr runs at link time, so a bad manifest fails
    /// before anything is built. Returns non-fatal warnings.
    pub fn validate(&self, herdr: Option<&Version>) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        if !plugin_id_ok(self.id.trim()) {
            bail!(
                "invalid plugin id {:?} (letters, digits, '.', ':', '_' and '-' only)",
                self.id
            );
        }
        if self.name.trim().is_empty() {
            bail!("manifest has no name");
        }
        if self.version.trim().is_empty() {
            bail!("manifest has no version");
        }
        let Some(min) = self
            .min_herdr_version
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            bail!("manifest has no min_herdr_version");
        };
        let required = Version::parse(min)
            .with_context(|| format!("min_herdr_version {min:?} is not a semantic version"))?;
        if let Some(current) = herdr {
            let current_core = Version {
                pre: None,
                ..current.clone()
            };
            if required > current_core {
                bail!("plugin requires Herdr {required} or newer; this Herdr is {current}");
            }
        }
        let mut all_platforms: Vec<&Option<Vec<String>>> = vec![&self.platforms];
        all_platforms.extend(self.build.iter().map(|e| &e.platforms));
        all_platforms.extend(self.startup.iter().map(|e| &e.platforms));
        all_platforms.extend(self.actions.iter().map(|e| &e.platforms));
        all_platforms.extend(self.events.iter().map(|e| &e.platforms));
        all_platforms.extend(self.panes.iter().map(|e| &e.platforms));
        all_platforms.extend(self.link_handlers.iter().map(|e| &e.platforms));
        for platforms in all_platforms.into_iter().flatten() {
            for platform in platforms {
                if !KNOWN_PLATFORMS.contains(&platform.as_str()) {
                    bail!("unknown platform {platform:?}");
                }
            }
        }
        match &self.platforms {
            None => warnings
                .push("manifest does not declare platforms; platform support unknown".into()),
            Some(list) if !list.iter().any(|p| p == util::current_platform()) => {
                warnings.push(format!(
                    "plugin declares {} only, not {}",
                    list.join(", "),
                    util::current_platform()
                ))
            }
            _ => {}
        }
        let mut seen = std::collections::BTreeSet::new();
        for action in &self.actions {
            if !local_id_ok(action.id.trim()) {
                bail!("invalid action id {:?} (no dots allowed)", action.id);
            }
            if !seen.insert(action.id.trim()) {
                bail!("duplicate action id {:?}", action.id);
            }
            if action.title.trim().is_empty() {
                bail!("action {:?} has no title", action.id);
            }
            if action.command.is_empty() {
                bail!("action {:?} has an empty command", action.id);
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for pane in &self.panes {
            if !local_id_ok(pane.id.trim()) {
                bail!("invalid pane id {:?}", pane.id);
            }
            if !seen.insert(pane.id.trim()) {
                bail!("duplicate pane id {:?}", pane.id);
            }
            if pane.command.is_empty() {
                bail!("pane {:?} has an empty command", pane.id);
            }
        }
        for handler in &self.link_handlers {
            if !self.actions.iter().any(|a| a.id == handler.action) {
                bail!(
                    "link handler {:?} points at unknown action {:?}",
                    handler.id,
                    handler.action
                );
            }
        }
        for (label, entries) in [("build", &self.build), ("startup", &self.startup)] {
            if entries.iter().any(|e| e.command.is_empty()) {
                bail!("a {label} entry has an empty command");
            }
        }
        if self
            .events
            .iter()
            .any(|e| e.command.is_empty() || e.on.trim().is_empty())
        {
            bail!("an event hook has no event name or an empty command");
        }
        Ok(warnings)
    }

    /// Build steps that run on this platform, in order.
    pub fn build_steps(&self) -> Vec<&Entry> {
        self.build
            .iter()
            .filter(|b| applies_here(&b.platforms, &self.platforms))
            .collect()
    }

    pub fn runnables(&self) -> Vec<Runnable> {
        let mut out = Vec::new();
        let top = &self.platforms;
        for (i, entry) in self.build.iter().enumerate() {
            out.push(Runnable {
                label: format!("build #{}", i + 1),
                command: render_argv(&entry.command),
                runs_here: applies_here(&entry.platforms, top),
            });
        }
        for (i, entry) in self.startup.iter().enumerate() {
            out.push(Runnable {
                label: format!("startup #{}", i + 1),
                command: render_argv(&entry.command),
                runs_here: applies_here(&entry.platforms, top),
            });
        }
        for action in &self.actions {
            out.push(Runnable {
                label: format!("action {}", action.id),
                command: render_argv(&action.command),
                runs_here: applies_here(&action.platforms, top),
            });
        }
        for (i, event) in self.events.iter().enumerate() {
            out.push(Runnable {
                label: format!("event {} #{}", event.on, i + 1),
                command: render_argv(&event.command),
                runs_here: applies_here(&event.platforms, top),
            });
        }
        for pane in &self.panes {
            out.push(Runnable {
                label: format!("pane {}", pane.id),
                command: render_argv(&pane.command),
                runs_here: applies_here(&pane.platforms, top),
            });
        }
        out
    }

    /// Lines summarising what a plugin will run, for the trust preview.
    pub fn preview_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "  plugin    {} ({}) {}",
            self.name, self.id, self.version
        ));
        if let Some(description) = self.description.as_deref().filter(|d| !d.trim().is_empty()) {
            lines.push(format!("  about     {}", util::sanitize(description)));
        }
        lines.push(format!(
            "  platforms {}",
            self.platforms
                .as_ref()
                .map(|p| p.join(", "))
                .unwrap_or_else(|| "(not declared)".into())
        ));
        lines.push(format!(
            "  requires  herdr >= {}",
            self.min_herdr_version.as_deref().unwrap_or("?")
        ));
        let runnables = self.runnables();
        if runnables.is_empty() {
            lines.push("  runs      nothing".into());
        } else {
            lines.push("  runs:".into());
            for r in runnables {
                let skipped = if r.runs_here {
                    String::new()
                } else {
                    format!("  (skipped on {})", util::current_platform())
                };
                lines.push(format!("    {:<22} {}{}", r.label, r.command, skipped));
            }
        }
        if !self.link_handlers.is_empty() {
            lines.push("  link handlers:".into());
            for h in &self.link_handlers {
                lines.push(format!(
                    "    {:<22} {} → action {}",
                    h.id, h.pattern, h.action
                ));
            }
        }
        lines
    }
}

/// What changed in what a plugin runs between two manifests. Empty when the
/// executable surface is identical.
pub fn diff_runnables(old: &Manifest, new: &Manifest) -> Vec<String> {
    let old_r = old.runnables();
    let new_r = new.runnables();
    let mut lines = Vec::new();
    for n in &new_r {
        match old_r.iter().find(|o| o.label == n.label) {
            None => lines.push(format!("  + {:<22} {}", n.label, n.command)),
            Some(o) if o.command != n.command => {
                lines.push(format!("  ~ {:<22} {}", n.label, o.command));
                lines.push(format!("    {:<22} → {}", "", n.command));
            }
            _ => {}
        }
    }
    for o in &old_r {
        if !new_r.iter().any(|n| n.label == o.label) {
            lines.push(format!("  - {:<22} {}", o.label, o.command));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
id = "example.layout"
name = "Layout"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Apply project layouts"
platforms = ["linux", "macos", "windows"]

[[build]]
command = ["npm", "ci"]

[[build]]
command = ["npm", "run", "build"]
platforms = ["windows"]

[[actions]]
id = "apply"
title = "Apply layout"
contexts = ["workspace"]
command = ["node", "dist/apply.js"]

[[panes]]
id = "board"
title = "Project board"
placement = "popup"
width = "80%"
height = 20
command = ["herdr-board"]

[[link_handlers]]
id = "gh"
title = "Open"
pattern = "^https://github\\.com/"
action = "apply"
"#;

    #[test]
    fn parses_and_validates() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.id, "example.layout");
        let warnings = m.validate(Version::parse("0.9.5").as_ref()).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(m.build_steps().len(), if cfg!(windows) { 2 } else { 1 });
        assert_eq!(m.runnables().len(), 4);
    }

    #[test]
    fn rejects_newer_herdr_and_bad_ids() {
        let m = Manifest::parse(&SAMPLE.replace("0.7.0", "9.0.0")).unwrap();
        assert!(m.validate(Version::parse("0.9.5").as_ref()).is_err());
        let m = Manifest::parse(&SAMPLE.replace("id = \"apply\"", "id = \"a.b\"")).unwrap();
        assert!(m.validate(None).is_err());
        let m =
            Manifest::parse(&SAMPLE.replace("action = \"apply\"", "action = \"nope\"")).unwrap();
        assert!(m.validate(None).is_err());
        let m = Manifest::parse(&SAMPLE.replace("min_herdr_version = \"0.7.0\"", "")).unwrap();
        assert!(m.validate(None).is_err());
    }

    #[test]
    fn preview_herdr_prerelease_counts_as_its_release() {
        let m = Manifest::parse(&SAMPLE.replace("0.7.0", "0.9.5")).unwrap();
        assert!(m
            .validate(Version::parse("0.9.5-preview.3").as_ref())
            .is_ok());
    }

    #[test]
    fn diffs_changed_commands() {
        let old = Manifest::parse(SAMPLE).unwrap();
        let new = Manifest::parse(
            &SAMPLE
                .replace("[\"npm\", \"ci\"]", "[\"curl\", \"evil.sh\"]")
                .replace(
                    "[\"herdr-board\"]",
                    "[\"herdr-board\"]\n\n[[startup]]\ncommand = [\"x\"]",
                ),
        )
        .unwrap();
        let diff = diff_runnables(&old, &new);
        assert!(diff.iter().any(|l| l.contains("~ build #1")), "{diff:?}");
        assert!(diff.iter().any(|l| l.contains("+ startup #1")), "{diff:?}");
        assert!(diff_runnables(&old, &old).is_empty());
    }
}
