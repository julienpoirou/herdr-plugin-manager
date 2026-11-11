//! The manager's own record of git-managed plugins, and its per-plugin
//! settings.
//!
//! To Herdr these are ordinary linked directories, so Herdr does not know
//! where they came from. This registry remembers the remote, the ref being
//! followed, the installed commit, and earlier installations for rollback.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::HistoryFormat;
use crate::{paths, util};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installation {
    pub commit: String,
    pub version: String,
    /// Directory registered with Herdr (the one holding herdr-plugin.toml).
    pub plugin_root: String,
    /// Top of this installation (removed as a whole by cleanup).
    pub install_dir: String,
    #[serde(default)]
    pub requested_ref: Option<String>,
    pub installed_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub subdir: Option<String>,
    /// Branch, tag or full commit followed by updates; None = default branch.
    #[serde(default)]
    pub requested_ref: Option<String>,
    pub current: Installation,
    /// Earlier installations, newest first.
    #[serde(default)]
    pub previous: Vec<Installation>,
    pub first_installed_unix: u64,
}

impl Record {
    pub fn source_display(&self) -> String {
        match &self.subdir {
            Some(subdir) => format!("{}//{subdir}", self.url),
            None => self.url.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "format_version")]
    pub version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, Record>,
    /// Plugin ids `herdr-pm auto-update` updates without a preview, whether
    /// the plugin is enabled or not (GitHub- and git-installed plugins).
    #[serde(default)]
    pub auto_update: BTreeSet<String>,
    /// Commits GitHub-installed plugins were held at by versions before
    /// pins went through a reinstall. `herdr plugin update` ignores them,
    /// so a moved plugin is reported and restored on request.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
    /// When `herdr-pm auto-update` last started (unix seconds).
    #[serde(default)]
    pub last_auto_update: u64,
}

fn format_version() -> u32 {
    1
}

pub struct Locked {
    _lock: std::fs::File,
    pub registry: Registry,
}

impl Registry {
    /// Read-only snapshot (no lock held afterwards).
    pub fn load() -> Result<Registry> {
        Ok(Registry::lock()?.registry)
    }

    /// Takes the registry lock for a read-modify-write cycle.
    pub fn lock() -> Result<Locked> {
        let lock_path = paths::registry_lock();
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
        let path = paths::registry_file();
        let registry = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!("{} is corrupt; move it aside to start over", path.display())
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Registry {
                version: 1,
                plugins: BTreeMap::new(),
                auto_update: BTreeSet::new(),
                pins: BTreeMap::new(),
                last_auto_update: 0,
            },
            Err(err) => {
                return Err(err).with_context(|| format!("could not read {}", path.display()))
            }
        };
        Ok(Locked {
            _lock: lock,
            registry,
        })
    }

    pub fn get(&self, id: &str) -> Option<&Record> {
        self.plugins.get(id)
    }
}

