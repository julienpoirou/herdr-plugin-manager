//! Everything that changes installed plugins. The CLI calls these directly;
//! the popup suspends itself and calls the same functions, so both share
//! one trust preview, one build runner and one registry.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::git::Git;
use crate::herdr::{Herdr, Plugin};
use crate::manifest::{self, Manifest};
use crate::paths;
use crate::registry::{self, Event, Installation, Record, Registry};
use crate::source::{self, Source};
use crate::util;

pub struct Ctx {
    pub herdr: Herdr,
    pub config: Config,
    pub yes: bool,
    pub dry_run: bool,
}

// ---------------------------------------------------------------- output

fn color_on() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

pub fn paint(code: &str, text: &str) -> String {
    if color_on() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(t: &str) -> String {
    paint("1", t)
}
pub fn dim(t: &str) -> String {
    paint("2", t)
}
pub fn green(t: &str) -> String {
    paint("32", t)
}
pub fn yellow(t: &str) -> String {
    paint("33", t)
}
pub fn red(t: &str) -> String {
    paint("31", t)
}
pub fn cyan(t: &str) -> String {
    paint("36", t)
}

fn step(text: &str) {
    eprintln!("{} {text}", cyan("›"));
}

// ---------------------------------------------------------------- rows

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Cloned, built and linked by this manager from a git remote.
    Git,
    /// Installed by Herdr itself from GitHub.
    Github,
    /// A directory linked by hand (`herdr plugin link`).
    Local,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Git => "git",
            Origin::Github => "github",
            Origin::Local => "local",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub plugin: Plugin,
    pub record: Option<Record>,
    pub origin: Origin,
}

impl Row {
    pub fn source_display(&self) -> String {
        match self.origin {
            Origin::Git => self
                .record
                .as_ref()
                .map(Record::source_display)
                .unwrap_or_default(),
            Origin::Github => self.plugin.source.github_spec().unwrap_or_default(),
            Origin::Local => self.plugin.plugin_root.clone(),
        }
    }

    pub fn commit(&self) -> Option<&str> {
        match self.origin {
            Origin::Git => self.record.as_ref().map(|r| r.current.commit.as_str()),
            Origin::Github => self.plugin.source.resolved_commit.as_deref(),
            Origin::Local => None,
        }
    }

    pub fn requested_ref(&self) -> Option<&str> {
        match self.origin {
            Origin::Git => self
                .record
                .as_ref()
                .and_then(|r| r.requested_ref.as_deref()),
            Origin::Github => self.plugin.source.requested_ref.as_deref(),
            Origin::Local => None,
        }
    }

    pub fn ref_display(&self) -> String {
        match self.requested_ref() {
            None if self.origin == Origin::Local => "-".into(),
            None => "default branch".into(),
            Some(r) if util::is_full_sha(r) => format!("pinned {}", util::short_sha(r)),
            Some(r) => r.to_string(),
        }
    }

    pub fn pinned(&self) -> bool {
        self.requested_ref().is_some_and(util::is_full_sha)
    }

    /// Browsable page for the source, when there is one.
    pub fn web_url(&self) -> Option<String> {
        match self.origin {
            Origin::Github => {
                let slug = self.plugin.source.github_slug()?;
                let base = format!("https://github.com/{slug}");
                match (self.plugin.source.subdir.as_deref(), self.commit()) {
                    (Some(sub), Some(sha)) => Some(format!("{base}/tree/{sha}/{sub}")),
                    _ => Some(base),
                }
            }
            Origin::Git => util::web_url_for_remote(&self.record.as_ref()?.url),
            Origin::Local => None,
        }
    }

    /// The git remote and ref used for update checks.
    pub fn remote(&self) -> Option<(String, Option<String>)> {
        match self.origin {
            Origin::Git => {
                let r = self.record.as_ref()?;
                Some((r.url.clone(), r.requested_ref.clone()))
            }
            Origin::Github => Some((
                format!(
                    "https://github.com/{}.git",
                    self.plugin.source.github_slug()?
                ),
                self.plugin.source.requested_ref.clone(),
            )),
            Origin::Local => None,
        }
    }
}

fn same_path(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Joins Herdr's plugin list with this manager's records.
pub fn rows(herdr: &Herdr) -> Result<Vec<Row>> {
    let plugins = herdr.list()?;
    let registry = Registry::load()?;
    Ok(plugins
        .into_iter()
        .map(|plugin| {
            let record = registry
                .get(&plugin.plugin_id)
                .filter(|r| {
                    !plugin.is_github() && same_path(&r.current.plugin_root, &plugin.plugin_root)
                })
                .cloned();
            let origin = if record.is_some() {
                Origin::Git
            } else if plugin.is_github() {
                Origin::Github
            } else {
                Origin::Local
            };
            Row {
                plugin,
                record,
                origin,
            }
        })
        .collect())
}

// ---------------------------------------------------------------- update status

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Unknown,
    Current,
    Available {
        commit: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    Pinned,
    Local,
    Error {
        message: String,
    },
}

/// Cheap check: one `git ls-remote`, no fetch.
pub fn check(row: &Row, git: Git) -> Status {
    if row.origin == Origin::Local {
        return Status::Local;
    }
    if row.pinned() {
        return Status::Pinned;
    }
    let Some((url, reference)) = row.remote() else {
        return Status::Unknown;
    };
    let Some(current) = row.commit() else {
        return Status::Unknown;
    };
    match git.ls_remote(&url, reference.as_deref()) {
        Ok(sha) if sha.eq_ignore_ascii_case(current) => Status::Current,
        Ok(sha) => Status::Available {
            commit: sha,
            version: None,
        },
        Err(err) => Status::Error {
            message: first_line(&err.to_string()),
        },
    }
}

fn first_line(text: &str) -> String {
    util::truncate(&util::sanitize(text.lines().next().unwrap_or(text)), 160)
}

/// Version string of the plugin at `commit` (best effort).
pub fn remote_version(row: &Row, commit: &str, git: Git) -> Option<String> {
    let manifest_rel = manifest_path(
        row.plugin
            .source
            .subdir
            .as_deref()
            .or(row.record.as_ref().and_then(|r| r.subdir.as_deref())),
    );
    match row.origin {
        Origin::Git => {
            let record = row.record.as_ref()?;
            let mirror = git.sync(&record.url).ok()?;
            let text = git.show_file(&mirror, commit, &manifest_rel).ok()??;
            Manifest::parse(&text).ok().map(|m| m.version)
        }
        Origin::Github => {
            let slug = row.plugin.source.github_slug()?;
            let url = format!("https://raw.githubusercontent.com/{slug}/{commit}/{manifest_rel}");
            let text = crate::market::http_get_text(&url, None, Duration::from_secs(6)).ok()?;
            Manifest::parse(&text).ok().map(|m| m.version)
        }
        Origin::Local => None,
    }
}

fn manifest_path(subdir: Option<&str>) -> String {
    match subdir.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}/{}", manifest::FILE_NAME),
        None => manifest::FILE_NAME.to_string(),
    }
}

