//! The popup: installed plugins and the marketplace, in one terminal UI.
//!
//! Quick changes (enable/disable) happen in place. Anything that shows a
//! trust preview or runs builds (install, update, uninstall, rollback, pin)
//! suspends the UI and runs the same code path as the CLI on the plain
//! terminal, then returns.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Stdout, Write};
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
use crate::git::Git;
use crate::market::{self, Entry, Sort};
use crate::ops::{self, Ctx, InstallRequest, Origin, Row, Status};
use crate::{detach, paths, source, util};

type Term = Terminal<CrosstermBackend<Stdout>>;

// ---------------------------------------------------------------- palette

const ACCENT: Color = Color::Cyan;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Plugin(usize),
    Action(usize, usize),
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
}

#[derive(Debug, Clone)]
enum Confirm {
    Uninstall(String),
    UpdateAll(Vec<String>),
    InstallEntry(Box<Entry>),
    Rollback(String),
    TogglePin(String, bool),
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
}

struct App {
    ctx: Ctx,
    view: View,
    rows: Vec<Row>,
    checks: HashMap<String, Check>,
    expanded: HashSet<String>,
    keys: BTreeMap<String, Vec<String>>,
    items: Vec<Item>,
    sel: usize,
    offset: usize,
    filter: String,
    prompt: Option<(Prompt, String)>,
    confirm: Option<Confirm>,
    message: Option<(Level, String)>,
    help: bool,
    market: MarketState,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    herdr_version: String,
    quit: bool,
    list_area: Rect,
    /// Lines above the first marketplace entry (loading/errors).
    market_header: usize,
    load_error: Option<String>,
}

impl App {
    fn new(ctx: Ctx) -> App {
        let (tx, rx) = mpsc::channel();
        let herdr_version = ctx
            .herdr
            .version()
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "?".into());
        App {
            ctx,
            view: View::Installed,
            rows: Vec::new(),
            checks: HashMap::new(),
            expanded: HashSet::new(),
            keys: BTreeMap::new(),
            items: Vec::new(),
            sel: 0,
            offset: 0,
            filter: String::new(),
            prompt: None,
            confirm: None,
            message: None,
            help: false,
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
            },
            tx,
            rx,
            herdr_version,
            quit: false,
            list_area: Rect::default(),
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
        self.keys = crate::herdr::action_keybindings();
        self.rebuild_items();
        if let Some(id) = selected_id {
            if let Some(pos) = self.items.iter().position(|i| match i {
                Item::Plugin(p) => self.rows[*p].plugin.plugin_id == id,
                _ => false,
            }) {
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
            self.items.push(Item::Plugin(i));
            if self.expanded.contains(&row.plugin.plugin_id) {
                for a in 0..row.plugin.actions.len() {
                    self.items.push(Item::Action(i, a));
                }
            }
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

    fn load_market(&mut self, refresh: bool) {
        self.market.generation += 1;
        self.market.loading = true;
        let generation = self.market.generation;
        let query = self.market.query.clone();
        let only = self.market.source.clone();
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
                Msg::Market(generation, results) => {
                    if generation != self.market.generation {
                        continue;
                    }
                    self.market.loading = false;
                    self.market.loaded = true;
                    self.market.entries.clear();
                    self.market.errors.clear();
                    for (name, result) in results {
                        match result {
                            Ok(listing) => self.market.entries.extend(listing.entries),
                            Err(err) => self.market.errors.push((name, err)),
                        }
                    }
                    market::sort_entries(&mut self.market.entries, self.market.sort);
                    self.market.sel = self
                        .market
                        .sel
                        .min(self.market.entries.len().saturating_sub(1));
                }
            }
        }
        changed
    }

    // ---- selection helpers

    fn selected_item(&self) -> Option<Item> {
        self.items.get(self.sel).copied()
    }

    fn selected_row(&self) -> Option<&Row> {
        match self.selected_item()? {
            Item::Plugin(i) | Item::Action(i, _) => self.rows.get(i),
        }
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

fn enter(term: &mut Term) -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )?;
    term.clear()?;
    Ok(())
}

fn leave() -> Result<()> {
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
    terminal::disable_raw_mode()?;
    Ok(())
}

