//! The popup: installed plugins and the marketplace, in one terminal UI.
//!
//! Quick changes (enable/disable) happen in place. Anything that shows a
//! trust preview or runs builds (install, update, uninstall, rollback, pin)
//! suspends the UI and runs the same code path as the CLI on the plain
//! terminal, then returns.

use std::borrow::Cow;
use std::cell::RefCell;
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
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::Config;
use crate::git::{Git, RemoteTag};
use crate::market::{self, Entry, Sort};
use crate::ops::{self, Ctx, InstallRequest, Origin, Orphan, Report, Row, Status};
use crate::{logos, paths, settings, source, util};

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
    /// Turning `cli_on_path` (or `agent_skill` when `skill`) on or off.
    Integration {
        skill: bool,
        on: bool,
        plan: Vec<String>,
    },
    BackupEnabled {
        files: usize,
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
    /// A picture for the detail panels, ready to draw (or why not).
    Logo(String, Result<ratatui_image::protocol::Protocol, String>),
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
    /// The terminal's image protocol; None where it has none.
    graphics: Option<ratatui_image::picker::Picker>,
    /// Pictures by URL: None while loading or when unavailable.
    logos: HashMap<String, Option<ratatui_image::protocol::Protocol>>,
    /// Default branch names, for the `ref` line.
    branches: HashMap<String, String>,
    market: MarketState,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
    /// What a click does where, for the frame on screen.
    hits: RefCell<Hits>,
    load_error: Option<String>,
}

impl App {
    fn new(ctx: Ctx, mouse: bool) -> App {
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
            graphics: None,
            logos: HashMap::new(),
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
            hits: RefCell::new(Hits::new(mouse)),
            load_error: None,
        }
    }

    fn say(&mut self, level: Level, text: impl Into<String>) {
        self.message = Some((level, text.into()));
    }

    /// False when Herdr runs keyboard-only: no capture, no buttons.
    fn mouse(&self) -> bool {
        self.hits.borrow().mouse
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
        if !self.ctx.dry_run {
            let refreshed = (|| -> Result<()> {
                let config = Config::load_read_only()?;
                if config.icons != self.ctx.config.icons {
                    self.graphics = if config.icons {
                        detect_graphics()
                    } else {
                        None
                    };
                }
                self.ctx.config = config;
                refresh_services(&self.ctx.herdr, &self.ctx.config)
            })();
            if let Err(err) = refreshed {
                self.say(Level::Warn, format!("local refresh: {err:#}"));
            }
        }
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
                "{} {} {} {} {}",
                row.plugin.plugin_id,
                row.plugin.name,
                row.source_display(),
                row.source_label(),
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
                Msg::Logo(url, result) => {
                    self.logos.insert(url, result.ok());
                }
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
                    Some(Check::Done(Status::Available { .. } | Status::Moved { .. }))
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

fn enter(term: &mut Term, mouse: bool) -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
        #[cfg(windows)]
        {
            use std::io::Write;
            let mut out = io::stdout();
            out.write_all(VT_MOUSE_ON.as_bytes())?;
            out.flush()?;
        }
    }
    term.clear()?;
    Ok(())
}

fn leave(mouse: bool) -> Result<()> {
    if mouse {
        #[cfg(windows)]
        {
            use std::io::Write;
            let mut out = io::stdout();
            let _ = out.write_all(VT_MOUSE_OFF.as_bytes());
            let _ = out.flush();
        }
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    terminal::disable_raw_mode()?;
    Ok(())
}

pub fn run(ctx: Ctx) -> Result<()> {
    if !util::stdin_is_tty() {
        anyhow::bail!(
            "the interactive manager needs a terminal; see `herdr-pm --help` for commands"
        );
    }
    let mut app = App::new(ctx, crate::herdr::mouse_enabled());
    let mouse = app.mouse();
    app.reload();
    app.start_checks(None);
    app.load_market(false);
    if app.ctx.dry_run {
        app.say(Level::Warn, "dry run: changes are printed, not made");
    }

    let config = &app.ctx.config;
    if !app.ctx.dry_run && (config.cli_on_path || config.agent_skill) {
        let herdr = app.ctx.herdr.clone();
        let (cli, skill) = (config.cli_on_path, config.agent_skill);
        std::thread::spawn(move || {
            let _ = crate::integrate::refresh(&herdr, cli, skill);
        });
    }
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave(mouse);
        default_hook(info);
    }));
    ops::set_plain();
    if app.ctx.config.icons {
        app.graphics = detect_graphics();
    }
    enter(&mut term, mouse)?;
    match ops::claim_auto_update(
        &app.ctx.config,
        Some(crate::config::Hook::Popup),
        app.ctx.dry_run,
    ) {
        Ok(true) => run_job(&mut app, &mut term, Job::AutoUpdate)?,
        Ok(false) => {}
        Err(err) => app.say(Level::Error, format!("auto-update: {err:#}")),
    }
    let result = event_loop(&mut app, &mut term);
    leave(mouse)?;
    result
}

fn event_loop(app: &mut App, term: &mut Term) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    loop {
        request_logo(app);
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
            let job = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dirty = true;
                    handle_key(app, key)
                }
                // Keyboard-only: whatever reaches us is ignored.
                Event::Mouse(m) if app.mouse() => {
                    let (changed, job) = handle_mouse(app, m.kind, m.column, m.row);
                    dirty |= changed;
                    job
                }
                Event::Resize(_, _) => {
                    dirty = true;
                    None
                }
                _ => None,
            };
            if let Some(job) = job {
                run_job(app, term, job)?;
                dirty = true;
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
    /// Move a plugin Herdr installed from GitHub to the git pipeline.
    TakeOver(String),
    /// What `[auto_update] trigger = "popup"` (or a due min-interval) runs.
    AutoUpdate,
    /// Turn `cli_on_path` (or `agent_skill` when `skill`) on or off.
    Integrate {
        skill: bool,
        on: bool,
    },
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
        leave(app.mouse())?;
        let result = edit_config(app);
        enter(term, app.mouse())?;
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
        Job::TakeOver(id) => (format!("Taking over {id}"), one(id), false, false),
        Job::AutoUpdate => ("Auto-updating plugins".to_string(), None, true, true),
        Job::Integrate { skill, on } => (
            match (skill, on) {
                (false, true) => "Putting herdr-pm on your PATH",
                (false, false) => "Taking herdr-pm off your PATH",
                (true, true) => "Linking the agent skill",
                (true, false) => "Unlinking the agent skill",
            }
            .to_string(),
            None,
            false,
            true,
        ),
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
        Job::TakeOver(id) => Box::new(move |ctx| ops::take_over(ctx, &id)),
        Job::Integrate { skill: false, on } => Box::new(move |ctx| ops::set_cli_on_path(ctx, on)),
        Job::Integrate { skill: true, on } => Box::new(move |ctx| ops::set_agent_skill(ctx, on)),
        Job::AutoUpdate => Box::new(|ctx| match ops::auto_update(ctx)? {
            0 => Ok(()),
            failures => anyhow::bail!("{failures} auto-update(s) failed"),
        }),
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
    refresh_services(&app.ctx.herdr, &app.ctx.config)?;
    Ok(())
}

