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
    std::fs::read_link(path).ok().map(|target| {
        if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or_else(|| Path::new(".")).join(target)
        }
    })
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
        // PowerShell 7 exports its own module path; Windows PowerShell would
        // then fail to load its built-in modules (Set-ItemProperty…).
        .env_remove("PSModulePath")
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
/// expandable string), then tells running programs. Already-running
/// programs keep their environment, notably a Herdr server started earlier
/// whose new panes inherit it: the PowerShell profiles cover those.
#[cfg(windows)]
fn set_registry_path(dir: &Path, present: bool) -> Result<bool> {
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

/// The user's all-hosts profile of `shell` and whether its execution policy
/// runs it; `None` when the shell is not installed.
#[cfg(windows)]
fn powershell_profile(shell: &str) -> Result<Option<(PathBuf, String)>> {
    let out = match Command::new(shell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$PROFILE.CurrentUserAllHosts; Get-ExecutionPolicy",
        ])
        .env_remove("PSModulePath")
        .output()
    {
        Ok(out) => out,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("could not run {shell}")),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    match (out.status.success(), lines.next(), lines.next()) {
        (true, Some(profile), Some(policy)) => {
            Ok(Some((PathBuf::from(profile), policy.to_string())))
        }
        _ => bail!(
            "{shell} did not report its profile: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("no output")
        ),
    }
}

/// Puts `dir` on PATH for new shells, including the panes of a Herdr server
/// started before the change (they inherit its old environment), and takes
/// it out again. Returns what changed, for the progress output.
#[cfg(windows)]
fn set_user_path(dir: &Path, present: bool) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    if set_registry_path(dir, present)? {
        notes.push(format!(
            "{} {} your user PATH",
            dir.display(),
            if present { "added to" } else { "removed from" }
        ));
    }
    let block = powershell_block(dir);
    for (shell, name) in [
        ("pwsh.exe", "PowerShell"),
        ("powershell.exe", "Windows PowerShell"),
    ] {
        let (profile, policy) = match powershell_profile(shell) {
            Ok(Some(found)) => found,
            Ok(None) => continue,
            Err(err) => {
                notes.push(format!("{name} profile left unchanged: {err:#}"));
                continue;
            }
        };
        // A profile the policy refuses to run would print an error in every
        // new shell instead of fixing PATH.
        let runs = matches!(policy.as_str(), "RemoteSigned" | "Unrestricted" | "Bypass");
        if present && !runs {
            notes.push(format!(
                "{name}: execution policy {policy} runs no profile; its new panes get herdr-pm once Herdr restarts"
            ));
            continue;
        }
        match update_profile(&profile, present.then_some(block.as_str()), true) {
            Ok(true) => notes.push(format!(
                "{name} profile {}: {}",
                if present { "updated" } else { "cleaned" },
                profile.display()
            )),
            Ok(false) => {}
            Err(err) => notes.push(format!("{name} profile left unchanged: {err:#}")),
        }
    }
    Ok(notes)
}

const PROFILE_START: &str = "# >>> herdr-plugin-manager >>>";
const PROFILE_END: &str = "# <<< herdr-plugin-manager <<<";

/// PowerShell lines adding `dir` to `$env:Path` unless it is there already.
#[cfg_attr(not(windows), allow(dead_code))]
fn powershell_block(dir: &Path) -> String {
    let quoted = dir.display().to_string().replace('\'', "''");
    format!(
        "$hpmDir = '{quoted}'\n\
         if (($env:Path -split ';') -notcontains $hpmDir) {{ $env:Path = \"$hpmDir;$env:Path\" }}\n\
         Remove-Variable hpmDir\n"
    )
}

/// POSIX shell lines adding `dir` to `$PATH` unless it is there already.
#[cfg_attr(windows, allow(dead_code))]
fn posix_block(dir: &Path) -> String {
    let quoted = dir.display().to_string().replace('\'', r"'\''");
    format!(
        "hpm_dir='{quoted}'\n\
         case \":$PATH:\" in *\":$hpm_dir:\"*) ;; *) export PATH=\"$hpm_dir:$PATH\" ;; esac\n\
         unset hpm_dir\n"
    )
}

