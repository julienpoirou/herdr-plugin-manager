//! herdr-plugin-manager (`herdr-pm`): manage Herdr plugins from a popup or
//! the command line, from GitHub or any git server.

mod config;
mod detach;
mod git;
mod herdr;
mod index;
mod manifest;
mod market;
mod ops;
mod paths;
mod registry;
mod source;
mod tui;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::Config;
use crate::git::Git;
use crate::herdr::Herdr;
use crate::ops::{Ctx, InstallRequest, Origin, Status};

#[derive(Parser)]
#[command(
    name = "herdr-pm",
    version,
    about = "Manage Herdr plugins from GitHub or any git server (Gitea, Forgejo, GitLab, plain git).",
    after_help = "Run without a command to open the interactive manager.\n\
                  Sources: owner/repo (GitHub), https://… ssh://… git@host:path git://… file://… /local/path, \
                  alias:path (see `herdr-pm config init`). Append //subdir to pick a plugin inside a repository."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Print what would change without changing anything.
    #[arg(long, global = true, env = "HERDR_PM_DRY_RUN")]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct YesFlag {
    /// Do not ask for confirmation.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the interactive manager (the default).
    Tui,
    /// List installed plugins.
    #[command(alias = "ls")]
    List {
        /// Also check every plugin for updates.
        #[arg(short, long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install a plugin from GitHub, a git URL, a local repository or an alias.
    #[command(alias = "add")]
    Install {
        source: String,
        /// Branch, tag or commit to install (and follow).
        #[arg(long = "ref", value_name = "REF")]
        reference: Option<String>,
        /// Plugin directory inside the repository.
        #[arg(long)]
        subdir: Option<String>,
        /// Register the plugin disabled.
        #[arg(long)]
        disabled: bool,
        /// Fetch GitHub shorthands with git instead of herdr's installer
        /// (for private repositories over SSH).
        #[arg(long)]
        git: bool,
        /// Take over a plugin with the same id installed another way.
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Update plugins (all GitHub- and git-installed ones by default).
    #[command(alias = "upgrade")]
    Update {
        ids: Vec<String>,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Show which plugins have updates.
    Outdated {
        #[arg(long)]
        json: bool,
    },
    /// Uninstall a plugin (linked plugins are only unlinked).
    #[command(alias = "remove", alias = "rm")]
    Uninstall {
        id: String,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Enable a plugin.
    Enable { id: String },
    /// Disable a plugin.
    Disable { id: String },
    /// Show everything known about a plugin.
    Info {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Go back to the previous installation of a git-managed plugin.
    Rollback {
        id: String,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Hold a plugin at its current commit (or at REF).
    Pin {
        id: String,
        #[arg(value_name = "REF")]
        reference: Option<String>,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Follow a branch or tag again (the default branch without REF).
    Unpin {
        id: String,
        #[arg(value_name = "REF")]
        reference: Option<String>,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Install another branch, tag or commit and follow it.
    Switch {
        id: String,
        #[arg(value_name = "REF")]
        reference: String,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Search every configured marketplace source.
    Search {
        /// Words to match (empty lists everything).
        query: Vec<String>,
        /// Only this marketplace source.
        #[arg(long)]
        source: Option<String>,
        /// stars, updated or name.
        #[arg(long, default_value = "stars")]
        sort: String,
        /// Ignore cached listings.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// List configured marketplace sources.
    Sources,
    /// Build or check a static plugin index (a marketplace for plain git).
    Index {
        #[command(subcommand)]
        command: IndexCmd,
    },
    /// Write installed plugins to a file.
    Export {
        /// Output file (stdout when omitted).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Install the plugins of an export.
    Import {
        file: PathBuf,
        /// Only these plugin ids.
        ids: Vec<String>,
        #[command(flatten)]
        yes: YesFlag,
    },
    /// Recent changes made by this manager.
    History {
        id: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Check herdr, git, config, plugins, key bindings and marketplaces.
    Doctor,
    /// Show or create the config file.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCmd>,
    },
    /// Open the manager as a Herdr popup (what the `open` action runs).
    Open,
    #[command(hide = true, name = "__invoke-after-exit")]
    InvokeAfterExit { pid: u32, action: String },
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Read manifests from git repositories and write an index JSON.
    Build {
        /// Repository URL (repeatable), optionally `URL@REF`… use --repos for refs.
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// File with one `url [ref]` per line.
        #[arg(long = "repos-file", value_name = "FILE")]
        repos_file: Option<PathBuf>,
        /// Index every repository found under this directory (bare or not).
        #[arg(long = "scan-dir", value_name = "DIR")]
        scan_dir: Option<PathBuf>,
        /// Public URL prefix (or `{path}` template) for repositories found by
        /// --scan-dir, e.g. ssh://git@git.corp.local/srv/git
        #[arg(long = "url-base", value_name = "URL")]
        url_base: Option<String>,
        /// Catalog name shown to users.
        #[arg(long)]
        name: Option<String>,
        /// Output file (stdout when omitted).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Validate an index file.
    Check { file: PathBuf },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the config file path.
    Path,
    /// Write a commented example config if none exists.
    Init,
    /// Print the effective configuration.
    Show,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{} {err:#}", ops::red("error:"));
            ExitCode::from(1)
        }
    }
}

fn ctx(dry_run: bool, yes: bool) -> Result<Ctx> {
    Ok(Ctx {
        herdr: Herdr::detect(dry_run),
        config: Config::load()?,
        yes,
        dry_run,
    })
}

fn code(failures: usize) -> ExitCode {
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let dry = cli.dry_run;
    let Some(command) = cli.command else {
        return tui::run(ctx(dry, false)?).map(|_| ExitCode::SUCCESS);
    };
    match command {
        Cmd::Tui => tui::run(ctx(dry, false)?).map(|_| ExitCode::SUCCESS),
        Cmd::List { check, json } => list(&ctx(dry, false)?, check, json),
        Cmd::Install {
            source,
            reference,
            subdir,
            disabled,
            git,
            replace,
            yes,
        } => {
            let ctx = ctx(dry, yes.yes)?;
            let mut parsed = source::parse(&source, &ctx.config.aliases)?;
            if git {
                parsed = parsed.into_git();
            }
            let parsed = parsed.with_subdir(subdir);
            ops::install(
                &ctx,
                InstallRequest {
                    source: parsed,
                    reference,
                    enabled: disabled.then_some(false),
                    replace,
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Update { ids, yes } => Ok(code(ops::update(&ctx(dry, yes.yes)?, &ids)?)),
        Cmd::Outdated { json } => outdated(&ctx(dry, false)?, json),
        Cmd::Uninstall { id, yes } => {
            ops::uninstall(&ctx(dry, yes.yes)?, &id).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Enable { id } => {
            ops::set_enabled(&ctx(dry, false)?, &id, true).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Disable { id } => {
            ops::set_enabled(&ctx(dry, false)?, &id, false).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Info { id, json } => info(&ctx(dry, false)?, &id, json),
        Cmd::Rollback { id, yes } => {
            ops::rollback(&ctx(dry, yes.yes)?, &id).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Pin { id, reference, yes } => {
            ops::pin(&ctx(dry, yes.yes)?, &id, reference.as_deref()).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Unpin { id, reference, yes } => {
            ops::unpin(&ctx(dry, yes.yes)?, &id, reference.as_deref()).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Switch { id, reference, yes } => {
            ops::switch(&ctx(dry, yes.yes)?, &id, &reference).map(|_| ExitCode::SUCCESS)
        }
        Cmd::Search {
            query,
            source,
            sort,
            refresh,
            json,
            limit,
        } => search(
            &Config::load()?,
            &query.join(" "),
            source.as_deref(),
            &sort,
            refresh,
            json,
            limit,
        ),
        Cmd::Sources => sources(&Config::load()?),
        Cmd::Index { command } => index_cmd(command),
        Cmd::Export { output } => export(&Herdr::detect(dry), output),
        Cmd::Import { file, ids, yes } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("could not read {}", file.display()))?;
            let parsed: ops::ExportFile =
                serde_json::from_str(&text).context("not a valid export file")?;
            Ok(code(ops::import(&ctx(dry, yes.yes)?, &parsed, &ids)?))
        }
        Cmd::History { id, limit } => history(id.as_deref(), limit),
        Cmd::Doctor => Ok(code(ops::doctor(&ctx(dry, false)?))),
        Cmd::Config { command } => config_cmd(command),
        Cmd::Open => {
            let herdr = Herdr::detect(false);
            herdr.open_popup(paths::PLUGIN_ID, "manager")?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::InvokeAfterExit { pid, action } => {
            detach::invoke_after_exit(&Herdr::detect(false), pid, &action);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn list(ctx: &Ctx, check: bool, json: bool) -> Result<ExitCode> {
    let rows = ops::rows(&ctx.herdr)?;
    let statuses: Vec<Option<Status>> = if check {
        check_all(&rows, ctx.config.check_concurrency)
    } else {
        vec![None; rows.len()]
    };
    if json {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .zip(&statuses)
            .map(|(row, status)| {
                serde_json::json!({
                    "id": row.plugin.plugin_id,
                    "name": row.plugin.name,
                    "version": row.plugin.version,
                    "enabled": row.plugin.enabled,
                    "origin": row.origin,
                    "source": row.source_display(),
                    "ref": row.requested_ref(),
                    "commit": row.commit(),
                    "root": row.plugin.plugin_root,
                    "status": status,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        println!("No plugins installed. Try `herdr-pm search` or `herdr-pm install <source>`.");
        return Ok(ExitCode::SUCCESS);
    }
    let width = rows
        .iter()
        .map(|r| r.plugin.plugin_id.len())
        .max()
        .unwrap_or(10)
        .min(36);
    for (row, status) in rows.iter().zip(&statuses) {
        let dot = if row.plugin.enabled {
            ops::green("●")
        } else {
            ops::dim("○")
        };
        let status_text = match status {
            None => String::new(),
            Some(Status::Current) => ops::green("up to date"),
            Some(Status::Available { commit, version }) => ops::yellow(&format!(
                "↑ {}",
                version
                    .clone()
                    .filter(|v| *v != row.plugin.version)
                    .unwrap_or_else(|| util::short_sha(commit).to_string())
            )),
            Some(Status::Pinned) => ops::dim("pinned"),
            Some(Status::Local) => ops::dim("local"),
            Some(Status::Unknown) => ops::dim("?"),
            Some(Status::Error { message }) => ops::red(&util::truncate(message, 50)),
        };
        println!(
            "{dot} {:<width$}  {:<9} {:<7} {}  {}",
            row.plugin.plugin_id,
            util::truncate(&row.plugin.version, 9),
            row.origin.label(),
            ops::dim(&util::truncate(&row.source_display(), 60)),
            status_text,
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn check_all(rows: &[ops::Row], concurrency: usize) -> Vec<Option<Status>> {
    let concurrency = concurrency.max(1);
    let mut out = vec![None; rows.len()];
    for (chunk_index, chunk) in rows.chunks(concurrency).enumerate() {
        let results: Vec<Status> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|row| {
                    scope.spawn(move || {
                        let status = ops::check(row, Git::BACKGROUND);
                        if let Status::Available { commit, .. } = &status {
                            let version = ops::remote_version(row, commit, Git::BACKGROUND);
                            return Status::Available {
                                commit: commit.clone(),
                                version,
                            };
                        }
                        status
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(Status::Unknown))
                .collect()
        });
        for (i, status) in results.into_iter().enumerate() {
            out[chunk_index * concurrency + i] = Some(status);
        }
    }
    out
}

fn outdated(ctx: &Ctx, json: bool) -> Result<ExitCode> {
    let rows = ops::rows(&ctx.herdr)?;
    let statuses = check_all(&rows, ctx.config.check_concurrency);
    let mut errors = 0;
    let mut found = Vec::new();
    for (row, status) in rows.iter().zip(statuses) {
        match status {
            Some(Status::Available { commit, version }) => found.push((row, commit, version)),
            Some(Status::Error { message }) => {
                errors += 1;
                eprintln!("{} {}: {message}", ops::red("✗"), row.plugin.plugin_id);
            }
            _ => {}
        }
    }
    if json {
        let items: Vec<serde_json::Value> = found
            .iter()
            .map(|(row, commit, version)| {
                serde_json::json!({
                    "id": row.plugin.plugin_id,
                    "current_version": row.plugin.version,
                    "current_commit": row.commit(),
                    "available_commit": commit,
                    "available_version": version,
                    "origin": row.origin,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if found.is_empty() {
        println!("{} Everything is up to date.", ops::green("✓"));
    } else {
        for (row, commit, version) in &found {
            println!(
                "{} {}  {} ({}) → {} ({})",
                ops::yellow("↑"),
                row.plugin.plugin_id,
                row.plugin.version,
                util::short_sha(row.commit().unwrap_or("")),
                version.clone().unwrap_or_else(|| "?".into()),
                util::short_sha(commit)
            );
        }
        println!("Run `herdr-pm update` to apply.");
    }
    Ok(code(errors))
}

fn info(ctx: &Ctx, id: &str, json: bool) -> Result<ExitCode> {
    let rows = ops::rows(&ctx.herdr)?;
    let Some(row) = ops::find_row(&rows, id) else {
        bail!("plugin not installed: {id}");
    };
    if json {
        let fields: serde_json::Map<String, serde_json::Value> = ops::describe(row)
            .into_iter()
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();
        println!("{}", serde_json::to_string_pretty(&fields)?);
        return Ok(ExitCode::SUCCESS);
    }
    for (key, value) in ops::describe(row) {
        println!("{:<10} {value}", ops::dim(&key));
    }
    if row.origin != Origin::Local {
        let status = ops::check(row, Git::BACKGROUND);
        let text = match status {
            Status::Current => ops::green("up to date"),
            Status::Available { commit, .. } => {
                ops::yellow(&format!("update available ({})", util::short_sha(&commit)))
            }
            Status::Pinned => "pinned".into(),
            Status::Error { message } => ops::red(&message),
            _ => "-".into(),
        };
        println!("{:<10} {text}", ops::dim("status"));
    }
    Ok(ExitCode::SUCCESS)
}

fn search(
    config: &Config,
    query: &str,
    only: Option<&str>,
    sort: &str,
    refresh: bool,
    json: bool,
    limit: usize,
) -> Result<ExitCode> {
    let sort = market::Sort::parse(sort).context("--sort must be stars, updated or name")?;
    if let Some(name) = only {
        if !config.marketplace.iter().any(|s| s.name == name) {
            bail!("no marketplace source named {name:?} (see `herdr-pm sources`)");
        }
    }
    let results = market::search_all(config, query, refresh, only);
    let mut entries = Vec::new();
    let mut failures = 0;
    for r in results {
        match r.result {
            Ok(listing) => entries.extend(listing.entries),
            Err(err) => {
                failures += 1;
                eprintln!("{} {}: {err:#}", ops::red("✗"), r.name);
            }
        }
    }
    market::sort_entries(&mut entries, sort);
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(code(failures));
    }
    if entries.is_empty() {
        println!("No plugins found.");
        return Ok(code(failures));
    }
    let shown = entries.len().min(limit);
    for e in entries.iter().take(limit) {
        let stars = e.stars.map(|s| format!("★{s}")).unwrap_or_default();
        let version = e
            .version
            .as_deref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        println!(
            "{}{} {}  {}  {}",
            ops::bold(&e.title),
            version,
            ops::dim(&format!("[{}]", e.market)),
            ops::yellow(&stars),
            ops::dim(&util::relative_age(e.updated_unix))
        );
        if !e.description.is_empty() {
            println!("    {}", util::truncate(&e.description, 90));
        }
        println!(
            "    {}",
            ops::cyan(&format!("herdr-pm install {}", e.install_spec()))
        );
    }
    if entries.len() > shown {
        println!(
            "{}",
            ops::dim(&format!("… {} more (use --limit)", entries.len() - shown))
        );
    }
    Ok(code(failures))
}

fn sources(config: &Config) -> Result<ExitCode> {
    for s in &config.marketplace {
        println!(
            "{} {:<14} {:<9} {}{}",
            if s.enabled {
                ops::green("●")
            } else {
                ops::dim("○")
            },
            s.name,
            s.kind.label(),
            s.url.as_deref().unwrap_or("https://api.github.com"),
            if matches!(
                s.kind,
                config::MarketKind::Index | config::MarketKind::GitIndex
            ) {
                String::new()
            } else {
                ops::dim(&format!("  topic:{}", s.topic))
            }
        );
    }
    for (name, target) in &config.aliases {
        println!("  alias {name}: → {target}");
    }
    println!(
        "{}",
        ops::dim(&format!("config: {}", Config::path().display()))
    );
    Ok(ExitCode::SUCCESS)
}

fn index_cmd(command: IndexCmd) -> Result<ExitCode> {
    match command {
        IndexCmd::Build {
            repos,
            repos_file,
            scan_dir,
            url_base,
            name,
            output,
        } => {
            let mut specs: Vec<(index::RepoSpec, Option<String>)> = repos
                .into_iter()
                .map(|url| (index::RepoSpec { url, git_ref: None }, None))
                .collect();
            if let Some(file) = repos_file {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("could not read {}", file.display()))?;
                specs.extend(index::parse_repo_list(&text).into_iter().map(|s| (s, None)));
            }
            if let Some(dir) = scan_dir {
                let local = index::scan_dir(&dir, None)?;
                let public = index::scan_dir(&dir, url_base.as_deref())?;
                for (l, p) in local.into_iter().zip(public) {
                    // Read from disk, publish the URL clients clone from.
                    specs.push((p, Some(l.url)));
                }
            }
            if specs.is_empty() {
                bail!("nothing to index: pass --repo, --repos-file or --scan-dir");
            }
            eprintln!(
                "indexing {} repositor{}…",
                specs.len(),
                if specs.len() == 1 { "y" } else { "ies" }
            );
            let report = index::build(name, &specs);
            for w in &report.warnings {
                eprintln!("{} {w}", ops::yellow("warning:"));
            }
            let text = serde_json::to_string_pretty(&report.index)?;
            match output {
                Some(path) => {
                    std::fs::write(&path, format!("{text}\n"))?;
                    eprintln!(
                        "{} wrote {} plugin(s) to {}",
                        ops::green("✓"),
                        report.index.plugins.len(),
                        path.display()
                    );
                }
                None => println!("{text}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        IndexCmd::Check { file } => {
            let text = std::fs::read_to_string(&file)?;
            let index = index::Index::parse(&text)?;
            let mut problems = 0;
            for p in &index.plugins {
                if source::parse(&p.source, &Default::default()).is_err() {
                    problems += 1;
                    eprintln!("{} {}: unusable source {:?}", ops::red("✗"), p.id, p.source);
                }
            }
            println!(
                "{} {} plugin(s){}",
                if problems == 0 {
                    ops::green("✓")
                } else {
                    ops::red("✗")
                },
                index.plugins.len(),
                index.name.map(|n| format!(" in {n}")).unwrap_or_default()
            );
            Ok(code(problems))
        }
    }
}

fn export(herdr: &Herdr, output: Option<PathBuf>) -> Result<ExitCode> {
    let (file, skipped) = ops::export(herdr)?;
    for s in &skipped {
        eprintln!("{} skipped {s}", ops::dim("•"));
    }
    let text = serde_json::to_string_pretty(&file)?;
    match output {
        Some(path) => {
            std::fs::write(&path, format!("{text}\n"))?;
            eprintln!(
                "{} exported {} plugin(s) to {}",
                ops::green("✓"),
                file.plugins.len(),
                path.display()
            );
        }
        None => println!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn history(id: Option<&str>, limit: usize) -> Result<ExitCode> {
    let events = registry::read_events(id, limit);
    if events.is_empty() {
        println!("No recorded changes.");
    }
    for e in events {
        let change = match (&e.from, &e.to) {
            (Some(f), Some(t)) => format!("{} → {}", util::short_sha(f), util::short_sha(t)),
            (None, Some(t)) => util::short_sha(t).to_string(),
            (Some(f), None) => format!("{} → -", util::short_sha(f)),
            _ => String::new(),
        };
        println!(
            "{} {} {:<10} {:<28} {}{}",
            ops::dim(&util::format_unix(e.unix)),
            if e.ok {
                ops::green("✓")
            } else {
                ops::red("✗")
            },
            e.action,
            e.plugin_id,
            change,
            e.detail
                .map(|d| ops::dim(&format!("  {}", util::truncate(&d, 80))))
                .unwrap_or_default()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn config_cmd(command: Option<ConfigCmd>) -> Result<ExitCode> {
    match command.unwrap_or(ConfigCmd::Path) {
        ConfigCmd::Path => println!("{}", Config::path().display()),
        ConfigCmd::Init => {
            let (path, created) = Config::init()?;
            if created {
                println!("{} wrote {}", ops::green("✓"), path.display());
            } else {
                println!("{} already exists", path.display());
            }
        }
        ConfigCmd::Show => println!("{}", toml::to_string_pretty(&Config::load()?)?),
    }
    Ok(ExitCode::SUCCESS)
}