// ---------------------------------------------------------------- settings

fn refresh_services(herdr: &crate::herdr::Herdr, config: &Config) -> Result<()> {
    if herdr.dry_run {
        return Ok(());
    }
    // Both actions are local. Existing shells/agent contexts cannot be rewritten.
    let integration = crate::integrate::refresh(herdr, config.cli_on_path, config.agent_skill);
    let backup = crate::backup::refresh(herdr, config);
    integration?;
    backup
}

/// The `C` dialog over `config.toml`.
struct SettingsDialog {
    sel: usize,
    error: Option<String>,
    candidates: Result<Vec<String>, String>,
    /// Automatic backup's current failure, read when the dialog opens and
    /// after each change.
    problem: Option<String>,
}

fn settings_rows(app: &App) -> Vec<settings::Row> {
    let plugins: Vec<String> = app
        .rows
        .iter()
        .filter(|r| r.origin != Origin::Local)
        .map(|r| r.plugin.plugin_id.clone())
        .collect();
    let candidates = app
        .settings
        .as_ref()
        .map(|dialog| {
            dialog
                .candidates
                .as_ref()
                .map(Vec::as_slice)
                .map_err(String::as_str)
        })
        .unwrap_or(Ok(&[]));
    let problem = app.settings.as_ref().and_then(|d| d.problem.as_deref());
    settings::rows(&app.ctx.config, &plugins, candidates, problem)
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
            if !config.icons {
                app.graphics = None;
            } else if !app.ctx.config.icons {
                app.graphics = detect_graphics();
            }
            app.ctx.config = config;
            if matches!(field, settings::Field::Market(_)) {
                app.market.source = None;
                if app.market.loaded || app.market.loading {
                    app.load_market(false);
                }
            }
            if app.ctx.dry_run {
                None
            } else {
                refresh_services(&app.ctx.herdr, &app.ctx.config)
                    .err()
                    .map(|err| format!("{err:#}"))
            }
        }
        Err(err) => Some(format!("{err:#}")),
    };
    if let Some(dialog) = app.settings.as_mut() {
        dialog.error = error;
        dialog.problem = crate::backup::problem();
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
        _ if kind == Kind::ReadOnly => return None,
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
                    | settings::Field::MinInterval
                    | settings::Field::LogDir
                    | settings::Field::BackupRemote
                    | settings::Field::BackupBranch
                    | settings::Field::BackupFile(_)
            ) =>
        {
            Action::Delete
        }
        _ => return None,
    };
    if kind == Kind::AddAlias {
        return None;
    }
    if field == settings::Field::BackupEnabled && !app.ctx.config.backup.enabled {
        app.confirm = Some(Confirm::BackupEnabled {
            files: app.ctx.config.backup.files.len(),
        });
        return None;
    }
    if matches!(
        field,
        settings::Field::CliOnPath | settings::Field::AgentSkill
    ) {
        // These change the system, not only config.toml: confirm, then run.
        let skill = field == settings::Field::AgentSkill;
        let config = &app.ctx.config;
        let on = !if skill {
            config.agent_skill
        } else {
            config.cli_on_path
        };
        let plan = if skill {
            crate::integrate::skill_plan(on, config.cli_on_path)
        } else {
            crate::integrate::cli_plan(on, config.agent_skill)
        };
        match plan {
            Ok(plan) => app.confirm = Some(Confirm::Integration { skill, on, plan }),
            Err(err) => {
                if let Some(dialog) = app.settings.as_mut() {
                    dialog.error = Some(format!("{err:#}"));
                }
            }
        }
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
    let mouse = app.mouse();
    // A value typed on the prompt line takes the keys: only the buttons
    // stay clickable, `←` back to the list, `×` out of both.
    let nested = app.prompt.is_some();
    let rows = settings_rows(app);
    let sel = dialog.sel.min(rows.len().saturating_sub(1));
    let key = key_style();
    // Each line, with its row and where its value starts and how wide it is.
    type ValueSpan = Option<(usize, u16, u16)>;
    let mut body: Vec<(Line, ValueSpan)> = Vec::new();
    let mut sel_line = 0;
    let mut section = "";
    for (i, row) in rows.iter().enumerate() {
        if row.section != section {
            if !section.is_empty() {
                body.push((Line::raw(""), None));
            }
            section = row.section;
            body.push((Line::styled(section, key), None));
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
        let label = Span::raw(format!("  {}", pad(&util::sanitize(&row.label), 34)));
        let value = Span::styled(util::sanitize(&row.value), muted());
        let place = (i, label.width() as u16, value.width() as u16);
        body.push((Line::from(vec![label, value]).style(style), Some(place)));
    }
    let height = (area.height.saturating_sub(10) as usize).clamp(5, 30);
    let start = sel_line
        .saturating_sub(height / 2)
        .min(body.len().saturating_sub(height));
    let (mut lines, places): (Vec<Line>, Vec<_>) =
        body.into_iter().skip(start).take(height).unzip();
    lines.push(Line::raw(""));
    if let Some(err) = &dialog.error {
        lines.push(Line::styled(
            util::truncate(&util::sanitize(err), 84),
            Style::default().fg(BAD),
        ));
    }
    let field = rows.get(sel).map(|r| &r.field);
    let enter = [Click::Key(ENTER)];
    let arrows = [press(KeyCode::Left), press(KeyCode::Right)];
    let delete = [ch('d')];
    let mut hint = Hint::new(mouse && !nested);
    match field.map(settings::Field::kind) {
        Some(Kind::Number) => {
            hint.key("↵", key, Span::raw(" type   "), &enter).key(
                "←→",
                key,
                Span::raw(" change   "),
                &arrows,
            );
        }
        Some(Kind::Toggle) => {
            hint.key("↵", key, Span::raw(" on/off   "), &enter);
        }
        Some(Kind::Choice) => {
            hint.cap("↵", key, Some(Click::Key(ENTER)))
                .cap(" ", key, None)
                .key("←→", key, Span::raw(" change   "), &arrows);
        }
        Some(Kind::Text) => {
            hint.key("↵", key, Span::raw(" type   "), &enter).key(
                "d",
                key,
                Span::raw(" default   "),
                &delete,
            );
        }
        Some(Kind::Alias) => {
            hint.key("↵", key, Span::raw(" edit   "), &enter).key(
                "d",
                key,
                Span::raw(" delete   "),
                &delete,
            );
        }
        Some(Kind::AddAlias) | None => {
            hint.key("↵", key, Span::raw(" add   "), &enter);
        }
        Some(Kind::ReadOnly) if matches!(field, Some(settings::Field::BackupError)) => {
            hint.push(Span::raw("close and reopen Settings to retry   "), None);
        }
        Some(Kind::ReadOnly) => {
            hint.push(Span::raw("read-only   "), None);
        }
    }
    if matches!(field, Some(settings::Field::PluginStrategy(_))) {
        hint.key("d", key, Span::raw(" default   "), &delete);
    }
    hint.key("E", key, Span::raw(" editor   "), &[ch('E')]).key(
        "Esc",
        key,
        Span::raw(" close"),
        &[Click::Key(ESC)],
    );
    let hint_line = lines.len();
    let (line, hint_clicks) = hint.into_parts();
    lines.push(line);
    lines.push(Line::styled(
        util::truncate(&paths::pm_config_file().display().to_string(), 84),
        muted(),
    ));
    let rect = centered(area, 90, lines.len() as u16 + 2);
    let (back, close) = if nested {
        (Some(Click::Key(ESC)), Click::Keys(ESC, ESC))
    } else {
        (None, Click::Key(ESC))
    };
    let mut title = Hint::title(mouse, back, Some(close));
    title.push(
        Span::styled(" Settings ", Style::default().add_modifier(Modifier::BOLD)),
        None,
    );
    let block = dialog_block(f, app, rect, ACCENT, title);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    if nested {
        return;
    }
    let inner = rect.inner(Margin::new(1, 1));
    for (index, place) in places.iter().enumerate() {
        let Some((i, value_at, value_width)) = *place else {
            continue;
        };
        place_row(app, inner, index, 1, Click::Row(List::Settings, i, None));
        if i == sel {
            // The value of the selected row does what Enter does.
            place_line(
                app,
                rect,
                index,
                &[(value_at, value_width, Click::Key(ENTER))],
            );
        }
    }
    place_line(app, rect, hint_line, &hint_clicks);
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
            // Even a failed task may have completed some local mutations.
            app.reload();
        }
    }
}

