//! `herdr-pm` on the user's PATH and the `herdr-pm` agent skill, both
//! pointing at the manager's current installation through links that are
//! moved after every update.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::herdr::Herdr;
use crate::paths;

/// The skill this binary ships (`herdr-pm --skill` prints it).
pub const SKILL: &str = include_str!("../skills/herdr-pm/SKILL.md");
const SKILL_NAME: &str = "herdr-pm";

#[cfg(windows)]
const EXE: &str = "herdr-pm.exe";
#[cfg(not(windows))]
const EXE: &str = "herdr-pm";

/// Coding agents with a global skills directory, as `(name, directory
/// whose presence means the agent is installed, skills directory)`,
/// relative to the home directory. `~/.agents/skills` (Codex, Pi, Cline,
/// Warp, Zed, Kimi…) holds the skill itself; the others link to it.
const AGENTS: &[(&str, &str, &str)] = &[
    ("Claude Code", ".claude", ".claude/skills"),
    ("OMP", ".omp/agent", ".omp/agent/skills"),
    ("Cursor", ".cursor", ".cursor/skills"),
    ("Gemini CLI", ".gemini", ".gemini/skills"),
    ("GitHub Copilot", ".copilot", ".copilot/skills"),
    ("OpenCode", ".config/opencode", ".config/opencode/skills"),
    ("Amp", ".config/agents", ".config/agents/skills"),
    ("Droid", ".factory", ".factory/skills"),
    ("Goose", ".config/goose", ".config/goose/skills"),
    ("Crush", ".config/crush", ".config/crush/skills"),
    ("Devin", ".config/devin", ".config/devin/skills"),
    ("Windsurf", ".codeium/windsurf", ".codeium/windsurf/skills"),
    ("Kiro", ".kiro", ".kiro/skills"),
    ("Qwen Code", ".qwen", ".qwen/skills"),
    ("Roo Code", ".roo", ".roo/skills"),
    ("Kilo Code", ".kilo", ".kilo/skills"),
    ("Augment", ".augment", ".augment/skills"),
    ("Continue", ".continue", ".continue/skills"),
    ("OpenHands", ".openhands", ".openhands/skills"),
    ("Hermes Agent", ".hermes", ".hermes/skills"),
    ("Trae", ".trae", ".trae/skills"),
    ("Junie", ".junie", ".junie/skills"),
];

fn home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("no home directory"))
}

/// Where the `herdr-pm` link lives: `%LOCALAPPDATA%\herdr-plugin-manager\bin`
/// on Windows, `~/.local/bin` elsewhere. `HERDR_PM_CLI_DIR` overrides it.
pub fn cli_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("HERDR_PM_CLI_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("LOCALAPPDATA is not set"))?;
        Ok(local.join("herdr-plugin-manager").join("bin"))
    } else {
        Ok(home()?.join(".local").join("bin"))
    }
}

pub fn cli_link() -> Result<PathBuf> {
    Ok(cli_dir()?.join(EXE))
}

/// `~/.agents/skills/herdr-pm`.
pub fn skill_home() -> Result<PathBuf> {
    Ok(home()?.join(".agents").join("skills").join(SKILL_NAME))
}

/// The agents found on this machine and the skill link each gets.
pub fn detected_agents() -> Result<Vec<(&'static str, PathBuf)>> {
    let home = home()?;
    Ok(AGENTS
        .iter()
        .filter(|(_, root, _)| under(&home, root).is_dir())
        .map(|(name, _, skills)| (*name, under(&home, skills).join(SKILL_NAME)))
        .collect())
}

/// `base` joined with a `/`-separated relative path, using the platform's
/// separator (`mklink` takes `/` for an option).
fn under(base: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(base.to_path_buf(), |p, part| p.join(part))
}

/// The manager's installation as Herdr runs it.
fn manager_root(herdr: &Herdr) -> Result<PathBuf> {
    herdr
        .list()?
        .into_iter()
        .find(|p| p.plugin_id == paths::PLUGIN_ID)
        .map(|p| PathBuf::from(p.plugin_root))
        .ok_or_else(|| anyhow!("{} is not installed in Herdr", paths::PLUGIN_ID))
}