/// `~/.local/bin` is often on PATH already, but not in the environment a
/// Herdr server started earlier hands to its new panes. A marked block in
/// `~/.profile` (and `~/.bashrc`, `~/.bash_profile`, `~/.zshrc` when they
/// exist) covers login and interactive shells alike; disabling removes it.
#[cfg(not(windows))]
fn set_user_path(dir: &Path, present: bool) -> Result<Vec<String>> {
    let home = home()?;
    let block = posix_block(dir);
    let mut notes = Vec::new();
    for name in [".profile", ".bashrc", ".bash_profile", ".zshrc"] {
        let file = home.join(name);
        if name != ".profile" && !file.exists() {
            continue;
        }
        if update_profile(&file, present.then_some(block.as_str()), false)? {
            notes.push(format!(
                "~/{name} {}",
                if present { "updated" } else { "cleaned" }
            ));
        }
    }
    Ok(notes)
}

/// Writes `file` with the marked block replaced by `block` (or removed),
/// keeping everything else, its line endings and a UTF-8 BOM. Returns
/// whether it changed. `bom` marks new files (Windows PowerShell 5.1 reads
/// BOM-less files as ANSI). Text that is not UTF-8 is left alone.
fn update_profile(file: &Path, block: Option<&str>, bom: bool) -> Result<bool> {
    const BOM: &str = "\u{feff}";
    let bytes = match std::fs::read(file) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("could not read {}", file.display())),
    };
    let Some(text) = bytes.as_deref().map(std::str::from_utf8).transpose().ok() else {
        bail!("{} is not UTF-8; add the PATH line by hand", file.display());
    };
    if text.is_none() && block.is_none() {
        return Ok(false);
    }
    let text = text.unwrap_or(if bom { BOM } else { "" });
    let updated = with_block(text, block);
    if updated == text {
        return Ok(false);
    }
    if updated.trim_start_matches(BOM).trim().is_empty() {
        std::fs::remove_file(file)
            .with_context(|| format!("could not remove {}", file.display()))?;
        return Ok(true);
    }
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, updated)
        .with_context(|| format!("could not update {}", file.display()))?;
    Ok(true)
}