/// Closes the task dialog and reports how it ended in the status line.
fn finish_task(app: &mut App) {
    let Some(task) = app.task.take() else {
        return;
    };
    app.reload();
    // Some tasks write config.toml (integrations).
    if let Ok(config) = Config::load() {
        app.ctx.config = config;
    }
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

// ---------------------------------------------------------------- mouse

const ESC: KeyEvent = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
const ENTER: KeyEvent = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
/// What the main window's close button presses: quit from anywhere.
const QUIT: KeyEvent = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

const fn press(code: KeyCode) -> Click {
    Click::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

const fn ch(c: char) -> Click {
    press(KeyCode::Char(c))
}

/// The lists whose rows take clicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Installed,
    Market,
    Picker,
    Cleaner,
    Settings,
}

/// What a click on a region does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
    /// The key the region shows, through the same path as the keyboard.
    Key(KeyEvent),
    /// Two keys in a row: leaves a dialog and the one it opened from.
    Keys(KeyEvent, KeyEvent),
    Tab(View),
    /// A row of a list: selects it; on the selected row, presses the key.
    Row(List, usize, Option<KeyEvent>),
}

/// The clickable regions of the frame on screen, in drawing order. Empty
/// when Herdr runs keyboard-only.
struct Hits {
    mouse: bool,
    regions: Vec<(Rect, Click)>,
}

impl Hits {
    fn new(mouse: bool) -> Hits {
        Hits {
            mouse,
            regions: Vec::new(),
        }
    }

    /// A new frame, or a dialog drawn on top: what lies below takes no
    /// more clicks.
    fn clear(&mut self) {
        self.regions.clear();
    }

    fn add(&mut self, rect: Rect, click: Click) {
        if self.mouse && !rect.is_empty() {
            self.regions.push((rect, click));
        }
    }

    /// The pieces of a `Hint` drawn from (`x`, `y`), cut at column `right`.
    fn place(&mut self, clicks: &[(u16, u16, Click)], x: u16, y: u16, right: u16) {
        for &(start, width, click) in clicks {
            let left = x.saturating_add(start);
            let end = left.saturating_add(width).min(right);
            if left < end {
                self.add(Rect::new(left, y, end - left, 1), click);
            }
        }
    }

    /// What a click at this cell does: the region drawn last wins.
    fn at(&self, col: u16, row: u16) -> Option<Click> {
        let cell = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(cell))
            .map(|&(_, click)| click)
    }
}

/// A line of text whose pieces stand for keys: hints and window buttons.
/// Without the mouse it is the plain keyboard text and records nothing.
struct Hint {
    mouse: bool,
    spans: Vec<Span<'static>>,
    /// Start column, width, and what a click there does.
    clicks: Vec<(u16, u16, Click)>,
    width: u16,
    /// The window's back `←` and close `×`, drawn at its top right.
    buttons: Vec<(&'static str, Click)>,
}

/// Clickable pieces of a line: start column, width, and what a click does.
type Clicks = Vec<(u16, u16, Click)>;

impl Hint {
    fn new(mouse: bool) -> Hint {
        Hint {
            mouse,
            spans: Vec::new(),
            clicks: Vec::new(),
            width: 0,
            buttons: Vec::new(),
        }
    }

    /// A window title, with its back `←` and close `×` buttons kept apart
    /// for the top right corner (with the mouse only).
    fn title(mouse: bool, back: Option<Click>, close: Option<Click>) -> Hint {
        let mut hint = Hint::new(mouse);
        if mouse {
            hint.buttons = [("←", back), ("×", close)]
                .into_iter()
                .filter_map(|(glyph, click)| Some((glyph, click?)))
                .collect();
        }
        hint
    }

    /// The `←×` buttons side by side, preceded by a space: the line, its
    /// clicks and its width. `None` without buttons.
    fn take_buttons(&mut self) -> Option<(Line<'static>, Clicks, u16)> {
        if self.buttons.is_empty() {
            return None;
        }
        let mut buttons = Hint::new(self.mouse);
        buttons.push(Span::raw(" "), None);
        for (glyph, click) in std::mem::take(&mut self.buttons) {
            buttons.push(Span::styled(glyph, key_style()), Some(click));
        }
        let width = buttons.width;
        let (line, clicks) = buttons.into_parts();
        Some((line, clicks, width))
    }

    /// Text; with a click, the text up to its trailing spaces takes it.
    fn push(&mut self, span: Span<'static>, click: Option<Click>) -> &mut Hint {
        let width = span.width() as u16;
        if let (true, Some(click)) = (self.mouse, click) {
            let trailing = span.content.chars().rev().take_while(|c| *c == ' ').count() as u16;
            let used = width.saturating_sub(trailing);
            if used > 0 {
                self.clicks.push((self.width, used, click));
            }
        }
        self.width = self.width.saturating_add(width);
        self.spans.push(span);
        self
    }