// ---------------------------------------------------------------- install

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Update,
    Switch,
    Import,
}

impl Mode {
    fn verb(self) -> &'static str {
        match self {
            Mode::Install | Mode::Import => "install",
            Mode::Update => "update",
            Mode::Switch => "switch",
        }
    }
}

pub struct GitRequest {
    pub url: String,
    pub subdir: Option<String>,
    /// What to check out (branch, tag, commit; None = default branch).
    pub checkout_ref: Option<String>,
    /// What later updates follow. Defaults to `checkout_ref`.
    pub follow_ref: Option<Option<String>>,
    pub mode: Mode,
    pub enabled: Option<bool>,
    pub replace: bool,
    /// For updates: the plugin id the source must still contain.
    pub expect_id: Option<String>,
}

pub struct InstallRequest {
    pub source: Source,
    pub reference: Option<String>,
    pub enabled: Option<bool>,
    pub replace: bool,
}

/// Entry point for `install`: GitHub shorthands go to Herdr's own
/// installer, everything else through the git pipeline.
pub fn install(ctx: &Ctx, request: InstallRequest) -> Result<bool> {
    if request.replace && matches!(&request.source, Source::Github { .. }) {
        bail!("--replace with a GitHub shorthand needs Herdr replacement support; rerun with --git to replace it through the safe git pipeline");
    }
    match request.source {
        Source::Github { .. } => {
            let spec = request.source.display();
            let status = ctx
                .herdr
                .install_github(&spec, request.reference.as_deref(), ctx.yes)?;
            registry::log_event(Event {
                unix: util::now_unix(),
                action: "install".into(),
                plugin_id: spec.clone(),
                source: Some(spec.clone()),
                from: None,
                to: request.reference.clone(),
                ok: status.success(),
                detail: Some("via herdr plugin install".into()),
            });
            if !status.success() {
                bail!("herdr plugin install {spec} did not complete ({status})");
            }
            if request.enabled == Some(false) {
                // Herdr registers installs as enabled; honour --disabled.
                if let Some(row) = rows(&ctx.herdr)?.into_iter().find(|r| {
                    r.origin == Origin::Github
                        && r.plugin.source.github_spec().as_deref() == Some(spec.as_str())
                }) {
                    ctx.herdr.set_enabled(&row.plugin.plugin_id, false)?;
                }
            }
            Ok(true)
        }
        Source::Git { url, subdir } => install_git(
            ctx,
            GitRequest {
                url,
                subdir,
                checkout_ref: request.reference,
                follow_ref: None,
                mode: Mode::Install,
                enabled: request.enabled,
                replace: request.replace,
                expect_id: None,
            },
        ),
    }
}

fn manifest_not_found(found: &[String], wanted: &str, url: &str) -> anyhow::Error {
    if found.is_empty() {
        return anyhow!("{url} contains no {} at that revision", manifest::FILE_NAME);
    }
    let dirs: Vec<String> = found
        .iter()
        .map(|p| {
            p.strip_suffix(manifest::FILE_NAME)
                .unwrap_or(p)
                .trim_end_matches('/')
                .to_string()
        })
        .map(|d| {
            if d.is_empty() {
                "(root)".to_string()
            } else {
                d
            }
        })
        .collect();
    anyhow!(
        "no {wanted} in {url}; plugins found in: {}. Pick one with --subdir <dir> or {url}//<dir>",
        dirs.join(", ")
    )
}