fn binary_in(root: &Path) -> Result<PathBuf> {
    let binary = root.join("bin").join(EXE);
    if !binary.is_file() {
        bail!("{} has no built binary", root.display());
    }
    Ok(binary)
}

fn skill_dir_in(root: &Path) -> Result<PathBuf> {
    let dir = root.join("skills").join(SKILL_NAME);
    if !dir.join("SKILL.md").is_file() {
        bail!("{} has no skills/{SKILL_NAME}/SKILL.md", root.display());
    }
    Ok(dir)
}

// ---------------------------------------------------------------- links

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Where a link points, when it is one.
fn link_target(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

/// Removes what `link_file`/`link_dir` created. A directory that is not a
/// link is only removed when it is a copy of this skill.
fn remove_link(path: &Path) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    let result = if meta.file_type().is_symlink() {
        std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
    } else if meta.is_dir() {
        let ours = std::fs::read_to_string(path.join("SKILL.md"))
            .is_ok_and(|s| s.contains(&format!("name: {SKILL_NAME}")));
        if !ours {
            bail!("{} exists and is not this manager's", path.display());
        }
        std::fs::remove_dir_all(path)
    } else {
        // A copied or hard-linked binary; one that is running cannot be
        // deleted on Windows, but it can be moved aside.
        std::fs::remove_file(path).or_else(|_| {
            let aside = path.with_extension(format!("old-{}", crate::util::now_unix()));
            std::fs::rename(path, aside)
        })
    };
    result.with_context(|| format!("could not remove {}", path.display()))
}

/// Points `link` at the file `target`: a symbolic link when the system
/// allows it, otherwise a hard link, otherwise a copy. Returns which.
fn link_file(target: &Path, link: &Path) -> Result<&'static str> {
    remove_link(link)?;
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    if std::os::unix::fs::symlink(target, link).is_ok() {
        return Ok("symbolic link");
    }
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(target, link).is_ok() {
        return Ok("symbolic link");
    }
    if std::fs::hard_link(target, link).is_ok() {
        return Ok("hard link");
    }
    std::fs::copy(target, link)
        .with_context(|| format!("could not copy {} to {}", target.display(), link.display()))?;
    Ok("copy")
}