    /// A key cap; with the mouse, a click on it presses the key.
    fn cap(
        &mut self,
        cap: impl Into<Cow<'static, str>>,
        style: Style,
        click: Option<Click>,
    ) -> &mut Hint {
        self.push(Span::styled(cap, style), click)
    }

    /// A key cap and its label: a click on either presses the key. With
    /// several keys (`←→`) each character of the cap is one of them and the
    /// label takes none.
    fn key(
        &mut self,
        cap: &'static str,
        style: Style,
        label: Span<'static>,
        keys: &[Click],
    ) -> &mut Hint {
        if let [click] = keys {
            self.cap(cap, style, Some(*click));
            return self.push(label, Some(*click));
        }
        for ((at, glyph), click) in cap.char_indices().zip(keys) {
            self.cap(&cap[at..at + glyph.len_utf8()], style, Some(*click));
        }
        self.push(label, None)
    }

    fn into_parts(self) -> (Line<'static>, Vec<(u16, u16, Click)>) {
        (Line::from(self.spans), self.clicks)
    }
}

fn key_style() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// What a click does, through the same paths as the keyboard.
fn click_on(app: &mut App, click: Click) -> Option<Job> {
    match click {
        Click::Key(key) => handle_key(app, key),
        Click::Keys(first, then) => handle_key(app, first).or_else(|| handle_key(app, then)),
        Click::Tab(view) => {
            switch_view(app, view);
            None
        }
        Click::Row(list, index, then) => {
            let sel = match list {
                List::Installed => &mut app.sel,
                List::Market => &mut app.market.sel,
                List::Picker => &mut app.picker.as_mut()?.sel,
                List::Cleaner => &mut app.cleaner.as_mut()?.sel,
                List::Settings => &mut app.settings.as_mut()?.sel,
            };
            if *sel != index {
                *sel = index;
                return None;
            }
            handle_key(app, then?)
        }
    }
}

// ---------------------------------------------------------------- input

/// The wheel moves the selection of what is on top; a left click does what
/// the region under it stands for. Returns whether to redraw, and the job
/// the click started.
fn handle_mouse(app: &mut App, kind: MouseEventKind, col: u16, row: u16) -> (bool, Option<Job>) {
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
            (true, None)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let click = app.hits.borrow().at(col, row);
            match click {
                Some(click) => (true, click_on(app, click)),
                None => (false, None),
            }
        }
        _ => (false, None),
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
                candidates: crate::gitops::candidates().map_err(|err| format!("{err:#}")),
                problem: crate::backup::problem(),
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
        KeyCode::Char('t') => {
            let row = row?;
            if row.origin != Origin::Github {
                app.say(
                    Level::Info,
                    format!("{} is already managed here, not by herdr", row.plugin.name),
                );
            } else {
                return Some(Job::TakeOver(row.plugin.plugin_id.clone()));
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
        Confirm::Integration { skill, on, .. } if yes => Some(Job::Integrate { skill, on }),
        Confirm::BackupEnabled { .. } if yes => {
            apply_setting(
                app,
                &settings::Field::BackupEnabled,
                settings::Action::Set("true".into()),
            );
            return None;
        }
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
    app.hits.borrow_mut().clear();
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
    // Overlays: the one that reads keys first is drawn last, and each
    // takes the clicks of what it covers.
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
    if app.prompt.is_some() {
        draw_prompt(f, app, status);
    }
    if app.confirm.is_some() {
        draw_confirm(f, app, area);
    }
    if app.task.is_some() {
        draw_task(f, app, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
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
    // Esc goes back from the marketplace, or clears the filter.
    let back = (app.view == View::Market || !app.filter.is_empty()).then_some(Click::Key(ESC));
    let mut hint = Hint::title(app.mouse(), back, Some(Click::Key(QUIT)));
    let buttons = hint.take_buttons();
    let buttons_width = buttons.as_ref().map_or(0, |(_, _, width)| *width);
    for (span, view) in tabs.into_iter().zip([View::Installed, View::Market]) {
        hint.push(span, Some(Click::Tab(view)));
        hint.push(Span::raw(" "), None);
    }
    if app.ctx.dry_run {
        let right = "DRY RUN";
        let gap = (area.width as usize)
            .saturating_sub(hint.width as usize + right.len() + 1 + buttons_width as usize);
        hint.push(Span::raw(" ".repeat(gap)), None);
        hint.push(Span::styled(right, Style::default().fg(WARN)), None);
    }
    let (line, clicks) = hint.into_parts();
    let mut hits = app.hits.borrow_mut();
    hits.place(&clicks, area.x, area.y, area.right());
    f.render_widget(Paragraph::new(line), area);
    if let Some((line, clicks, width)) = buttons {
        let x = area.right().saturating_sub(width);
        hits.place(&clicks, x, area.y, area.right());
        f.render_widget(
            Paragraph::new(line),
            Rect::new(x, area.y, width.min(area.width), 1),
        );
    }
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
            Status::Moved { .. } => Span::styled(
                "⚠ moved by herdr",
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
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
            Span::styled(pad(&row.source_label(), src_w), muted()),
            Span::raw(" "),
            badge(row.auto_update, "⟳ ", Style::default().fg(ACCENT)),
            badge(row.pinned(), "📌", Style::default()),
            status,
        ];
        // Width in cells: 📌 takes two.
        let used_now: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used_now))));
        place_row(
            app,
            inner,
            lines.len(),
            1,
            Click::Row(List::Installed, pos, None),
        );
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
            match &row.record {
                Some(r) => {
                    lines.push(kv("source", r.url.clone()));
                    if let Some(subdir) = &r.subdir {
                        lines.push(kv("subdir", subdir.clone()));
                    }
                }
                None => lines.push(kv("source", row.source_display())),
            }
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
            let auto = &app.ctx.config.auto_update;
            let strategy = auto.strategy_for(&row.plugin.plugin_id);
            let when = match auto.trigger {
                crate::config::Trigger::MinInterval => {
                    format!("at most every {}", auto.min_interval)
                }
                trigger => trigger.describe().to_string(),
            };
            lines.push(kv(
                "auto-update",
                if row.auto_update {
                    format!(
                        "on · {}: {}, {when} (A turns it off)",
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
        Some(Check::Done(Status::Moved { pin })) => lines.push(Line::from(vec![
            Span::styled(format!("{:<LABEL$} ", "pin"), muted()),
            Span::styled(
                format!(
                    "pinned at {} but `herdr plugin update` moved it to {} — u restores the pin",
                    util::short_sha(pin),
                    util::short_sha(row.commit().unwrap_or(""))
                ),
                Style::default().fg(WARN),
            ),
        ])),
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
    render_detail(f, app, area, lines, logos::for_row(row));
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
        // Selected again, an entry installs: through its trust preview.
        place_row(
            app,
            inner,
            lines.len(),
            2,
            Click::Row(List::Market, idx, Some(ENTER)),
        );
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
    let logo = app
        .market
        .entries
        .get(app.market.sel)
        .and_then(logos::for_entry);
    render_detail(f, app, area, lines, logo);
}

/// Width and height of the picture in the detail panels, in cells (about
/// square on usual fonts).
const LOGO: (u16, u16) = (10, 5);

/// The detail text, with the picture at its right when there is one.
fn render_detail(
    f: &mut Frame,
    app: &App,
    area: Rect,
    lines: Vec<Line<'static>>,
    logo: Option<String>,
) {
    let area = pad_rect(area);
    let picture = logo
        .and_then(|url| app.logos.get(&url))
        .and_then(Option::as_ref)
        .filter(|_| area.width > LOGO.0 * 3);
    let text_area = match picture {
        Some(protocol) => {
            let rect = Rect {
                x: area.right() - LOGO.0,
                y: area.y,
                width: LOGO.0,
                height: LOGO.1.min(area.height),
            };
            f.render_widget(ratatui_image::Image::new(protocol), rect);
            Rect {
                width: area.width - LOGO.0 - 1,
                ..area
            }
        }
        None => area,
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), text_area);
}

/// The terminal's image protocol (Kitty, Sixel, iTerm2), or None.
///
/// Only terminals known to draw images are asked: the capability query
/// leaves a thread reading stdin when nothing answers, which would eat
/// keystrokes. Inside Herdr, which draws Kitty images itself in compatible
/// outer terminals and does not answer the query, Kitty is used directly.
fn detect_graphics() -> Option<ratatui_image::picker::Picker> {
    use ratatui_image::picker::{cap_parser::QueryStdioOptions, Picker, ProtocolType};
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let (term, program) = (var("TERM"), var("TERM_PROGRAM"));
    let kitty_outer = !var("KITTY_WINDOW_ID").is_empty()
        || term == "xterm-kitty"
        || term == "xterm-ghostty"
        || program.eq_ignore_ascii_case("ghostty");
    let other = matches!(program.as_str(), "WezTerm" | "iTerm.app" | "rio")
        || var("LC_TERMINAL").contains("iTerm")
        || !var("KONSOLE_VERSION").is_empty()
        || ["foot", "mlterm", "contour"]
            .iter()
            .any(|t| term.contains(t));
    if var("HERDR_ENV") == "1" {
        if !kitty_outer {
            return None;
        }
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        return Some(picker);
    }
    if !kitty_outer && !other {
        return None;
    }
    let options = QueryStdioOptions {
        timeout: Duration::from_millis(500),
        ..QueryStdioOptions::default()
    };
    Picker::from_query_stdio_with_options(options)
        .ok()
        .filter(|p| p.protocol_type() != ProtocolType::Halfblocks)
}

/// Starts loading the picture the detail panel shows, once per URL.
fn request_logo(app: &mut App) {
    let Some(graphics) = app.graphics.clone() else {
        return;
    };
    let url = match app.view {
        View::Installed => app.selected_row().and_then(logos::for_row),
        View::Market => app
            .market
            .entries
            .get(app.market.sel)
            .and_then(logos::for_entry),
    };
    let Some(url) = url.filter(|u| !app.logos.contains_key(u)) else {
        return;
    };
    app.logos.insert(url.clone(), None);
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let result = logos::load(&url)
            .and_then(|image| {
                let size = ratatui::layout::Size::new(LOGO.0, LOGO.1);
                graphics
                    .new_protocol(image, size, ratatui_image::Resize::Fit(None))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(Msg::Logo(url, result));
    });
}

/// The line where a value is typed, with clickable `↵`/`Esc` at its right.
/// It takes the clicks of the window it was opened from, except the
/// buttons of a Settings dialog.
fn draw_prompt(f: &mut Frame, app: &App, area: Rect) {
    let Some((kind, buffer)) = &app.prompt else {
        return;
    };
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
    let area = pad_rect(area);
    let mut spans = vec![
        Span::styled(label, Style::default().fg(ACCENT)),
        Span::raw(buffer.clone()),
        Span::styled("▏", Style::default().fg(ACCENT)),
    ];
    if app.settings.is_none() {
        app.hits.borrow_mut().clear();
    }
    let mut hint = Hint::new(app.mouse());
    hint.key("↵", key_style(), Span::raw(" ok   "), &[Click::Key(ENTER)])
        .key("Esc", key_style(), Span::raw(" cancel"), &[Click::Key(ESC)]);
    let used: usize = spans.iter().map(Span::width).sum();
    if app.mouse() && used + hint.width as usize + 2 <= area.width as usize {
        let at = area.width - hint.width;
        spans.push(Span::raw(" ".repeat(at as usize - used)));
        let (line, clicks) = hint.into_parts();
        spans.extend(line.spans);
        app.hits
            .borrow_mut()
            .place(&clicks, area.x + at, area.y, area.right());
    }
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    if app.prompt.is_some() {
        return; // draw_prompt
    }
    let line_area = pad_rect(area);
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
            let mut hint = Hint::new(app.mouse());
            hint.push(
                Span::styled(format!("filter: {}  (", app.filter), muted()),
                None,
            )
            .cap("Esc", muted(), Some(Click::Key(ESC)))
            .push(Span::styled(" clears", muted()), Some(Click::Key(ESC)))
            .push(Span::styled(")", muted()), None);
            let (line, clicks) = hint.into_parts();
            app.hits
                .borrow_mut()
                .place(&clicks, line_area.x, line_area.y, line_area.right());
            line
        }
        None => Line::raw(""),
    };
    f.render_widget(Paragraph::new(line), line_area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    const INSTALLED: &[(&str, &str, &[Click])] = &[
        ("u", "update", &[ch('u')]),
        ("U", "update all", &[ch('U')]),
        ("e", "enable/disable", &[ch('e')]),
        ("A", "auto-update", &[ch('A')]),
        ("x", "remove", &[ch('x')]),
        ("i", "install", &[ch('i')]),
        ("m", "marketplace", &[ch('m')]),
        ("?", "help", &[ch('?')]),
        ("q", "quit", &[ch('q')]),
    ];
    const MARKET: &[(&str, &str, &[Click])] = &[
        ("↵", "install", &[Click::Key(ENTER)]),
        ("/", "search", &[ch('/')]),
        ("←→", "page", &[press(KeyCode::Left), press(KeyCode::Right)]),
        ("s", "sort", &[ch('s')]),
        ("f", "switch source", &[ch('f')]),
        ("o", "open", &[ch('o')]),
        ("r", "refresh", &[ch('r')]),
        ("q", "back", &[ch('q')]),
    ];
    let keys = match app.view {
        View::Installed => INSTALLED,
        View::Market => MARKET,
    };
    let mut hint = Hint::new(app.mouse());
    let mut used = 0usize;
    for &(k, label, clicks) in keys {
        let piece = k.chars().count() + label.chars().count() + 3;
        if used + piece > area.width as usize {
            break;
        }
        used += piece;
        hint.key(
            k,
            key_style(),
            Span::styled(format!(" {label}  "), muted()),
            clicks,
        );
    }
    let area = pad_rect(area);
    let (line, clicks) = hint.into_parts();
    app.hits
        .borrow_mut()
        .place(&clicks, area.x, area.y, area.right());
    f.render_widget(Paragraph::new(line), area);
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

/// Starts a dialog over what is drawn: clears the area and the clicks
/// below it, and frames it with `title`, its buttons at the top right.
fn dialog_block(
    f: &mut Frame,
    app: &App,
    rect: Rect,
    border: Color,
    mut title: Hint,
) -> Block<'static> {
    f.render_widget(Clear, rect);
    let buttons = title.take_buttons();
    let (title, clicks) = title.into_parts();
    let mut hits = app.hits.borrow_mut();
    hits.clear();
    hits.place(
        &clicks,
        rect.x.saturating_add(1),
        rect.y,
        rect.right().saturating_sub(1),
    );
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(title);
    if let Some((line, clicks, width)) = buttons {
        // Right after the last dash, against the top right corner.
        let right = rect.right().saturating_sub(1);
        hits.place(&clicks, right.saturating_sub(width), rect.y, right);
        block = block.title_top(line.right_aligned());
    }
    block
}

/// Makes line `index` of a dialog's text clickable, when it is on screen.
fn place_line(app: &App, rect: Rect, index: usize, clicks: &[(u16, u16, Click)]) {
    let inner = rect.inner(Margin::new(1, 1));
    if index < inner.height as usize {
        app.hits
            .borrow_mut()
            .place(clicks, inner.x, inner.y + index as u16, inner.right());
    }
}

/// Makes `height` lines of `inner` from line `index` a row of a list.
fn place_row(app: &App, inner: Rect, index: usize, height: u16, click: Click) {
    let top = inner.y.saturating_add(index as u16);
    if top < inner.bottom() {
        let rect = Rect::new(inner.x, top, inner.width, height.min(inner.bottom() - top));
        app.hits.borrow_mut().add(rect, click);
    }
}

fn draw_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(confirm) = &app.confirm else {
        return;
    };
    let mouse = app.mouse();
    let (title, body, hint): (&str, Vec<String>, Hint) = match confirm {
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
                {
                    let mut hint = Hint::new(mouse);
                    let bold = Style::default().add_modifier(Modifier::BOLD);
                    hint.key("u", bold.fg(OK), Span::raw(" remove   "), &[ch('u')])
                        .key(
                            "d",
                            bold.fg(BAD),
                            Span::raw(" delete completely   "),
                            &[ch('d')],
                        )
                        .cap("any other key", muted(), Some(Click::Key(ESC)))
                        .push(Span::raw(" cancels"), Some(Click::Key(ESC)));
                    hint
                },
            )
        }
        Confirm::AutoUpdateOn { name, .. } => (
            "Auto-update",
            vec![
                format!("Updates of {name} will be installed without review"),
                "when Herdr starts. Turn auto-update on?".to_string(),
            ],
            yes_no(mouse),
        ),
        Confirm::BackupEnabled { files } => (
            "Enable local backup",
            vec![
                "Selected configuration files are copied as PLAINTEXT.".into(),
                "They may contain passwords, tokens, or private data.".into(),
                "Only exact files you explicitly approve are copied; none are selected by default."
                    .into(),
                format!("{files} file(s) approved so far."),
                "An independent local background worker watches files and plugin metadata.".into(),
                "It does not commit or use the network; push/pull are explicit commands.".into(),
                "Disabling backup stops the worker, but keeps snapshots on disk.".into(),
                "Enable local backup?".into(),
            ],
            yes_no(mouse),
        ),
        Confirm::Integration { skill, on, plan } => {
            let mut b = plan.clone();
            b.push(String::new());
            b.push(if *on { "Turn it on?" } else { "Turn it off?" }.to_string());
            (
                if *skill {
                    "Agent skill"
                } else {
                    "herdr-pm on PATH"
                },
                b,
                yes_no(mouse),
            )
        }
        Confirm::UpdateAll(ids) => {
            let mut b = vec![format!("Update {} plugin(s)?", ids.len())];
            b.extend(ids.iter().take(8).map(|i| format!("  • {i}")));
            if ids.len() > 8 {
                b.push(format!("  … {} more", ids.len() - 8));
            }
            b.push("Each one still shows its preview before anything runs.".into());
            ("Update all", b, yes_no(mouse))
        }
    };
    let width = body
        .iter()
        .map(|l| l.chars().count())
        .chain(std::iter::once(hint.width as usize))
        .max()
        .unwrap_or(20)
        .max(30) as u16
        + 4;
    let rect = centered(area, width, body.len() as u16 + 4);
    let mut lines: Vec<Line> = body
        .into_iter()
        .map(|l| Line::raw(util::sanitize(&l)))
        .collect();
    lines.push(Line::raw(""));
    let hint_line = lines.len();
    let (line, hint_clicks) = hint.into_parts();
    lines.push(line);
    // Opened from Settings: `←` goes back there, `×` closes both.
    let (back, close) = if app.settings.is_some() {
        (Some(Click::Key(ESC)), Click::Keys(ESC, ESC))
    } else {
        (None, Click::Key(ESC))
    };
    let mut heading = Hint::title(mouse, back, Some(close));
    heading.push(
        Span::styled(
            format!(" {title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        None,
    );
    let block = dialog_block(f, app, rect, ACCENT, heading);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    place_line(app, rect, hint_line, &hint_clicks);
}

fn yes_no(mouse: bool) -> Hint {
    let mut hint = Hint::new(mouse);
    hint.key(
        "y",
        Style::default().fg(OK).add_modifier(Modifier::BOLD),
        Span::raw(" yes   "),
        &[ch('y')],
    )
    .cap("any other key", muted(), Some(Click::Key(ESC)))
    .push(Span::raw(" no"), Some(Click::Key(ESC)));
    hint
}

fn draw_help(f: &mut Frame, app: &mut App, area: Rect) {
    let mouse = app.mouse();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let section = |t: &str| {
        (
            Line::styled(t.to_string(), key_style()),
            Vec::<(u16, u16, Click)>::new(),
        )
    };
    // A key's line, which a click closes the help and presses the key from.
    let key = |k: &'static str, d: &'static str, click: Option<Click>| {
        let mut hint = Hint::new(mouse);
        hint.push(Span::styled("  ", bold), None)
            .cap(k, bold, click)
            .push(
                Span::styled(" ".repeat(7usize.saturating_sub(k.chars().count())), bold),
                None,
            )
            .push(Span::raw(d), click);
        hint.into_parts()
    };
    let then = |code: KeyCode| Some(Click::Keys(ESC, KeyEvent::new(code, KeyModifiers::NONE)));
    let installed = |c: char| then(KeyCode::Char(c)).filter(|_| app.view == View::Installed);
    let market = |code: KeyCode| then(code).filter(|_| app.view == View::Market);
    let close = Some(Click::Key(ESC));
    let mut lines = vec![
        section("Installed"),
        key("u", "update the selected plugin", installed('u')),
        key("U", "update all outdated plugins", installed('U')),
        key(
            "A",
            "auto-update on/off (when Herdr starts)",
            installed('A'),
        ),
        key("e", "enable or disable", installed('e')),
        key("x", "remove, or delete completely", installed('x')),
        key(
            "X",
            "pick removed plugins whose leftovers to delete",
            installed('X'),
        ),
        key(
            "i",
            "install from a URL, owner/repo or alias:path",
            installed('i'),
        ),
        key("p", "pick a version to pin (or unpin)", installed('p')),
        key(
            "t",
            "take a GitHub plugin over from herdr (git-managed)",
            installed('t'),
        ),
        key(
            "s",
            "switch to another branch, tag or commit",
            installed('s'),
        ),
        key(
            "z",
            "roll back to the previous installation",
            installed('z'),
        ),
        key("o", "open the source in a browser", installed('o')),
        key("C", "settings: this manager's config.toml", installed('C')),
        key("E", "open config.toml in your editor", installed('E')),
        key("/", "filter the list", installed('/')),
        key("r", "refresh and re-check updates", installed('r')),
        (Line::raw(""), Vec::new()),
        section("Marketplace"),
        key(
            "↵",
            "install the selection (preview first)",
            market(KeyCode::Enter),
        ),
        key("/", "search", market(KeyCode::Char('/'))),
        key("s", "sort", market(KeyCode::Char('s'))),
        key("f", "switch source", market(KeyCode::Char('f'))),
        key("←", "previous page", market(KeyCode::Left)),
        key("→", "next page", market(KeyCode::Right)),
        key("o", "open in browser", market(KeyCode::Char('o'))),
        key("r", "refresh listings", market(KeyCode::Char('r'))),
        (Line::raw(""), Vec::new()),
        section("Anywhere"),
        key("↑ ↓", "move", None),
        key("m", "switch views", then(KeyCode::Char('m'))),
        key("Tab", "switch views", then(KeyCode::Tab)),
        key("q", "back or close", close),
        key("Esc", "back or close", close),
        key("?", "this help", close),
    ];
    if mouse {
        lines.extend([
            key("wheel", "move the selection", None),
            key("click", "a key, a tab, × or ←: what that key does", None),
            key("click", "a row selects it; again, what ↵ does there", None),
        ]);
    }
    lines.extend([
        (Line::raw(""), Vec::new()),
        (
            Line::styled(format!("config: {}", Config::path().display()), muted()),
            Vec::new(),
        ),
    ]);
    let rect = centered(area, 72, lines.len() as u16 + 2);
    let visible = rect.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let first = app.help_scroll;
    let last = (first + visible).min(lines.len());
    let mut title = Hint::title(mouse, None, close);
    if max_scroll == 0 {
        title.push(Span::styled(" keys · ", bold), None);
    } else {
        title
            .push(
                Span::styled(
                    format!(" keys {}–{}/{} · ", first + 1, last, lines.len()),
                    bold,
                ),
                None,
            )
            .key(
                "↑↓",
                bold,
                Span::styled(" scroll · ", bold),
                &[press(KeyCode::Up), press(KeyCode::Down)],
            );
    }
    title
        .cap("?", bold, Some(ch('?')))
        .push(Span::styled(" closes ", bold), Some(ch('?')));
    let (shown, clicks): (Vec<Line>, Vec<_>) = lines.drain(first..last).unzip();
    let block = dialog_block(f, app, rect, ACCENT, title);
    f.render_widget(Paragraph::new(shown).block(block), rect);
    for (index, clicks) in clicks.iter().enumerate() {
        place_line(app, rect, index, clicks);
    }
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
    // Herdr keeps a plugin at a commit only when it installed it there, so
    // pinning a plugin Herdr manages reinstalls it, even at the same commit.
    let held_by_herdr = row
        .requested_ref()
        .is_some_and(|r| r.eq_ignore_ascii_case(&commit));
    if !commit.eq_ignore_ascii_case(&current) || (row.origin == Origin::Github && !held_by_herdr) {
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
    // Line of each pick shown, and its index.
    let mut shown = Vec::new();
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
        shown.push((lines.len(), i));
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
    let mut hint = Hint::new(app.mouse());
    hint.key("↵", key_style(), Span::raw(" pin   "), &[Click::Key(ENTER)])
        .key(
            "Esc",
            key_style(),
            Span::raw(" close   "),
            &[Click::Key(ESC)],
        )
        .push(
            Span::styled(
                "another version is installed first, with its preview",
                muted(),
            ),
            None,
        );
    let hint_line = lines.len();
    let (line, hint_clicks) = hint.into_parts();
    lines.push(line);
    let rect = centered(area, 76, lines.len() as u16 + 2);
    let mut title = Hint::title(app.mouse(), None, Some(Click::Key(ESC)));
    title.push(
        Span::styled(
            format!(" Pin {} ", util::sanitize(&picker.name)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        None,
    );
    let block = dialog_block(f, app, rect, ACCENT, title);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    let inner = rect.inner(Margin::new(1, 1));
    for (index, i) in shown {
        place_row(
            app,
            inner,
            index,
            1,
            Click::Row(List::Picker, i, Some(ENTER)),
        );
    }
    place_line(app, rect, hint_line, &hint_clicks);
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
    let key = key_style();
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
    // Line of each leftover shown, and its index.
    let mut shown = Vec::new();
    for (i, orphan) in cleaner.orphans.iter().enumerate().skip(start).take(height) {
        let chosen = cleaner.chosen.get(i).copied().unwrap_or(false);
        let style = if i == cleaner.sel {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        shown.push((lines.len(), i));
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
    let mut hint = Hint::new(app.mouse());
    hint.key("Space", key, Span::raw(" select   "), &[ch(' ')])
        .key("a", key, Span::raw(" all   "), &[ch('a')])
        .key(
            "↵",
            key,
            Span::raw(if count == 0 {
                " clean highlighted   ".to_string()
            } else {
                format!(" clean {count} selected   ")
            }),
            &[Click::Key(ENTER)],
        )
        .key("Esc", key, Span::raw(" close"), &[Click::Key(ESC)]);
    let hint_line = lines.len();
    let (line, hint_clicks) = hint.into_parts();
    lines.push(line);
    let rect = centered(area, 86, lines.len() as u16 + 2);
    let mut title = Hint::title(app.mouse(), None, Some(Click::Key(ESC)));
    title.push(
        Span::styled(
            " Leftovers of removed plugins ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        None,
    );
    let block = dialog_block(f, app, rect, ACCENT, title);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    let inner = rect.inner(Margin::new(1, 1));
    for (index, i) in shown {
        // Selected again, a row is cleaned (after its review); its box
        // is checked or unchecked like Space does.
        place_row(
            app,
            inner,
            index,
            1,
            Click::Row(List::Cleaner, i, Some(ENTER)),
        );
        if i == cleaner.sel {
            place_line(app, rect, index, &[(0, 3, ch(' '))]);
        }
    }
    place_line(app, rect, hint_line, &hint_clicks);
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
    let mut hint = Hint::new(app.mouse());
    hint.key("l", key_style(), Span::styled(toggle, muted()), &[ch('l')]);
    if task.done.is_some() {
        hint.push(Span::styled("   ", muted()), None)
            .cap("any other key", muted(), Some(Click::Key(ESC)))
            .push(Span::styled(" closes", muted()), Some(Click::Key(ESC)));
    }
    let hint_line = lines.len();
    let (line, hint_clicks) = hint.into_parts();
    lines.push(line);
    let rect = centered(area, width, lines.len() as u16 + 2);
    // A running task cannot be closed; a finished one closes on any key.
    let close = task.done.is_some().then_some(Click::Key(ESC));
    let mut title = Hint::title(app.mouse(), None, close);
    title.push(
        Span::styled(
            format!(" {} ", util::sanitize(&task.title)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        None,
    );
    let block = dialog_block(f, app, rect, color, title);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    place_line(app, rect, hint_line, &hint_clicks);
    if let Some(review) = &task.review {
        draw_review(f, app, review, task.batch, area);
    }
}

/// The trust preview of an operation, waiting for y/n.
fn draw_review(f: &mut Frame, app: &App, review: &Review, batch: bool, area: Rect) {
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
    let key = key_style();
    let mut hint = Hint::new(app.mouse());
    hint.key("y", key, Span::raw(" yes   "), &[ch('y')]).key(
        "n",
        key,
        Span::raw(" no   "),
        &[ch('n')],
    );
    if batch {
        hint.key("a", key, Span::raw(" yes to all   "), &[ch('a')]);
    }
    if max_scroll > 0 {
        hint.key(
            "↑↓",
            key,
            Span::styled(
                format!(" scroll {}–{last}/{}", first + 1, review.lines.len()),
                muted(),
            ),
            &[press(KeyCode::Up), press(KeyCode::Down)],
        );
    }
    let hint_line = lines.len();
    let (line, hint_clicks) = hint.into_parts();
    lines.push(line);
    let rect = centered(area, width, lines.len() as u16 + 2);
    // Esc answers no.
    let mut title = Hint::title(app.mouse(), None, Some(Click::Key(ESC)));
    title.push(
        Span::styled(
            format!(" {} ", util::sanitize(&review.title)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        None,
    );
    let block = dialog_block(f, app, rect, WARN, title);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    place_line(app, rect, hint_line, &hint_clicks);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn topmost_region_wins() {
        let mut hits = Hits::new(true);
        hits.add(Rect::new(0, 0, 20, 1), ch('a'));
        hits.add(Rect::new(5, 0, 3, 1), ch('b'));
        assert_eq!(hits.at(6, 0), Some(ch('b')));
        assert_eq!(hits.at(4, 0), Some(ch('a')));
        assert_eq!(hits.at(8, 0), Some(ch('a')));
        assert_eq!(hits.at(6, 1), None);
        assert_eq!(hits.at(25, 0), None);
    }

    #[test]
    fn a_dialog_takes_the_clicks_of_what_it_covers() {
        let mut hits = Hits::new(true);
        // The footer and a list row under the dialog…
        hits.add(Rect::new(0, 9, 40, 1), ch('u'));
        hits.add(Rect::new(0, 2, 40, 1), Click::Row(List::Installed, 0, None));
        // …then the dialog, drawn on top with one button.
        hits.clear();
        hits.add(Rect::new(10, 3, 1, 1), Click::Key(ESC));
        assert_eq!(hits.at(0, 9), None);
        assert_eq!(hits.at(0, 2), None);
        assert_eq!(hits.at(10, 3), Some(Click::Key(ESC)));
    }

    #[test]
    fn keyboard_only_registers_nothing() {
        let mut hits = Hits::new(false);
        hits.add(Rect::new(0, 0, 10, 1), ch('a'));
        assert_eq!(hits.at(0, 0), None);

        let mut hint = Hint::title(false, Some(Click::Key(ESC)), Some(Click::Key(QUIT)));
        assert!(hint.take_buttons().is_none());
        hint.key("u", key_style(), Span::raw(" update  "), &[ch('u')]);
        let (line, clicks) = hint.into_parts();
        assert!(clicks.is_empty());
        // No buttons: just the hint.
        assert_eq!(line.width(), "u update  ".len());
    }

    #[test]
    fn hint_pieces_land_on_their_text() {
        let mut hint = Hint::title(true, Some(Click::Key(ESC)), Some(Click::Key(QUIT)));
        let (buttons, button_clicks, width) = hint.take_buttons().unwrap();
        // " ←×": side by side, for the top right corner.
        assert_eq!((buttons.width(), width), (3, 3));
        let mut hits = Hits::new(true);
        hits.place(&button_clicks, 97, 0, 100);
        assert_eq!(hits.at(97, 0), None);
        assert_eq!(hits.at(98, 0), Some(Click::Key(ESC)));
        assert_eq!(hits.at(99, 0), Some(Click::Key(QUIT)));
        hint.push(Span::raw(" Title "), None);
        hint.key("u", key_style(), Span::raw(" update  "), &[ch('u')])
            .key(
                "←→",
                key_style(),
                Span::raw(" page"),
                &[press(KeyCode::Left), press(KeyCode::Right)],
            );
        let (_, clicks) = hint.into_parts();
        let mut hits = Hits::new(true);
        hits.place(&clicks, 10, 0, 100);
        // " Title u update  ←→ page": the buttons are no longer in the title.
        assert_eq!(hits.at(10, 0), None);
        // `u` and its label, not the spaces after it.
        for col in 17..25 {
            assert_eq!(hits.at(col, 0), Some(ch('u')), "column {col}");
        }
        assert_eq!(hits.at(25, 0), None);
        assert_eq!(hits.at(27, 0), Some(press(KeyCode::Left)));
        assert_eq!(hits.at(28, 0), Some(press(KeyCode::Right)));
        assert_eq!(hits.at(30, 0), None);
    }

    #[test]
    fn regions_stop_at_the_right_edge() {
        let mut hint = Hint::new(true);
        hint.key(
            "q",
            key_style(),
            Span::raw(" quit"),
            &[Click::Keys(ESC, key('q'))],
        );
        let (_, clicks) = hint.into_parts();
        let mut hits = Hits::new(true);
        hits.place(&clicks, 0, 0, 3);
        assert_eq!(hits.at(2, 0), Some(Click::Keys(ESC, key('q'))));
        assert_eq!(hits.at(3, 0), None);
    }
}
