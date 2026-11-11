//! Explicit identity cutover. Runtime snapshots and unrelated plugin roots never move.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::herdr::{self, Herdr};
use crate::manifest::{self, Manifest};
use crate::{config::Config, integrate, paths, util};

#[cfg(windows)]
const EXE: &str = "herdr-pm.exe";
#[cfg(not(windows))]
const EXE: &str = "herdr-pm";

fn override_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn old_home() -> PathBuf {
    override_path("HERDR_PM_HOME").unwrap_or_else(|| {
        paths::herdr_state_dir()
            .join("plugins")
            .join(paths::OLD_PLUGIN_ID)
    })
}

fn old_config() -> PathBuf {
    override_path("HERDR_PM_CONFIG_DIR")
        .unwrap_or_else(|| paths::herdr_plugin_config_dir(paths::OLD_PLUGIN_ID))
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn read_json(path: &Path, default: Value) -> Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid JSON in {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(err) => Err(err).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Detects unmigrated identity metadata without creating files or loading Config.
/// The retained old installation directory alone is deliberately not a trigger.
pub fn required() -> Result<bool> {
    let plugins = read_json(&paths::herdr_plugins_json(), json!([]))?;
    if plugins.as_array().is_some_and(|entries| {
        entries
            .iter()
            .any(|p| p["plugin_id"] == paths::OLD_PLUGIN_ID)
    }) {
        return Ok(true);
    }
    let registry_path = old_home().join("registry.json");
    if registry_path != paths::registry_file() && exists(&registry_path) {
        return Ok(true);
    }
    let registry = read_json(&registry_path, json!({}))?;
    Ok(registry["plugins"].get(paths::OLD_PLUGIN_ID).is_some()
        || (old_config() != paths::pm_config_dir() && exists(&old_config())))
}

fn inputs(root: &Path, binary: Option<&Path>) -> Result<(PathBuf, PathBuf, Manifest)> {
    let root = std::fs::canonicalize(root)
        .with_context(|| format!("could not open migration root {}", root.display()))?;
    let text = std::fs::read_to_string(root.join(manifest::FILE_NAME))?;
    let manifest = Manifest::parse(&text)?;
    manifest.validate(None)?;
    if manifest.id.trim() != paths::PLUGIN_ID {
        bail!(
            "migration root must declare {}, not {}",
            paths::PLUGIN_ID,
            manifest.id
        );
    }
    let binary = binary
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("bin").join(EXE));
    let binary = std::fs::canonicalize(&binary)
        .with_context(|| format!("build the new binary first: {}", binary.display()))?;
    if !binary.is_file() || !root.join("skills/herdr-pm/SKILL.md").is_file() {
        bail!("migration requires a built binary and skills/herdr-pm/SKILL.md");
    }
    for directory in ["src", "scripts"] {
        if !root.join(directory).is_dir() {
            bail!("migration root must include {directory}/ so its build commands remain usable");
        }
    }
    for file in ["Cargo.toml", "Cargo.lock"] {
        if !root.join(file).is_file() {
            bail!("migration root must include {file}");
        }
    }
    Ok((root, binary, manifest))
}

pub fn plan(root: &Path, binary: Option<&Path>) -> Result<Vec<String>> {
    let (root, binary, _) = inputs(root, binary)?;
    Ok(vec![
        format!("Migrate {} to {}", paths::OLD_PLUGIN_ID, paths::PLUGIN_ID),
        format!("Use manifest and skill from {}", root.display()),
        format!("Install a managed runtime snapshot of {}", binary.display()),
        "Preserve config, enabled state, remote/ref, pins, auto-update and installation history."
            .into(),
        "Leave all existing installation paths and unrelated plugin files untouched.".into(),
        "Update Herdr action keybindings and refresh enabled CLI/skill integrations.".into(),
    ])
}

fn lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

/// A relative symlink is relative to its old parent, not its new location.
fn move_metadata(from: &Path, to: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        if target.is_relative() {
            let source = std::path::absolute(from)?;
            let target = source
                .parent()
                .context("metadata path has no parent")?
                .join(target);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, to)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileTypeExt;
                if metadata.file_type().is_symlink_dir() {
                    std::os::windows::fs::symlink_dir(&target, to)?;
                } else {
                    std::os::windows::fs::symlink_file(&target, to)?;
                }
            }
            let remove = |path: &Path| -> std::io::Result<()> {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileTypeExt;
                    if metadata.file_type().is_symlink_dir() {
                        return std::fs::remove_dir(path);
                    }
                }
                std::fs::remove_file(path)
            };
            if let Err(err) = remove(from) {
                let _ = remove(to);
                return Err(err.into());
            }
            return Ok(());
        }
    }
    std::fs::rename(from, to)?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = herdr::resolve_write_target(path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("migration.tmp");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .with_context(|| {
            format!(
                "could not stage {} (remove a stale migration.tmp only after investigating)",
                target.display()
            )
        })?;
    use std::io::Write;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

fn rename_key(map: &mut serde_json::Map<String, Value>) -> Result<()> {
    if map.contains_key(paths::OLD_PLUGIN_ID) {
        if map.contains_key(paths::PLUGIN_ID) {
            bail!("both manager identities have settings; refusing to overwrite either");
        }
        let value = map.remove(paths::OLD_PLUGIN_ID).unwrap();
        map.insert(paths::PLUGIN_ID.into(), value);
    }
    Ok(())
}

fn migrate_registry(registry: &mut Value, installation: Value, name: &str) -> Result<()> {
    if !registry.is_object() {
        bail!("manager registry must be a JSON object");
    }
    let commit = installation["commit"].clone();
    if let Some(plugins) = registry.get_mut("plugins") {
        let plugins = plugins
            .as_object_mut()
            .context("registry plugins must be an object")?;
        rename_key(plugins)?;
        if let Some(record) = plugins.get_mut(paths::PLUGIN_ID) {
            record["id"] = paths::PLUGIN_ID.into();
            record["name"] = name.into();
            let old = record["current"].take();
            if record["requested_ref"]
                .as_str()
                .is_some_and(util::is_full_sha)
            {
                record["requested_ref"] = commit.clone();
            }
            let previous = record
                .as_object_mut()
                .context("manager record must be an object")?
                .entry("previous")
                .or_insert_with(|| json!([]));
            previous
                .as_array_mut()
                .context("installation history must be an array")?
                .insert(0, old);
            let mut installation = installation;
            installation["requested_ref"] = record["requested_ref"].clone();
            record["current"] = installation;
        }
    }
    if let Some(pins) = registry.get_mut("pins") {
        rename_key(
            pins.as_object_mut()
                .context("registry pins must be an object")?,
        )?;
        if let Some(pin) = pins.get_mut(paths::PLUGIN_ID) {
            *pin = commit;
        }
    }
    if let Some(ids) = registry.get_mut("auto_update") {
        let ids = ids
            .as_array_mut()
            .context("registry auto_update must be an array")?;
        for id in ids.iter_mut() {
            if id.as_str() == Some(paths::OLD_PLUGIN_ID) {
                *id = paths::PLUGIN_ID.into();
            }
        }
    }
    Ok(())
}

fn migrate_config(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    if let Some(plugins) = doc
        .get_mut("auto_update")
        .and_then(|a| a.get_mut("plugins"))
        .and_then(|p| p.as_table_like_mut())
    {
        if plugins.contains_key(paths::OLD_PLUGIN_ID) {
            if plugins.contains_key(paths::PLUGIN_ID) {
                bail!("both manager identities have auto-update strategies; refusing to overwrite either");
            }
            let item = plugins.remove(paths::OLD_PLUGIN_ID).unwrap();
            plugins.insert(paths::PLUGIN_ID, item);
        }
    }
    let text = doc.to_string();
    let text = Config::migrate_text(&text)?.unwrap_or(text);
    Config::parse(&text)?;
    Ok(text)
}

fn migrate_bindings(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    if let Some(commands) = doc
        .get_mut("keys")
        .and_then(|k| k.get_mut("command"))
        .and_then(|c| c.as_array_of_tables_mut())
    {
        let prefix = format!("{}.", paths::OLD_PLUGIN_ID);
        for entry in commands.iter_mut() {
            if entry.get("type").and_then(|i| i.as_str()) != Some("plugin_action") {
                continue;
            }
            if let Some(action) = entry
                .get("command")
                .and_then(|i| i.as_str())
                .and_then(|command| command.strip_prefix(&prefix))
            {
                let command = format!("{}.{action}", paths::PLUGIN_ID);
                let item = entry.get_mut("command").unwrap();
                let decor = item.as_value().unwrap().decor().clone();
                *item = toml_edit::value(command);
                *item.as_value_mut().unwrap().decor_mut() = decor;
            }
        }
    }
    Ok(doc.to_string())
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!(
                "migration build resources must not contain symlinks: {}",
                entry.path().display()
            );
        }
        let destination = target.join(entry.file_name());
        if metadata.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

/// Explicit bootstrap for old updaters, which correctly reject a changed manifest id.
/// The caller confirms the plan before calling this; dry runs never create locks/files.
pub fn run(herdr: &Herdr, root: &Path, binary: Option<&Path>, dry_run: bool) -> Result<()> {
    let (root, binary, manifest) = inputs(root, binary)?;
    if dry_run {
        for line in plan(&root, Some(&binary))? {
            println!("[dry-run] {line}");
        }
        return Ok(());
    }
    let _migration_lock = lock(&paths::herdr_config_dir().join(".plugin-manager-migration.lock"))?;
    if !required()? {
        println!("Manager identity is already current; no legacy metadata needs migration.");
        return Ok(());
    }
    let plugins = herdr.list()?;
    if plugins.iter().any(|p| p.plugin_id == paths::PLUGIN_ID) {
        bail!(
            "both manager identities are present; refusing to overwrite the canonical installation"
        );
    }
    let old = plugins.iter().find(|p| p.plugin_id == paths::OLD_PLUGIN_ID);
    if old.is_some_and(|p| p.is_github()) {
        bail!("the legacy manager is Herdr GitHub-managed; first relink its existing plugin_root with `herdr plugin link <root>` so migration cannot delete its checkout");
    }
    let enabled = old.is_none_or(|p| p.enabled);
    let old_home = old_home();
    let new_home = paths::pm_home();
    let _old_lock = lock(&old_home.join(".registry.lock"))?;
    let _new_lock = if old_home != new_home {
        Some(lock(&new_home.join(".registry.lock"))?)
    } else {
        None
    };
    let _config_lock = lock(&paths::herdr_config_dir().join(".config.lock"))?;
    let source_registry = old_home.join("registry.json");
    let target_registry = paths::registry_file();
    if source_registry != target_registry && exists(&target_registry) {
        bail!("canonical registry {} already exists; refusing to overwrite it (an interrupted migration must be restored before retrying)", target_registry.display());
    }
    let source_config = old_config();
    let target_config = paths::pm_config_dir();
    let mut moves = Vec::new();
    for (from, to) in [
        (source_config.clone(), target_config.clone()),
        (source_registry.clone(), target_registry.clone()),
        (
            old_home.join("history.jsonl"),
            new_home.join("history.jsonl"),
        ),
        (
            old_home.join("history.ecs.ndjson"),
            new_home.join("history.ecs.ndjson"),
        ),
    ] {
        if from != to && exists(&from) {
            if exists(&to) {
                bail!(
                    "migration destination {} already exists; refusing to merge or overwrite it",
                    to.display()
                );
            }
            moves.push((from, to));
        }
    }
    let original_registry = std::fs::read(&source_registry).ok();
    let mut registry = read_json(
        &source_registry,
        json!({"version":1,"plugins":{},"auto_update":[],"pins":{}}),
    )?;
    let config_file = if exists(&source_config) {
        source_config.join("config.toml")
    } else {
        target_config.join("config.toml")
    };
    let original_config = match std::fs::read_to_string(&config_file) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let config = original_config.as_deref().map(migrate_config).transpose()?;
    let settings = config
        .as_deref()
        .map(Config::parse)
        .transpose()?
        .unwrap_or_default();
    integrate::validate_migration_links(
        old.map(|p| Path::new(&p.plugin_root)),
        settings.cli_on_path,
        settings.agent_skill,
    )?;
    let herdr_config = paths::herdr_config_toml();
    let original_bindings = match std::fs::read_to_string(&herdr_config) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let bindings = original_bindings
        .as_deref()
        .map(migrate_bindings)
        .transpose()?;
    let snapshot = new_home
        .join("installs")
        .join(paths::PLUGIN_ID)
        .join(format!(
            "identity-{}-{}",
            util::now_unix(),
            std::process::id()
        ));
    if exists(&snapshot) {
        bail!("migration snapshot {} already exists", snapshot.display());
    }
    let current = &registry["plugins"][paths::OLD_PLUGIN_ID]["current"];
    let commit = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| util::is_full_sha(s))
        .or_else(|| current["commit"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let installation = json!({
        "commit": commit,
        "version": manifest.version,
        "plugin_root": snapshot,
        "install_dir": snapshot,
        "requested_ref": registry["plugins"][paths::OLD_PLUGIN_ID]["requested_ref"],
        "installed_unix": util::now_unix(),
    });
    migrate_registry(&mut registry, installation, &manifest.name)?;
    let mut completed_moves = Vec::new();
    let mut linked = false;
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(snapshot.join("bin"))?;
        std::fs::create_dir_all(snapshot.join("skills/herdr-pm"))?;
        std::fs::copy(
            root.join(manifest::FILE_NAME),
            snapshot.join(manifest::FILE_NAME),
        )?;
        std::fs::copy(&binary, snapshot.join("bin").join(EXE))?;
        std::fs::copy(
            root.join("skills/herdr-pm/SKILL.md"),
            snapshot.join("skills/herdr-pm/SKILL.md"),
        )?;
        // Keep the manifest's build commands meaningful, including forced source builds.
        for directory in ["src", "scripts"] {
            copy_tree(&root.join(directory), &snapshot.join(directory))?;
        }
        for file in ["Cargo.toml", "Cargo.lock"] {
            std::fs::copy(root.join(file), snapshot.join(file))?;
        }
        for (from, to) in &moves {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            move_metadata(from, to).with_context(|| {
                format!("could not move {} to {}", from.display(), to.display())
            })?;
            completed_moves.push((from.clone(), to.clone()));
        }
        write_atomic(&target_registry, &serde_json::to_vec_pretty(&registry)?)?;
        if let Some(text) = &config {
            write_atomic(&target_config.join("config.toml"), text.as_bytes())?;
        }
        if let Some(text) = &bindings {
            write_atomic(&herdr_config, text.as_bytes())?;
        }
        let linked_manifest = Manifest::parse(&std::fs::read_to_string(
            snapshot.join(manifest::FILE_NAME),
        )?)?;
        if linked_manifest.id.trim() != paths::PLUGIN_ID {
            bail!("migration manifest changed identity after its preview");
        }
        herdr.link(&snapshot, enabled)?;
        linked = true;
        if old.is_some() {
            herdr.unregister(paths::OLD_PLUGIN_ID)?;
        }
        if bindings != original_bindings {
            herdr.reload_config()?;
        }
        Ok(())
    })();
    if let Err(err) = result {
        let mut failures = Vec::new();
        if linked {
            if let Err(err) = herdr.unregister(paths::PLUGIN_ID) {
                failures.push(err.to_string());
            }
            if let Some(old) = old {
                if let Err(err) = herdr.link(Path::new(&old.plugin_root), old.enabled) {
                    failures.push(err.to_string());
                }
            }
        }
        let restore = (|| -> Result<()> {
            if let Some(text) = &original_bindings {
                write_atomic(&herdr_config, text.as_bytes())?;
            }
            if let Some(text) = &original_config {
                let config_dir = if exists(&target_config) {
                    &target_config
                } else {
                    &source_config
                };
                write_atomic(&config_dir.join("config.toml"), text.as_bytes())?;
            }
            if let Some(bytes) = &original_registry {
                let path = if exists(&target_registry) {
                    &target_registry
                } else {
                    &source_registry
                };
                write_atomic(path, bytes)?;
            } else if exists(&target_registry) {
                std::fs::remove_file(&target_registry)?;
            }
            for (from, to) in completed_moves.iter().rev() {
                move_metadata(to, from)?;
            }
            if bindings != original_bindings {
                herdr.reload_config()?;
            }
            Ok(())
        })();
        if let Err(err) = restore {
            failures.push(err.to_string());
        }
        if !linked || failures.is_empty() {
            let _ = std::fs::remove_dir_all(&snapshot);
        }
        if !failures.is_empty() {
            bail!(
                "migration failed: {err:#}; restoration needs attention: {}",
                failures.join("; ")
            );
        }
        return Err(err.context("identity migration rolled back; legacy manager remains installed"));
    }
    drop(_old_lock);
    drop(_new_lock);
    drop(_config_lock);
    // Refresh only after the canonical registration and all preserved settings are committed.
    let config = Config::load()?;
    if let Err(err) = integrate::refresh(herdr, config.cli_on_path, config.agent_skill) {
        bail!("identity migrated, but integration refresh failed: {err:#}; run the new binary with `path enable` / `skill enable` to repair enabled integrations");
    }
    println!(
        "Migrated {} to {}. Existing installation roots were retained.",
        paths::OLD_PLUGIN_ID,
        paths::PLUGIN_ID
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn moving_metadata_keeps_relative_symlink_targets() {
        let root = std::env::temp_dir().join(format!(
            "hpm-migration-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let old = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(old.join("data")).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(old.join("data/state.json"), "retained").unwrap();
        std::os::unix::fs::symlink("data/state.json", old.join("registry.json")).unwrap();
        move_metadata(&old.join("registry.json"), &new.join("registry.json")).unwrap();
        write_atomic(&new.join("registry.json"), b"updated").unwrap();
        assert_eq!(
            std::fs::read_to_string(old.join("data/state.json")).unwrap(),
            "updated"
        );
        assert!(std::fs::symlink_metadata(new.join("registry.json"))
            .unwrap()
            .file_type()
            .is_symlink());
        move_metadata(&new.join("registry.json"), &old.join("registry.json")).unwrap();
        assert_eq!(
            std::fs::read_to_string(old.join("registry.json")).unwrap(),
            "updated"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preserves_unrelated_installations_settings_and_history() {
        let unrelated = json!({"id":"ttt.editor","current":{"plugin_root":"C:/legacy/installs/editor","enabled":false}});
        let prior = json!({"commit":"old","plugin_root":"C:/legacy/installs/manager"});
        let older = json!({"commit":"older","plugin_root":"C:/legacy/installs/older"});
        let mut state = json!({
            "plugins": {"herdr-plugin-manager":{"id":"herdr-plugin-manager","url":"ssh://git.example/manager","requested_ref":"main","current":prior,"previous":[older],"first_installed_unix":17},"ttt.editor":unrelated},
            "auto_update":["herdr-plugin-manager","ttt.editor"],
            "pins":{"herdr-plugin-manager":"pinned","ttt.editor":"other"},
            "last_auto_update":123,"future_field":true,
        });
        let snapshot =
            json!({"commit":"new","plugin_root":"C:/new/manager","requested_ref":"main"});
        migrate_registry(&mut state, snapshot.clone(), "Manager").unwrap();
        assert_eq!(state["plugins"]["ttt.editor"], unrelated);
        let manager = &state["plugins"][paths::PLUGIN_ID];
        assert_eq!(manager["current"], snapshot);
        assert_eq!(manager["previous"], json!([prior, older]));
        assert_eq!(manager["first_installed_unix"], 17);
        assert_eq!(manager["requested_ref"], "main");
        assert_eq!(state["pins"][paths::PLUGIN_ID], "new");
        assert_eq!(state["pins"]["ttt.editor"], "other");
        assert_eq!(
            state["auto_update"],
            json!([paths::PLUGIN_ID, "ttt.editor"])
        );
        assert_eq!(state["last_auto_update"], 123);
        assert_eq!(state["future_field"], true);
        assert!(state["plugins"].get(paths::OLD_PLUGIN_ID).is_none());
    }

    #[test]
    fn rebases_legacy_pin_to_the_canonical_snapshot() {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let mut state = json!({
            "plugins": {"herdr-plugin-manager": {
                "id":"herdr-plugin-manager",
                "requested_ref":old,
                "current":{"commit":old,"requested_ref":old},
                "previous":[]
            }},
            "pins":{"herdr-plugin-manager":old}
        });
        migrate_registry(&mut state, json!({"commit":new}), "Manager").unwrap();
        let manager = &state["plugins"][paths::PLUGIN_ID];
        assert_eq!(manager["requested_ref"], new);
        assert_eq!(manager["current"]["requested_ref"], new);
        assert_eq!(manager["previous"][0]["requested_ref"], old);
        assert_eq!(state["pins"][paths::PLUGIN_ID], new);
    }

    #[test]
    fn updates_only_manager_action_bindings() {
        let original = "# retained\n[[keys.command]]\nkey = 'prefix+p'\ntype = 'plugin_action'\ncommand = 'herdr-plugin-manager.open' # note\n[[keys.command]]\nkey = 'prefix+e'\ntype = 'plugin_action'\ncommand = 'ttt.editor.open'\n";
        let updated = migrate_bindings(original).unwrap();
        assert!(updated.contains("julienpoirou.herdr-plugin-manager.open"));
        assert!(updated.contains("ttt.editor.open"));
        assert!(updated.contains("# retained"));
        assert!(updated.contains("# note"));
    }

    #[test]
    fn refuses_colliding_settings() {
        let mut state =
            json!({"plugins":{"herdr-plugin-manager":{},"julienpoirou.herdr-plugin-manager":{}}});
        assert!(migrate_registry(&mut state, json!({}), "Manager").is_err());
    }
}