/// `text` without the marked block, then with `block` appended between the
/// markers, in the line endings the text already uses. A leading BOM stays
/// in front (it would otherwise hide a marker on the first line).
fn with_block(text: &str, block: Option<&str>) -> String {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(body) => ("\u{feff}", body),
        None => ("", text),
    };
    let crlf = body.contains("\r\n") || (cfg!(windows) && !body.contains('\n'));
    let mut out = strip_block(body);
    if let Some(block) = block {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{PROFILE_START}\n{block}{PROFILE_END}\n"));
    }
    if crlf {
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    format!("{bom}{out}")
}

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
/// confirmation dialog. `skill_on`: the agent skill, which needs it.
pub fn cli_plan(on: bool, skill_on: bool) -> Result<Vec<String>> {
    let link = cli_link()?;
    let dir = cli_dir()?;
    Ok(if on {
        vec![
            format!("Link {}", link.display()),
            "  to the manager's current herdr-pm (symbolic link when allowed,".into(),
            "  otherwise a hard link or a copy), moved after every update.".into(),
            if cfg!(windows) {
                format!(
                    "Add {} to your user PATH and to your PowerShell profiles,",
                    dir.display()
                )
            } else {
                format!(
                    "Add {} to PATH in ~/.profile (and ~/.bashrc, ~/.bash_profile, ~/.zshrc),",
                    dir.display()
                )
            },
            "  so new terminals and new Herdr panes find herdr-pm at once,".into(),
            "  without restarting Herdr; shells already open keep their PATH.".into(),
        ]
    } else {
        let mut lines = vec![
            format!("Remove {}", link.display()),
            if cfg!(windows) {
                format!(
                    "and take {} out of your user PATH and PowerShell profiles.",
                    dir.display()
                )
            } else {
                format!("and take {} out of your shell profiles.", dir.display())
            },
        ];
        if skill_on {
            lines.push("Agents need herdr-pm, so the agent skill goes off too:".into());
            lines.extend(
                skill_plan(false, true)?
                    .into_iter()
                    .map(|l| format!("  {l}")),
            );
        }
        lines
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
        lines.extend(cli_plan(true, false)?.into_iter().map(|l| format!("  {l}")));
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
    let notes = set_user_path(&dir, true)?;
    if notes.is_empty() {
        out.line(&format!("{} was already on your PATH", dir.display()));
    }
    for note in notes {
        out.line(&note);
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
    for note in set_user_path(&dir, false)? {
        out.line(&note);
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

/// Migration must not replace an unrelated file that now occupies an old
/// integration's name. Validate everything before changing identity metadata.
pub(crate) fn validate_migration_links(
    legacy_root: Option<&Path>,
    cli_on: bool,
    skill_on: bool,
) -> Result<()> {
    let owned = |path: &Path, target: Option<PathBuf>| -> Result<()> {
        if std::fs::symlink_metadata(path).is_ok()
            && !target
                .as_deref()
                .is_some_and(|target| up_to_date(target, path))
        {
            bail!(
                "{} is not the legacy manager's integration; refusing to overwrite it",
                path.display()
            );
        }
        Ok(())
    };
    if cli_on {
        owned(&cli_link()?, legacy_root.map(|r| r.join("bin").join(EXE)))?;
    }
    if skill_on {
        let home = skill_home()?;
        owned(
            &home,
            legacy_root.map(|r| r.join("skills").join(SKILL_NAME)),
        )?;
        for (_, link) in detected_agents()? {
            owned(&link, Some(home.clone()))?;
        }
    }
    Ok(())
}

/// Moves the PATH link and the skill to the manager's current installation
/// when an update changed it, links the skill for agents installed since,
/// and puts the link directory on PATH when it is missing. The startup hook
/// and the popup run with the Herdr server's environment, so a missing
/// directory there means new panes need the profile blocks (a server started
/// before the change, or a config.toml copied from another machine). Does
/// nothing for integrations that are off.
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
        let dir = cli_dir()?;
        let on_path = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| same_file(&d, &dir)));
        if !on_path {
            set_user_path(&dir, true)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_block_keeps_user_content_and_line_endings() {
        let block = posix_block(Path::new("/h/.local/bin"));
        let original = "export EDITOR=vim";
        let added = with_block(original, Some(&block));
        assert!(added.starts_with("export EDITOR=vim"));
        assert_eq!(with_block(&added, Some(&block)), added);
        assert_eq!(
            with_block(&added, None).trim_end(),
            "export EDITOR=vim",
            "removing the block keeps the user's lines"
        );

        let moved = posix_block(Path::new("/elsewhere"));
        let replaced = with_block(&added, Some(&moved));
        assert!(replaced.contains("/elsewhere") && !replaced.contains("/h/.local/bin"));
        assert_eq!(replaced.matches(PROFILE_START).count(), 1);

        let windows = "\u{feff}Set-Alias g git\r\n";
        let added = with_block(windows, Some(&powershell_block(Path::new(r"C:\bin"))));
        assert!(added.starts_with("\u{feff}Set-Alias g git\r\n"));
        assert!(!added.replace("\r\n", "").contains('\n'));
        assert_eq!(with_block(&added, None), windows);
    }

    #[test]
    fn profile_files_are_created_updated_and_removed() {
        let dir = std::env::temp_dir().join(format!("hpm-profile-{}", std::process::id()));
        let file = dir.join("PowerShell").join("profile.ps1");
        let block = powershell_block(Path::new(r"C:\Users\O'Brien\bin"));
        assert!(block.contains(r"'C:\Users\O''Brien\bin'"));
        assert!(update_profile(&file, Some(&block), true).unwrap());
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .starts_with('\u{feff}'));
        assert!(!update_profile(&file, Some(&block), true).unwrap());
        assert!(update_profile(&file, None, true).unwrap());
        assert!(
            !file.exists(),
            "a profile holding only the block is removed"
        );
        assert!(!update_profile(&file, None, true).unwrap());

        std::fs::write(&file, "\u{feff}Import-Module posh-git\n").unwrap();
        update_profile(&file, Some(&block), true).unwrap();
        update_profile(&file, None, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "\u{feff}Import-Module posh-git\n"
        );
        std::fs::write(&file, [0xff, 0xfe, b'a', 0]).unwrap();
        assert!(update_profile(&file, Some(&block), true).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