impl Locked {
    pub fn save(&self) -> Result<()> {
        let path = crate::herdr::resolve_write_target(&paths::registry_file())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.registry)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// One line per change, so `herdr-pm logs` can show what happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub unix: u64,
    pub action: String,
    pub plugin_id: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Appends `event` to the change log in the configured format
/// (`history` in config.toml), or nowhere when it is off.
pub fn log_event(event: Event) {
    if let Err(err) = append_event(&event) {
        eprintln!("warning: could not record change log: {err:#}");
    }
}

fn append_event(event: &Event) -> Result<()> {
    let config = crate::config::Config::load()?;
    let filename = match config.history {
        HistoryFormat::Off => return Ok(()),
        HistoryFormat::Jsonl => "history.jsonl",
        HistoryFormat::Ecs => "history.ecs.ndjson",
    };
    let path = config.log_dir.unwrap_or_else(paths::pm_home).join(filename);
    let line = match config.history {
        HistoryFormat::Jsonl => serde_json::to_string(event)?,
        HistoryFormat::Ecs => serde_json::to_string(&to_ecs(event))?,
        HistoryFormat::Off => unreachable!(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create log directory {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open log {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("could not append log {}", path.display()))?;
    Ok(())
}

/// Version of the Elastic Common Schema the ECS log follows.
pub const ECS_VERSION: &str = "8.11.0";

fn env_first(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .find(|v| !v.trim().is_empty())
}

/// One change as an Elastic Common Schema document for JSON log shippers.
pub fn to_ecs(event: &Event) -> serde_json::Value {
    let kind = match event.action.as_str() {
        "install" | "import" => "installation",
        "uninstall" | "purge" => "deletion",
        _ => "change",
    };
    let change = match (&event.from, &event.to) {
        (Some(f), Some(t)) => format!(" {} → {}", util::short_sha(f), util::short_sha(t)),
        (None, Some(t)) => format!(" {}", util::short_sha(t)),
        _ => String::new(),
    };
    let mut doc = serde_json::json!({
        "@timestamp": util::format_iso8601(event.unix),
        "ecs": { "version": ECS_VERSION },
        "message": format!("{} {}{change}", event.action, event.plugin_id),
        "event": {
            "kind": "event",
            "category": ["package"],
            "type": [kind],
            "action": event.action,
            "outcome": if event.ok { "success" } else { "failure" },
            "provider": paths::PLUGIN_ID,
            "dataset": "herdr_plugin_manager.history",
        },
        "package": { "name": event.plugin_id },
        "host": { "name": env_first(&["COMPUTERNAME", "HOSTNAME"]) },
        "user": { "name": env_first(&["USERNAME", "USER", "LOGNAME"]) },
        "herdr_plugin_manager": {
            "source": event.source,
            "from": event.from,
            "to": event.to,
            "detail": event.detail,
        },
    });
    if let Some(to) = &event.to {
        doc["package"]["reference"] = to.clone().into();
    }
    if let Some(detail) = event.detail.as_ref().filter(|_| !event.ok) {
        doc["error"] = serde_json::json!({ "message": detail });
    }
    doc
}

/// The change an ECS document describes, when it is one of ours.
pub fn from_ecs(doc: &serde_json::Value) -> Option<Event> {
    let custom = &doc["herdr_plugin_manager"];
    let text = |v: &serde_json::Value| v.as_str().map(str::to_string);
    Some(Event {
        unix: util::parse_iso8601(doc["@timestamp"].as_str()?)?,
        action: text(&doc["event"]["action"])?,
        plugin_id: text(&doc["package"]["name"])?,
        source: text(&custom["source"]),
        from: text(&custom["from"]),
        to: text(&custom["to"]),
        ok: doc["event"]["outcome"].as_str() == Some("success"),
        detail: text(&custom["detail"]),
    })
}

/// Independent byte cursors keep format switches and equal timestamps from
/// skipping changes. Incomplete final lines are retried on the next read.
#[derive(Default)]
pub struct EventReader {
    offsets: [u64; 2],
    files: [Option<std::path::PathBuf>; 2],
}

impl EventReader {
    pub fn read_new(&mut self) -> std::io::Result<Vec<Event>> {
        let directory = paths::log_dir().map_err(std::io::Error::other)?;
        self.read_files([
            directory.join("history.jsonl"),
            directory.join("history.ecs.ndjson"),
        ])
    }

    fn read_files(&mut self, files: [std::path::PathBuf; 2]) -> std::io::Result<Vec<Event>> {
        let mut events = Vec::new();
        for (index, path) in files.into_iter().enumerate() {
            if self.files[index].as_ref() != Some(&path) {
                self.offsets[index] = 0;
                self.files[index] = Some(path.clone());
            }
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    self.offsets[index] = 0;
                    continue;
                }
                Err(err) => return Err(err),
            };
            if file.metadata()?.len() < self.offsets[index] {
                self.offsets[index] = 0;
            }
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(self.offsets[index]))?;
            let mut line = Vec::new();
            loop {
                line.clear();
                let count = reader.read_until(b'\n', &mut line)?;
                if count == 0 || line.last() != Some(&b'\n') {
                    break;
                }
                self.offsets[index] += count as u64;
                let event = if index == 0 {
                    serde_json::from_slice::<Event>(&line).ok()
                } else {
                    serde_json::from_slice::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|doc| from_ecs(&doc))
                };
                if let Some(event) = event {
                    events.push(event);
                }
            }
        }
        events.sort_by_key(|e| e.unix);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecs_documents_carry_the_change_and_read_back() {
        let event = Event {
            unix: 1_794_064_289,
            action: "update".into(),
            plugin_id: "demo.tags".into(),
            source: Some("https://git.example/demo.git".into()),
            from: Some("a".repeat(40)),
            to: Some("b".repeat(40)),
            ok: false,
            detail: Some("build failed".into()),
        };
        let doc = to_ecs(&event);
        assert_eq!(doc["@timestamp"], "2026-11-07T15:11:29Z");
        assert_eq!(doc["event"]["type"][0], "change");
        assert_eq!(doc["event"]["outcome"], "failure");
        assert_eq!(doc["package"]["name"], "demo.tags");
        assert_eq!(doc["package"]["reference"], "b".repeat(40));
        assert_eq!(doc["error"]["message"], "build failed");
        let back = from_ecs(&doc).unwrap();
        assert_eq!(
            (back.unix, &back.action, &back.from, back.ok, &back.detail),
            (event.unix, &event.action, &event.from, false, &event.detail)
        );
        let purge = Event {
            action: "purge".into(),
            ok: true,
            ..back
        };
        assert_eq!(to_ecs(&purge)["event"]["type"][0], "deletion");
        assert!(to_ecs(&purge).get("error").is_none());
    }

    #[test]
    fn follow_keeps_partial_lines_and_independent_format_cursors() {
        let root = std::env::temp_dir().join(format!(
            "hpm-log-reader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let files = [root.join("history.jsonl"), root.join("history.ecs.ndjson")];
        let mut reader = EventReader::default();
        let event = Event {
            unix: 200,
            action: "install".into(),
            plugin_id: "demo.logs".into(),
            source: None,
            from: None,
            to: None,
            ok: true,
            detail: None,
        };
        std::fs::write(&files[1], format!("{}\n", to_ecs(&event))).unwrap();
        assert_eq!(
            reader.read_files(files.clone()).unwrap()[0].action,
            "install"
        );
        let event = Event {
            unix: 100,
            action: "pin".into(),
            ..event
        };
        let line = serde_json::to_string(&event).unwrap();
        std::fs::write(&files[0], &line[..line.len() / 2]).unwrap();
        assert!(reader.read_files(files.clone()).unwrap().is_empty());
        let mut file = OpenOptions::new().append(true).open(&files[0]).unwrap();
        writeln!(file, "{}", &line[line.len() / 2..]).unwrap();
        drop(file);
        let fresh = reader.read_files(files.clone()).unwrap();
        assert_eq!(
            fresh.iter().map(|e| e.action.as_str()).collect::<Vec<_>>(),
            ["pin"]
        );
        assert!(reader.read_files(files.clone()).unwrap().is_empty());
        std::fs::write(&files[0], "").unwrap();
        assert!(reader.read_files(files.clone()).unwrap().is_empty());
        std::fs::write(&files[0], format!("{line}\n")).unwrap();
        assert_eq!(reader.read_files(files.clone()).unwrap()[0].action, "pin");
        let moved = root.join("moved");
        std::fs::create_dir_all(&moved).unwrap();
        let moved_files = [
            moved.join("history.jsonl"),
            moved.join("history.ecs.ndjson"),
        ];
        std::fs::write(&moved_files[0], format!("{line}\n")).unwrap();
        std::fs::write(&moved_files[1], format!("{}\n", to_ecs(&event))).unwrap();
        // Changing just one resolved path resets only that format's cursor.
        let fresh = reader
            .read_files([moved_files[0].clone(), files[1].clone()])
            .unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].action, "pin");
        let fresh = reader.read_files(moved_files.clone()).unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].action, "pin");
        assert!(reader.read_files(moved_files.clone()).unwrap().is_empty());
        // Following a new directory still retries incomplete lines.
        let mut file = OpenOptions::new()
            .append(true)
            .open(&moved_files[0])
            .unwrap();
        write!(file, "{}", &line[..line.len() / 2]).unwrap();
        file.flush().unwrap();
        assert!(reader.read_files(moved_files.clone()).unwrap().is_empty());
        writeln!(file, "{}", &line[line.len() / 2..]).unwrap();
        drop(file);
        assert_eq!(reader.read_files(moved_files.clone()).unwrap().len(), 1);
        assert!(reader.read_files(moved_files).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
