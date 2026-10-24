//! Where Herdr keeps its files, and where this manager keeps its own.
//!
//! Everything mirrors Herdr's own resolution (`XDG_CONFIG_HOME`/`XDG_STATE_HOME`
//! first, then the platform default) so the CLI, the popup, and Herdr agree
//! on locations without asking a running server.

use std::path::PathBuf;

/// The plugin id declared in `herdr-plugin.toml`.
pub const PLUGIN_ID: &str = "herdr-plugin-manager";

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home() -> PathBuf {
    env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/.config/herdr` (or `%APPDATA%\herdr`).
pub fn herdr_config_dir() -> PathBuf {
    if let Some(dir) = env_path("XDG_CONFIG_HOME") {
        return dir.join("herdr");
    }
    if cfg!(windows) {
        if let Some(dir) = env_path("APPDATA") {
            return dir.join("herdr");
        }
    }
    home().join(".config").join("herdr")
}

/// `~/.local/state/herdr` (or `%LOCALAPPDATA%\herdr`).
pub fn herdr_state_dir() -> PathBuf {
    if let Some(dir) = env_path("XDG_STATE_HOME") {
        return dir.join("herdr");
    }
    if cfg!(windows) {
        if let Some(dir) = env_path("LOCALAPPDATA") {
            return dir.join("herdr");
        }
    }
    home().join(".local").join("state").join("herdr")
}

/// Herdr's main config file, honouring `HERDR_CONFIG_PATH`.
pub fn herdr_config_toml() -> PathBuf {
    env_path("HERDR_CONFIG_PATH").unwrap_or_else(|| herdr_config_dir().join("config.toml"))
}

/// Herdr's global plugin registry.
pub fn herdr_plugins_json() -> PathBuf {
    herdr_config_dir().join("plugins.json")
}

/// Herdr's per-plugin config directory for any plugin id, matching
/// `herdr plugin config-dir <id>`.
pub fn herdr_plugin_config_dir(plugin_id: &str) -> PathBuf {
    herdr_config_dir()
        .join("plugins")
        .join("config")
        .join(plugin_path_component(plugin_id))
}

/// Same encoding Herdr uses for per-plugin directories: lowercase letters,
/// digits, `.`, `_` and `-` pass through; everything else is `%XX`.
pub fn plugin_path_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    if out.ends_with('.') {
        out.pop();
        out.push_str("%2E");
    }
    if out.is_empty() {
        "%plugin".to_string()
    } else {
        out
    }
}

fn running_as_this_plugin() -> bool {
    std::env::var("HERDR_PLUGIN_ID").as_deref() == Ok(PLUGIN_ID)
}

/// This manager's config directory (holds `config.toml`).
pub fn pm_config_dir() -> PathBuf {
    if let Some(dir) = env_path("HERDR_PM_CONFIG_DIR") {
        return dir;
    }
    if running_as_this_plugin() {
        if let Some(dir) = env_path("HERDR_PLUGIN_CONFIG_DIR") {
            return dir;
        }
    }
    herdr_plugin_config_dir(PLUGIN_ID)
}

pub fn pm_config_file() -> PathBuf {
    pm_config_dir().join("config.toml")
}

/// This manager's data directory: installs, git caches, registry.
pub fn pm_home() -> PathBuf {
    if let Some(dir) = env_path("HERDR_PM_HOME") {
        return dir;
    }
    if running_as_this_plugin() {
        if let Some(dir) = env_path("HERDR_PLUGIN_STATE_DIR") {
            return dir;
        }
    }
    herdr_state_dir()
        .join("plugins")
        .join(plugin_path_component(PLUGIN_ID))
}

pub fn registry_file() -> PathBuf {
    pm_home().join("registry.json")
}

pub fn registry_lock() -> PathBuf {
    pm_home().join(".registry.lock")
}

pub fn installs_dir() -> PathBuf {
    pm_home().join("installs")
}

pub fn trash_dir() -> PathBuf {
    pm_home().join("trash")
}

pub fn repo_cache_dir() -> PathBuf {
    pm_home().join("cache").join("repos")
}

pub fn market_cache_dir() -> PathBuf {
    pm_home().join("cache").join("market")
}

pub fn history_file() -> PathBuf {
    pm_home().join("history.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_component_matches_herdr() {
        assert_eq!(
            plugin_path_component("examples.agent-telegram-notify"),
            "examples.agent-telegram-notify"
        );
        assert_eq!(plugin_path_component("example:a"), "example%3Aa");
        assert_eq!(plugin_path_component("Example"), "%45xample");
        assert_eq!(plugin_path_component("example."), "example%2E");
        assert_eq!(plugin_path_component(PLUGIN_ID), PLUGIN_ID);
    }
}