/// Installs, updates or switches a plugin from a git remote:
/// fetch → resolve → read manifest → preview → confirm → checkout into a
/// fresh directory → build → re-check manifest → link → record → clean up.
/// Earlier installations stay on disk (running processes keep their files
/// and `rollback` can return to them).
pub fn install_git(ctx: &Ctx, request: GitRequest) -> Result<bool> {
    let git = Git::FOREGROUND;
    let herdr_version = ctx.herdr.version().ok();
    let subdir = request
        .subdir
        .as_deref()
        .map(source::normalize_subdir)
        .filter(|s| !s.is_empty());
    if let Some(s) = subdir.as_deref() {
        source::validate_subdir(s)?;
    }

    step(&format!("fetching {}", util::sanitize(&request.url)));
    let mirror = git.sync(&request.url)?;
    let sha = git.resolve(&mirror, request.checkout_ref.as_deref())?;
    let mut subdir = subdir;
    let mut manifest_rel = manifest_path(subdir.as_deref());
    let text = match git.show_file(&mirror, &sha, &manifest_rel)? {
        Some(text) => text,
        None => {
            let found = git.find_manifests(&mirror, &sha).unwrap_or_default();
            // A repository with exactly one plugin somewhere inside: use it.
            match (subdir.is_none(), found.as_slice()) {
                (true, [only]) => {
                    let dir = only
                        .strip_suffix(manifest::FILE_NAME)
                        .unwrap_or("")
                        .trim_end_matches('/')
                        .to_string();
                    step(&format!("using the only plugin in the repository: {dir}"));
                    manifest_rel = only.clone();
                    subdir = (!dir.is_empty()).then_some(dir);
                    git.show_file(&mirror, &sha, &manifest_rel)?
                        .ok_or_else(|| anyhow!("could not read {manifest_rel}"))?
                }
                _ => return Err(manifest_not_found(&found, &manifest_rel, &request.url)),
            }
        }
    };
    let source_display = match &subdir {
        Some(s) => format!("{}//{s}", request.url),
        None => request.url.clone(),
    };
    let manifest = Manifest::parse(&text)?;
    let warnings = manifest.validate(herdr_version.as_ref())?;
    let id = manifest.id.trim().to_string();
    if let Some(expected) = &request.expect_id {
        if &id != expected {
            bail!(
                "{source_display} now contains plugin {id}, expected {expected}; refusing to {}",
                request.mode.verb()
            );
        }
    }

    let installed = ctx.herdr.list()?;
    let existing = installed.iter().find(|p| p.plugin_id == id).cloned();
    let record = Registry::load()?.get(&id).cloned();
    let record_live = match (&record, &existing) {
        (Some(r), Some(p)) => !p.is_github() && same_path(&r.current.plugin_root, &p.plugin_root),
        _ => false,
    };

    if let Some(plugin) = &existing {
        if !record_live && !request.replace {
            if plugin.is_github() {
                bail!(
                    "{id} is already installed from GitHub ({}) by herdr. Re-run with --replace to manage it from {source_display} instead.",
                    plugin.source.github_spec().unwrap_or_default()
                );
            }
            bail!(
                "{id} is already linked from {}. Re-run with --replace to replace that link with {source_display}.",
                plugin.plugin_root
            );
        }
        if record_live && request.mode == Mode::Install && !request.replace {
            let r = record.as_ref().unwrap();
            if r.url != request.url || r.subdir != subdir {
                bail!(
                    "{id} is already installed from {}. Re-run with --replace to switch it to {source_display}.",
                    r.source_display()
                );
            }
        }
    }

    let follow = request.follow_ref.clone().unwrap_or_else(|| {
        request.checkout_ref.as_ref().map(|reference| {
            if util::looks_like_sha(reference) {
                sha.clone()
            } else {
                reference.clone()
            }
        })
    });

    if let (true, Some(r)) = (record_live, &record) {
        if r.current.commit == sha && r.url == request.url && r.subdir == subdir {
            if r.requested_ref != follow {
                let mut locked = Registry::lock()?;
                if let Some(rec) = locked.registry.plugins.get_mut(&id) {
                    rec.requested_ref = follow.clone();
                    rec.current.requested_ref = follow.clone();
                }
                if !ctx.dry_run {
                    locked.save()?;
                }
                println!(
                    "{id} is already at {}; it now follows {}.",
                    util::short_sha(&sha),
                    follow.as_deref().unwrap_or("the default branch")
                );
                return Ok(true);
            }
            println!(
                "{} {id} is already up to date ({}).",
                green("✓"),
                util::short_sha(&sha)
            );
            return Ok(false);
        }
    }

    // ---- preview
    let commit = git.commit_info(&mirror, &sha).ok();
    let ref_label = match &request.checkout_ref {
        Some(r) => format!("{r} ({})", git.ref_kind(&mirror, Some(r))),
        None => "default branch".to_string(),
    };
    let updating = record_live;
    eprintln!();
    eprintln!(
        "{}",
        bold(&format!(
            "Plugin {} preview",
            if updating {
                request.mode.verb()
            } else {
                "install"
            }
        ))
    );
    eprintln!("  source    {}", util::sanitize(&source_display));
    eprintln!(
        "  ref       {} → {}",
        util::sanitize(&ref_label),
        bold(util::short_sha(&sha))
    );
    if let Some(c) = &commit {
        eprintln!(
            "  commit    {}  {}",
            dim(&format!("{} · {}", c.author, util::format_unix(c.time))),
            util::truncate(&c.subject, 60)
        );
    }
    if follow != request.checkout_ref {
        eprintln!(
            "  follows   {}",
            util::sanitize(follow.as_deref().unwrap_or("default branch"))
        );
    }
    for line in manifest.preview_lines() {
        eprintln!("{line}");
    }
    if let (true, Some(r)) = (updating, &record) {
        let old_text =
            std::fs::read_to_string(Path::new(&r.current.plugin_root).join(manifest::FILE_NAME))
                .ok();
        let old_manifest = old_text.as_deref().and_then(|t| Manifest::parse(t).ok());
        eprintln!();
        eprintln!(
            "  {} {} ({}) → {} ({})",
            bold("change"),
            util::sanitize(&r.current.version),
            util::short_sha(&r.current.commit),
            util::sanitize(&manifest.version),
            util::short_sha(&sha)
        );
        if r.url != request.url || r.subdir != subdir {
            eprintln!(
                "  {} {} → {}",
                yellow("source changes:"),
                util::sanitize(&r.source_display()),
                util::sanitize(&source_display)
            );
        }
        match git.count_range(&mirror, &r.current.commit, &sha) {
            Some(0) => eprintln!("  {}", yellow("this goes back to an older commit")),
            Some(n) => {
                eprintln!("  {n} new commit{}:", if n == 1 { "" } else { "s" });
                for line in git.log_range(&mirror, &r.current.commit, &sha, 12) {
                    eprintln!("    {line}");
                }
                if n > 12 {
                    eprintln!("    {}", dim(&format!("… and {} more", n - 12)));
                }
            }
            None => eprintln!("  {}", yellow("history diverged (force-push?)")),
        }
        if let Some(old) = old_manifest {
            let diff = manifest::diff_runnables(&old, &manifest);
            if diff.is_empty() {
                eprintln!("  {}", green("what it runs is unchanged"));
            } else {
                eprintln!("  {}", yellow("what it runs changed:"));
                for line in diff {
                    eprintln!("{}", yellow(&line));
                }
            }
        }
    }
    for warning in &warnings {
        eprintln!("  {} {warning}", yellow("warning:"));
    }
    if let Some(plugin) = &existing {
        if !record_live {
            let what = if plugin.is_github() {
                format!(
                    "GitHub install {}",
                    plugin.source.github_spec().unwrap_or_default()
                )
            } else {
                format!("local link {}", plugin.plugin_root)
            };
            eprintln!(
                "  {} {what}",
                yellow("replaces"),
                what = util::sanitize(&what)
            );
        }
    }
    eprintln!();

    let prompt = if updating {
        format!("{} {id}?", capitalize(request.mode.verb()))
    } else {
        format!("Install {id}?")
    };
    if !ctx.yes {
        if !util::stdin_is_tty() {
            bail!(
                "refusing to {} without confirmation: stdin is not a terminal (pass --yes)",
                request.mode.verb()
            );
        }
        if !util::confirm(&prompt)? {
            eprintln!("cancelled");
            return Ok(false);
        }
    }
    if ctx.dry_run {
        eprintln!(
            "[dry-run] would check out {}, run {} build step(s) and link {id}",
            util::short_sha(&sha),
            manifest.build_steps().len()
        );
        return Ok(false);
    }

    // ---- materialise
    let install_dir = new_install_dir(&id, &sha)?;
    let result = (|| -> Result<PathBuf> {
        let checkout = install_dir.join("src");
        step(&format!("checking out {}", util::short_sha(&sha)));
        git.checkout(&mirror, &sha, &checkout)?;
        let plugin_root = match &subdir {
            Some(s) => checkout.join(s),
            None => checkout.clone(),
        };
        let manifest_file = plugin_root.join(manifest::FILE_NAME);
        let on_disk = std::fs::read_to_string(&manifest_file)
            .with_context(|| format!("checkout has no {}", manifest_file.display()))?;
        if on_disk != text {
            bail!("checked-out manifest differs from the previewed one; aborting");
        }
        run_builds(&manifest, &plugin_root)?;
        let after = std::fs::read_to_string(&manifest_file).unwrap_or_default();
        if after != text {
            bail!(
                "a build step changed {} after the preview; aborting",
                manifest::FILE_NAME
            );
        }
        Ok(plugin_root)
    })();
    let plugin_root = match result {
        Ok(root) => root,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&install_dir);
            log_change(
                &request,
                &id,
                None,
                Some(&sha),
                false,
                Some(err.to_string()),
            );
            return Err(err.context(format!("{id} was not {}", past(request.mode))));
        }
    };

    // ---- activate
    let enabled = request
        .enabled
        .or(existing.as_ref().map(|p| p.enabled))
        .unwrap_or(true);
    step(&format!("linking {}", plugin_root.display()));
    if let Err(err) = ctx.herdr.link(&plugin_root, enabled) {
        let _ = std::fs::remove_dir_all(&install_dir);
        log_change(
            &request,
            &id,
            None,
            Some(&sha),
            false,
            Some(err.to_string()),
        );
        return Err(err.context(format!("herdr refused to link {id}")));
    }

    let now = util::now_unix();
    let installation = Installation {
        commit: sha.clone(),
        version: manifest.version.clone(),
        plugin_root: plugin_root.display().to_string(),
        install_dir: install_dir.display().to_string(),
        requested_ref: follow.clone(),
        installed_unix: now,
    };
    let previous_commit;
    {
        let mut locked = Registry::lock()?;
        let old = locked.registry.plugins.remove(&id);
        previous_commit = old.as_ref().map(|o| o.current.commit.clone());
        let mut previous = Vec::new();
        let mut first = now;
        if let Some(old) = old {
            first = old.first_installed_unix;
            // Only keep history from the same plugin lineage.
            previous.push(old.current);
            previous.extend(old.previous);
        }
        previous.retain(|p| p.install_dir != installation.install_dir);
        previous.truncate(ctx.config.keep_installs);
        locked.registry.plugins.insert(
            id.clone(),
            Record {
                id: id.clone(),
                name: manifest.name.clone(),
                url: request.url.clone(),
                subdir: subdir.clone(),
                requested_ref: follow.clone(),
                current: installation,
                previous,
                first_installed_unix: first,
            },
        );
        locked.save()?;
    }
    log_change(
        &request,
        &id,
        previous_commit.as_deref(),
        Some(&sha),
        true,
        None,
    );
    cleanup_installs(&id);

    println!(
        "{} {} {id} {} ({}) from {}.",
        green("✓"),
        capitalize(past(request.mode)),
        util::sanitize(&manifest.version),
        util::short_sha(&sha),
        util::sanitize(&source_display),
    );
    println!(
        "  config: {}",
        paths::herdr_plugin_config_dir(&id).display()
    );
    if !enabled {
        println!(
            "  {} it is disabled; enable it with herdr-pm enable {id}",
            dim("note:")
        );
    }
    Ok(true)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn past(mode: Mode) -> &'static str {
    match mode {
        Mode::Install | Mode::Import => "installed",
        Mode::Update => "updated",
        Mode::Switch => "switched",
    }
}