fn wait_for_key() {
    let _ = terminal::enable_raw_mode();
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let _ = terminal::disable_raw_mode();
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
    if app.ctx.dry_run {
        app.say(Level::Warn, "dry run: changes are printed, not made");
    }

    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave();
        default_hook(info);
    }));
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
        let busy = app.market.loading || app.checks.values().any(|c| matches!(c, Check::Pending));
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
    Uninstall(String),
    Rollback(String),
    Pin(String),
    Unpin(String),
    Switch(String, String),
    EditFile(std::path::PathBuf),
    Logs(String),
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

fn run_job(app: &mut App, term: &mut Term, job: Job) -> Result<()> {
    // An editor returns straight to the UI; everything else waits so the
    // output (preview, build log, result) can be read.
    let pause = !matches!(job, Job::EditFile(_));
    leave()?;
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
    let mut touched: Option<Vec<String>> = None;
    let result: Result<()> = (|| match job {
        Job::Install(spec) => {
            let words = util::split_words(&spec);
            let mut source_arg = None;
            let mut reference = None;
            let mut subdir = None;
            let mut force_git = false;
            let mut replace = false;
            let mut it = words.into_iter();
            while let Some(w) = it.next() {
                match w.as_str() {
                    "--ref" => reference = it.next(),
                    "--subdir" => subdir = it.next(),
                    "--git" => force_git = true,
                    "--replace" => replace = true,
                    _ if source_arg.is_none() => source_arg = Some(w),
                    other => anyhow::bail!("unexpected {other:?}"),
                }
            }
            let source_arg = source_arg.ok_or_else(|| anyhow::anyhow!("no source given"))?;
            let mut parsed = source::parse(&source_arg, &app.ctx.config.aliases)?;
            if force_git {
                parsed = parsed.into_git();
            }
            println!("{} {}\n", ops::bold("install"), parsed.display());
            ops::install(
                &app.ctx,
                InstallRequest {
                    source: parsed.with_subdir(subdir),
                    reference,
                    enabled: None,
                    replace,
                },
            )
            .map(|_| ())
        }
        Job::InstallEntry(entry) => {
            let spec = entry.install_spec();
            let parsed = if entry.is_github() {
                source::parse(&format!("github:{spec}"), &app.ctx.config.aliases)?
            } else {
                source::parse(&spec, &app.ctx.config.aliases)?
            };
            println!(
                "{} {} {}\n",
                ops::bold("install"),
                parsed.display(),
                ops::dim(&format!("[{}]", entry.market))
            );
            ops::install(
                &app.ctx,
                InstallRequest {
                    source: parsed,
                    reference: entry.git_ref.clone(),
                    enabled: None,
                    replace: false,
                },
            )
            .map(|_| ())
        }
        Job::Update(ids) => {
            touched = Some(ids.clone());
            let failures = ops::update(&app.ctx, &ids)?;
            if failures > 0 {
                anyhow::bail!("{failures} update(s) failed");
            }
            Ok(())
        }
        Job::Uninstall(id) => {
            let ctx = Ctx {
                yes: true,
                ..clone_ctx(&app.ctx)
            };
            ops::uninstall(&ctx, &id)
        }
        Job::Rollback(id) => {
            touched = Some(vec![id.clone()]);
            ops::rollback(&app.ctx, &id)
        }
        Job::Pin(id) => {
            touched = Some(vec![id.clone()]);
            ops::pin(&app.ctx, &id, None)
        }
        Job::Unpin(id) => {
            touched = Some(vec![id.clone()]);
            ops::unpin(&app.ctx, &id, None)
        }
        Job::Switch(id, reference) => {
            touched = Some(vec![id.clone()]);
            ops::switch(&app.ctx, &id, &reference)
        }
        Job::EditFile(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !path.exists() && path.ends_with("config.toml") {
                crate::config::Config::init()?;
            }
            let argv = editor_command(&app.ctx.config);
            if app.ctx.dry_run {
                println!("[dry-run] {} {}", argv.join(" "), path.display());
                return Ok(());
            }
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .arg(&path)
                .status()?;
            if !status.success() {
                anyhow::bail!("editor exited with {status}");
            }
            if path.ends_with("config.toml") {
                match Config::load() {
                    Ok(c) => app.ctx.config = c,
                    Err(err) => anyhow::bail!("config not reloaded: {err:#}"),
                }
            }
            Ok(())
        }
        Job::Logs(id) => {
            let status = app
                .ctx
                .herdr
                .interactive(&["plugin", "log", "list", "--plugin", &id, "--limit", "20"])?;
            if !status.success() {
                anyhow::bail!("herdr plugin log list failed");
            }
            Ok(())
        }
    })();
    if pause || result.is_err() {
        match &result {
            Ok(()) => println!("\n{}", ops::green("✓ done")),
            Err(err) => println!("\n{} {err:#}", ops::red("✗")),
        }
        print!("{}", ops::dim("press any key to return"));
        let _ = io::stdout().flush();
        wait_for_key();
    }
    enter(term)?;
    app.reload();
    match touched {
        Some(ids) => app.start_checks(Some(&ids)),
        None => app.start_checks(None),
    }
    match result {
        Ok(()) => app.message = None,
        Err(err) => app.say(Level::Error, util::truncate(&format!("{err:#}"), 200)),
    }
    Ok(())
}

