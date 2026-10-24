//! The manager's own record of git-managed plugins.
//!
//! To Herdr these are ordinary linked directories, so Herdr does not know
//! where they came from. This registry remembers the remote, the ref being
//! followed, the installed commit, and earlier installations for rollback.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

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
        let path = paths::registry_file();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.registry)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// One line per change, so `herdr-pm history` can show what happened.
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

pub fn log_event(event: Event) {
    let path = paths::history_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        if let Ok(line) = serde_json::to_string(&event) {
            let _ = writeln!(file, "{line}");
        }
    }
}

pub fn read_events(plugin_id: Option<&str>, limit: usize) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(paths::history_file()) else {
        return Vec::new();
    };
    let mut events: Vec<Event> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .filter(|e| plugin_id.is_none_or(|id| e.plugin_id == id))
        .collect();
    events.reverse();
    events.truncate(limit);
    events
}
