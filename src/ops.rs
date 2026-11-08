//! Everything that changes installed plugins. The CLI calls these directly
//! and reports on the terminal; the popup runs the same functions on a
//! worker thread with its own `Report`, so both share one trust preview,
//! one build runner and one registry.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::git::{Git, Mirror};
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
    pub report: Arc<dyn Report>,
}

impl Ctx {
    fn step(&self, text: &str) {
        self.report.step(text);
    }

    fn line(&self, text: impl AsRef<str>) {
        self.report.line(text.as_ref());
    }

    /// Git that prompts for credentials only when someone can answer, and
    /// waits as long as a user-started operation needs.
    fn git(&self) -> Git {
        if self.report.interactive() {
            Git::FOREGROUND
        } else {
            Git::UNATTENDED
        }
    }
}

/// Where operations report progress and ask for approval.
pub trait Report: Send + Sync {
    /// A stage of the operation ("fetching …", "build 1/2: …").
    fn step(&self, text: &str);
    /// Everything else: results, notes, command output.
    fn line(&self, text: &str);
    /// Shows a trust preview. With a `question` it waits for approval
    /// (false = declined); without one it only shows the preview.
    fn review(&self, title: &str, lines: &[String], question: Option<&str>) -> Result<bool>;
    /// Position in a batch: plugin `done` of `total`.
    fn progress(&self, _done: usize, _total: usize) {}
    /// Whether tools may use the terminal themselves (git credential
    /// prompts, Herdr's own install preview).
    fn interactive(&self) -> bool;
}

/// The CLI: stages and previews on stderr, results on stdout, `[y/N]`.
pub struct TerminalReport;

impl Report for TerminalReport {
    fn step(&self, text: &str) {
        eprintln!("{} {text}", cyan("›"));
    }

    fn line(&self, text: &str) {
        println!("{text}");
    }

    fn review(&self, title: &str, lines: &[String], question: Option<&str>) -> Result<bool> {
        eprintln!();
        eprintln!("{}", bold(title));
        for line in lines {
            eprintln!("{line}");
        }
        eprintln!();
        let Some(question) = question else {
            return Ok(true);
        };
        if !util::stdin_is_tty() {
            bail!(
                "refusing to continue without confirmation: stdin is not a terminal (pass --yes)"
            );
        }
        util::confirm(question)
    }

