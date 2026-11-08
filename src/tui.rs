//! The popup: installed plugins and the marketplace, in one terminal UI.
//!
//! Quick changes (enable/disable) happen in place. Anything that shows a
//! trust preview or runs builds (install, update, uninstall, rollback, pin)
//! suspends the UI and runs the same code path as the CLI on the plain
//! terminal, then returns.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::Config;
use crate::git::{Git, RemoteTag};
use crate::market::{self, Entry, Sort};
use crate::ops::{self, Ctx, InstallRequest, Origin, Orphan, Report, Row, Status};
use crate::{paths, settings, source, util};

type Term = Terminal<CrosstermBackend<Stdout>>;

// ---------------------------------------------------------------- palette

const ACCENT: Color = Color::Indexed(141);
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const MUTED: Color = Color::DarkGray;
const SELECTED_BG: Color = Color::Indexed(236);

fn muted() -> Style {
    Style::default().fg(MUTED)
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Installed,
    Market,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    Pending,
    Done(Status),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Prompt {
    Install,
    Switch(String),
    Filter,
    Search,
    /// Typing the id confirms deleting a local link's directory.
    Purge(String),
    /// A value typed in the settings dialog.
    Setting(settings::Field),
}

#[derive(Debug, Clone)]
enum Confirm {
    /// `u` removes the plugin, `d` deletes everything it left on disk.
    Remove {
        id: String,
        origin: Origin,
        path: String,
    },
    UpdateAll(Vec<String>),
    AutoUpdateOn {
        id: String,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

enum Msg {
    Status(String, Status),
    Market(u64, Vec<(String, Result<market::Listing, String>)>),
    /// Default branch name of a plugin that follows it.
    Branch(String, String),
    /// Tags for the version picker.
    Versions(String, Result<Vec<RemoteTag>, String>),
    /// Leftovers of removed plugins, for the `X` dialog.
    Orphans(Result<Vec<Orphan>, String>),
    /// Progress of the running operation.
    Task(TaskEvent),
}

struct MarketState {
    entries: Vec<Entry>,
    errors: Vec<(String, String)>,
    loading: bool,
    loaded: bool,
    generation: u64,
    query: String,
    sort: Sort,
    source: Option<String>,
    sel: usize,
    page_size: usize,
    /// Report the outcome of the running refresh even when nothing changed.
    announce: bool,
}

/// One line of the `p` version picker.
#[derive(Debug, Clone)]
enum Pick {
    /// The commit installed now, when no tag points at it.
    Current(String),
    Tag(RemoteTag),
    /// Follow the default branch again.
    Latest,
}

struct Picker {
    id: String,
    name: String,
    picks: Vec<Pick>,
    sel: usize,
    loading: bool,
    error: Option<String>,
}

/// The `X` dialog: removed plugins whose leftovers can be deleted.
struct Cleaner {
    orphans: Vec<Orphan>,
    chosen: Vec<bool>,
    sel: usize,
    loading: bool,
    error: Option<String>,
}

enum TaskEvent {
    Step(String),
    Line(String),
    Review(Review),
    Progress(usize, usize),
    Done(Result<(), String>),
}

/// A trust preview waiting for y/n.
struct Review {
    title: String,
    lines: Vec<String>,
    question: String,
    reply: Sender<bool>,
    scroll: usize,
}

/// An install, update, removal… running on a worker thread. Its output is
/// kept in `log` (shown with `l`, and when it fails).
struct Task {
    title: String,
    started: Instant,
    step: String,
    log: Vec<String>,
    progress: Option<(usize, usize)>,
    review: Option<Review>,
    show_log: bool,
    done: Option<Result<(), String>>,
    /// `a` on a review approves the rest of a batch.
    batch: bool,
    accept_all: Arc<AtomicBool>,
    touched: Option<Vec<String>>,
}

/// How operations report while the popup runs them: stages and output go
/// to the task dialog, previews become a y/n dialog, nothing touches the
/// terminal.
struct TaskReport {
    tx: Sender<Msg>,
    accept_all: Arc<AtomicBool>,
}

impl TaskReport {
    fn send(&self, event: TaskEvent) {
        let _ = self.tx.send(Msg::Task(event));
    }
}

impl Report for TaskReport {
    fn step(&self, text: &str) {
        self.send(TaskEvent::Step(text.to_string()));
    }

    fn line(&self, text: &str) {
        self.send(TaskEvent::Line(text.to_string()));
    }

    fn review(&self, title: &str, lines: &[String], question: Option<&str>) -> Result<bool> {
        let Some(question) = question else {
            for line in lines {
                self.line(line);
            }
            return Ok(true);
        };
        if self.accept_all.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let (reply, answer) = mpsc::channel();
        self.send(TaskEvent::Review(Review {
            title: title.to_string(),
            lines: lines.to_vec(),
            question: question.to_string(),
            reply,
            scroll: 0,
        }));
        Ok(answer.recv().unwrap_or(false))
    }

    fn progress(&self, done: usize, total: usize) {
        self.send(TaskEvent::Progress(done, total));
    }

    fn interactive(&self) -> bool {
        false
    }
}

struct App {
    ctx: Ctx,
    /// The operation running in the background, if any.
    task: Option<Task>,
    view: View,
    rows: Vec<Row>,
    checks: HashMap<String, Check>,
    /// Indexes into `rows` of the plugins the filter lets through.
    items: Vec<usize>,
    sel: usize,
    offset: usize,
    filter: String,
    prompt: Option<(Prompt, String)>,
    confirm: Option<Confirm>,
    message: Option<(Level, String)>,
    help: bool,
    help_scroll: usize,
    picker: Option<Picker>,
    cleaner: Option<Cleaner>,
    settings: Option<SettingsDialog>,
    /// Default branch names, for the `ref` line.
    branches: HashMap<String, String>,
    market: MarketState,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
    list_area: Rect,
    /// Clickable header tabs.
    tab_areas: Vec<(Rect, View)>,
    /// Lines above the first marketplace entry (loading/errors).
    market_header: usize,
    load_error: Option<String>,
}

impl App {
    fn new(ctx: Ctx) -> App {
        let (tx, rx) = mpsc::channel();
        App {
            ctx,
            view: View::Installed,
            rows: Vec::new(),
            checks: HashMap::new(),
            items: Vec::new(),
            sel: 0,
            offset: 0,
            filter: String::new(),
            prompt: None,
            confirm: None,
            message: None,
            help: false,
            help_scroll: 0,
            picker: None,
            cleaner: None,
            settings: None,
            branches: HashMap::new(),
            market: MarketState {
                entries: Vec::new(),
                errors: Vec::new(),
                loading: false,
                loaded: false,
                generation: 0,
                query: String::new(),
                sort: Sort::Stars,
                source: None,
                sel: 0,
                page_size: 10,
                announce: false,
            },
            tx,
            rx,
            quit: false,
            task: None,
            list_area: Rect::default(),
            tab_areas: Vec::new(),
            market_header: 0,
            load_error: None,
        }
    }

    fn say(&mut self, level: Level, text: impl Into<String>) {
        self.message = Some((level, text.into()));
    }

    // ---- data

    fn reload(&mut self) {
        let selected_id = self.selected_row().map(|r| r.plugin.plugin_id.clone());
        match ops::rows(&self.ctx.herdr) {
            Ok(rows) => {
                self.rows = rows;
                self.load_error = None;
            }
            Err(err) => {
                self.rows.clear();
                self.load_error = Some(format!("{err:#}"));
            }
        }
        self.rows.sort_by_key(|r| r.plugin.name.to_lowercase());
        self.rebuild_items();
        if let Some(id) = selected_id {
            if let Some(pos) = self
                .items
                .iter()
                .position(|&i| self.rows[i].plugin.plugin_id == id)
            {
                self.sel = pos;
            }
        }
        self.clamp();
    }

    fn rebuild_items(&mut self) {
        let terms: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.items.clear();
        for (i, row) in self.rows.iter().enumerate() {
            let hay = format!(
                "{} {} {} {}",
                row.plugin.plugin_id,
                row.plugin.name,
                row.source_display(),
                row.origin.label()
            )
            .to_lowercase();
            if !terms.iter().all(|t| hay.contains(t)) {
                continue;
            }
            self.items.push(i);
        }
    }

    fn clamp(&mut self) {
        if self.items.is_empty() {
            self.sel = 0;
        } else if self.sel >= self.items.len() {
            self.sel = self.items.len() - 1;
        }
    }

    /// Starts update checks for every plugin (or just `only`).
    fn start_checks(&mut self, only: Option<&[String]>) {
        let queue: VecDeque<Row> = self
            .rows
            .iter()
            .filter(|r| r.origin != Origin::Local)
            .filter(|r| only.is_none_or(|ids| ids.contains(&r.plugin.plugin_id)))
            .cloned()
            .collect();
        for row in &queue {
            self.checks
                .insert(row.plugin.plugin_id.clone(), Check::Pending);
        }
        for row in self.rows.iter().filter(|r| r.origin == Origin::Local) {
            self.checks
                .insert(row.plugin.plugin_id.clone(), Check::Done(Status::Local));
        }
        if queue.is_empty() {
            return;
        }
        let queue = Arc::new(Mutex::new(queue));
        let workers = self.ctx.config.check_concurrency.clamp(1, 16);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let tx = self.tx.clone();
            std::thread::spawn(move || loop {
                let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                let Some(row) = next else { break };
                let id = row.plugin.plugin_id.clone();
                let status = ops::check(&row, Git::BACKGROUND);
                let _ = tx.send(Msg::Status(id.clone(), status.clone()));
                if let Some(branch) = ops::default_branch(&row) {
                    let _ = tx.send(Msg::Branch(id.clone(), branch));
                }
                if let Status::Available { commit, .. } = &status {
                    if let Some(version) = ops::remote_version(&row, commit, Git::BACKGROUND) {
                        let _ = tx.send(Msg::Status(
                            id,
                            Status::Available {
                                commit: commit.clone(),
                                version: Some(version),
                            },
                        ));
                    }
                }
            });
        }
    }

    /// Shows the cached catalog at once (any age), then revalidates it in
    /// the background; `drain` swaps in the result and reports the changes.
    fn load_market(&mut self, refresh: bool) {
        self.market.generation += 1;
        let generation = self.market.generation;
        let query = self.market.query.clone();
        let only = self.market.source.clone();
        if !refresh {
            let cached = market::cached_all(&self.ctx.config, &query, only.as_deref());
            if !cached.is_empty() {
                self.market.entries = cached.into_iter().flat_map(|(_, l)| l.entries).collect();
                market::sort_entries(&mut self.market.entries, self.market.sort);
                self.market.errors.clear();
                self.market.loaded = true;
                self.market.sel = self
                    .market
                    .sel
                    .min(self.market.entries.len().saturating_sub(1));
            }
        }
        self.market.loading = true;
        let config = self.ctx.config.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let results = market::search_all(&config, &query, refresh, only.as_deref())
                .into_iter()
                .map(|r| (r.name, r.result.map_err(|e| format!("{e:#}"))))
                .collect();
            let _ = tx.send(Msg::Market(generation, results));
        });
    }

    fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Status(id, status) => {
                    self.checks.insert(id, Check::Done(status));
                }
                Msg::Branch(id, branch) => {
                    self.branches.insert(id, branch);
                }
                Msg::Versions(id, result) => fill_picker(self, &id, result),
                Msg::Orphans(result) => fill_cleaner(self, result),
                Msg::Task(event) => task_event(self, event),
                Msg::Market(generation, results) => {
                    if generation != self.market.generation {
                        continue;
                    }
                    self.market.loading = false;
                    let selected = self
                        .market
                        .entries
                        .get(self.market.sel)
                        .map(|e| (e.market.clone(), e.install_spec()));
                    let previous = std::mem::take(&mut self.market.entries);
                    let had_listing = self.market.loaded && !previous.is_empty();
                    self.market.errors.clear();
                    let mut entries = Vec::new();
                    for (name, result) in results {
                        match result {
                            Ok(listing) => entries.extend(listing.entries),
                            Err(err) => {
                                // Keep what this source listed before.
                                let kept: Vec<Entry> = previous
                                    .iter()
                                    .filter(|e| e.market == name)
                                    .cloned()
                                    .collect();
                                let err = if kept.is_empty() {
                                    err
                                } else {
                                    format!("{err} (showing the cached list)")
                                };
                                entries.extend(kept);
                                self.market.errors.push((name, err));
                            }
                        }
                    }
                    let changes = market::changes(&previous, &entries);
                    market::sort_entries(&mut entries, self.market.sort);
                    self.market.sel = selected
                        .and_then(|key| {
                            entries
                                .iter()
                                .position(|e| (e.market.clone(), e.install_spec()) == key)
                        })
                        .unwrap_or(self.market.sel)
                        .min(entries.len().saturating_sub(1));
                    self.market.entries = entries;
                    self.market.loaded = true;
                    let announce = std::mem::take(&mut self.market.announce);
                    if had_listing && !changes.is_empty() {
                        self.say(
                            Level::Info,
                            format!(
                                "marketplace updated: {} new · {} removed · {} changed",
                                changes.added, changes.removed, changes.updated
                            ),
                        );
                    } else if announce && self.market.errors.is_empty() {
                        self.say(Level::Ok, "marketplace is up to date");
                    }
                }
            }
        }
        changed
    }

    // ---- selection helpers

    fn selected_row(&self) -> Option<&Row> {
        self.items.get(self.sel).and_then(|&i| self.rows.get(i))
    }

    fn status_of(&self, id: &str) -> Option<&Check> {
        self.checks.get(id)
    }

    fn outdated_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    self.checks.get(&r.plugin.plugin_id),
                    Some(Check::Done(Status::Available { .. }))
                )
            })
            .map(|r| r.plugin.plugin_id.clone())
            .collect()
    }

    fn entry_installed(&self, entry: &Entry) -> Option<&Row> {
        self.rows.iter().find(|row| {
            if let Some(id) = &entry.plugin_id {
                if &row.plugin.plugin_id == id {
                    return true;
                }
            }
            match row.origin {
                Origin::Github => {
                    entry.is_github()
                        && row
                            .plugin
                            .source
                            .github_slug()
                            .is_some_and(|s| s.eq_ignore_ascii_case(&entry.install))
                        && row.plugin.source.subdir == entry.subdir
                }
                Origin::Git => row
                    .record
                    .as_ref()
                    .is_some_and(|r| r.url == entry.install && r.subdir == entry.subdir),
                Origin::Local => false,
            }
        })
    }

    fn market_visible(&self) -> Vec<&Entry> {
        self.market.entries.iter().collect()
    }
}