fn log_change(
    request: &GitRequest,
    id: &str,
    from: Option<&str>,
    to: Option<&str>,
    ok: bool,
    detail: Option<String>,
) {
    registry::log_event(Event {
        unix: util::now_unix(),
        action: request.mode.verb().to_string(),
        plugin_id: id.to_string(),
        source: Some(request.url.clone()),
        from: from.map(str::to_string),
        to: to.map(str::to_string),
        ok,
        detail,
    });
}

// ---------------------------------------------------------------- builds

/// Herdr's argv resolution: a relative program containing a separator is
/// relative to the plugin directory, never to our own cwd.
fn program_for_cwd(program: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(program);
    let has_sep = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if path.is_relative() && has_sep {
        cwd.join(path.strip_prefix(".").unwrap_or(path))
    } else {
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn windows_command(program: &Path) -> Command {
    // Resolve `npm` → `npm.cmd` the way Herdr does, and run batch files
    // through cmd.exe.
    let name = program.as_os_str();
    let has_ext = program.extension().is_some();
    let mut resolved: Option<PathBuf> = None;
    if program.components().count() == 1 {
        let exts: Vec<String> = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .map(|s| s.to_string())
            .collect();
        if let Some(path_var) = std::env::var_os("PATH") {
            'outer: for dir in std::env::split_paths(&path_var) {
                if has_ext {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        resolved = Some(candidate);
                        break;
                    }
                } else {
                    for ext in &exts {
                        let mut file = name.to_os_string();
                        file.push(ext);
                        let candidate = dir.join(file);
                        if candidate.is_file() {
                            resolved = Some(candidate);
                            break 'outer;
                        }
                    }
                }
            }
        }
    }
    let target = resolved.unwrap_or_else(|| program.to_path_buf());
    let is_batch = target
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if is_batch {
        let mut c = Command::new(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()));
        c.arg("/d").arg("/c").arg(target);
        c
    } else {
        Command::new(target)
    }
}

pub fn build_command(argv: &[String], cwd: &Path) -> Result<Command> {
    let Some(program) = argv.first() else {
        bail!("empty command");
    };
    let program = program_for_cwd(program, cwd);
    #[cfg(windows)]
    let mut command = windows_command(&program);
    #[cfg(not(windows))]
    let mut command = Command::new(&program);
    command.args(&argv[1..]).current_dir(cwd);
    // Build steps get no Herdr runtime context, exactly like Herdr's own
    // installer: no socket, no session, no plugin variables.
    for (key, _) in std::env::vars_os() {
        let key_str = key.to_string_lossy();
        if key_str.starts_with("HERDR_") && key_str != "HERDR_CONFIG_PATH" {
            command.env_remove(&key);
        }
    }
    Ok(command)
}