    fn interactive(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------- output

static PLAIN: AtomicBool = AtomicBool::new(false);

/// Turns ANSI colors off: the popup renders operation output itself.
pub fn set_plain() {
    PLAIN.store(true, Ordering::Relaxed);
}

fn color_on() -> bool {
    !PLAIN.load(Ordering::Relaxed)
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stderr().is_terminal()
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
    /// Updated without a preview by `herdr-pm auto-update`.
    pub auto_update: bool,
    /// Commit this manager holds a GitHub-installed plugin at.
    pub pin: Option<String>,
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
        if let Some(pin) = &self.pin {
            return format!("pinned {}", util::short_sha(pin));
        }
        match self.requested_ref() {
            None if self.origin == Origin::Local => "-".into(),
            None => "default branch".into(),
            Some(r) if util::is_full_sha(r) => format!("pinned {}", util::short_sha(r)),
            Some(r) => r.to_string(),
        }
    }

    /// `name@commit`: the followed branch or tag, the default branch when
    /// its name is known (`HEAD` until then), or `@commit (pinned)`.
    pub fn ref_label(&self, default_branch: Option<&str>) -> String {
        let commit = self.commit().map(util::short_sha).unwrap_or("-");
        if self.pinned() {
            return format!("@{commit} (pinned)");
        }
        let name = self.requested_ref().or(default_branch).unwrap_or("HEAD");
        format!("{name}@{commit}")
    }

    pub fn pinned(&self) -> bool {
        self.pin.is_some() || self.requested_ref().is_some_and(util::is_full_sha)
    }

    /// Unpinning needs a reinstall when Herdr itself installed the plugin at
    /// a commit (its recorded ref can only change through `install`).
    pub fn unpin_needs_install(&self) -> bool {
        self.origin == Origin::Github
            && self.pin.is_none()
            && self.requested_ref().is_some_and(util::is_full_sha)
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
            let auto_update = registry.auto_update.contains(&plugin.plugin_id);
            let pin = match origin {
                Origin::Github => registry.pins.get(&plugin.plugin_id).cloned(),
                _ => None,
            };
            Row {
                plugin,
                record,
                origin,
                auto_update,
                pin,
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
            let result = github_install(
                ctx,
                &spec,
                request.reference.as_deref(),
                None,
                Mode::Install,
            );
            if !matches!(result, Ok(false)) {
                registry::log_event(Event {
                    unix: util::now_unix(),
                    action: "install".into(),
                    plugin_id: spec.clone(),
                    source: Some(spec.clone()),
                    from: None,
                    to: request.reference.clone(),
                    ok: result.is_ok(),
                    detail: Some("via herdr plugin install".into()),
                });
            }
            if !result? {
                return Ok(false);
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
    let git = ctx.git();
    let herdr_version = ctx.herdr.version().ok();
    let subdir = request
        .subdir
        .as_deref()
        .map(source::normalize_subdir)
        .filter(|s| !s.is_empty());
    if let Some(s) = subdir.as_deref() {
        source::validate_subdir(s)?;
    }

    ctx.step(&format!("fetching {}", util::sanitize(&request.url)));
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
                    ctx.step(&format!("using the only plugin in the repository: {dir}"));
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
                ctx.line(format!(
                    "{id} is already at {}; it now follows {}.",
                    util::short_sha(&sha),
                    follow.as_deref().unwrap_or("the default branch")
                ));
                return Ok(true);
            }
            ctx.line(format!(
                "{} {id} is already up to date ({}).",
                green("✓"),
                util::short_sha(&sha)
            ));
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
    let mut review = vec![
        format!("  source    {}", util::sanitize(&source_display)),
        format!(
            "  ref       {} → {}",
            util::sanitize(&ref_label),
            bold(util::short_sha(&sha))
        ),
    ];
    if let Some(c) = &commit {
        review.push(commit_line(c));
    }
    if follow != request.checkout_ref {
        review.push(format!(
            "  follows   {}",
            util::sanitize(follow.as_deref().unwrap_or("default branch"))
        ));
    }
    review.extend(manifest.preview_lines());
    if let (true, Some(r)) = (updating, &record) {
        review.extend(change_lines(
            git,
            &mirror,
            (
                &r.current.commit,
                &r.current.version,
                &r.current.plugin_root,
            ),
            &sha,
            &manifest,
        ));
        if r.url != request.url || r.subdir != subdir {
            review.push(format!(
                "  {} {} → {}",
                yellow("source changes:"),
                util::sanitize(&r.source_display()),
                util::sanitize(&source_display)
            ));
        }
    }
    for warning in &warnings {
        review.push(format!("  {} {warning}", yellow("warning:")));
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
            review.push(format!(
                "  {} {what}",
                yellow("replaces"),
                what = util::sanitize(&what)
            ));
        }
    }
    let verb = if updating {
        request.mode.verb()
    } else {
        "install"
    };
    let question = format!("{} {id}?", capitalize(verb));
    let title = format!("Plugin {verb} preview");
    if !ctx
        .report
        .review(&title, &review, (!ctx.yes).then_some(question.as_str()))?
    {
        ctx.line("cancelled");
        return Ok(false);
    }
    if ctx.dry_run {
        ctx.line(format!(
            "[dry-run] would check out {}, run {} build step(s) and link {id}",
            util::short_sha(&sha),
            manifest.build_steps().len()
        ));
        return Ok(false);
    }

    // ---- materialise
    let install_dir = new_install_dir(&id, &sha)?;
    let result = (|| -> Result<PathBuf> {
        let checkout = install_dir.join("src");
        ctx.step(&format!("checking out {}", util::short_sha(&sha)));
        git.checkout(&mirror, &sha, &checkout)?;
        let plugin_root = match &subdir {
            Some(s) => checkout.join(s),
            None => checkout.clone(),
        };
        let manifest_file = plugin_root.join(manifest::FILE_NAME);
        let on_disk = std::fs::read_to_string(&manifest_file)
            .with_context(|| format!("checkout has no {}", manifest_file.display()))?;
        if !same_text(&on_disk, &text) {
            bail!("checked-out manifest differs from the previewed one; aborting");
        }
        run_builds(ctx, &manifest, &plugin_root)?;
        let after = std::fs::read_to_string(&manifest_file).unwrap_or_default();
        if !same_text(&after, &text) {
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
    ctx.step(&format!("linking {}", plugin_root.display()));
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

    ctx.line(format!(
        "{} {} {id} {} ({}) from {}.",
        green("✓"),
        capitalize(past(request.mode)),
        util::sanitize(&manifest.version),
        util::short_sha(&sha),
        util::sanitize(&source_display),
    ));
    ctx.line(format!(
        "  config: {}",
        paths::herdr_plugin_config_dir(&id).display()
    ));
    if !enabled {
        ctx.line(format!(
            "  {} it is disabled; enable it with herdr-pm enable {id}",
            dim("note:")
        ));
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

fn commit_line(c: &crate::git::CommitInfo) -> String {
    format!(
        "  commit    {}  {}",
        dim(&format!("{} · {}", c.author, util::format_unix(c.time))),
        util::truncate(&c.subject, 60)
    )
}

/// The "what changes" part of an update preview: versions, commits in
/// between, and changes to what the plugin runs. `old` is the installed
/// commit, version and plugin root.
fn change_lines(
    git: Git,
    mirror: &Mirror,
    old: (&str, &str, &str),
    sha: &str,
    manifest: &Manifest,
) -> Vec<String> {
    let (old_commit, old_version, old_root) = old;
    let mut lines = vec![
        String::new(),
        format!(
            "  {} {} ({}) → {} ({})",
            bold("change"),
            util::sanitize(old_version),
            util::short_sha(old_commit),
            util::sanitize(&manifest.version),
            util::short_sha(sha)
        ),
    ];
    match git.count_range(mirror, old_commit, sha) {
        Some(0) => lines.push(format!("  {}", yellow("this goes back to an older commit"))),
        Some(n) => {
            lines.push(format!(
                "  {n} new commit{}:",
                if n == 1 { "" } else { "s" }
            ));
            for line in git.log_range(mirror, old_commit, sha, 12) {
                lines.push(format!("    {line}"));
            }
            if n > 12 {
                lines.push(format!("    {}", dim(&format!("… and {} more", n - 12))));
            }
        }
        None => lines.push(format!("  {}", yellow("history diverged (force-push?)"))),
    }
    let old_manifest = std::fs::read_to_string(Path::new(old_root).join(manifest::FILE_NAME))
        .ok()
        .and_then(|t| Manifest::parse(&t).ok());
    if let Some(old) = old_manifest {
        let diff = manifest::diff_runnables(&old, manifest);
        if diff.is_empty() {
            lines.push(format!("  {}", green("what it runs is unchanged")));
        } else {
            lines.push(format!("  {}", yellow("what it runs changed:")));
            lines.extend(diff.into_iter().map(|line| yellow(&line)));
        }
    }
    lines
}

/// Herdr installs GitHub shorthands itself and shows its own preview on
/// the terminal. Reports without a terminal (the popup) get the same review
/// from a mirror of the repository first; Herdr then runs with `--yes`.
/// Returns false when the review is declined. `current` is the installed
/// plugin for updates and version switches.
fn github_review(
    ctx: &Ctx,
    spec: &str,
    reference: Option<&str>,
    current: Option<&Row>,
    mode: Mode,
) -> Result<bool> {
    let mut parts = spec.splitn(3, '/');
    let (owner, repo) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let subdir = parts.next().map(str::to_string);
    let url = format!("https://github.com/{owner}/{repo}.git");
    let git = ctx.git();
    ctx.step(&format!("fetching {url}"));
    let mirror = git.sync(&url)?;
    let sha = git.resolve(&mirror, reference)?;
    let manifest_rel = manifest_path(subdir.as_deref());
    let text = match git.show_file(&mirror, &sha, &manifest_rel)? {
        Some(text) => text,
        None => {
            let found = git.find_manifests(&mirror, &sha).unwrap_or_default();
            return Err(manifest_not_found(&found, &manifest_rel, spec));
        }
    };
    let manifest = Manifest::parse(&text)?;
    let warnings = manifest.validate(ctx.herdr.version().ok().as_ref())?;
    let mut review = vec![
        format!("  source    github:{}", util::sanitize(spec)),
        format!(
            "  ref       {} → {}",
            util::sanitize(reference.unwrap_or("default branch")),
            bold(util::short_sha(&sha))
        ),
    ];
    if let Ok(c) = git.commit_info(&mirror, &sha) {
        review.push(commit_line(&c));
    }
    review.extend(manifest.preview_lines());
    if let Some(row) = current {
        if let Some(old_commit) = row.commit() {
            review.extend(change_lines(
                git,
                &mirror,
                (old_commit, &row.plugin.version, &row.plugin.plugin_root),
                &sha,
                &manifest,
            ));
        }
    }
    for warning in &warnings {
        review.push(format!("  {} {warning}", yellow("warning:")));
    }
    let verb = if current.is_some() {
        mode.verb()
    } else {
        "install"
    };
    let question = format!("{} {}?", capitalize(verb), manifest.id.trim());
    ctx.report.review(
        &format!("Plugin {verb} preview"),
        &review,
        (!ctx.yes).then_some(question.as_str()),
    )
}

/// `herdr plugin install` for a GitHub shorthand, reviewed. Returns false
/// when the review is declined.
fn github_install(
    ctx: &Ctx,
    spec: &str,
    reference: Option<&str>,
    current: Option<&Row>,
    mode: Mode,
) -> Result<bool> {
    let status = if ctx.report.interactive() {
        ctx.herdr.install_github(spec, reference, ctx.yes)?
    } else {
        if !github_review(ctx, spec, reference, current, mode)? {
            ctx.line("cancelled");
            return Ok(false);
        }
        ctx.step("installing with herdr");
        ctx.herdr
            .install_github_quiet(spec, reference, &|line| ctx.line(line))?
    };
    if !status.success() {
        bail!("herdr plugin install {spec} did not complete ({status})");
    }
    if !ctx.report.interactive() {
        let verb = if current.is_some() {
            past(mode)
        } else {
            "installed"
        };
        ctx.line(format!("{} {} {spec}", green("✓"), capitalize(verb)));
    }
    Ok(true)
}

/// `herdr plugin update` for a GitHub-installed plugin, reviewed.
fn github_update(ctx: &Ctx, row: &Row) -> Result<bool> {
    let id = &row.plugin.plugin_id;
    let status = if ctx.report.interactive() {
        ctx.herdr.update_github(id, ctx.yes)?
    } else {
        let spec = row.plugin.source.github_spec().unwrap_or_default();
        if !github_review(ctx, &spec, row.requested_ref(), Some(row), Mode::Update)? {
            ctx.line("cancelled");
            return Ok(false);
        }
        ctx.step("updating with herdr");
        ctx.herdr.update_github_quiet(id, &|line| ctx.line(line))?
    };
    if !status.success() {
        bail!("herdr plugin update {id} did not complete ({status})");
    }
    if !ctx.report.interactive() {
        ctx.line(format!("{} Updated {id}", green("✓")));
    }
    Ok(true)
}

/// The manifest as reviewed and as checked out may differ only in line
/// endings (an `eol=crlf` attribute in the repository).
fn same_text(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
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

fn run_builds(ctx: &Ctx, manifest: &Manifest, root: &Path) -> Result<()> {
    let steps = manifest.build_steps();
    let total = steps.len();
    for (index, entry) in steps.into_iter().enumerate() {
        let shown = manifest::render_argv(&entry.command);
        ctx.step(&format!("build {}/{total}: {shown}", index + 1));
        let mut command = build_command(&entry.command, root)?;
        let status = util::stream_command(&mut command, &|line| ctx.line(line))
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
        ctx.line("Nothing to update: no plugins installed from GitHub or git.");
        return Ok(0);
    }
    let total = selected.len();
    let mut failures = 0;
    for (index, row) in selected.into_iter().enumerate() {
        if total > 1 {
            ctx.report.progress(index, total);
        }
        let id = &row.plugin.plugin_id;
        ctx.step(&format!("checking {id}"));
        match check(row, ctx.git()) {
            Status::Current => ctx.line(format!("{} {id} is up to date", green("✓"))),
            Status::Pinned => ctx.line(format!(
                "{} {id} is pinned at {} (skipped)",
                dim("•"),
                util::short_sha(row.commit().unwrap_or(""))
            )),
            Status::Local => ctx.line(format!(
                "{} {id} is a local link (update it in {})",
                dim("•"),
                row.plugin.plugin_root
            )),
            Status::Unknown => ctx.line(format!("{} {id}: no source recorded", dim("•"))),
            Status::Error { message } => {
                failures += 1;
                ctx.line(format!("{} {id}: {message}", red("✗")));
            }
            Status::Available { commit, .. } => {
                if ctx.dry_run {
                    ctx.line(format!(
                        "{} {id}: {} → {} available (dry run, nothing changed)",
                        yellow("↑"),
                        util::short_sha(row.commit().unwrap_or("")),
                        util::short_sha(&commit)
                    ));
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
                    Origin::Github => github_update(ctx, row).map(|_| ()),
                    Origin::Local => Ok(()),
                };
                if let Err(err) = result {
                    failures += 1;
                    ctx.line(format!("{} {id}: {err:#}", red("✗")));
                }
            }
        }
    }
    if total > 1 {
        ctx.report.progress(total, total);
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

/// The `<id>-<hash>` directory under Herdr's `github-installations` that
/// holds every checkout of a GitHub-installed plugin.
fn github_installation_root(plugin_root: &str) -> Option<PathBuf> {
    Path::new(plugin_root)
        .ancestors()
        .find(|a| {
            a.parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "github-installations")
        })
        .map(Path::to_path_buf)
}

/// What `uninstall --purge` deletes besides the installations it always
/// removes: config and data directories, Herdr's leftover GitHub checkouts
/// and their lock files, the cached repository when no other plugin uses it,
/// and a local link's directory.
fn purge_targets(id: &str, row: Option<&Row>, record: Option<&Record>) -> Result<Vec<PathBuf>> {
    let mut targets = Vec::new();
    if let Some(row) = row {
        match row.origin {
            Origin::Local => {
                let dir = PathBuf::from(&row.plugin.plugin_root);
                let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
                if dir.parent().is_none() || home.is_some_and(|h| Path::new(&h) == dir) {
                    bail!("refusing to delete {}", dir.display());
                }
                targets.push(dir);
            }
            Origin::Github => targets.extend(github_installation_root(&row.plugin.plugin_root)),
            Origin::Git => {}
        }
    }
    if let Some(record) = record {
        let shared = Registry::load()?
            .plugins
            .values()
            .any(|r| r.id != id && r.url == record.url);
        if !shared {
            let mirror = crate::git::cache_path_for(&record.url);
            targets.push(mirror.with_extension("lock"));
            targets.push(mirror);
        }
    }
    targets.push(paths::herdr_plugin_config_dir(id));
    let component = paths::plugin_path_component(id);
    targets.push(paths::herdr_state_dir().join("plugins").join(&component));
    let herdr_plugins = paths::herdr_config_dir().join("plugins");
    for checkout in dirs_in(&herdr_plugins.join("github-installations")) {
        if checkout_id(&file_name(&checkout)) == component && !targets.contains(&checkout) {
            targets.push(checkout);
        }
    }
    if let Ok(entries) = std::fs::read_dir(herdr_plugins.join(".locks")) {
        for lock in entries.flatten().map(|e| e.path()) {
            let name = file_name(&lock);
            let stem = name.strip_prefix('.').and_then(|n| n.strip_suffix(".lock"));
            if stem.is_some_and(|s| checkout_id(s) == component) {
                targets.push(lock);
            }
        }
    }
    targets.retain(|p| p.exists());
    Ok(targets)
}

fn delete_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .with_context(|| format!("could not delete {}", path.display()))
}

fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// What one removed plugin left behind.
#[derive(Debug, Clone)]
pub struct Orphan {
    /// The plugin id as it appears on disk.
    pub id: String,
    /// `(kind, path)`: kind is `config`, `data`, `checkout` or `lock`.
    pub paths: Vec<(&'static str, PathBuf)>,
    /// Ids with auto-update or pin settings in the registry.
    pub settings: Vec<String>,
    /// Bytes on disk.
    pub size: u64,
}

impl Orphan {
    /// `config, data, checkout` and so on, each kind once.
    pub fn kinds(&self) -> String {
        let mut kinds: Vec<&str> = Vec::new();
        for (kind, _) in &self.paths {
            if !kinds.contains(kind) {
                kinds.push(kind);
            }
        }
        if !self.settings.is_empty() {
            kinds.push("settings");
        }
        kinds.join(", ")
    }
}

fn disk_size(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| disk_size(&e.path())).sum())
            .unwrap_or(0),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

/// The id part of Herdr's `<id>-<hash>` checkout names.
fn checkout_id(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((id, hash)) if !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()) => id,
        _ => name,
    }
}

/// What Herdr and this manager keep for plugins that are no longer
/// installed, one entry per plugin: config and state directories, Herdr's
/// GitHub checkouts and their lock files, auto-update and pin settings.
pub fn orphans(herdr: &Herdr) -> Result<Vec<Orphan>> {
    let plugins = herdr.list()?;
    let installed: Vec<String> = plugins.iter().map(|p| p.plugin_id.clone()).collect();
    let mut kept: Vec<String> = installed
        .iter()
        .map(|id| paths::plugin_path_component(id))
        .collect();
    kept.push(paths::plugin_path_component(paths::PLUGIN_ID));
    let roots: Vec<PathBuf> = plugins
        .iter()
        .filter_map(|p| std::fs::canonicalize(&p.plugin_root).ok())
        .collect();
    let mut found: Vec<(String, &'static str, PathBuf)> = Vec::new();
    let plugin_dirs = [
        (
            "config",
            paths::herdr_config_dir().join("plugins").join("config"),
        ),
        ("data", paths::herdr_state_dir().join("plugins")),
    ];
    for (kind, base) in &plugin_dirs {
        for dir in dirs_in(base) {
            let name = file_name(&dir);
            if !name.starts_with('.') && !kept.contains(&name) {
                found.push((name, kind, dir));
            }
        }
    }
    // `<id>-<hash>` checkouts. One still in use either holds an installed
    // plugin's root or carries an installed id; both keep it.
    let herdr_plugins = paths::herdr_config_dir().join("plugins");
    let checkouts = herdr_plugins.join("github-installations");
    for dir in dirs_in(&checkouts) {
        let name = file_name(&dir);
        let id = checkout_id(&name).to_string();
        let in_use = std::fs::canonicalize(&dir)
            .map(|d| roots.iter().any(|r| r.starts_with(&d)))
            .unwrap_or(true)
            || kept.contains(&id);
        if !in_use {
            found.push((id, "checkout", dir));
        }
    }
    // Herdr's per-checkout lock files (`.locks/.<id>-<hash>.lock`).
    if let Ok(entries) = std::fs::read_dir(herdr_plugins.join(".locks")) {
        for lock in entries.flatten().map(|e| e.path()).filter(|p| p.is_file()) {
            let name = file_name(&lock);
            let Some(stem) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".lock")) else {
                continue;
            };
            let id = checkout_id(stem).to_string();
            let checkout = checkouts.join(stem);
            let orphaned = !checkout.exists() || found.iter().any(|(_, _, p)| *p == checkout);
            if !kept.contains(&id) && orphaned {
                found.push((id, "lock", lock));
            }
        }
    }
    let registry = Registry::load()?;
    let mut stale: Vec<&String> = registry
        .auto_update
        .iter()
        .chain(registry.pins.keys())
        .filter(|id| !installed.contains(id))
        .collect();
    stale.sort();
    stale.dedup();

    let mut orphans: Vec<Orphan> = Vec::new();
    let entry = |orphans: &mut Vec<Orphan>, id: String| -> usize {
        match orphans.iter().position(|o| o.id == id) {
            Some(i) => i,
            None => {
                orphans.push(Orphan {
                    id,
                    paths: Vec::new(),
                    settings: Vec::new(),
                    size: 0,
                });
                orphans.len() - 1
            }
        }
    };
    for (id, kind, path) in found {
        let i = entry(&mut orphans, id);
        orphans[i].size += disk_size(&path);
        orphans[i].paths.push((kind, path));
    }
    for id in stale {
        let i = entry(&mut orphans, paths::plugin_path_component(id));
        orphans[i].settings.push(id.clone());
    }
    orphans.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(orphans)
}

/// Deletes what the given removed plugins left behind (all of them when
/// `ids` is empty), after a review.
pub fn clean(ctx: &Ctx, ids: &[String]) -> Result<()> {
    ctx.step("looking for leftovers of removed plugins");
    let mut orphans = orphans(&ctx.herdr)?;
    if !ids.is_empty() {
        if let Some(missing) = ids.iter().find(|id| {
            !orphans
                .iter()
                .any(|o| o.id == **id || o.id == paths::plugin_path_component(id))
        }) {
            bail!("no leftovers of a removed plugin named {missing}");
        }
        orphans.retain(|o| {
            ids.iter()
                .any(|id| o.id == *id || o.id == paths::plugin_path_component(id))
        });
    }
    if orphans.is_empty() {
        ctx.line(format!("{} Nothing to clean.", green("✓")));
        return Ok(());
    }
    let mut review = Vec::new();
    for orphan in &orphans {
        review.push(format!(
            "  {}  {}",
            bold(&orphan.id),
            dim(&util::human_size(orphan.size))
        ));
        for (kind, path) in &orphan.paths {
            review.push(format!("    {:<9}{}", kind, path.display()));
        }
        if !orphan.settings.is_empty() {
            review.push(format!("    {:<9}auto-update and pin", "settings"));
        }
    }
    let question = "Delete them?";
    if !ctx.report.review(
        &format!("Leftovers of {} removed plugin(s)", orphans.len()),
        &review,
        (!ctx.yes).then_some(question),
    )? {
        ctx.line("cancelled");
        return Ok(());
    }
    if ctx.dry_run {
        ctx.line("[dry-run] nothing deleted");
        return Ok(());
    }
    let mut failures = Vec::new();
    for (_, path) in orphans.iter().flat_map(|o| &o.paths) {
        match delete_path(path) {
            Ok(()) => ctx.line(format!("  {} deleted {}", dim("•"), path.display())),
            Err(err) => failures.push(format!("{err:#}")),
        }
    }
    let settings: Vec<&String> = orphans.iter().flat_map(|o| &o.settings).collect();
    if !settings.is_empty() {
        let mut locked = Registry::lock()?;
        locked
            .registry
            .auto_update
            .retain(|id| !settings.contains(&id));
        locked.registry.pins.retain(|id, _| !settings.contains(&id));
        locked.save()?;
    }
    if !failures.is_empty() {
        bail!(
            "some leftovers could not be deleted: {}",
            failures.join("; ")
        );
    }
    let names: Vec<&str> = orphans.iter().map(|o| o.id.as_str()).collect();
    ctx.line(format!(
        "{} Cleaned the leftovers of {}.",
        green("✓"),
        names.join(", ")
    ));
    Ok(())
}

/// Removes a plugin. `purge` also deletes what it leaves on disk (see
/// `purge_targets`) and removes git-managed installations right away
/// instead of through the trash.
pub fn uninstall(ctx: &Ctx, id: &str, purge: bool) -> Result<()> {
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
                Origin::Github => "uninstall it with herdr".into(),
                Origin::Local if purge => "unlink it".into(),
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
    let doomed = if purge {
        purge_targets(&plugin_id, row.as_ref(), record.as_ref())?
    } else {
        Vec::new()
    };
    let what = if doomed.is_empty() {
        what
    } else {
        let list: Vec<String> = doomed
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect();
        format!("{what}, then delete:\n{}\n", list.join("\n"))
    };
    let local_dir = row
        .as_ref()
        .filter(|r| purge && r.origin == Origin::Local)
        .map(|r| r.plugin.plugin_root.clone());
    if !ctx.yes {
        if !util::stdin_is_tty() {
            bail!(
                "refusing to uninstall without confirmation: stdin is not a terminal (pass --yes)"
            );
        }
        let accepted = match &local_dir {
            Some(dir) => {
                eprintln!("Uninstall {plugin_id}: {what}");
                util::ask_line(&format!("Type {plugin_id} to delete {dir}:"))? == plugin_id
            }
            None => util::confirm(&format!("Uninstall {plugin_id}: {what}?"))?,
        };
        if !accepted {
            ctx.line("cancelled");
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
        if let Some(record) = &record {
            let mut locked = Registry::lock()?;
            locked.registry.plugins.remove(&plugin_id);
            locked.save()?;
            drop(locked);
            for inst in std::iter::once(&record.current).chain(record.previous.iter()) {
                let dir = Path::new(&inst.install_dir);
                if !dir.exists() {
                    continue;
                }
                if !purge || delete_path(dir).is_err() {
                    trash(dir);
                }
            }
            cleanup_installs(&plugin_id);
        }
    }
    {
        let mut locked = Registry::lock()?;
        let had_setting = locked.registry.auto_update.remove(&plugin_id)
            | locked.registry.pins.remove(&plugin_id).is_some();
        if had_setting {
            locked.save()?;
        }
    }
    let mut failures = Vec::new();
    for path in &doomed {
        match delete_path(path) {
            Ok(()) => ctx.line(format!("  {} deleted {}", dim("•"), path.display())),
            Err(err) => failures.push(format!("{err:#}")),
        }
    }
    registry::log_event(Event {
        unix: util::now_unix(),
        action: if purge { "purge" } else { "uninstall" }.into(),
        plugin_id: plugin_id.clone(),
        source: row.as_ref().map(|r| r.source_display()),
        from: row.as_ref().and_then(|r| r.commit().map(str::to_string)),
        to: None,
        ok: failures.is_empty(),
        detail: (!failures.is_empty()).then(|| failures.join("; ")),
    });
    if !failures.is_empty() {
        bail!(
            "{plugin_id} was uninstalled, but some files could not be deleted: {}",
            failures.join("; ")
        );
    }
    ctx.line(format!(
        "{} {} {plugin_id}.",
        green("✓"),
        if purge { "Deleted" } else { "Uninstalled" }
    ));
    Ok(())
}

pub fn set_enabled(ctx: &Ctx, id: &str, enabled: bool) -> Result<()> {
    let row = require_row(ctx, id)?;
    ctx.herdr.set_enabled(&row.plugin.plugin_id, enabled)?;
    ctx.line(format!(
        "{} {} {}",
        green("✓"),
        if enabled { "Enabled" } else { "Disabled" },
        row.plugin.plugin_id
    ));
    Ok(())
}

/// Turns unattended updates on or off for one plugin, enabled or not.
/// Prints nothing, so the popup can call it.
pub fn set_auto_update(ctx: &Ctx, id: &str, on: bool) -> Result<()> {
    let row = require_row(ctx, id)?;
    let id = &row.plugin.plugin_id;
    if row.origin == Origin::Local {
        bail!("{id} is a local link: there is no remote to update it from");
    }
    if ctx.dry_run {
        return Ok(());
    }
    let mut locked = Registry::lock()?;
    let changed = if on {
        locked.registry.auto_update.insert(id.clone())
    } else {
        locked.registry.auto_update.remove(id)
    };
    if changed {
        locked.save()?;
        drop(locked);
        registry::log_event(Event {
            unix: util::now_unix(),
            action: if on {
                "auto-update on"
            } else {
                "auto-update off"
            }
            .into(),
            plugin_id: id.clone(),
            source: Some(row.source_display()),
            from: None,
            to: None,
            ok: true,
            detail: None,
        });
    }
    Ok(())
}

/// Updates every plugin with auto-update on (what Herdr runs at startup).
/// Callers pass a `ctx` with `yes`: nobody is there to review a preview.
/// Pinned plugins stay where they are. Returns the number of failures.
pub fn auto_update(ctx: &Ctx) -> Result<usize> {
    let rows: Vec<Row> = rows(&ctx.herdr)?
        .into_iter()
        .filter(|r| r.auto_update && r.origin != Origin::Local)
        .collect();
    if rows.is_empty() {
        ctx.line("No plugin has auto-update turned on.");
        return Ok(0);
    }
    let strategies = &ctx.config.auto_update;
    let (latest, tagged): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(|r| {
        strategies
            .strategy_for(&r.plugin.plugin_id)
            .same()
            .is_none()
    });
    let mut failures = 0;
    if !latest.is_empty() {
        let ids: Vec<String> = latest.into_iter().map(|r| r.plugin.plugin_id).collect();
        failures += update(ctx, &ids)?;
    }
    for row in &tagged {
        let strategy = strategies.strategy_for(&row.plugin.plugin_id);
        if let Err(err) = update_to_release(ctx, row, strategy) {
            failures += 1;
            ctx.line(format!("{} {}: {err:#}", red("✗"), row.plugin.plugin_id));
        }
    }
    Ok(failures)
}

/// Moves a plugin to the newest version tag its strategy allows. A git
/// plugin keeps following its branch; a GitHub plugin is reinstalled at the
/// tag, which Herdr then follows.
fn update_to_release(ctx: &Ctx, row: &Row, strategy: crate::config::Strategy) -> Result<()> {
    let id = &row.plugin.plugin_id;
    if row.pinned() {
        ctx.line(format!("{} {id} is pinned (skipped)", dim("•")));
        return Ok(());
    }
    ctx.step(&format!("checking {id} ({})", strategy.label()));
    let tags = versions(row)?;
    let installed = row.commit().unwrap_or("");
    // The tag at the installed commit says more than the manifest version.
    let current = tags
        .iter()
        .find(|t| t.commit.eq_ignore_ascii_case(installed))
        .map_or(row.plugin.version.as_str(), |t| t.name.as_str());
    let same = strategy.same().unwrap_or(0);
    let Some(tag) = crate::git::newest_release(&tags, current, same)
        .filter(|t| !t.commit.eq_ignore_ascii_case(installed))
    else {
        ctx.line(format!(
            "{} {id} is up to date ({})",
            green("✓"),
            strategy.label()
        ));
        return Ok(());
    };
    if ctx.dry_run {
        ctx.line(format!(
            "{} {id}: {current} → {} available (dry run, nothing changed)",
            yellow("↑"),
            tag.name
        ));
        return Ok(());
    }
    match row.origin {
        Origin::Git => {
            let record = row.record.as_ref().unwrap();
            install_git(
                ctx,
                GitRequest {
                    url: record.url.clone(),
                    subdir: record.subdir.clone(),
                    checkout_ref: Some(tag.commit.clone()),
                    follow_ref: Some(record.requested_ref.clone()),
                    mode: Mode::Update,
                    enabled: None,
                    replace: false,
                    expect_id: Some(id.clone()),
                },
            )?;
        }
        Origin::Github => {
            let spec = row.plugin.source.github_spec().unwrap_or_default();
            github_install(ctx, &spec, Some(&tag.name), Some(row), Mode::Update)?;
        }
        Origin::Local => {}
    }
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
    let mut review = vec![format!(
        "  change    {} ({}) → {} ({}, installed {})",
        util::sanitize(&record.current.version),
        util::short_sha(&record.current.commit),
        util::sanitize(&prev.version),
        util::short_sha(&prev.commit),
        util::format_unix(prev.installed_unix)
    )];
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
            let diff = manifest::diff_runnables(&cur, &old);
            if diff.is_empty() {
                review.push(format!("  {}", green("what it runs is unchanged")));
            } else {
                review.push(format!("  {}", yellow("what it runs changes:")));
                review.extend(diff.into_iter().map(|line| yellow(&line)));
            }
        }
    }
    let question = format!("Roll back {id}?");
    if !ctx.report.review(
        &format!("Roll back {id}"),
        &review,
        (!ctx.yes).then_some(question.as_str()),
    )? {
        ctx.line("cancelled");
        return Ok(());
    }
    if ctx.dry_run {
        ctx.line(format!("[dry-run] would link {}", prev.plugin_root));
        return Ok(());
    }
    ctx.step(&format!("linking {}", prev.plugin_root));
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
    ctx.line(format!(
        "{} Rolled back {id} to {} ({}).",
        green("✓"),
        prev.version,
        util::short_sha(&prev.commit)
    ));
    if !util::is_full_sha(prev.requested_ref.as_deref().unwrap_or("")) {
        ctx.line(format!(
            "  {} updates will be offered again; `herdr-pm pin {id}` holds this version.",
            dim("note:")
        ));
    }
    Ok(())
}

fn log_pin_change(row: &Row, action: &str, commit: Option<&str>) {
    registry::log_event(Event {
        unix: util::now_unix(),
        action: action.into(),
        plugin_id: row.plugin.plugin_id.clone(),
        source: Some(row.source_display()),
        from: None,
        to: commit.map(str::to_string),
        ok: true,
        detail: None,
    });
}

/// Holds a plugin at the commit it runs now, without reinstalling it.
/// Prints nothing, so the popup can call it. Returns the short commit.
pub fn pin_current(ctx: &Ctx, id: &str) -> Result<String> {
    let row = require_row(ctx, id)?;
    let id = row.plugin.plugin_id.clone();
    if row.origin == Origin::Local {
        bail!("{id} is a local link; there is nothing to pin");
    }
    let commit = row
        .commit()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{id} has no recorded commit"))?;
    if !ctx.dry_run {
        let mut locked = Registry::lock()?;
        if row.origin == Origin::Git {
            if let Some(rec) = locked.registry.plugins.get_mut(&id) {
                rec.requested_ref = Some(commit.clone());
                rec.current.requested_ref = rec.requested_ref.clone();
            }
        } else {
            locked.registry.pins.insert(id.clone(), commit.clone());
        }
        locked.save()?;
        drop(locked);
        log_pin_change(&row, "pin", Some(&commit));
    }
    Ok(util::short_sha(&commit).to_string())
}

/// Follows the default branch again without reinstalling. Plugins Herdr
/// installed at a commit (`Row::unpin_needs_install`) need `unpin`.
/// Prints nothing, so the popup can call it.
pub fn clear_pin(ctx: &Ctx, id: &str) -> Result<()> {
    let row = require_row(ctx, id)?;
    let id = &row.plugin.plugin_id;
    if row.origin == Origin::Local {
        bail!("{id} is a local link; there is nothing to unpin");
    }
    if row.unpin_needs_install() {
        bail!("herdr installed {id} at a commit; unpinning reinstalls it");
    }
    if !ctx.dry_run {
        let mut locked = Registry::lock()?;
        if row.origin == Origin::Git {
            if let Some(rec) = locked.registry.plugins.get_mut(id) {
                rec.requested_ref = None;
            }
        } else {
            locked.registry.pins.remove(id);
        }
        locked.save()?;
        drop(locked);
        log_pin_change(&row, "unpin", None);
    }
    Ok(())
}

fn forget_pin(id: &str) -> Result<()> {
    let mut locked = Registry::lock()?;
    if locked.registry.pins.remove(id).is_some() {
        locked.save()?;
    }
    Ok(())
}

/// Tags of the plugin's repository, newest version first.
pub fn versions(row: &Row) -> Result<Vec<crate::git::RemoteTag>> {
    let (url, _) = row
        .remote()
        .ok_or_else(|| anyhow!("{} has no remote", row.plugin.plugin_id))?;
    Git::BACKGROUND.list_tags(&url)
}

/// Name of the default branch the plugin follows, when it follows one.
pub fn default_branch(row: &Row) -> Option<String> {
    if row.requested_ref().is_some() {
        return None;
    }
    let (url, _) = row.remote()?;
    Git::BACKGROUND.default_branch(&url).ok().flatten()
}

/// Holds a plugin at its current commit, or at `reference`. Pinning the
/// commit already installed only records it; another commit is installed
/// (with its preview) and held there.
pub fn pin(ctx: &Ctx, id: &str, reference: Option<&str>) -> Result<()> {
    let row = require_row(ctx, id)?;
    if row.origin == Origin::Local {
        bail!("{id} is a local link; there is nothing to pin");
    }
    let target = match (reference, row.origin) {
        (None, _) => None,
        (Some(r), Origin::Git) => {
            let record = row.record.as_ref().unwrap();
            let git = ctx.git();
            let mirror = git.sync(&record.url)?;
            Some(git.resolve(&mirror, Some(r))?)
        }
        (Some(r), _) => {
            let (url, _) = row.remote().ok_or_else(|| anyhow!("{id} has no remote"))?;
            Some(ctx.git().ls_remote(&url, Some(r))?)
        }
    };
    let sha = match target {
        Some(sha) if row.commit().is_none_or(|c| !c.eq_ignore_ascii_case(&sha)) => sha,
        _ => {
            let short = pin_current(ctx, id)?;
            ctx.line(format!("{} Pinned {id} at {short}.", green("✓")));
            return Ok(());
        }
    };
    match row.origin {
        Origin::Git => {
            let record = row.record.unwrap();
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
        _ => {
            let spec = row.plugin.source.github_spec().unwrap_or_default();
            if github_install(ctx, &spec, Some(&sha), Some(&row), Mode::Switch)? {
                // Herdr now records the commit itself.
                forget_pin(id)?;
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
            ctx.line(format!(
                "{} {id} now follows {}; `herdr-pm update {id}` applies it.",
                green("✓"),
                reference.unwrap_or("the default branch")
            ));
            Ok(())
        }
        Origin::Github if reference.is_none() && !row.unpin_needs_install() => {
            clear_pin(ctx, id)?;
            ctx.line(format!(
                "{} {id} follows its default branch again; `herdr-pm update {id}` applies it.",
                green("✓")
            ));
            Ok(())
        }
        Origin::Github => {
            let spec = row.plugin.source.github_spec().unwrap_or_default();
            if github_install(ctx, &spec, reference, Some(&row), Mode::Switch)? {
                forget_pin(id)?;
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
            if github_install(ctx, &spec, Some(reference), Some(&row), Mode::Switch)? {
                forget_pin(id)?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- export / import

pub const EXPORT_FORMAT: &str = "herdr-plugin-manager-export";

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
        bail!(
            "not a herdr-plugin-manager export (format {:?})",
            file.format
        );
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
                ctx.line(format!(
                    "{} {}: {}",
                    green("✓"),
                    entry.id,
                    if entry.enabled { "enabled" } else { "disabled" }
                ));
            } else {
                ctx.line(format!("{} {} unchanged", dim("•"), entry.id));
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
            ctx.line(format!("{} {}: {err:#}", red("✗"), entry.id));
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
        (
            "auto-update".into(),
            if row.auto_update { "on" } else { "off" }.into(),
        ),
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