// ---------------------------------------------------------------- terminal

/// crossterm turns mouse capture on through the console API on Windows and
/// never emits the VT request, so the hosting terminal (Herdr, Windows
/// Terminal) keeps the mouse for itself. Ask for button reports in SGR
/// encoding explicitly; ConPTY hands them to crossterm as mouse events.
#[cfg(windows)]
const VT_MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
#[cfg(windows)]
const VT_MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

fn enter(term: &mut Term) -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )?;
    #[cfg(windows)]
    {
        use std::io::Write;
        let mut out = io::stdout();
        out.write_all(VT_MOUSE_ON.as_bytes())?;
        out.flush()?;
    }
    term.clear()?;
    Ok(())
}

fn leave() -> Result<()> {
    #[cfg(windows)]
    {
        use std::io::Write;
        let mut out = io::stdout();
        let _ = out.write_all(VT_MOUSE_OFF.as_bytes());
        let _ = out.flush();
    }
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
    terminal::disable_raw_mode()?;
    Ok(())
}

pub fn run(ctx: Ctx) -> Result<()> {
    if !util::stdin_is_tty() {
        anyhow::bail!(
            "the interactive manager needs a terminal; see `herdr-pm --help` for commands"
        );
    }
    let mut app = App::new(ctx);
    app.reload();
    app.start_checks(None);
    app.load_market(false);
    if app.ctx.dry_run {
        app.say(Level::Warn, "dry run: changes are printed, not made");
    }

    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave();
        default_hook(info);
    }));
    ops::set_plain();
    enter(&mut term)?;
    let result = event_loop(&mut app, &mut term);
    leave()?;
    result
}

fn event_loop(app: &mut App, term: &mut Term) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    loop {
        if app.drain() {
            dirty = true;
        }
        if dirty {
            term.draw(|f| draw(f, app))?;
            dirty = false;
        }
        if app.quit {
            return Ok(());
        }
        let busy = app.market.loading
            || app.task.is_some()
            || app.checks.values().any(|c| matches!(c, Check::Pending));
        let timeout = if busy {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(500)
        };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(job) = handle_key(app, key) {
                        run_job(app, term, job)?;
                    }
                    dirty = true;
                }
                Event::Mouse(m) => {
                    if handle_mouse(app, m.kind, m.column, m.row) {
                        dirty = true;
                    }
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        if busy && last_tick.elapsed() > Duration::from_millis(120) {
            last_tick = Instant::now();
            dirty = true; // spinner
        }
    }
}

// ---------------------------------------------------------------- jobs (suspended)

enum Job {
    Install(String),
    InstallEntry(Box<Entry>),
    Update(Vec<String>),
    /// Plugin id, and whether to delete everything it left on disk.
    Uninstall(String, bool),
    Rollback(String),
    /// Install this commit and hold the plugin there.
    PinTo(String, String),
    Unpin(String),
    Switch(String, String),
    /// Open config.toml in the external editor.
    EditConfig,
    /// Delete what these removed plugins left behind.
    Clean(Vec<String>),
}

fn editor_command(config: &Config) -> Vec<String> {
    let raw = config
        .editor
        .clone()
        .or_else(|| std::env::var("HERDR_PM_EDITOR").ok())
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    util::split_words(&raw)
}

/// Runs a job: the editor takes over the terminal; everything else runs as
/// a background task behind a progress dialog.
fn run_job(app: &mut App, term: &mut Term, job: Job) -> Result<()> {
    if let Job::EditConfig = job {
        leave()?;
        let result = edit_config(app);
        enter(term)?;
        app.reload();
        match result {
            Ok(()) => app.message = None,
            Err(err) => app.say(Level::Error, util::truncate(&format!("{err:#}"), 200)),
        }
        return Ok(());
    }
    let one = |id: &String| Some(vec![id.clone()]);
    let (title, touched, batch, yes): (String, Option<Vec<String>>, bool, bool) = match &job {
        Job::Install(spec) => (format!("Installing {spec}"), None, false, false),
        Job::InstallEntry(entry) => (format!("Installing {}", entry.title), None, false, false),
        Job::Update(ids) if ids.len() == 1 => (
            format!("Updating {}", ids[0]),
            Some(ids.clone()),
            false,
            false,
        ),
        Job::Update(ids) => (
            format!("Updating {} plugins", ids.len()),
            Some(ids.clone()),
            true,
            false,
        ),
        Job::Uninstall(id, true) => (format!("Deleting {id}"), None, false, true),
        Job::Uninstall(id, false) => (format!("Removing {id}"), None, false, true),
        Job::Rollback(id) => (format!("Rolling back {id}"), one(id), false, false),
        Job::PinTo(id, commit) => (
            format!("Pinning {id} at {}", util::short_sha(commit)),
            one(id),
            false,
            false,
        ),
        Job::Unpin(id) => (format!("Unpinning {id}"), one(id), false, false),
        Job::Switch(id, r) => (format!("Switching {id} to {r}"), one(id), false, false),
        Job::Clean(ids) => (
            format!("Cleaning leftovers of {}", ids.join(", ")),
            None,
            false,
            false,
        ),
        Job::EditConfig => unreachable!(),
    };
    let run: Operation = match job {
        Job::Install(spec) => Box::new(move |ctx| {
            let request = parse_install(&spec, &ctx.config.aliases)?;
            ops::install(ctx, request).map(|_| ())
        }),
        Job::InstallEntry(entry) => Box::new(move |ctx| {
            let spec = entry.install_spec();
            let parsed = if entry.is_github() {
                source::parse(&format!("github:{spec}"), &ctx.config.aliases)?
            } else {
                source::parse(&spec, &ctx.config.aliases)?
            };
            let request = InstallRequest {
                source: parsed,
                reference: entry.git_ref.clone(),
                enabled: None,
                replace: false,
            };
            ops::install(ctx, request).map(|_| ())
        }),
        Job::Update(ids) => Box::new(move |ctx| match ops::update(ctx, &ids)? {
            0 => Ok(()),
            failures => anyhow::bail!("{failures} update(s) failed"),
        }),
        Job::Uninstall(id, purge) => Box::new(move |ctx| ops::uninstall(ctx, &id, purge)),
        Job::Rollback(id) => Box::new(move |ctx| ops::rollback(ctx, &id)),
        Job::PinTo(id, commit) => Box::new(move |ctx| ops::pin(ctx, &id, Some(&commit))),
        Job::Unpin(id) => Box::new(move |ctx| ops::unpin(ctx, &id, None)),
        Job::Switch(id, reference) => Box::new(move |ctx| ops::switch(ctx, &id, &reference)),
        Job::Clean(ids) => Box::new(move |ctx| ops::clean(ctx, &ids)),
        Job::EditConfig => unreachable!(),
    };
    start_task(app, title, touched, batch, yes, run);
    Ok(())
}