fn clone_ctx(ctx: &Ctx) -> Ctx {
    Ctx {
        herdr: ctx.herdr.clone(),
        config: ctx.config.clone(),
        yes: ctx.yes,
        dry_run: ctx.dry_run,
    }
}

// ---------------------------------------------------------------- input

fn handle_mouse(app: &mut App, kind: MouseEventKind, col: u16, row: u16) -> bool {
    let area = app.list_area;
    match kind {
        MouseEventKind::ScrollDown => {
            move_sel(app, 1);
            true
        }
        MouseEventKind::ScrollUp => {
            move_sel(app, -1);
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if col < area.x
                || col >= area.x + area.width
                || row < area.y
                || row >= area.y + area.height
            {
                return false;
            }
            let line = (row - area.y) as usize;
            match app.view {
                View::Installed => {
                    let idx = app.offset + line;
                    if idx < app.items.len() {
                        if idx == app.sel {
                            if let Item::Plugin(_) = app.items[idx] {
                                toggle_expand(app);
                            }
                        }
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

fn toggle_expand(app: &mut App) {
    let Some(Item::Plugin(i)) = app.selected_item() else {
        return;
    };
    let row = &app.rows[i];
    let id = row.plugin.plugin_id.clone();
    if row.plugin.actions.is_empty() {
        let name = row.plugin.name.clone();
        app.say(Level::Info, format!("{name} declares no actions"));
        return;
    }
    if !app.expanded.remove(&id) {
        app.expanded.insert(id);
    }
    app.rebuild_items();
    app.clamp();
}

fn handle_key(app: &mut App, key: KeyEvent) -> Option<Job> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return None;
    }
    if app.help {
        app.help = false;
        return None;
    }
    if let Some(confirm) = app.confirm.take() {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => confirmed(app, confirm),
            _ => {
                app.say(Level::Info, "cancelled");
                None
            }
        };
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
    app.message = None;
    match key.code {
        KeyCode::Char('?') => {
            app.help = true;
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
            app.view = if app.view == View::Installed {
                View::Market
            } else {
                View::Installed
            };
            if app.view == View::Market && !app.market.loaded && !app.market.loading {
                app.load_market(false);
            }
            return None;
        }
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
        KeyCode::Enter | KeyCode::Char(' ') if key.code == KeyCode::Enter => {
            match app.selected_item() {
                Some(Item::Plugin(_)) => toggle_expand(app),
                Some(Item::Action(p, a)) => {
                    let row = &app.rows[p];
                    let action = format!("{}.{}", row.plugin.plugin_id, row.plugin.actions[a].id);
                    if !row.plugin.enabled {
                        app.say(
                            Level::Warn,
                            format!(
                                "{} is disabled; press e to enable it first",
                                row.plugin.name
                            ),
                        );
                    } else if app.ctx.dry_run {
                        app.say(
                            Level::Warn,
                            format!("[dry-run] close popup → herdr plugin action invoke {action}"),
                        );
                    } else {
                        match detach::spawn_invoke_after_exit(&action) {
                            Ok(()) => app.quit = true,
                            Err(err) => app.say(Level::Error, format!("{err:#}")),
                        }
                    }
                }
                None => {}
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if let Some(Item::Plugin(i)) = app.selected_item() {
                if !app.expanded.contains(&app.rows[i].plugin.plugin_id) {
                    toggle_expand(app);
                }
            }
        }
        KeyCode::Left | KeyCode::Char('h') => match app.selected_item() {
            Some(Item::Action(p, _)) => {
                let id = app.rows[p].plugin.plugin_id.clone();
                app.expanded.remove(&id);
                app.rebuild_items();
                if let Some(pos) = app.items.iter().position(|i| *i == Item::Plugin(p)) {
                    app.sel = pos;
                }
            }
            Some(Item::Plugin(i)) => {
                let id = app.rows[i].plugin.plugin_id.clone();
                if app.expanded.remove(&id) {
                    app.rebuild_items();
                    app.clamp();
                }
            }
            None => {}
        },
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
            let id = id?;
            if id == paths::PLUGIN_ID {
                app.say(Level::Warn, "use `herdr-pm uninstall herdr-plugin-manager` from a shell to remove the manager itself");
            } else {
                app.confirm = Some(Confirm::Uninstall(id));
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
                app.confirm = Some(Confirm::Rollback(row.plugin.plugin_id.clone()));
            }
        }
        KeyCode::Char('p') => {
            let row = row?;
            if row.origin == Origin::Local {
                app.say(Level::Info, "local links cannot be pinned");
            } else {
                app.confirm = Some(Confirm::TogglePin(
                    row.plugin.plugin_id.clone(),
                    !row.pinned(),
                ));
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
        KeyCode::Char('c') => return Some(Job::EditFile(paths::herdr_plugins_json())),
        KeyCode::Char('C') => return Some(Job::EditFile(paths::pm_config_file())),
        KeyCode::Char('L') => return Some(Job::Logs(id?)),
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
                app.confirm = Some(Confirm::InstallEntry(Box::new(entry)));
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
    }
}

fn confirmed(app: &mut App, confirm: Confirm) -> Option<Job> {
    let _ = app;
    Some(match confirm {
        Confirm::Uninstall(id) => Job::Uninstall(id),
        Confirm::UpdateAll(ids) => Job::Update(ids),
        Confirm::InstallEntry(entry) => Job::InstallEntry(entry),
        Confirm::Rollback(id) => Job::Rollback(id),
        Confirm::TogglePin(id, true) => Job::Pin(id),
        Confirm::TogglePin(id, false) => Job::Unpin(id),
    })
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
    let t = util::truncate(text, width);
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
    if let Some(confirm) = &app.confirm {
        draw_confirm(f, confirm, area);
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
    let mut spans = vec![
        Span::styled(
            " herdr plugins ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        tab(installed_label, app.view == View::Installed),
        Span::raw(" "),
        tab(market_label, app.view == View::Market),
    ];
    let mut right = format!("herdr {}", app.herdr_version);
    if app.ctx.dry_run {
        right = format!("DRY RUN · {right}");
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let gap = (area.width as usize).saturating_sub(used + right.chars().count() + 1);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(
        right,
        if app.ctx.dry_run {
            Style::default().fg(WARN)
        } else {
            muted()
        },
    ));
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
                    version
                        .clone()
                        .filter(|v| *v != row.plugin.version)
                        .unwrap_or_else(|| util::short_sha(commit).to_string())
                ),
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
            Status::Pinned => Span::styled("pinned", muted()),
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
            Line::raw(err.clone()),
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
    let mut plugin_number = 0usize;
    for (pos, item) in app.items.iter().enumerate() {
        if let Item::Plugin(_) = item {
            plugin_number += 1;
        }
        if pos < app.offset || pos >= app.offset + height {
            continue;
        }
        let selected = pos == app.sel;
        let base = if selected {
            Style::default()
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let line = match *item {
            Item::Plugin(i) => {
                let row = &app.rows[i];
                let dot = if row.plugin.enabled {
                    Span::styled("● ", Style::default().fg(OK))
                } else {
                    Span::styled("○ ", muted())
                };
                let fold = if row.plugin.actions.is_empty() {
                    " "
                } else if app.expanded.contains(&row.plugin.plugin_id) {
                    "⌄"
                } else {
                    "›"
                };
                let origin_color = match row.origin {
                    Origin::Git => Color::Magenta,
                    Origin::Github => Color::Blue,
                    Origin::Local => MUTED,
                };
                let status = status_span(app, row);
                let left = format!("{plugin_number:>2} ");
                let used = left.len() + 2 + name_w + 1 + 9 + 1 + 7 + 1 + 1;
                let status_w = status.content.chars().count();
                let src_w = width.saturating_sub(used + status_w + 2);
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
                    status,
                ];
                let used_now: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                spans.push(Span::raw(" ".repeat(width.saturating_sub(used_now + 1))));
                spans.push(Span::styled(fold, Style::default().fg(ACCENT)));
                Line::from(spans)
            }
            Item::Action(p, a) => {
                let row = &app.rows[p];
                let action = &row.plugin.actions[a];
                let full = format!("{}.{}", row.plugin.plugin_id, action.id);
                let keys = app.keys.get(&full).map(|k| k.join(", "));
                let mut spans = vec![
                    Span::raw("       "),
                    Span::styled("↳ ", Style::default().fg(ACCENT)),
                    Span::raw(pad(&action.id, 18)),
                    Span::raw(" "),
                    Span::styled(
                        util::truncate(&action.title, width.saturating_sub(48).max(10)),
                        muted(),
                    ),
                ];
                let used_now: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                let key_text = keys.unwrap_or_default();
                spans.push(Span::raw(" ".repeat(
                    width.saturating_sub(used_now + key_text.chars().count() + 1),
                )));
                spans.push(Span::styled(key_text, Style::default().fg(OK)));
                Line::from(spans)
            }
        };
        lines.push(line.style(base));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn kv(key: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<8} "), muted()),
        Span::raw(value.into()),
    ])
}

fn draw_installed_detail(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    match app.selected_item() {
        Some(Item::Plugin(i)) => {
            let row = &app.rows[i];
            lines.push(Line::from(vec![
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
            ]));
            match row.origin {
                Origin::Local => lines.push(kv("path", row.plugin.plugin_root.clone())),
                _ => {
                    lines.push(kv("source", row.source_display()));
                    let commit = row.commit().map(util::short_sha).unwrap_or("-").to_string();
                    let mut text = format!("{} @ {commit}", row.ref_display());
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
                }
            }
            match app.checks.get(&row.plugin.plugin_id) {
                Some(Check::Done(Status::Error { message })) => lines.push(Line::from(vec![
                    Span::styled("check    ", muted()),
                    Span::styled(message.clone(), Style::default().fg(BAD)),
                ])),
                Some(Check::Done(Status::Available { commit, version })) => {
                    lines.push(Line::from(vec![
                        Span::styled("update   ", muted()),
                        Span::styled(
                            format!(
                                "{} → {} ({}) — press u",
                                row.plugin.version,
                                version.clone().unwrap_or_else(|| "new commit".into()),
                                util::short_sha(commit)
                            ),
                            Style::default().fg(WARN),
                        ),
                    ]))
                }
                _ => {}
            }
            if let Some(d) = &row.plugin.description {
                lines.push(Line::styled(util::sanitize(d), muted()));
            }
            for w in &row.plugin.warnings {
                lines.push(Line::styled(
                    format!("warning: {w}"),
                    Style::default().fg(WARN),
                ));
            }
        }
        Some(Item::Action(p, a)) => {
            let row = &app.rows[p];
            let action = &row.plugin.actions[a];
            let full = format!("{}.{}", row.plugin.plugin_id, action.id);
            lines.push(Line::styled(
                full.clone(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
            lines.push(match app.keys.get(&full) {
                Some(keys) => kv("key", keys.join(", ")),
                None => Line::from(vec![
                    Span::styled("key      ", muted()),
                    Span::styled("not bound", muted()),
                ]),
            });
            lines.push(kv("runs", crate::manifest::render_argv(&action.command)));
            if !action.contexts.is_empty() {
                lines.push(kv("contexts", action.contexts.join(", ")));
            }
            lines.push(Line::styled("Enter closes the popup and runs it", muted()));
        }
        None => {}
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
            util::truncate(&format!("✗ {name}: {err}"), inner.width as usize),
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
        let stars = entry.stars.map(|s| format!("★{s}")).unwrap_or_default();
        let age = util::relative_age(entry.updated_unix);
        let right = format!("{stars:>6} {age:>8}");
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
            entry.title.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        if let Some(v) = &entry.version {
            head.push(Span::styled(format!("  {v}"), muted()));
        }
        if let Some(id) = &entry.plugin_id {
            head.push(Span::styled(format!("  ({id})"), muted()));
        }
        lines.push(Line::from(head));
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
            lines.push(Line::styled(
                format!(
                    "installed as {} {}",
                    row.plugin.plugin_id, row.plugin.version
                ),
                Style::default().fg(OK),
            ));
        } else {
            lines.push(Line::styled(
                "Enter installs after a preview of everything it runs. Listings are not reviewed.",
                muted(),
            ));
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
        View::Installed => match app.selected_item() {
            Some(Item::Action(..)) => &[
                ("↵", "run"),
                ("h", "fold"),
                ("m", "market"),
                ("?", "help"),
                ("q", "quit"),
            ],
            _ => &[
                ("↵", "actions"),
                ("u", "update"),
                ("U", "all"),
                ("e", "toggle"),
                ("x", "remove"),
                ("i", "install"),
                ("m", "market"),
                ("?", "help"),
                ("q", "quit"),
            ],
        },
        View::Market => &[
            ("↵", "install"),
            ("/", "search"),
            ("←→", "page"),
            ("s", "sort"),
            ("f", "source"),
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
    let (title, body): (&str, Vec<String>) = match confirm {
        Confirm::Uninstall(id) => ("Uninstall", vec![format!("Uninstall {id}?")]),
        Confirm::UpdateAll(ids) => {
            let mut b = vec![format!("Update {} plugin(s)?", ids.len())];
            b.extend(ids.iter().take(8).map(|i| format!("  • {i}")));
            if ids.len() > 8 {
                b.push(format!("  … {} more", ids.len() - 8));
            }
            b.push("Each one still shows its preview before anything runs.".into());
            ("Update all", b)
        }
        Confirm::InstallEntry(e) => (
            "Install",
            vec![
                format!("Install {} from [{}]?", e.title, e.market),
                format!("  {}", e.install_spec()),
                "A preview of everything it runs comes next.".into(),
            ],
        ),
        Confirm::Rollback(id) => (
            "Rollback",
            vec![format!("Return {id} to its previous installation?")],
        ),
        Confirm::TogglePin(id, true) => ("Pin", vec![format!("Hold {id} at its current commit?")]),
        Confirm::TogglePin(id, false) => {
            ("Unpin", vec![format!("Let {id} follow its branch again?")])
        }
    };
    let width = body
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(20)
        .max(30) as u16
        + 4;
    let rect = centered(area, width, body.len() as u16 + 4);
    f.render_widget(Clear, rect);
    let mut lines: Vec<Line> = body.into_iter().map(Line::raw).collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("y", Style::default().fg(OK).add_modifier(Modifier::BOLD)),
        Span::raw(" yes   "),
        Span::styled("any other key", muted()),
        Span::raw(" no"),
    ]));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let section = |t: &str| {
        Line::styled(
            t.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    };
    let key = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(
                format!("  {k:<10}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(d.to_string()),
        ])
    };
    let mut lines = vec![
        section("Installed"),
        key("↵ / → / ←", "fold actions · run the selected action"),
        key("u / U", "update selected / every outdated plugin"),
        key("e", "enable or disable"),
        key("x", "uninstall (local links are only unlinked)"),
        key("i", "install from a URL, owner/repo or alias:path"),
        key("p", "pin to current commit / unpin"),
        key("s", "switch to another branch, tag or commit"),
        key("z", "roll back to the previous installation"),
        key("o", "open the source in a browser"),
        key("L", "recent commands herdr ran for the plugin"),
        key("c / C", "edit plugins.json / this manager's config"),
        key("/", "filter the list"),
        key("r", "refresh and re-check updates"),
        section("Marketplace"),
        key("↵", "install the selection (preview first)"),
        key("/ s f", "search · sort · cycle sources"),
        key("← →", "previous / next page"),
        key("o r", "open in browser · refresh listings"),
        section("Anywhere"),
        key("m / Tab", "switch views"),
        key("q / Esc", "back or close"),
    ];
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("config: {}", Config::path().display()),
        muted(),
    ));
    let _ = app;
    let rect = centered(area, 72, lines.len() as u16 + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " keys — any key closes ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}