fn run_builds(manifest: &Manifest, root: &Path) -> Result<()> {
    let steps = manifest.build_steps();
    let total = steps.len();
    for (index, entry) in steps.into_iter().enumerate() {
        let shown = manifest::render_argv(&entry.command);
        step(&format!("build {}/{total}: {shown}", index + 1));
        let mut command = build_command(&entry.command, root)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command
            .status()
            .with_context(|| format!("build {}/{total} could not start: {shown}", index + 1))?;
        if !status.success() {
            bail!("build {}/{total} failed ({status}): {shown}", index + 1);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- disk layout

fn plugin_installs_root(id: &str) -> PathBuf {
    paths::installs_dir().join(format!(
        "{}-{}",
        util::slug(id, 48),
        &util::fnv1a64(id)[..8]
    ))
}

fn new_install_dir(id: &str, sha: &str) -> Result<PathBuf> {
    let root = plugin_installs_root(id);
    std::fs::create_dir_all(&root)?;
    for n in 0u32.. {
        let dir = root.join(format!(
            "{}-{}-{}",
            util::short_sha(sha),
            util::now_unix(),
            n
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    unreachable!()
}

/// Moves installations nobody references into the trash, and empties trash
/// older than a day (giving running plugin processes time to finish).
pub fn cleanup_installs(id: &str) {
    let registry = match Registry::load() {
        Ok(registry) => registry,
        Err(_) => return,
    };
    let referenced: Vec<PathBuf> = registry
        .get(id)
        .cloned()
        .map(|r| {
            std::iter::once(&r.current)
                .chain(r.previous.iter())
                .map(|i| PathBuf::from(&i.install_dir))
                .collect()
        })
        .unwrap_or_default();
    let root = plugin_installs_root(id);
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !referenced.iter().any(|r| r == &path) {
                trash(&path);
            }
        }
    }
    if referenced.is_empty() {
        let _ = std::fs::remove_dir(&root);
    }
    purge_trash();
}

fn trash(path: &Path) {
    let dir = paths::trash_dir();
    let _ = std::fs::create_dir_all(&dir);
    let name = format!(
        "{}-{}",
        util::now_unix(),
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    );
    if std::fs::rename(path, dir.join(name)).is_err() {
        let _ = std::fs::remove_dir_all(path);
    }
}

fn purge_trash() {
    let cutoff = util::now_unix().saturating_sub(86_400);
    if let Ok(entries) = std::fs::read_dir(paths::trash_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let ts: u64 = name
                .split('-')
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            if ts < cutoff {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

// ---------------------------------------------------------------- update

pub fn find_row<'a>(rows: &'a [Row], target: &str) -> Option<&'a Row> {
    rows.iter()
        .find(|r| r.plugin.plugin_id == target)
        .or_else(|| rows.iter().find(|r| r.source_display() == target))
}

/// Updates the named plugins (all GitHub- and git-managed ones when empty).
/// Returns the number of failures.
pub fn update(ctx: &Ctx, targets: &[String]) -> Result<usize> {
    let rows = rows(&ctx.herdr)?;
    let selected: Vec<&Row> = if targets.is_empty() {
        rows.iter().filter(|r| r.origin != Origin::Local).collect()
    } else {
        let mut out = Vec::new();
        for target in targets {
            let row =
                find_row(&rows, target).ok_or_else(|| anyhow!("plugin not installed: {target}"))?;
            out.push(row);
        }
        out
    };
    if selected.is_empty() {
        println!("Nothing to update: no plugins installed from GitHub or git.");
        return Ok(0);
    }
    let mut failures = 0;
    for row in selected {
        let id = &row.plugin.plugin_id;
        match check(row, Git::FOREGROUND) {
            Status::Current => println!("{} {id} is up to date", green("✓")),
            Status::Pinned => println!(
                "{} {id} is pinned at {} (skipped)",
                dim("•"),
                util::short_sha(row.commit().unwrap_or(""))
            ),
            Status::Local => println!(
                "{} {id} is a local link (update it in {})",
                dim("•"),
                row.plugin.plugin_root
            ),
            Status::Unknown => println!("{} {id}: no source recorded", dim("•")),
            Status::Error { message } => {
                failures += 1;
                println!("{} {id}: {message}", red("✗"));
            }
            Status::Available { commit, .. } => {
                if ctx.dry_run {
                    println!(
                        "{} {id}: {} → {} available (dry run, nothing changed)",
                        yellow("↑"),
                        util::short_sha(row.commit().unwrap_or("")),
                        util::short_sha(&commit)
                    );
                    continue;
                }
                let result = match row.origin {
                    Origin::Git => {
                        let r = row.record.as_ref().unwrap();
                        install_git(
                            ctx,
                            GitRequest {
                                url: r.url.clone(),
                                subdir: r.subdir.clone(),
                                checkout_ref: r.requested_ref.clone(),
                                follow_ref: None,
                                mode: Mode::Update,
                                enabled: None,
                                replace: false,
                                expect_id: Some(id.clone()),
                            },
                        )
                        .map(|_| ())
                    }
                    Origin::Github => ctx.herdr.update_github(id, ctx.yes).and_then(|status| {
                        if status.success() {
                            Ok(())
                        } else {
                            Err(anyhow!(
                                "herdr plugin update {id} did not complete ({status})"
                            ))
                        }
                    }),
                    Origin::Local => Ok(()),
                };
                if let Err(err) = result {
                    failures += 1;
                    eprintln!("{} {id}: {err:#}", red("✗"));
                }
            }
        }
    }
    Ok(failures)
}

// ---------------------------------------------------------------- other changes

fn require_row(ctx: &Ctx, id: &str) -> Result<Row> {
    let rows = rows(&ctx.herdr)?;
    find_row(&rows, id)
        .cloned()
        .ok_or_else(|| anyhow!("plugin not installed: {id}"))
}

pub fn uninstall(ctx: &Ctx, id: &str) -> Result<()> {
    let rows = rows(&ctx.herdr)?;
    let row = find_row(&rows, id).cloned();
    let record = Registry::load()?.get(id).cloned();
    let (plugin_id, what) = match (&row, &record) {
        (Some(row), _) => (
            row.plugin.plugin_id.clone(),
            match row.origin {
                Origin::Git => format!(
                    "unregister it and remove its installations ({})",
                    row.source_display()
                ),
                Origin::Github => {
                    "uninstall it with herdr (herdr removes its GitHub checkout)".into()
                }
                Origin::Local => format!(
                    "unlink it (files in {} stay untouched)",
                    row.plugin.plugin_root
                ),
            },
        ),
        (None, Some(r)) => (
            r.id.clone(),
            "forget its record and installations (herdr no longer lists it)".into(),
        ),
        (None, None) => bail!("plugin not installed: {id}"),
    };
    if !ctx.yes {
        if !util::stdin_is_tty() {
            bail!(
                "refusing to uninstall without confirmation: stdin is not a terminal (pass --yes)"
            );
        }
        if !util::confirm(&format!("Uninstall {plugin_id}: {what}?"))? {
            eprintln!("cancelled");
            return Ok(());
        }
    }
    if ctx.dry_run {
        eprintln!("[dry-run] would {what}");
        return Ok(());
    }
    if row.is_some() {
        ctx.herdr.unregister(&plugin_id)?;
    }
    if row.as_ref().is_none_or(|r| r.origin == Origin::Git) {
        if let Some(record) = record {
            let mut locked = Registry::lock()?;
            locked.registry.plugins.remove(&plugin_id);
            locked.save()?;
            drop(locked);
            for inst in std::iter::once(&record.current).chain(record.previous.iter()) {
                let dir = Path::new(&inst.install_dir);
                if dir.exists() {
                    trash(dir);
                }
            }
            cleanup_installs(&plugin_id);
        }
    }
    registry::log_event(Event {
        unix: util::now_unix(),
        action: "uninstall".into(),
        plugin_id: plugin_id.clone(),
        source: row.as_ref().map(|r| r.source_display()),
        from: row.as_ref().and_then(|r| r.commit().map(str::to_string)),
        to: None,
        ok: true,
        detail: None,
    });
    println!("{} Uninstalled {plugin_id}.", green("✓"));
    Ok(())
}

pub fn set_enabled(ctx: &Ctx, id: &str, enabled: bool) -> Result<()> {
    let row = require_row(ctx, id)?;
    ctx.herdr.set_enabled(&row.plugin.plugin_id, enabled)?;
    println!(
        "{} {} {}",
        green("✓"),
        if enabled { "Enabled" } else { "Disabled" },
        row.plugin.plugin_id
    );
    Ok(())
}

pub fn rollback(ctx: &Ctx, id: &str) -> Result<()> {
    let row = require_row(ctx, id)?;
    if row.origin != Origin::Git {
        bail!(
            "{id} is {}; rollback works for git-managed plugins. Use `herdr-pm switch {id} <ref>` instead.",
            if row.origin == Origin::Github { "managed by herdr's GitHub installer" } else { "a local link" }
        );
    }
    let record = row.record.clone().unwrap();
    let Some(prev) = record
        .previous
        .iter()
        .find(|p| Path::new(&p.plugin_root).join(manifest::FILE_NAME).exists())
        .cloned()
    else {
        bail!("no earlier installation of {id} is kept on disk");
    };
    eprintln!(
        "Roll back {id}: {} ({}) → {} ({}, installed {})",
        record.current.version,
        util::short_sha(&record.current.commit),
        prev.version,
        util::short_sha(&prev.commit),
        util::format_unix(prev.installed_unix)
    );
    if let Ok(text) =
        std::fs::read_to_string(Path::new(&prev.plugin_root).join(manifest::FILE_NAME))
    {
        if let (Ok(old), Ok(cur)) = (
            Manifest::parse(&text),
            std::fs::read_to_string(
                Path::new(&record.current.plugin_root).join(manifest::FILE_NAME),
            )
            .map_err(anyhow::Error::from)
            .and_then(|t| Manifest::parse(&t)),
        ) {
            for line in manifest::diff_runnables(&cur, &old) {
                eprintln!("{}", yellow(&line));
            }
        }
    }
    if !ctx.yes {
        if !util::stdin_is_tty() {
            bail!(
                "refusing to roll back without confirmation: stdin is not a terminal (pass --yes)"
            );
        }
        if !util::confirm(&format!("Roll back {id}?"))? {
            eprintln!("cancelled");
            return Ok(());
        }
    }
    if ctx.dry_run {
        eprintln!("[dry-run] would link {}", prev.plugin_root);
        return Ok(());
    }
    ctx.herdr
        .link(Path::new(&prev.plugin_root), row.plugin.enabled)?;
    {
        let mut locked = Registry::lock()?;
        if let Some(rec) = locked.registry.plugins.get_mut(id) {
            let current = rec.current.clone();
            rec.previous.retain(|p| p.install_dir != prev.install_dir);
            rec.previous.insert(0, current);
            rec.previous.truncate(ctx.config.keep_installs.max(1));
            rec.current = prev.clone();
            rec.requested_ref = prev.requested_ref.clone();
        }
        locked.save()?;
    }
    registry::log_event(Event {
        unix: util::now_unix(),
        action: "rollback".into(),
        plugin_id: id.to_string(),
        source: Some(record.source_display()),
        from: Some(record.current.commit.clone()),
        to: Some(prev.commit.clone()),
        ok: true,
        detail: None,
    });
    println!(
        "{} Rolled back {id} to {} ({}).",
        green("✓"),
        prev.version,
        util::short_sha(&prev.commit)
    );
    if !util::is_full_sha(prev.requested_ref.as_deref().unwrap_or("")) {
        println!(
            "  {} updates will be offered again; `herdr-pm pin {id}` holds this version.",
            dim("note:")
        );
    }
    Ok(())
}

/// Holds a plugin at its current commit (or at `reference`, resolved).
pub fn pin(ctx: &Ctx, id: &str, reference: Option<&str>) -> Result<()> {
    let row = require_row(ctx, id)?;
    match row.origin {
        Origin::Local => bail!("{id} is a local link; there is nothing to pin"),
        Origin::Git => {
            let record = row.record.clone().unwrap();
            match reference {
                None => {
                    let mut locked = Registry::lock()?;
                    if let Some(rec) = locked.registry.plugins.get_mut(id) {
                        rec.requested_ref = Some(rec.current.commit.clone());
                        rec.current.requested_ref = rec.requested_ref.clone();
                    }
                    if !ctx.dry_run {
                        locked.save()?;
                    }
                    println!(
                        "{} Pinned {id} at {}.",
                        green("✓"),
                        util::short_sha(&record.current.commit)
                    );
                    Ok(())
                }
                Some(r) => {
                    let git = Git::FOREGROUND;
                    let mirror = git.sync(&record.url)?;
                    let sha = git.resolve(&mirror, Some(r))?;
                    install_git(
                        ctx,
                        GitRequest {
                            url: record.url,
                            subdir: record.subdir,
                            checkout_ref: Some(sha.clone()),
                            follow_ref: Some(Some(sha)),
                            mode: Mode::Switch,
                            enabled: None,
                            replace: false,
                            expect_id: Some(id.to_string()),
                        },
                    )
                    .map(|_| ())
                }
            }
        }
        Origin::Github => {
            let spec = row.plugin.source.github_spec().unwrap_or_default();
            let sha = match reference {
                None => row
                    .commit()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("{id} has no recorded commit"))?,
                Some(r) => {
                    let url = format!(
                        "https://github.com/{}.git",
                        row.plugin.source.github_slug().unwrap_or_default()
                    );
                    Git::FOREGROUND.ls_remote(&url, Some(r))?
                }
            };
            let status = ctx.herdr.install_github(&spec, Some(&sha), ctx.yes)?;
            if !status.success() {
                bail!("herdr plugin install did not complete ({status})");
            }
            Ok(())
        }
    }
}

/// Follows a branch or tag again (the default branch when None).
pub fn unpin(ctx: &Ctx, id: &str, reference: Option<&str>) -> Result<()> {
    let row = require_row(ctx, id)?;
    match row.origin {
        Origin::Local => bail!("{id} is a local link; there is nothing to unpin"),
        Origin::Git => {
            let mut locked = Registry::lock()?;
            if let Some(rec) = locked.registry.plugins.get_mut(id) {
                rec.requested_ref = reference.map(str::to_string);
            }
            if !ctx.dry_run {
                locked.save()?;
            }
            println!(
                "{} {id} now follows {}; `herdr-pm update {id}` applies it.",
                green("✓"),
                reference.unwrap_or("the default branch")
            );
            Ok(())
        }
        Origin::Github => {
            let spec = row.plugin.source.github_spec().unwrap_or_default();
            let status = ctx.herdr.install_github(&spec, reference, ctx.yes)?;
            if !status.success() {
                bail!("herdr plugin install did not complete ({status})");
            }
            Ok(())
        }
    }
}

/// Installs another branch, tag or commit and follows it from then on.
pub fn switch(ctx: &Ctx, id: &str, reference: &str) -> Result<()> {
    let row = require_row(ctx, id)?;
    match row.origin {
        Origin::Local => bail!(
            "{id} is a local link; switch branches in {}",
            row.plugin.plugin_root
        ),
        Origin::Git => {
            let record = row.record.unwrap();
            install_git(
                ctx,
                GitRequest {
                    url: record.url,
                    subdir: record.subdir,
                    checkout_ref: Some(reference.to_string()),
                    follow_ref: None,
                    mode: Mode::Switch,
                    enabled: None,
                    replace: false,
                    expect_id: Some(id.to_string()),
                },
            )
            .map(|_| ())
        }
        Origin::Github => {
            let spec = row.plugin.source.github_spec().unwrap_or_default();
            let status = ctx.herdr.install_github(&spec, Some(reference), ctx.yes)?;
            if !status.success() {
                bail!("herdr plugin install did not complete ({status})");
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- export / import

pub const EXPORT_FORMAT: &str = "gitbridge-export";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportEntry {
    pub id: String,
    /// `github` or `git`.
    pub kind: String,
    /// GitHub `owner/repo` or a git URL.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub follow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportFile {
    pub format: String,
    pub version: u32,
    pub exported_unix: u64,
    pub plugins: Vec<ExportEntry>,
}

pub fn export(herdr: &Herdr) -> Result<(ExportFile, Vec<String>)> {
    let rows = rows(herdr)?;
    let mut plugins = Vec::new();
    let mut skipped = Vec::new();
    for row in rows {
        match row.origin {
            Origin::Local => skipped.push(format!(
                "{} (local link {})",
                row.plugin.plugin_id, row.plugin.plugin_root
            )),
            Origin::Github => plugins.push(ExportEntry {
                id: row.plugin.plugin_id.clone(),
                kind: "github".into(),
                source: row.plugin.source.github_slug().unwrap_or_default(),
                subdir: row.plugin.source.subdir.clone(),
                follow_ref: row.plugin.source.requested_ref.clone(),
                commit: row.plugin.source.resolved_commit.clone(),
                enabled: row.plugin.enabled,
            }),
            Origin::Git => {
                let r = row.record.as_ref().unwrap();
                plugins.push(ExportEntry {
                    id: r.id.clone(),
                    kind: "git".into(),
                    source: r.url.clone(),
                    subdir: r.subdir.clone(),
                    follow_ref: r.requested_ref.clone(),
                    commit: Some(r.current.commit.clone()),
                    enabled: row.plugin.enabled,
                })
            }
        }
    }
    Ok((
        ExportFile {
            format: EXPORT_FORMAT.into(),
            version: 1,
            exported_unix: util::now_unix(),
            plugins,
        },
        skipped,
    ))
}

/// Installs an export as exported: git plugins at the exported commit, then
/// following the exported ref; GitHub plugins through Herdr. Returns the
/// number of failures.
pub fn import(ctx: &Ctx, file: &ExportFile, only: &[String]) -> Result<usize> {
    if file.format != EXPORT_FORMAT {
        bail!("not a GitBridge export (format {:?})", file.format);
    }
    let rows = rows(&ctx.herdr)?;
    let mut failures = 0;
    for entry in &file.plugins {
        if !only.is_empty() && !only.contains(&entry.id) {
            continue;
        }
        let current = rows.iter().find(|r| r.plugin.plugin_id == entry.id);
        let same = current.is_some_and(|row| match (row.origin, entry.kind.as_str()) {
            (Origin::Git, "git") => {
                let r = row.record.as_ref().unwrap();
                r.url == entry.source
                    && r.subdir == entry.subdir
                    && Some(&r.current.commit) == entry.commit.as_ref()
            }
            (Origin::Github, "github") => {
                row.plugin.source.github_slug().as_deref() == Some(entry.source.as_str())
                    && row.plugin.source.subdir == entry.subdir
                    && row.plugin.source.resolved_commit == entry.commit
            }
            _ => false,
        });
        if same {
            let row = current.unwrap();
            if row.plugin.enabled != entry.enabled && !ctx.dry_run {
                ctx.herdr.set_enabled(&entry.id, entry.enabled)?;
                println!(
                    "{} {}: {}",
                    green("✓"),
                    entry.id,
                    if entry.enabled { "enabled" } else { "disabled" }
                );
            } else {
                println!("{} {} unchanged", dim("•"), entry.id);
            }
            continue;
        }
        let result = match entry.kind.as_str() {
            "git" => (|| -> Result<()> {
                let parsed = source::parse(&entry.source, &ctx.config.aliases)?
                    .with_subdir(entry.subdir.clone());
                let Source::Git { url, subdir } = parsed else {
                    bail!(
                        "git import entry {} does not contain a git source",
                        entry.id
                    );
                };
                install_git(
                    ctx,
                    GitRequest {
                        url,
                        subdir,
                        checkout_ref: entry.commit.clone().or_else(|| entry.follow_ref.clone()),
                        follow_ref: Some(entry.follow_ref.clone()),
                        mode: Mode::Import,
                        enabled: Some(entry.enabled),
                        replace: false,
                        expect_id: Some(entry.id.clone()),
                    },
                )
                .map(|_| ())
            })(),
            "github" => (|| -> Result<()> {
                let spec = match &entry.subdir {
                    Some(s) => format!("{}/{s}", entry.source),
                    None => entry.source.clone(),
                };
                install(
                    ctx,
                    InstallRequest {
                        source: source::parse(&format!("github:{spec}"), &Default::default())?,
                        // Reproduce the exported state exactly; a following
                        // ref is only a fallback for older/incomplete exports.
                        reference: entry.commit.clone().or_else(|| entry.follow_ref.clone()),
                        enabled: Some(entry.enabled),
                        replace: false,
                    },
                )
                .map(|_| ())
            })(),
            other => Err(anyhow!("unknown kind {other:?}")),
        };
        if let Err(err) = result {
            failures += 1;
            eprintln!("{} {}: {err:#}", red("✗"), entry.id);
        }
    }
    Ok(failures)
}

// ---------------------------------------------------------------- doctor

pub fn doctor(ctx: &Ctx) -> usize {
    let mut problems = 0;
    let ok = |label: &str, text: String| println!("{} {label:<14} {text}", green("✓"));
    let mut bad = |label: &str, text: String| {
        problems += 1;
        println!("{} {label:<14} {text}", red("✗"));
    };
    let warn = |label: &str, text: String| println!("{} {label:<14} {text}", yellow("!"));

    match ctx.herdr.version() {
        Ok(v) => ok("herdr", format!("{v} ({})", ctx.herdr.bin.display())),
        Err(err) => bad("herdr", format!("{err:#}")),
    }
    match Git::BACKGROUND.version() {
        Ok(v) => ok("git", v),
        Err(err) => bad("git", format!("{err:#}")),
    }
    let cfg_path = Config::path();
    match Config::load() {
        Ok(c) => ok(
            "config",
            format!(
                "{}{} · {} marketplace source(s), {} alias(es)",
                cfg_path.display(),
                if cfg_path.exists() {
                    ""
                } else {
                    " (defaults; `herdr-pm config init` writes one)"
                },
                c.marketplace.len(),
                c.aliases.len()
            ),
        ),
        Err(err) => bad("config", format!("{err:#}")),
    }
    ok("data", paths::pm_home().display().to_string());
    let rows = match rows(&ctx.herdr) {
        Ok(rows) => {
            ok("plugins", format!("{} registered with herdr", rows.len()));
            rows
        }
        Err(err) => {
            bad("plugins", format!("{err:#}"));
            Vec::new()
        }
    };
    for row in &rows {
        let id = &row.plugin.plugin_id;
        if !Path::new(&row.plugin.manifest_path).exists() {
            bad(
                id,
                format!("manifest missing: {}", row.plugin.manifest_path),
            );
        }
        for w in &row.plugin.warnings {
            warn(id, w.clone());
        }
        if let Some(platforms) = &row.plugin.platforms {
            if !platforms.iter().any(|p| p == util::current_platform()) {
                warn(id, format!("declares {} only", platforms.join(", ")));
            }
        }
    }
    if let Ok(registry) = Registry::load() {
        for (id, record) in &registry.plugins {
            match rows.iter().find(|r| &r.plugin.plugin_id == id) {
                None => warn(id, format!("recorded from {} but herdr no longer lists it (`herdr-pm uninstall {id}` forgets it)", record.url)),
                Some(row) if row.origin != Origin::Git => warn(
                    id,
                    format!("herdr now points at {} instead of the managed install; reinstall with --replace to manage it again", row.plugin.plugin_root),
                ),
                _ => {}
            }
        }
    }
    let bindings = crate::herdr::action_keybindings();
    for (action, keys) in &bindings {
        let known = rows.iter().any(|r| {
            r.plugin.enabled
                && r.plugin
                    .actions
                    .iter()
                    .any(|a| format!("{}.{}", r.plugin.plugin_id, a.id) == *action)
        });
        if !known {
            warn(
                "keys",
                format!(
                    "{} → {action}: no enabled plugin declares that action",
                    keys.join(", ")
                ),
            );
        }
    }
    if let Ok(config) = Config::load() {
        for source in config.marketplace.iter().filter(|s| s.enabled) {
            match crate::market::fetch(source, "", true, &config) {
                Ok(list) => ok(
                    &format!("market:{}", source.name),
                    format!("{} plugin(s) listed", list.entries.len()),
                ),
                Err(err) => bad(
                    &format!("market:{}", source.name),
                    first_line(&format!("{err:#}")),
                ),
            }
        }
    }
    problems
}

/// For `info`: everything known about one plugin.
pub fn describe(row: &Row) -> Vec<(String, String)> {
    let p = &row.plugin;
    let mut out = vec![
        ("id".into(), p.plugin_id.clone()),
        ("name".into(), p.name.clone()),
        ("version".into(), p.version.clone()),
        ("enabled".into(), p.enabled.to_string()),
        ("origin".into(), row.origin.label().into()),
        ("source".into(), row.source_display()),
        ("ref".into(), row.ref_display()),
    ];
    if let Some(c) = row.commit() {
        out.push(("commit".into(), c.to_string()));
    }
    out.push(("root".into(), p.plugin_root.clone()));
    out.push((
        "config".into(),
        paths::herdr_plugin_config_dir(&p.plugin_id)
            .display()
            .to_string(),
    ));
    if let Some(d) = &p.description {
        out.push(("about".into(), util::sanitize(d)));
    }
    if let Some(pl) = &p.platforms {
        out.push(("platforms".into(), pl.join(", ")));
    }
    out.push((
        "requires".into(),
        format!("herdr >= {}", p.min_herdr_version),
    ));
    if let Some(r) = &row.record {
        out.push((
            "installed".into(),
            util::format_unix(r.current.installed_unix),
        ));
        for (i, prev) in r.previous.iter().enumerate() {
            out.push((
                if i == 0 {
                    "previous".into()
                } else {
                    String::new()
                },
                format!(
                    "{} ({}) {}",
                    prev.version,
                    util::short_sha(&prev.commit),
                    util::format_unix(prev.installed_unix)
                ),
            ));
        }
    }
    for a in &p.actions {
        out.push((
            "action".into(),
            format!("{}.{} — {}", p.plugin_id, a.id, a.title),
        ));
    }
    for w in &p.warnings {
        out.push(("warning".into(), w.clone()));
    }
    out
}