/// The words typed at the `i` prompt: a source and optional flags.
fn parse_install(
    spec: &str,
    aliases: &std::collections::BTreeMap<String, String>,
) -> Result<InstallRequest> {
    let mut source_arg = None;
    let mut reference = None;
    let mut subdir = None;
    let mut force_git = false;
    let mut replace = false;
    let mut words = util::split_words(spec).into_iter();
    while let Some(w) = words.next() {
        match w.as_str() {
            "--ref" => reference = words.next(),
            "--subdir" => subdir = words.next(),
            "--git" => force_git = true,
            "--replace" => replace = true,
            _ if source_arg.is_none() => source_arg = Some(w),
            other => anyhow::bail!("unexpected {other:?}"),
        }
    }
    let source_arg = source_arg.ok_or_else(|| anyhow::anyhow!("no source given"))?;
    let mut parsed = source::parse(&source_arg, aliases)?;
    if force_git {
        parsed = parsed.into_git();
    }
    if let Some(s) = subdir.as_deref() {
        source::validate_subdir(&source::normalize_subdir(s))?;
    }
    Ok(InstallRequest {
        source: parsed.with_subdir(subdir),
        reference,
        enabled: None,
        replace,
    })
}

fn edit_config(app: &mut App) -> Result<()> {
    let (path, _) = Config::init()?;
    let argv = editor_command(&app.ctx.config);
    if app.ctx.dry_run {
        return Ok(());
    }
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .arg(&path)
        .status()?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    match Config::load() {
        Ok(c) => app.ctx.config = c,
        Err(err) => anyhow::bail!("config not reloaded: {err:#}"),
    }
    Ok(())
}

// ---------------------------------------------------------------- settings

/// The `C` dialog over `config.toml`.
struct SettingsDialog {
    sel: usize,
    error: Option<String>,
}

fn settings_rows(app: &App) -> Vec<settings::Row> {
    let plugins: Vec<String> = app
        .rows
        .iter()
        .filter(|r| r.origin != Origin::Local)
        .map(|r| r.plugin.plugin_id.clone())
        .collect();
    settings::rows(&app.ctx.config, &plugins)
}

/// Applies one change to `config.toml`, or shows why it was refused.
fn apply_setting(app: &mut App, field: &settings::Field, action: settings::Action) {
    let path = paths::pm_config_file();
    let result = (|| -> Result<Config> {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => crate::config::EXAMPLE.into(),
            Err(err) => return Err(err.into()),
        };
        let updated = settings::change(&text, field, &action)?;
        let config = Config::parse(&updated)?;
        if updated != text && !app.ctx.dry_run {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, updated)?;
        }
        Ok(config)
    })();
    let error = match result {
        Ok(config) => {
            app.ctx.config = config;
            if matches!(field, settings::Field::Market(_)) {
                app.market.source = None;
                if app.market.loaded || app.market.loading {
                    app.load_market(false);
                }
            }
            None
        }
        Err(err) => Some(format!("{err:#}")),
    };
    if let Some(dialog) = app.settings.as_mut() {
        dialog.error = error;
    }
}