/// Points `link` at the directory `target`: a symbolic link, a junction on
/// Windows without symlink rights, otherwise a copy.
fn link_dir(target: &Path, link: &Path) -> Result<&'static str> {
    remove_link(link)?;
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    if std::os::unix::fs::symlink(target, link).is_ok() {
        return Ok("symbolic link");
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok("symbolic link");
        }
        let junction = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output();
        if junction.is_ok_and(|o| o.status.success()) {
            return Ok("junction");
        }
    }
    copy_dir(target, link)?;
    Ok("copy")
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Whether `link` already leads to `target` (a link pointing there, or a
/// copy with the same content).
fn up_to_date(target: &Path, link: &Path) -> bool {
    if let Some(points_to) = link_target(link) {
        return same_file(&points_to, target);
    }
    let read = |p: &Path| {
        if p.is_dir() {
            std::fs::read(p.join("SKILL.md")).ok()
        } else {
            std::fs::read(p).ok()
        }
    };
    let content = read(link);
    content.is_some() && content == read(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ---------------------------------------------------------------- PATH

#[cfg(windows)]
fn powershell(script: &str, dir: &Path) -> Result<String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("HPM_DIR", dir)
        .output()
        .context("could not run powershell.exe")?;
    if !out.status.success() {
        bail!(
            "updating the user PATH failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Adds or removes `dir` in the user PATH (HKCU\Environment, kept as an
/// expandable string), then tells running programs. Returns whether it
/// changed.
#[cfg(windows)]
fn set_user_path(dir: &Path, present: bool) -> Result<bool> {
    let script = format!(
        r#"$d = $env:HPM_DIR
$key = Get-Item 'HKCU:\Environment'
$old = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
$parts = @($old -split ';' | Where-Object {{ $_ -ne '' -and $_.TrimEnd('\') -ne $d.TrimEnd('\') }})
if ({add}) {{ $parts += $d }}
$new = $parts -join ';'
if ($new -ne $old) {{
  Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value $new -Type ExpandString
  [Environment]::SetEnvironmentVariable('HERDR_PM_PATH_BROADCAST', $null, 'User')
  'changed'
}}"#,
        add = if present { "$true" } else { "$false" }
    );
    Ok(powershell(&script, dir)? == "changed")
}

#[cfg(not(windows))]
const PROFILE_START: &str = "# >>> herdr-plugin-manager >>>";
#[cfg(not(windows))]
const PROFILE_END: &str = "# <<< herdr-plugin-manager <<<";

/// `~/.local/bin` is on PATH on most systems; when it is not, a marked
/// block in `~/.profile` (and `~/.bashrc`/`~/.zshrc` when they exist) adds
/// it, and removing it takes the block out again.
#[cfg(not(windows))]
fn set_user_path(dir: &Path, present: bool) -> Result<bool> {
    let home = home()?;
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
    let mut changed = false;
    for name in [".profile", ".bashrc", ".zshrc"] {
        let file = home.join(name);
        if name != ".profile" && !file.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let kept = strip_block(&text);
        let updated = if present && !on_path {
            format!(
                "{kept}{}{PROFILE_START}\nexport PATH=\"{}:$PATH\"\n{PROFILE_END}\n",
                if kept.is_empty() || kept.ends_with('\n') {
                    ""
                } else {
                    "\n"
                },
                dir.display()
            )
        } else {
            kept
        };
        if updated != text {
            std::fs::write(&file, updated)
                .with_context(|| format!("could not update {}", file.display()))?;
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(not(windows))]
fn strip_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        match line.trim_end() {
            PROFILE_START => inside = true,
            PROFILE_END => inside = false,
            _ if !inside => out.push_str(line),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- plans

/// What enabling (or disabling) `herdr-pm` on PATH does, for the
/// confirmation dialog.
pub fn cli_plan(on: bool) -> Result<Vec<String>> {
    let link = cli_link()?;
    let dir = cli_dir()?;
    Ok(if on {
        vec![
            format!("Link {}", link.display()),
            "  to the manager's current herdr-pm (symbolic link when allowed,".into(),
            "  otherwise a hard link or a copy), moved after every update.".into(),
            format!("Add {} to your user PATH.", dir.display()),
            "New terminals find herdr-pm; open ones keep their PATH.".into(),
        ]
    } else {
        vec![
            format!("Remove {}", link.display()),
            format!("and take {} out of your user PATH.", dir.display()),
        ]
    })
}

/// What enabling (or disabling) the agent skill does.
pub fn skill_plan(on: bool, cli_on: bool) -> Result<Vec<String>> {
    let agents = detected_agents()?;
    let names: Vec<&str> = agents.iter().map(|(n, _)| *n).collect();
    let mut lines = if on {
        vec![
            format!(
                "Link {} to the skill of the installed manager,",
                skill_home()?.display()
            ),
            "  moved after every update (read by Codex, Pi, Cline, Zed…).".into(),
        ]
    } else {
        vec![format!("Remove {}", skill_home()?.display())]
    };
    if !names.is_empty() {
        lines.push(format!(
            "{} it for: {}.",
            if on { "Link" } else { "Unlink" },
            names.join(", ")
        ));
    }
    if on && !cli_on {
        lines.push("Agents run herdr-pm, so it also goes on your PATH:".into());
        lines.extend(cli_plan(true)?.into_iter().map(|l| format!("  {l}")));
    }
    Ok(lines)
}

// ---------------------------------------------------------------- actions

/// Progress of an integration change.
pub trait Steps {
    fn step(&self, text: &str);
    fn line(&self, text: &str);
    fn progress(&self, done: usize, total: usize);
}

pub fn enable_cli(herdr: &Herdr, out: &dyn Steps) -> Result<()> {
    out.progress(0, 2);
    out.step("finding the installed herdr-pm");
    let binary = binary_in(&manager_root(herdr)?)?;
    let link = cli_link()?;
    out.step(&format!("linking {}", link.display()));
    let how = link_file(&binary, &link)?;
    out.line(&format!(
        "{} → {} ({how})",
        link.display(),
        binary.display()
    ));
    out.progress(1, 2);
    out.step("adding it to your PATH");
    let dir = cli_dir()?;
    if set_user_path(&dir, true)? {
        out.line(&format!("{} added to your user PATH", dir.display()));
    } else {
        out.line(&format!("{} was already on your PATH", dir.display()));
    }
    out.progress(2, 2);
    Ok(())
}

pub fn disable_cli(out: &dyn Steps) -> Result<()> {
    out.progress(0, 2);
    let link = cli_link()?;
    out.step(&format!("removing {}", link.display()));
    remove_link(&link)?;
    out.progress(1, 2);
    out.step("taking it out of your PATH");
    let dir = cli_dir()?;
    if set_user_path(&dir, false)? {
        out.line(&format!("{} removed from your user PATH", dir.display()));
    }
    out.progress(2, 2);
    Ok(())
}

pub fn enable_skill(herdr: &Herdr, out: &dyn Steps) -> Result<()> {
    let agents = detected_agents()?;
    let total = agents.len() + 1;
    out.progress(0, total);
    out.step("finding the installed skill");
    let source = skill_dir_in(&manager_root(herdr)?)?;
    let home = skill_home()?;
    out.step(&format!("linking {}", home.display()));
    let how = link_dir(&source, &home)?;
    out.line(&format!(
        "{} → {} ({how})",
        home.display(),
        source.display()
    ));
    for (i, (name, link)) in agents.iter().enumerate() {
        out.progress(i + 1, total);
        out.step(&format!("linking the skill for {name}"));
        let how = link_dir(&home, link)?;
        out.line(&format!("{name}: {} ({how})", link.display()));
    }
    out.progress(total, total);
    Ok(())
}

pub fn disable_skill(out: &dyn Steps) -> Result<()> {
    let agents = detected_agents()?;
    let total = agents.len() + 1;
    for (i, (name, link)) in agents.iter().enumerate() {
        out.progress(i, total);
        if is_link(link) || link.exists() {
            out.step(&format!("unlinking the skill for {name}"));
            remove_link(link)?;
        }
    }
    out.step("removing the skill");
    remove_link(&skill_home()?)?;
    out.progress(total, total);
    Ok(())
}

/// Moves the PATH link and the skill to the manager's current installation
/// when an update changed it, and links the skill for agents installed
/// since. Does nothing for integrations that are off.
pub fn refresh(herdr: &Herdr, cli_on: bool, skill_on: bool) -> Result<()> {
    if !cli_on && !skill_on {
        return Ok(());
    }
    let root = manager_root(herdr)?;
    if cli_on {
        let binary = binary_in(&root)?;
        let link = cli_link()?;
        if !up_to_date(&binary, &link) {
            link_file(&binary, &link)?;
        }
    }
    if skill_on {
        let source = skill_dir_in(&root)?;
        let home = skill_home()?;
        if !up_to_date(&source, &home) {
            link_dir(&source, &home)?;
        }
        for (_, link) in detected_agents()? {
            // An agent's own directory under that name is left alone.
            if !up_to_date(&home, &link) {
                let _ = link_dir(&home, &link);
            }
        }
    }
    Ok(())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn profile_block_is_added_once_and_removed_cleanly() {
        let original = "export EDITOR=vim\n";
        let with = format!(
            "{original}{PROFILE_START}\nexport PATH=\"/h/.local/bin:$PATH\"\n{PROFILE_END}\n"
        );
        assert_eq!(strip_block(&with), original);
        assert_eq!(strip_block(original), original);
    }
}