fn settings_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    use settings::{Action, Kind};
    let rows = settings_rows(app);
    let dialog = app.settings.as_mut()?;
    let last = rows.len().saturating_sub(1);
    dialog.sel = dialog.sel.min(last);
    let field = rows.get(dialog.sel).map(|r| r.field.clone())?;
    let kind = field.kind();
    let action = match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('C') => {
            app.settings = None;
            return None;
        }
        KeyCode::Char('E') => {
            app.settings = None;
            return Some(Job::EditConfig);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            dialog.sel = (dialog.sel + 1).min(last);
            return None;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            dialog.sel = dialog.sel.saturating_sub(1);
            return None;
        }
        KeyCode::PageDown => {
            dialog.sel = (dialog.sel + 10).min(last);
            return None;
        }
        KeyCode::PageUp => {
            dialog.sel = dialog.sel.saturating_sub(10);
            return None;
        }
        KeyCode::Home => {
            dialog.sel = 0;
            return None;
        }
        KeyCode::End => {
            dialog.sel = last;
            return None;
        }
        KeyCode::Enter if matches!(kind, Kind::Toggle | Kind::Choice) => Action::Next,
        KeyCode::Enter => {
            let (_, initial) = field.prompt(&app.ctx.config)?;
            app.prompt = Some((Prompt::Setting(field), initial));
            return None;
        }
        KeyCode::Char(' ') if matches!(kind, Kind::Toggle | Kind::Choice) => Action::Next,
        KeyCode::Right | KeyCode::Char('l') if kind != Kind::Text && kind != Kind::Alias => {
            Action::Next
        }
        KeyCode::Left | KeyCode::Char('h') if kind != Kind::Text && kind != Kind::Alias => {
            Action::Prev
        }
        KeyCode::Char('d') | KeyCode::Delete
            if matches!(
                field,
                settings::Field::Alias(_)
                    | settings::Field::PluginStrategy(_)
                    | settings::Field::Editor
            ) =>
        {
            Action::Delete
        }
        _ => return None,
    };
    if kind == Kind::AddAlias {
        return None;
    }
    apply_setting(app, &field, action);
    None
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    use settings::Kind;
    let Some(dialog) = &app.settings else {
        return;
    };
    let rows = settings_rows(app);
    let sel = dialog.sel.min(rows.len().saturating_sub(1));
    let key =
        |k: &'static str| Span::styled(k, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut body: Vec<Line> = Vec::new();
    let mut sel_line = 0;
    let mut section = "";
    for (i, row) in rows.iter().enumerate() {
        if row.section != section {
            if !section.is_empty() {
                body.push(Line::raw(""));
            }
            section = row.section;
            body.push(Line::styled(
                section,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        if i == sel {
            sel_line = body.len();
        }
        let style = if i == sel {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        body.push(
            Line::from(vec![
                Span::raw(format!("  {}", pad(&util::sanitize(&row.label), 34))),
                Span::styled(util::sanitize(&row.value), muted()),
            ])
            .style(style),
        );
    }
    let height = (area.height.saturating_sub(10) as usize).clamp(5, 30);
    let start = sel_line
        .saturating_sub(height / 2)
        .min(body.len().saturating_sub(height));
    let mut lines: Vec<Line> = body.into_iter().skip(start).take(height).collect();
    lines.push(Line::raw(""));
    if let Some(err) = &dialog.error {
        lines.push(Line::styled(
            util::truncate(&util::sanitize(err), 84),
            Style::default().fg(BAD),
        ));
    }
    let mut hint = match rows.get(sel).map(|r| r.field.kind()) {
        Some(Kind::Number) => vec![
            key("↵"),
            Span::raw(" type   "),
            key("←→"),
            Span::raw(" change   "),
        ],
        Some(Kind::Toggle) => vec![key("↵"), Span::raw(" on/off   ")],
        Some(Kind::Choice) => vec![key("↵ ←→"), Span::raw(" change   ")],
        Some(Kind::Text) => vec![
            key("↵"),
            Span::raw(" type   "),
            key("d"),
            Span::raw(" default   "),
        ],
        Some(Kind::Alias) => vec![
            key("↵"),
            Span::raw(" edit   "),
            key("d"),
            Span::raw(" delete   "),
        ],
        Some(Kind::AddAlias) | None => vec![key("↵"), Span::raw(" add   ")],
    };
    if matches!(
        rows.get(sel).map(|r| &r.field),
        Some(settings::Field::PluginStrategy(_))
    ) {
        hint.extend([key("d"), Span::raw(" default   ")]);
    }
    hint.extend([
        key("E"),
        Span::raw(" editor   "),
        key("Esc"),
        Span::raw(" close"),
    ]);
    lines.push(Line::from(hint));
    lines.push(Line::styled(
        util::truncate(&paths::pm_config_file().display().to_string(), 84),
        muted(),
    ));
    let rect = centered(area, 90, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Settings ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The work of a task, run on its worker thread.
type Operation = Box<dyn FnOnce(&Ctx) -> Result<()> + Send>;

fn start_task(
    app: &mut App,
    title: String,
    touched: Option<Vec<String>>,
    batch: bool,
    yes: bool,
    run: Operation,
) {
    let accept_all = Arc::new(AtomicBool::new(false));
    let ctx = Ctx {
        herdr: app.ctx.herdr.clone(),
        config: app.ctx.config.clone(),
        yes,
        dry_run: app.ctx.dry_run,
        report: Arc::new(TaskReport {
            tx: app.tx.clone(),
            accept_all: Arc::clone(&accept_all),
        }),
    };
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let result = run(&ctx).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Msg::Task(TaskEvent::Done(result)));
    });
    app.message = None;
    app.task = Some(Task {
        title,
        started: Instant::now(),
        step: "starting".into(),
        log: Vec::new(),
        progress: None,
        review: None,
        show_log: false,
        done: None,
        batch,
        accept_all,
        touched,
    });
}

fn task_event(app: &mut App, event: TaskEvent) {
    let Some(task) = app.task.as_mut() else {
        return;
    };
    match event {
        TaskEvent::Step(text) => {
            task.log.push(format!("› {text}"));
            task.step = text;
        }
        TaskEvent::Line(text) => {
            task.log.push(text);
            if task.log.len() > 5000 {
                task.log.drain(..1000);
            }
        }
        TaskEvent::Review(review) => task.review = Some(review),
        TaskEvent::Progress(done, total) => task.progress = Some((done, total)),
        TaskEvent::Done(Ok(())) => finish_task(app),
        TaskEvent::Done(result) => {
            task.show_log = true;
            task.done = Some(result);
        }
    }
}

/// Closes the task dialog and reports how it ended in the status line.
fn finish_task(app: &mut App) {
    let Some(task) = app.task.take() else {
        return;
    };
    app.reload();
    match &task.touched {
        Some(ids) => app.start_checks(Some(ids)),
        None => app.start_checks(None),
    }
    match task.done {
        Some(Err(err)) => app.say(Level::Error, util::truncate(&err, 200)),
        _ => {
            let summary = task
                .log
                .iter()
                .rev()
                .find(|l| l.starts_with('✓') || l.as_str() == "cancelled")
                .cloned()
                .unwrap_or_else(|| format!("{}: done", task.title));
            let level = if summary == "cancelled" {
                Level::Info
            } else {
                Level::Ok
            };
            app.say(level, summary.trim_start_matches("✓ ").to_string());
        }
    }
}

fn task_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    let task = app.task.as_mut()?;
    if let Some(review) = task.review.as_mut() {
        let answer = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
            KeyCode::Char('a') | KeyCode::Char('A') if task.batch => {
                task.accept_all.store(true, Ordering::Relaxed);
                Some(true)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
            KeyCode::Down | KeyCode::Char('j') => {
                review.scroll += 1;
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                review.scroll = review.scroll.saturating_sub(1);
                None
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                review.scroll += 10;
                None
            }
            KeyCode::PageUp => {
                review.scroll = review.scroll.saturating_sub(10);
                None
            }
            _ => None,
        };
        if let Some(answer) = answer {
            let review = task.review.take().unwrap();
            let _ = review.reply.send(answer);
            task.step = if answer { "approved" } else { "declined" }.into();
        }
        return None;
    }
    match key.code {
        KeyCode::Char('l') | KeyCode::Char('L') => task.show_log = !task.show_log,
        _ if task.done.is_some() => finish_task(app),
        _ => {}
    }
    None
}

// ---------------------------------------------------------------- input

fn handle_mouse(app: &mut App, kind: MouseEventKind, col: u16, row: u16) -> bool {
    let inside = |area: Rect| {
        col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
    };
    match kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
            let delta = if kind == MouseEventKind::ScrollDown {
                1
            } else {
                -1
            };
            if let Some(review) = app.task.as_mut().and_then(|t| t.review.as_mut()) {
                review.scroll = review.scroll.saturating_add_signed(delta * 3);
            } else if app.task.is_some() {
                // The task dialog covers the list.
            } else if app.help {
                app.help_scroll = app.help_scroll.saturating_add_signed(delta * 3);
            } else if let Some(picker) = &mut app.picker {
                picker.sel = (picker.sel as isize + delta)
                    .clamp(0, picker.picks.len().saturating_sub(1) as isize)
                    as usize;
            } else if app.settings.is_some() {
                let last = settings_rows(app).len().saturating_sub(1);
                if let Some(dialog) = app.settings.as_mut() {
                    dialog.sel = (dialog.sel as isize + delta).clamp(0, last as isize) as usize;
                }
            } else if let Some(cleaner) = &mut app.cleaner {
                cleaner.sel = (cleaner.sel as isize + delta)
                    .clamp(0, cleaner.orphans.len().saturating_sub(1) as isize)
                    as usize;
            } else {
                move_sel(app, delta);
            }
            true
        }
        MouseEventKind::Down(MouseButton::Left)
            if app.help
                || app.picker.is_some()
                || app.cleaner.is_some()
                || app.settings.is_some()
                || app.task.is_some() =>
        {
            false
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(&(_, view)) = app.tab_areas.iter().find(|(area, _)| inside(*area)) {
                switch_view(app, view);
                return true;
            }
            let area = app.list_area;
            if !inside(area) {
                return false;
            }
            let line = (row - area.y) as usize;
            match app.view {
                View::Installed => {
                    let idx = app.offset + line;
                    if idx < app.items.len() {
                        app.sel = idx;
                    }
                }
                View::Market => {
                    let page_start = (app.market.sel / app.market.page_size.max(1))
                        * app.market.page_size.max(1);
                    let Some(line) = line.checked_sub(app.market_header) else {
                        return false;
                    };
                    let idx = page_start + line / 2;
                    if idx < app.market.entries.len() {
                        app.market.sel = idx;
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn move_sel(app: &mut App, delta: isize) {
    match app.view {
        View::Installed => {
            if app.items.is_empty() {
                return;
            }
            let max = app.items.len() as isize - 1;
            app.sel = (app.sel as isize + delta).clamp(0, max) as usize;
        }
        View::Market => {
            if app.market.entries.is_empty() {
                return;
            }
            let max = app.market.entries.len() as isize - 1;
            app.market.sel = (app.market.sel as isize + delta).clamp(0, max) as usize;
        }
    }
}

fn switch_view(app: &mut App, view: View) {
    app.view = view;
    if view == View::Market && !app.market.loaded && !app.market.loading {
        app.load_market(false);
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return None;
    }
    if app.task.is_some() {
        return task_key(app, key);
    }
    if app.help {
        match key.code {
            KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc => app.help = false,
            KeyCode::Down | KeyCode::Char('j') => app.help_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => app.help_scroll = app.help_scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => app.help_scroll += 10,
            KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
            KeyCode::Home | KeyCode::Char('g') => app.help_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => app.help_scroll = usize::MAX,
            _ => {}
        }
        return None;
    }
    if app.picker.is_some() {
        return picker_key(app, key);
    }
    if app.cleaner.is_some() {
        return cleaner_key(app, key);
    }
    if let Some(confirm) = app.confirm.take() {
        return answer_confirm(app, confirm, key);
    }
    if let Some((kind, mut buffer)) = app.prompt.take() {
        match key.code {
            KeyCode::Esc => {
                if kind == Prompt::Filter {
                    app.filter.clear();
                    app.rebuild_items();
                    app.clamp();
                }
                return None;
            }
            KeyCode::Enter => return submit_prompt(app, kind, buffer),
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => buffer.clear(),
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let trimmed = buffer.trim_end().to_string();
                let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                buffer.truncate(cut);
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        if kind == Prompt::Filter {
            app.filter = buffer.clone();
            app.rebuild_items();
            app.sel = 0;
        }
        app.prompt = Some((kind, buffer));
        return None;
    }
    if app.settings.is_some() {
        return settings_key(app, key);
    }
    app.message = None;
    match key.code {
        KeyCode::Char('?') => {
            app.help = true;
            app.help_scroll = 0;
            return None;
        }
        KeyCode::Char('q') => {
            if app.view == View::Market {
                app.view = View::Installed;
            } else {
                app.quit = true;
            }
            return None;
        }
        KeyCode::Esc => {
            if app.view == View::Market {
                app.view = View::Installed;
            } else if !app.filter.is_empty() {
                app.filter.clear();
                app.rebuild_items();
                app.clamp();
            } else {
                app.quit = true;
            }
            return None;
        }
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('m') => {
            let next = if app.view == View::Installed {
                View::Market
            } else {
                View::Installed
            };
            switch_view(app, next);
            return None;
        }
        KeyCode::Char('C') => {
            app.settings = Some(SettingsDialog {
                sel: 0,
                error: None,
            });
            return None;
        }
        KeyCode::Char('E') => return Some(Job::EditConfig),
        KeyCode::Down | KeyCode::Char('j') => move_sel(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_sel(app, -1),
        KeyCode::PageDown => move_sel(app, 10),
        KeyCode::PageUp => move_sel(app, -10),
        KeyCode::Home | KeyCode::Char('g') => move_sel(app, -100_000),
        KeyCode::End | KeyCode::Char('G') => move_sel(app, 100_000),
        _ => {
            return match app.view {
                View::Installed => installed_key(app, key),
                View::Market => market_key(app, key),
            };
        }
    }
    None
}

fn installed_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    let row = app.selected_row().cloned();
    let id = row.as_ref().map(|r| r.plugin.plugin_id.clone());
    match key.code {
        KeyCode::Char('u') => {
            let (row, id) = (row?, id?);
            match app.status_of(&id) {
                _ if row.origin == Origin::Local => app.say(
                    Level::Info,
                    format!(
                        "{} is a local link; update it in {}",
                        row.plugin.name, row.plugin.plugin_root
                    ),
                ),
                Some(Check::Done(Status::Pinned)) => app.say(
                    Level::Info,
                    format!("{} is pinned; press p to unpin", row.plugin.name),
                ),
                Some(Check::Done(Status::Current)) => {
                    app.say(Level::Ok, format!("{} is up to date", row.plugin.name))
                }
                Some(Check::Pending) => app.say(Level::Info, "still checking…"),
                _ => return Some(Job::Update(vec![id])),
            }
        }
        KeyCode::Char('U') => {
            let ids = app.outdated_ids();
            if ids.is_empty() {
                if app.checks.values().any(|c| matches!(c, Check::Pending)) {
                    app.say(Level::Info, "still checking for updates…");
                } else {
                    app.say(Level::Ok, "everything is up to date");
                }
            } else {
                app.confirm = Some(Confirm::UpdateAll(ids));
            }
        }
        KeyCode::Char('A') => {
            let row = row?;
            if row.origin == Origin::Local {
                app.say(Level::Info, "local links have no remote to update from");
            } else if row.auto_update {
                match ops::set_auto_update(&app.ctx, &row.plugin.plugin_id, false) {
                    Ok(()) => {
                        app.reload();
                        app.say(
                            Level::Ok,
                            format!("auto-update off for {}", row.plugin.name),
                        );
                    }
                    Err(err) => app.say(Level::Warn, format!("{err:#}")),
                }
            } else {
                app.confirm = Some(Confirm::AutoUpdateOn {
                    id: row.plugin.plugin_id.clone(),
                    name: row.plugin.name.clone(),
                });
            }
        }
        KeyCode::Char('e') => {
            let row = row?;
            let enable = !row.plugin.enabled;
            match app.ctx.herdr.set_enabled(&row.plugin.plugin_id, enable) {
                Ok(()) => {
                    app.reload();
                    if !app.ctx.dry_run {
                        app.say(
                            Level::Ok,
                            format!(
                                "{} {}",
                                if enable { "enabled" } else { "disabled" },
                                row.plugin.name
                            ),
                        );
                    }
                }
                Err(err) => app.say(Level::Error, format!("{err:#}")),
            }
        }
        KeyCode::Char('x') | KeyCode::Delete => {
            let row = row?;
            if row.plugin.plugin_id == paths::PLUGIN_ID {
                app.say(
                    Level::Warn,
                    format!(
                        "use `herdr-pm uninstall {}` from a shell to remove the manager itself",
                        paths::PLUGIN_ID
                    ),
                );
            } else {
                app.confirm = Some(Confirm::Remove {
                    id: row.plugin.plugin_id.clone(),
                    origin: row.origin,
                    path: row.plugin.plugin_root.clone(),
                });
            }
        }
        KeyCode::Char('z') => {
            let row = row?;
            if row.origin != Origin::Git {
                app.say(
                    Level::Info,
                    "rollback works for plugins installed from git by this manager",
                );
            } else if row.record.as_ref().is_none_or(|r| r.previous.is_empty()) {
                app.say(Level::Info, "no earlier installation kept");
            } else {
                return Some(Job::Rollback(row.plugin.plugin_id.clone()));
            }
        }
        KeyCode::Char('p') => {
            let row = row?;
            if row.origin == Origin::Local {
                app.say(Level::Info, "local links cannot be pinned");
            } else {
                open_picker(app, row);
            }
        }
        KeyCode::Char('s') => {
            let row = row?;
            if row.origin == Origin::Local {
                app.say(Level::Info, "local links have no remote to switch");
            } else {
                app.prompt = Some((Prompt::Switch(row.plugin.plugin_id.clone()), String::new()));
            }
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.prompt = Some((Prompt::Install, String::new()))
        }
        KeyCode::Char('/') => app.prompt = Some((Prompt::Filter, app.filter.clone())),
        KeyCode::Char('o') => {
            let row = row?;
            match row.web_url() {
                Some(url) => match util::open_url(&url) {
                    Ok(()) => app.say(Level::Info, format!("opened {url}")),
                    Err(err) => app.say(Level::Error, format!("{err:#}")),
                },
                None => app.say(
                    Level::Info,
                    format!("no web page for {}", row.source_display()),
                ),
            }
        }
        KeyCode::Char('X') => {
            open_cleaner(app);
            return None;
        }
        KeyCode::Char('r') => {
            app.reload();
            app.start_checks(None);
            app.say(Level::Info, "refreshed");
        }
        _ => {}
    }
    None
}

fn market_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    let page = app.market.page_size.max(1);
    match key.code {
        KeyCode::Right | KeyCode::Char('l') => {
            let n = app.market.entries.len();
            if n > 0 {
                let next = (app.market.sel / page + 1) * page;
                app.market.sel = if next >= n { 0 } else { next };
            }
        }
        KeyCode::Left | KeyCode::Char('h') => {
            let n = app.market.entries.len();
            if n > 0 {
                let cur = app.market.sel / page;
                app.market.sel = if cur == 0 {
                    ((n - 1) / page) * page
                } else {
                    (cur - 1) * page
                };
            }
        }
        KeyCode::Char('/') => app.prompt = Some((Prompt::Search, app.market.query.clone())),
        KeyCode::Char('s') => {
            app.market.sort = app.market.sort.next();
            market::sort_entries(&mut app.market.entries, app.market.sort);
            app.market.sel = 0;
        }
        KeyCode::Char('f') => {
            let names: Vec<String> = app
                .ctx
                .config
                .marketplace
                .iter()
                .filter(|s| s.enabled)
                .map(|s| s.name.clone())
                .collect();
            app.market.source = match &app.market.source {
                None => names.first().cloned(),
                Some(cur) => {
                    let pos = names
                        .iter()
                        .position(|n| n == cur)
                        .map(|p| p + 1)
                        .unwrap_or(0);
                    names.get(pos).cloned()
                }
            };
            app.market.sel = 0;
            app.load_market(false);
        }
        KeyCode::Char('r') => {
            app.market.announce = true;
            app.load_market(true);
            app.say(Level::Info, "refreshing marketplace…");
        }
        KeyCode::Char('o') => {
            if let Some(e) = app.market.entries.get(app.market.sel) {
                match e.web_url.clone() {
                    Some(url) => {
                        if let Err(err) = util::open_url(&url) {
                            app.say(Level::Error, format!("{err:#}"));
                        }
                    }
                    None => app.say(Level::Info, "no web page for this entry"),
                }
            }
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.prompt = Some((Prompt::Install, String::new()))
        }
        KeyCode::Enter => {
            let entry = app.market.entries.get(app.market.sel).cloned()?;
            if let Some(row) = app.entry_installed(&entry) {
                app.say(
                    Level::Info,
                    format!(
                        "{} is already installed (as {})",
                        entry.title, row.plugin.plugin_id
                    ),
                );
            } else {
                return Some(Job::InstallEntry(Box::new(entry)));
            }
        }
        _ => {}
    }
    None
}

fn submit_prompt(app: &mut App, kind: Prompt, buffer: String) -> Option<Job> {
    let text = buffer.trim().to_string();
    match kind {
        Prompt::Filter => {
            app.filter = text;
            app.rebuild_items();
            app.clamp();
            None
        }
        Prompt::Search => {
            app.market.query = text;
            app.market.sel = 0;
            app.load_market(false);
            None
        }
        Prompt::Install => {
            if text.is_empty() {
                None
            } else {
                Some(Job::Install(text))
            }
        }
        Prompt::Switch(id) => {
            if text.is_empty() {
                None
            } else {
                Some(Job::Switch(id, text))
            }
        }
        Prompt::Setting(field) => {
            apply_setting(app, &field, settings::Action::Set(text));
            None
        }
        Prompt::Purge(id) => {
            if text == id {
                Some(Job::Uninstall(id, true))
            } else {
                app.say(Level::Info, "cancelled: the id did not match");
                None
            }
        }
    }
}

fn answer_confirm(app: &mut App, confirm: Confirm, key: KeyEvent) -> Option<Job> {
    let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
    let job = match confirm {
        Confirm::Remove { id, origin, .. } => match key.code {
            KeyCode::Char('u') => Some(Job::Uninstall(id, false)),
            KeyCode::Char('d') if origin == Origin::Local => {
                app.prompt = Some((Prompt::Purge(id), String::new()));
                return None;
            }
            KeyCode::Char('d') => Some(Job::Uninstall(id, true)),
            _ => None,
        },
        Confirm::UpdateAll(ids) if yes => Some(Job::Update(ids)),
        Confirm::AutoUpdateOn { id, name } if yes => {
            match ops::set_auto_update(&app.ctx, &id, true) {
                Ok(()) => {
                    app.reload();
                    if !app.ctx.dry_run {
                        app.say(Level::Ok, format!("auto-update on for {name}"));
                    }
                }
                Err(err) => app.say(Level::Warn, format!("{err:#}")),
            }
            return None;
        }
        _ => None,
    };
    if job.is_none() {
        app.say(Level::Info, "cancelled");
    }
    job
}

// ---------------------------------------------------------------- drawing

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

fn spinner() -> &'static str {
    let tick = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        / 120) as usize;
    SPINNER[tick % SPINNER.len()]
}

fn pad(text: &str, width: usize) -> String {
    let t = util::truncate(&util::sanitize(text), width);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(width.saturating_sub(n)))
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let detail_height = if area.height >= 24 {
        6
    } else if area.height >= 16 {
        4
    } else {
        0
    };
    let [header, body, detail, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(detail_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, app, header);
    match app.view {
        View::Installed => {
            draw_installed(f, app, body);
            if detail_height > 0 {
                draw_installed_detail(f, app, detail);
            }
        }
        View::Market => {
            draw_market(f, app, body);
            if detail_height > 0 {
                draw_market_detail(f, app, detail);
            }
        }
    }
    draw_status(f, app, status);
    draw_footer(f, app, footer);
    if app.help {
        draw_help(f, app, area);
    }
    if app.picker.is_some() {
        draw_picker(f, app, area);
    }
    if app.cleaner.is_some() {
        draw_cleaner(f, app, area);
    }
    if app.settings.is_some() {
        draw_settings(f, app, area);
    }
    if let Some(confirm) = &app.confirm {
        draw_confirm(f, confirm, area);
    }
    if app.task.is_some() {
        draw_task(f, app, area);
    }
}

fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    let tab = |label: String, active: bool| {
        if active {
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), muted())
        }
    };
    let updates = app.outdated_ids().len();
    let installed_label = if updates > 0 {
        format!("Installed {} · ↑{updates}", app.rows.len())
    } else {
        format!("Installed {}", app.rows.len())
    };
    let market_label = if app.market.loaded {
        format!("Marketplace {}", app.market.entries.len())
    } else {
        "Marketplace".to_string()
    };
    let tabs = [
        tab(installed_label, app.view == View::Installed),
        tab(market_label, app.view == View::Market),
    ];
    let mut spans = Vec::new();
    let mut x = area.x;
    app.tab_areas.clear();
    for (span, view) in tabs.into_iter().zip([View::Installed, View::Market]) {
        let width = span.content.chars().count() as u16;
        app.tab_areas.push((
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            view,
        ));
        x += width + 1;
        spans.push(span);
        spans.push(Span::raw(" "));
    }
    if app.ctx.dry_run {
        let right = "DRY RUN";
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let gap = (area.width as usize).saturating_sub(used + right.len() + 1);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(right, Style::default().fg(WARN)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn status_span(app: &App, row: &Row) -> Span<'static> {
    if row.origin == Origin::Local {
        return Span::styled("local", muted());
    }
    match app.checks.get(&row.plugin.plugin_id) {
        None => Span::raw(""),
        Some(Check::Pending) => Span::styled(spinner().to_string(), muted()),
        Some(Check::Done(status)) => match status {
            Status::Current => Span::styled("✓", Style::default().fg(OK)),
            Status::Available { commit, version } => Span::styled(
                format!(
                    "↑ {}",
                    util::sanitize(
                        &version
                            .clone()
                            .filter(|v| *v != row.plugin.version)
                            .unwrap_or_else(|| util::short_sha(commit).to_string())
                    )
                ),
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
            Status::Pinned => Span::raw(""),
            Status::Local => Span::styled("local", muted()),
            Status::Unknown => Span::styled("?", muted()),
            Status::Error { .. } => Span::styled("! check failed", Style::default().fg(BAD)),
        },
    }
}

fn draw_installed(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(muted());
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.list_area = inner;
    let height = inner.height as usize;
    if let Some(err) = &app.load_error {
        let p = Paragraph::new(vec![
            Line::styled(
                "could not read the plugin list from herdr:",
                Style::default().fg(BAD),
            ),
            Line::raw(util::sanitize(err)),
        ])
        .wrap(Wrap { trim: true });
        f.render_widget(p, inner);
        return;
    }
    if app.items.is_empty() {
        let text = if app.rows.is_empty() {
            "No plugins installed. Press m to browse the marketplace or i to install from a URL."
        } else {
            "Nothing matches the filter (Esc clears it)."
        };
        f.render_widget(
            Paragraph::new(Line::styled(text, muted())).wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    if app.sel < app.offset {
        app.offset = app.sel;
    }
    if height > 0 && app.sel >= app.offset + height {
        app.offset = app.sel + 1 - height;
    }
    let width = inner.width as usize;
    let name_w = app
        .rows
        .iter()
        .map(|r| r.plugin.name.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(8, (width / 3).max(8));
    let mut lines = Vec::new();
    for (pos, &i) in app.items.iter().enumerate() {
        if pos < app.offset || pos >= app.offset + height {
            continue;
        }
        let base = if pos == app.sel {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let row = &app.rows[i];
        let dot = if row.plugin.enabled {
            Span::styled("● ", Style::default().fg(OK))
        } else {
            Span::styled("○ ", muted())
        };
        let origin_color = match row.origin {
            Origin::Git => Color::Magenta,
            Origin::Github => Color::Blue,
            Origin::Local => MUTED,
        };
        let status = status_span(app, row);
        let left = format!("{:>2} ", pos + 1);
        let used = left.len() + 2 + name_w + 1 + 9 + 1 + 7 + 1 + 1 + 4;
        let status_w = status.width();
        let src_w = width.saturating_sub(used + status_w + 2);
        let badge = |on: bool, glyph: &'static str, style: Style| {
            if on {
                Span::styled(glyph, style)
            } else {
                Span::raw("  ")
            }
        };
        let mut spans = vec![
            Span::styled(left, muted()),
            dot,
            Span::raw(pad(&row.plugin.name, name_w)),
            Span::raw(" "),
            Span::styled(pad(&row.plugin.version, 9), muted()),
            Span::styled(
                pad(row.origin.label(), 7),
                Style::default().fg(origin_color),
            ),
            Span::raw(" "),
            Span::styled(pad(&row.source_display(), src_w), muted()),
            Span::raw(" "),
            badge(row.auto_update, "⟳ ", Style::default().fg(ACCENT)),
            badge(row.pinned(), "📌", Style::default()),
            status,
        ];
        // Width in cells: 📌 takes two.
        let used_now: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used_now))));
        lines.push(Line::from(spans).style(base));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Width of the labels in the detail panels (`description`, `auto-update`).
const LABEL: usize = 11;

fn kv(key: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<LABEL$} "), muted()),
        Span::raw(util::sanitize(&value.into())),
    ])
}

fn draw_installed_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            row.plugin.plugin_id.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} · {}",
                if row.plugin.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                row.origin.label()
            ),
            muted(),
        ),
    ])];
    match row.origin {
        Origin::Local => lines.push(kv("path", row.plugin.plugin_root.clone())),
        _ => {
            lines.push(kv("source", row.source_display()));
            let branch = app.branches.get(&row.plugin.plugin_id).map(String::as_str);
            let mut text = row.ref_label(branch);
            if let Some(r) = &row.record {
                text.push_str(&format!(
                    "  · installed {}{}",
                    util::relative_age(r.current.installed_unix),
                    if r.previous.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} kept for rollback", r.previous.len())
                    }
                ));
            }
            lines.push(kv("ref", text));
            let strategy = app
                .ctx
                .config
                .auto_update
                .strategy_for(&row.plugin.plugin_id);
            lines.push(kv(
                "auto-update",
                if row.auto_update {
                    format!(
                        "on · {}: {} when Herdr starts (A turns it off)",
                        strategy.label(),
                        strategy.describe()
                    )
                } else {
                    "off (A turns it on)".to_string()
                },
            ));
        }
    }
    match app.checks.get(&row.plugin.plugin_id) {
        Some(Check::Done(Status::Error { message })) => lines.push(Line::from(vec![
            Span::styled(format!("{:<LABEL$} ", "check"), muted()),
            Span::styled(util::sanitize(message), Style::default().fg(BAD)),
        ])),
        Some(Check::Done(Status::Available { commit, version })) => lines.push(Line::from(vec![
            Span::styled(format!("{:<LABEL$} ", "update"), muted()),
            Span::styled(
                format!(
                    "{} → {} ({}) — press u",
                    util::sanitize(&row.plugin.version),
                    util::sanitize(&version.clone().unwrap_or_else(|| "new commit".into())),
                    util::short_sha(commit)
                ),
                Style::default().fg(WARN),
            ),
        ])),
        _ => {}
    }
    if let Some(d) = row
        .plugin
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        lines.push(kv("description", d));
    }
    for w in &row.plugin.warnings {
        lines.push(Line::styled(
            util::sanitize(&format!("warning: {w}")),
            Style::default().fg(WARN),
        ));
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }),
        pad_rect(area),
    );
}

fn pad_rect(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    }
}

fn draw_market(f: &mut Frame, app: &mut App, area: Rect) {
    let mut title = format!(
        " {} ",
        app.market.source.as_deref().unwrap_or("all sources")
    );
    if !app.market.query.is_empty() {
        title.push_str(&format!("· \"{}\" ", app.market.query));
    }
    title.push_str(&format!("· sort: {} ", app.market.sort.label()));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(muted())
        .title(Span::styled(title, muted()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.list_area = inner;
    let height = inner.height as usize;

    let mut lines: Vec<Line> = Vec::new();
    if app.market.loading && app.market.entries.is_empty() {
        lines.push(Line::styled(
            format!("{} loading marketplace…", spinner()),
            muted(),
        ));
    }
    for (name, err) in &app.market.errors {
        lines.push(Line::styled(
            util::truncate(
                &util::sanitize(&format!("✗ {name}: {err}")),
                inner.width as usize,
            ),
            Style::default().fg(BAD),
        ));
    }
    let entries = app.market_visible();
    if entries.is_empty() && !app.market.loading && app.market.loaded {
        lines.push(Line::styled(
            "No plugins found. / searches, f picks a source, r refreshes.",
            muted(),
        ));
    }
    let total = entries.len();
    let header_lines = lines.len();
    let page_size = (height.saturating_sub(header_lines + 1) / 2).max(1);
    let page = app.market.sel / page_size;
    let start = page * page_size;
    let width = inner.width as usize;
    for (offset, entry) in entries.iter().skip(start).take(page_size).enumerate() {
        let idx = start + offset;
        let selected = idx == app.market.sel;
        let installed = app.entry_installed(entry).is_some();
        let base = if selected {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // ★ is ambiguous-width: many terminals draw it two cells wide, so a
        // space keeps it off the first digit.
        let stars = entry.stars.map(|s| format!("★ {s}")).unwrap_or_default();
        let age = util::relative_age(entry.updated_unix);
        let right = format!("{stars:>7} {age:>8}");
        let mark = if installed {
            Span::styled("✓ ", Style::default().fg(OK))
        } else {
            Span::raw("  ")
        };
        let title_w = width.saturating_sub(6 + 2 + right.chars().count() + entry.market.len() + 5);
        let mut spans = vec![
            Span::styled(format!("{:>4} ", idx + 1), muted()),
            mark,
            Span::raw(pad(&entry.title, title_w)),
            Span::styled(
                format!(" [{}]", entry.market),
                Style::default().fg(Color::Blue),
            ),
        ];
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let right_w = right.chars().count();
        spans.push(Span::raw(
            " ".repeat(width.saturating_sub(used + right_w + 1)),
        ));
        spans.push(Span::styled(right, Style::default().fg(WARN)));
        lines.push(Line::from(spans).style(base));
        let desc = if entry.description.is_empty() {
            "—".to_string()
        } else {
            entry.description.clone()
        };
        lines.push(
            Line::styled(
                format!("        {}", util::truncate(&desc, width.saturating_sub(9))),
                muted(),
            )
            .style(base),
        );
    }
    if total > 0 {
        let pages = total.div_ceil(page_size);
        let loading = if app.market.loading {
            format!(" {} ", spinner())
        } else {
            String::new()
        };
        let footer = format!("page {}/{pages} · {total} plugins{loading}", page + 1);
        while lines.len() + 1 < height {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(footer, muted()));
    }
    f.render_widget(Paragraph::new(lines), inner);
    app.market_header = header_lines;
    app.market.page_size = page_size;
}

fn draw_market_detail(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(entry) = app.market.entries.get(app.market.sel) {
        let mut head = vec![Span::styled(
            util::sanitize(&entry.title),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        if let Some(v) = &entry.version {
            head.push(Span::styled(format!("  {}", util::sanitize(v)), muted()));
        }
        if let Some(id) = &entry.plugin_id {
            head.push(Span::styled(format!("  ({})", util::sanitize(id)), muted()));
        }
        lines.push(Line::from(head));
        if !entry.description.trim().is_empty() {
            lines.push(kv("description", entry.description.clone()));
        }
        lines.push(kv(
            "install",
            format!("herdr-pm install {}", entry.install_spec()),
        ));
        let mut meta = Vec::new();
        if let Some(p) = &entry.platforms {
            meta.push(p.join(", "));
        }
        if let Some(m) = &entry.min_herdr_version {
            meta.push(format!("herdr >= {m}"));
        }
        if let Some(r) = &entry.git_ref {
            meta.push(format!("ref {r}"));
        }
        if !meta.is_empty() {
            lines.push(kv("info", meta.join(" · ")));
        }
        if let Some(row) = app.entry_installed(entry) {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<LABEL$} ", "installed"), muted()),
                Span::styled(
                    format!(
                        "{} {}",
                        util::sanitize(&row.plugin.plugin_id),
                        util::sanitize(&row.plugin.version)
                    ),
                    Style::default().fg(OK),
                ),
            ]));
        }
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }),
        pad_rect(area),
    );
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    if let Some((kind, buffer)) = &app.prompt {
        let label = match kind {
            Prompt::Install => {
                "install (url, owner/repo, alias:path [--ref R] [--subdir D]): ".to_string()
            }
            Prompt::Switch(id) => format!("switch {id} to branch/tag/commit: "),
            Prompt::Purge(id) => format!("type {id} to delete it and its directory: "),
            Prompt::Setting(field) => field
                .prompt(&app.ctx.config)
                .map(|(label, _)| label)
                .unwrap_or_default(),
            Prompt::Filter => "filter: ".to_string(),
            Prompt::Search => "search marketplace: ".to_string(),
        };
        let line = Line::from(vec![
            Span::styled(label, Style::default().fg(ACCENT)),
            Span::raw(buffer.clone()),
            Span::styled("▏", Style::default().fg(ACCENT)),
        ]);
        f.render_widget(Paragraph::new(line), pad_rect(area));
        return;
    }
    let line = match &app.message {
        Some((level, text)) => {
            let color = match level {
                Level::Info => Color::Reset,
                Level::Ok => OK,
                Level::Warn => WARN,
                Level::Error => BAD,
            };
            Line::styled(
                util::truncate(text, area.width as usize),
                Style::default().fg(color),
            )
        }
        None if !app.filter.is_empty() && app.view == View::Installed => {
            Line::styled(format!("filter: {}  (Esc clears)", app.filter), muted())
        }
        None => Line::raw(""),
    };
    f.render_widget(Paragraph::new(line), pad_rect(area));
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = match app.view {
        View::Installed => &[
            ("u", "update"),
            ("U", "update all"),
            ("e", "enable/disable"),
            ("A", "auto-update"),
            ("x", "remove"),
            ("i", "install"),
            ("m", "marketplace"),
            ("?", "help"),
            ("q", "quit"),
        ],
        View::Market => &[
            ("↵", "install"),
            ("/", "search"),
            ("←→", "page"),
            ("s", "sort"),
            ("f", "switch source"),
            ("o", "open"),
            ("r", "refresh"),
            ("q", "back"),
        ],
    };
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (k, label) in keys {
        let piece = k.chars().count() + label.chars().count() + 3;
        if used + piece > area.width as usize {
            break;
        }
        used += piece;
        spans.push(Span::styled(
            k.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}  "), muted()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), pad_rect(area));
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_confirm(f: &mut Frame, confirm: &Confirm, area: Rect) {
    let (title, body, hint): (&str, Vec<String>, Vec<Span>) = match confirm {
        Confirm::Remove { id, origin, path } => {
            let (keep, delete) = match origin {
                Origin::Local => (
                    format!("unlink it; {path} stays on disk"),
                    format!("also delete {path} (you type the id to confirm)"),
                ),
                Origin::Github => (
                    "uninstall it with Herdr".to_string(),
                    "also delete leftover checkouts, its config and settings".to_string(),
                ),
                Origin::Git => (
                    "unregister it; installations go to the trash for a day".to_string(),
                    "delete installations, cached repository, config and settings now".to_string(),
                ),
            };
            (
                "Remove",
                vec![
                    format!("Remove {id}?"),
                    format!("  u  {keep}"),
                    format!("  d  {delete}"),
                ],
                vec![
                    Span::styled("u", Style::default().fg(OK).add_modifier(Modifier::BOLD)),
                    Span::raw(" remove   "),
                    Span::styled("d", Style::default().fg(BAD).add_modifier(Modifier::BOLD)),
                    Span::raw(" delete completely   "),
                    Span::styled("any other key", muted()),
                    Span::raw(" cancels"),
                ],
            )
        }
        Confirm::AutoUpdateOn { name, .. } => (
            "Auto-update",
            vec![
                format!("Updates of {name} will be installed without review"),
                "when Herdr starts. Turn auto-update on?".to_string(),
            ],
            yes_no(),
        ),
        Confirm::UpdateAll(ids) => {
            let mut b = vec![format!("Update {} plugin(s)?", ids.len())];
            b.extend(ids.iter().take(8).map(|i| format!("  • {i}")));
            if ids.len() > 8 {
                b.push(format!("  … {} more", ids.len() - 8));
            }
            b.push("Each one still shows its preview before anything runs.".into());
            ("Update all", b, yes_no())
        }
    };
    let width = body
        .iter()
        .map(|l| l.chars().count())
        .chain(std::iter::once(hint.iter().map(Span::width).sum()))
        .max()
        .unwrap_or(20)
        .max(30) as u16
        + 4;
    let rect = centered(area, width, body.len() as u16 + 4);
    f.render_widget(Clear, rect);
    let mut lines: Vec<Line> = body
        .into_iter()
        .map(|l| Line::raw(util::sanitize(&l)))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(hint));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn yes_no() -> Vec<Span<'static>> {
    vec![
        Span::styled("y", Style::default().fg(OK).add_modifier(Modifier::BOLD)),
        Span::raw(" yes   "),
        Span::styled("any other key", muted()),
        Span::raw(" no"),
    ]
}

fn draw_help(f: &mut Frame, app: &mut App, area: Rect) {
    let section = |t: &str| {
        Line::styled(
            t.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    };
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {k:<7}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(d.to_string()),
        ])
    };
    let mut lines = vec![
        section("Installed"),
        key("u", "update the selected plugin"),
        key("U", "update all outdated plugins"),
        key("A", "auto-update on/off (when Herdr starts)"),
        key("e", "enable or disable"),
        key("x", "remove, or delete completely"),
        key("X", "pick removed plugins whose leftovers to delete"),
        key("i", "install from a URL, owner/repo or alias:path"),
        key("p", "pick a version to pin (or unpin)"),
        key("s", "switch to another branch, tag or commit"),
        key("z", "roll back to the previous installation"),
        key("o", "open the source in a browser"),
        key("C", "settings: this manager's config.toml"),
        key("E", "open config.toml in your editor"),
        key("/", "filter the list"),
        key("r", "refresh and re-check updates"),
        Line::raw(""),
        section("Marketplace"),
        key("↵", "install the selection (preview first)"),
        key("/", "search"),
        key("s", "sort"),
        key("f", "switch source"),
        key("←", "previous page"),
        key("→", "next page"),
        key("o", "open in browser"),
        key("r", "refresh listings"),
        Line::raw(""),
        section("Anywhere"),
        key("↑ ↓", "move"),
        key("m", "switch views"),
        key("Tab", "switch views"),
        key("q", "back or close"),
        key("Esc", "back or close"),
        key("?", "this help"),
        key("wheel", "move the selection"),
        key("click", "select a row, or switch views on a tab"),
        Line::raw(""),
        Line::styled(format!("config: {}", Config::path().display()), muted()),
    ];
    let rect = centered(area, 72, lines.len() as u16 + 2);
    let visible = rect.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let first = app.help_scroll;
    let last = (first + visible).min(lines.len());
    let title = if max_scroll == 0 {
        " keys · ? closes ".to_string()
    } else {
        format!(
            " keys {}–{}/{} · ↑↓ scroll · ? closes ",
            first + 1,
            last,
            lines.len()
        )
    };
    let shown: Vec<Line> = lines.drain(first..last).collect();
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(shown).block(block), rect);
}

// ---------------------------------------------------------------- version picker

fn open_picker(app: &mut App, row: Row) {
    let id = row.plugin.plugin_id.clone();
    app.picker = Some(Picker {
        id: id.clone(),
        name: row.plugin.name.clone(),
        picks: Vec::new(),
        sel: 0,
        loading: true,
        error: None,
    });
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let result = ops::versions(&row).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Msg::Versions(id, result));
    });
}

fn fill_picker(app: &mut App, id: &str, result: Result<Vec<RemoteTag>, String>) {
    let current = app
        .rows
        .iter()
        .find(|r| r.plugin.plugin_id == id)
        .and_then(|r| r.commit().map(str::to_string));
    let Some(picker) = app.picker.as_mut().filter(|p| p.id == id) else {
        return;
    };
    picker.loading = false;
    let tags = match result {
        Ok(tags) => tags,
        Err(err) => {
            picker.error = Some(err);
            Vec::new()
        }
    };
    let is_current = |commit: &str| {
        current
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(commit))
    };
    let mut picks = Vec::new();
    if let Some(commit) = current
        .as_ref()
        .filter(|_| !tags.iter().any(|t| is_current(&t.commit)))
    {
        picks.push(Pick::Current(commit.clone()));
    }
    picks.extend(tags.into_iter().map(Pick::Tag));
    picks.insert(0, Pick::Latest);
    picker.sel = picks
        .iter()
        .position(|p| match p {
            Pick::Current(_) => true,
            Pick::Tag(t) => is_current(&t.commit),
            Pick::Latest => false,
        })
        .unwrap_or(0);
    picker.picks = picks;
}

fn picker_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    let picker = app.picker.as_mut()?;
    let last = picker.picks.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => app.picker = None,
        KeyCode::Down | KeyCode::Char('j') => picker.sel = (picker.sel + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => picker.sel = picker.sel.saturating_sub(1),
        KeyCode::PageDown => picker.sel = (picker.sel + 10).min(last),
        KeyCode::PageUp => picker.sel = picker.sel.saturating_sub(10),
        KeyCode::Home => picker.sel = 0,
        KeyCode::End => picker.sel = last,
        KeyCode::Enter => {
            let pick = picker.picks.get(picker.sel).cloned()?;
            let id = picker.id.clone();
            let name = picker.name.clone();
            app.picker = None;
            return apply_pick(app, &id, &name, pick);
        }
        _ => {}
    }
    None
}

fn apply_pick(app: &mut App, id: &str, name: &str, pick: Pick) -> Option<Job> {
    let row = app.rows.iter().find(|r| r.plugin.plugin_id == id)?.clone();
    let current = row.commit().unwrap_or("").to_string();
    let commit = match pick {
        Pick::Latest => {
            if !row.pinned() {
                app.say(
                    Level::Info,
                    format!("{name} already follows its default branch"),
                );
                return None;
            }
            if row.unpin_needs_install() {
                return Some(Job::Unpin(id.to_string()));
            }
            match ops::clear_pin(&app.ctx, id) {
                Ok(()) => {
                    app.reload();
                    app.start_checks(Some(&[id.to_string()]));
                    app.say(
                        Level::Ok,
                        format!("{name} follows its default branch again (u updates it)"),
                    );
                }
                Err(err) => app.say(Level::Error, format!("{err:#}")),
            }
            return None;
        }
        Pick::Current(commit) => commit,
        Pick::Tag(tag) => tag.commit,
    };
    if !commit.eq_ignore_ascii_case(&current) {
        return Some(Job::PinTo(id.to_string(), commit));
    }
    if row.pinned() {
        app.say(Level::Info, format!("{name} is already pinned here"));
        return None;
    }
    match ops::pin_current(&app.ctx, id) {
        Ok(short) => {
            app.reload();
            app.checks
                .insert(id.to_string(), Check::Done(Status::Pinned));
            app.say(Level::Ok, format!("pinned {name} at {short}"));
        }
        Err(err) => app.say(Level::Error, format!("{err:#}")),
    }
    None
}

fn draw_picker(f: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.picker else {
        return;
    };
    let current = app
        .rows
        .iter()
        .find(|r| r.plugin.plugin_id == picker.id)
        .and_then(|r| r.commit().map(str::to_string))
        .unwrap_or_default();
    let mut lines: Vec<Line> = Vec::new();
    if picker.loading {
        lines.push(Line::styled(
            format!("{} listing versions…", spinner()),
            muted(),
        ));
    }
    if let Some(err) = &picker.error {
        lines.push(Line::styled(
            util::truncate(&util::sanitize(err), 66),
            Style::default().fg(BAD),
        ));
    }
    let height = (area.height.saturating_sub(8) as usize).clamp(3, 18);
    let start = picker
        .sel
        .saturating_sub(height / 2)
        .min(picker.picks.len().saturating_sub(height));
    for (i, pick) in picker.picks.iter().enumerate().skip(start).take(height) {
        let installed = match pick {
            Pick::Current(_) => true,
            Pick::Tag(t) => t.commit.eq_ignore_ascii_case(&current),
            Pick::Latest => false,
        };
        let (label, commit, note) = match pick {
            Pick::Current(c) => (
                "current".to_string(),
                util::short_sha(c).to_string(),
                "installed, untagged",
            ),
            Pick::Tag(t) => (
                util::sanitize(&t.name),
                util::short_sha(&t.commit).to_string(),
                if installed { "installed" } else { "" },
            ),
            Pick::Latest => (
                "latest (unpin)".to_string(),
                String::new(),
                "follow the default branch",
            ),
        };
        let style = if i == picker.sel {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::styled(if installed { "● " } else { "  " }, Style::default().fg(OK)),
                Span::raw(pad(&label, 22)),
                Span::styled(format!(" {commit:<8} "), muted()),
                Span::styled(note.to_string(), muted()),
            ])
            .style(style),
        );
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "↵",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" pin   "),
        Span::styled(
            "Esc",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" close   "),
        Span::styled(
            "another version is installed first, with its preview",
            muted(),
        ),
    ]));
    let rect = centered(area, 76, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" Pin {} ", util::sanitize(&picker.name)),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

// ---------------------------------------------------------------- leftovers

fn open_cleaner(app: &mut App) {
    app.cleaner = Some(Cleaner {
        orphans: Vec::new(),
        chosen: Vec::new(),
        sel: 0,
        loading: true,
        error: None,
    });
    let tx = app.tx.clone();
    let herdr = app.ctx.herdr.clone();
    std::thread::spawn(move || {
        let result = ops::orphans(&herdr).map_err(|e| format!("{e:#}"));
        let _ = tx.send(Msg::Orphans(result));
    });
}

fn fill_cleaner(app: &mut App, result: Result<Vec<Orphan>, String>) {
    let Some(cleaner) = app.cleaner.as_mut() else {
        return;
    };
    cleaner.loading = false;
    match result {
        Ok(orphans) if orphans.is_empty() => {
            app.cleaner = None;
            app.say(
                Level::Ok,
                "Nothing to clean: no leftovers of removed plugins.",
            );
        }
        Ok(orphans) => {
            cleaner.chosen = vec![false; orphans.len()];
            cleaner.orphans = orphans;
        }
        Err(err) => cleaner.error = Some(err),
    }
}

fn cleaner_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    let cleaner = app.cleaner.as_mut()?;
    let last = cleaner.orphans.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('X') => app.cleaner = None,
        KeyCode::Down | KeyCode::Char('j') => cleaner.sel = (cleaner.sel + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => cleaner.sel = cleaner.sel.saturating_sub(1),
        KeyCode::PageDown => cleaner.sel = (cleaner.sel + 10).min(last),
        KeyCode::PageUp => cleaner.sel = cleaner.sel.saturating_sub(10),
        KeyCode::Home => cleaner.sel = 0,
        KeyCode::End => cleaner.sel = last,
        KeyCode::Char(' ') => {
            if let Some(chosen) = cleaner.chosen.get_mut(cleaner.sel) {
                *chosen = !*chosen;
                cleaner.sel = (cleaner.sel + 1).min(last);
            }
        }
        KeyCode::Char('a') => {
            let all = cleaner.chosen.iter().all(|c| *c);
            cleaner.chosen.iter_mut().for_each(|c| *c = !all);
        }
        KeyCode::Enter => {
            // The checked plugins, or the highlighted one when none is.
            let mut ids: Vec<String> = cleaner
                .orphans
                .iter()
                .zip(&cleaner.chosen)
                .filter(|(_, chosen)| **chosen)
                .map(|(o, _)| o.id.clone())
                .collect();
            if ids.is_empty() {
                ids.extend(cleaner.orphans.get(cleaner.sel).map(|o| o.id.clone()));
            }
            if ids.is_empty() {
                return None;
            }
            app.cleaner = None;
            return Some(Job::Clean(ids));
        }
        _ => {}
    }
    None
}

fn draw_cleaner(f: &mut Frame, app: &App, area: Rect) {
    let Some(cleaner) = &app.cleaner else {
        return;
    };
    let key =
        |k: &'static str| Span::styled(k, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut lines: Vec<Line> = Vec::new();
    if cleaner.loading {
        lines.push(Line::styled(
            format!("{} looking for leftovers…", spinner()),
            muted(),
        ));
    }
    if let Some(err) = &cleaner.error {
        lines.push(Line::styled(
            util::truncate(&util::sanitize(err), 76),
            Style::default().fg(BAD),
        ));
    }
    let height = (area.height.saturating_sub(8) as usize).clamp(3, 18);
    let start = cleaner
        .sel
        .saturating_sub(height / 2)
        .min(cleaner.orphans.len().saturating_sub(height));
    for (i, orphan) in cleaner.orphans.iter().enumerate().skip(start).take(height) {
        let chosen = cleaner.chosen.get(i).copied().unwrap_or(false);
        let style = if i == cleaner.sel {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::styled(
                    if chosen { "[x] " } else { "[ ] " },
                    Style::default().fg(if chosen { OK } else { ACCENT }),
                ),
                Span::raw(pad(&util::sanitize(&orphan.id), 34)),
                Span::styled(format!(" {:>9}  ", util::human_size(orphan.size)), muted()),
                Span::styled(orphan.kinds(), muted()),
            ])
            .style(style),
        );
    }
    let count = cleaner.chosen.iter().filter(|c| **c).count();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        key("Space"),
        Span::raw(" select   "),
        key("a"),
        Span::raw(" all   "),
        key("↵"),
        Span::raw(if count == 0 {
            " clean highlighted   ".to_string()
        } else {
            format!(" clean {count} selected   ")
        }),
        key("Esc"),
        Span::raw(" close"),
    ]));
    let rect = centered(area, 86, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " Leftovers of removed plugins ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

// ---------------------------------------------------------------- task dialogs

fn progress_bar(task: &Task, width: usize) -> String {
    if task.done.is_some() {
        return "█".repeat(width);
    }
    if let Some((done, total)) = task.progress.filter(|(_, t)| *t > 0) {
        let filled = (done * width / total).min(width);
        return format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
    }
    // Unknown duration: a block sweeping across the bar.
    let block = 6.min(width);
    let span = width + block;
    let pos = (task.started.elapsed().as_millis() / 60) as usize % span.max(1);
    (0..width)
        .map(|i| {
            if i + block > pos && i <= pos {
                '█'
            } else {
                '░'
            }
        })
        .collect()
}

fn draw_task(f: &mut Frame, app: &App, area: Rect) {
    let Some(task) = &app.task else {
        return;
    };
    let width: u16 = 78.min(area.width.saturating_sub(2));
    let inner = width.saturating_sub(2) as usize;
    let (state, color) = match &task.done {
        None => (
            format!(
                "{} {}",
                spinner(),
                util::truncate(&util::sanitize(&task.step), inner.saturating_sub(3))
            ),
            ACCENT,
        ),
        Some(Ok(())) => ("✓ done".to_string(), OK),
        Some(Err(err)) => (
            format!(
                "✗ {}",
                util::truncate(&util::sanitize(err), inner.saturating_sub(3))
            ),
            BAD,
        ),
    };
    let label = match task.progress {
        Some((done, total)) => format!(" {done}/{total}"),
        None => format!(" {}s", task.started.elapsed().as_secs()),
    };
    let bar_width = inner.saturating_sub(label.chars().count() + 1);
    let mut lines = vec![
        Line::styled(state, Style::default().fg(color)),
        Line::from(vec![
            Span::styled(progress_bar(task, bar_width), Style::default().fg(color)),
            Span::styled(label, muted()),
        ]),
    ];
    if task.show_log {
        lines.push(Line::raw(""));
        let height = (area.height as usize).saturating_sub(12).clamp(4, 18);
        if task.log.is_empty() {
            lines.push(Line::styled("(no output yet)", muted()));
        }
        for line in task.log.iter().rev().take(height).rev() {
            lines.push(Line::styled(
                util::truncate(&util::sanitize(line), inner),
                muted(),
            ));
        }
    }
    lines.push(Line::raw(""));
    let toggle = if task.show_log {
        " hide output"
    } else {
        " show output"
    };
    let mut hint = vec![
        Span::styled(
            "l",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(toggle, muted()),
    ];
    if task.done.is_some() {
        hint.push(Span::styled("   any other key closes", muted()));
    }
    lines.push(Line::from(hint));
    let rect = centered(area, width, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {} ", util::sanitize(&task.title)),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
    if let Some(review) = &task.review {
        draw_review(f, review, task.batch, area);
    }
}

/// The trust preview of an operation, waiting for y/n.
fn draw_review(f: &mut Frame, review: &Review, batch: bool, area: Rect) {
    let width: u16 = 100.min(area.width.saturating_sub(2));
    let inner = width.saturating_sub(2) as usize;
    let max_height = area.height.saturating_sub(2) as usize;
    let visible = review.lines.len().min(max_height.saturating_sub(5)).max(1);
    let max_scroll = review.lines.len().saturating_sub(visible);
    let first = review.scroll.min(max_scroll);
    let last = (first + visible).min(review.lines.len());
    let mut lines: Vec<Line> = review.lines[first..last]
        .iter()
        .map(|l| Line::raw(util::truncate(l, inner)))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        util::sanitize(&review.question),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let key =
        |k: &'static str| Span::styled(k, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
    let mut hint = vec![
        key("y"),
        Span::raw(" yes   "),
        key("n"),
        Span::raw(" no   "),
    ];
    if batch {
        hint.push(key("a"));
        hint.push(Span::raw(" yes to all   "));
    }
    if max_scroll > 0 {
        hint.push(key("↑↓"));
        hint.push(Span::styled(
            format!(" scroll {}–{last}/{}", first + 1, review.lines.len()),
            muted(),
        ));
    }
    lines.push(Line::from(hint));
    let rect = centered(area, width, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(WARN))
        .title(Span::styled(
            format!(" {} ", util::sanitize(&review.title)),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}
