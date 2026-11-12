# Herdr Plugin Manager

<img src="assets/logo.png" alt="Herdr Plugin Manager logo" width="112" align="right">

[![CI](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/ci.yml)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-a78bfa)](https://www.rust-lang.org/)
[![Herdr 0.7.4+](https://img.shields.io/badge/herdr-0.7.4%2B-2dd4bf)](https://herdr.dev/)
[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Herdr Plugin Manager connects Herdr to the Git servers you actually use.** It
installs plugins from GitHub, GitLab, Gitea/Forgejo, a company Git appliance, a
bare repository, or a directory on disk. The same `herdr-pm` binary provides a
Ratatui popup and a scriptable CLI.

The repository, Herdr plugin id, Cargo package and published packages are all
**`herdr-plugin-manager`**.

[English](#why-herdr-plugin-manager) · [Français](#français)

## Why herdr-plugin-manager

Herdr's own installer accepts GitHub `owner/repo`. herdr-plugin-manager adds the
other transports without asking a forge API for a checkout:

| Capability | Behavior |
|---|---|
| Git transports | HTTPS, HTTP, SSH, scp-style SSH, `git://`, `file://`, absolute or relative repository paths |
| Monorepos | `repo.git//plugins/tool` or `--subdir plugins/tool`; a single manifest is discovered automatically |
| Reviewed execution | Resolved commit and every build/startup/action/event/pane command are shown before confirmation |
| Version history | Separate installations, rollback, pin/unpin and branch/tag/commit switching |
| Update context | New commits and changes to the executable surface, not only a version bump |
| Discovery | herdr.dev, gitlab.com and codeberg.org built in; GitHub/GHE topic search, Gitea/Forgejo, GitLab, static JSON indexes and indexes stored in Git |
| Reproducibility | GitOps snapshots at exact commits, with explicitly approved plaintext configuration |
| Operations | Enable/disable (including offline), uninstall, logs, doctor, dry-run |

Remote-helper transports such as `ext::` are deliberately rejected. This also
protects GitOps: a synchronized source URL cannot select a Git helper or a
command-line option before the review.

## Install

Requirements: Herdr 0.7.4+, Git, and a Rust toolchain with a C linker when no
matching prebuilt `bin/herdr-pm` is present.

```sh
herdr plugin install julienpoirou/herdr-plugin-manager
```

For another Git server:

```sh
cargo install --path . --locked
herdr-pm install ssh://git@git.corp.example/tools/herdr-plugin-manager.git
```

For a development checkout:

```sh
sh scripts/build.sh
herdr plugin link "$PWD"
```

Bind the popup (recommended: `prefix+m`; Herdr uses `prefix+g` for `goto`):

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "julienpoirou.herdr-plugin-manager.open"
description = "Open the plugin manager"
```

Then run `herdr server reload-config`. Put `bin/herdr-pm` on `PATH` (or use
`cargo install --path . --locked`) to use the CLI directly.

On native Windows, the manifest uses `scripts/build.ps1`; Git for Windows,
PowerShell and Rust/MSVC must be available.

The plugin id is `julienpoirou.herdr-plugin-manager`; the GitHub repository,
crate and executable names have not changed.

### Upgrading the old plugin identity

Older updaters correctly refuse a manifest whose id changed. Close old manager
popups/commands, obtain a fresh source checkout at the new release, build it,
and run its explicit migration before updating normally:

```sh
git clone https://github.com/julienpoirou/herdr-plugin-manager.git
cd herdr-plugin-manager
cargo build --release --locked
./target/release/herdr-pm migrate . --binary ./target/release/herdr-pm --dry-run
./target/release/herdr-pm migrate . --binary ./target/release/herdr-pm --yes
```

On Windows use `.\target\release\herdr-pm.exe` for both executable paths.
A binary-only release archive is not a migration root: the command needs the
manifest, skill, source and build scripts from the checkout.

Migration preserves preferences, enabled state, followed ref, auto-update,
logs and installation history. Existing physical checkouts—including other
plugins inside the old manager directory—stay in place. It updates action
keybindings and reloads a running Herdr without restarting it. A held legacy
manager remains held at the migration snapshot's source commit; the old exact
commit remains historical. Rollback refuses old manifests declaring the old id.
Use a committed release checkout for a reproducible snapshot.

If the old manager is still registered as Herdr GitHub-managed, first relink
its existing `plugin_root` with `herdr plugin link <root>` so its historical
checkout is not eligible for Herdr's managed cleanup. Conflicting destinations
are refused rather than merged or overwritten.


## Releases and packages

Each [release](https://github.com/julienpoirou/herdr-plugin-manager/releases)
publishes the same `herdr-pm` binary in three forms:

| Package | Install |
|---|---|
| Archives with `.sha256` checksums: Linux x86_64/arm64 (static musl), macOS x86_64/arm64, Windows x86_64 | Download `herdr-plugin-manager-<version>-<target>.tar.gz` or `.zip` and put `herdr-pm` on `PATH` |
| [crates.io](https://crates.io/crates/herdr-plugin-manager) | `cargo install herdr-plugin-manager --locked` |
| [GHCR](https://github.com/julienpoirou/herdr-plugin-manager/pkgs/container/herdr-plugin-manager) image (linux/amd64, linux/arm64) with Git | `docker run --rm -v "$PWD:/work" ghcr.io/julienpoirou/herdr-plugin-manager index check herdr-index.json` |

The container image is meant for catalog jobs (`index build`, `index check`,
`search`, `sources`). Plugin management needs a local Herdr.

Releases are prepared by
[release-please](https://github.com/googleapis/release-please) from
Conventional Commits (`feat:`, `fix:`, `perf:`, `deps:`). Every push to `main`
updates a release pull request that bumps `Cargo.toml`, `Cargo.lock` and
`herdr-plugin.toml` and extends `CHANGELOG.md`. Merging it tags `vX.Y.Z` and runs
`.github/workflows/release.yml`, which builds the archives, creates the GitHub
release, publishes the crate (when the `CARGO_REGISTRY_TOKEN` secret is set)
and pushes the image. To publish an existing tag again, run the workflow manually
with that tag.

## Install sources

```sh
herdr-pm install owner/repo[/subdir]                       # GitHub, through the Git pipeline
herdr-pm take-over <id>                                    # same for a plugin Herdr installed
herdr-pm install https://gitlab.corp.example/tools/notify.git
herdr-pm install git@git.corp.example:tools/notify.git
herdr-pm install ssh://git@host:2222/srv/git/mono.git//plugins/foo
herdr-pm install git://git.lan/notify.git
herdr-pm install /srv/git/notify.git
herdr-pm install ./checkout
herdr-pm install corp:tools/notify.git                     # configured alias
herdr-pm install <source> --ref v1.4.0                     # branch, tag or commit
herdr-pm install <source> --subdir plugins/foo
herdr-pm install <source> --disabled
```

Credentials stay with Git: SSH agent/keys, credential helpers, `.netrc` and
`GIT_SSH_COMMAND`. Read-only background checks do not prompt.

`--replace` belongs to the Git pipeline:

```sh
herdr-pm install <git-source> --replace
```

It replaces an existing local link or a GitHub-owned registration after the
new build succeeds.

GitHub shorthands go through the same Git pipeline as every other source
(`github_via_git = true`, the default): Herdr sees a linked plugin, so a
`herdr plugin update` run outside the manager leaves it alone, and rollback and
history apply. With `github_via_git = false` they go to Herdr's own installer,
which cannot replace a local link, so `--replace` then needs `--git`.
`herdr-pm take-over <id>` (`t` in the popup) moves a plugin Herdr installed
earlier to the Git pipeline, at the commit it runs now and following the same
ref. That includes the manager itself (`herdr-pm take-over
herdr-plugin-manager`): it then updates itself with `u`, `U` or auto-update,
and `herdr plugin update` no longer touches it.

## Trust and build model

For Git-managed installations, herdr-plugin-manager:

1. mirrors the remote and resolves the branch, annotated/lightweight tag or
   commit to a full object id;
2. reads and validates `herdr-plugin.toml` at that object id;
3. prints the source, ref, commit metadata and every declared command;
4. refuses non-interactive installation without `--yes`;
5. checks out that exact commit, initializes submodules, and verifies the
   checked-out manifest against the reviewed text;
6. runs applicable `[[build]]` entries as argv (no shell), then compares the
   manifest again;
7. links the new directory into Herdr and records it in `registry.json`.

Control characters in manifest fields, refs, URLs and marketplace labels are
neutralized in the preview and popup. Parent components in `--subdir` are
rejected. A submodule or build failure prevents linking. Previous installations
stay available for rollback.

Plugins remain arbitrary code running as the user. Review the commands and the
repository, and pin the reviewed commit when appropriate. GitHub shorthand
installs use Herdr's own preview/build implementation instead.

A hard crash between Herdr's registration and the manager's `registry.json`
update can require manual recovery. Avoid running multiple mutating
`herdr-pm` commands for the same plugin concurrently; `herdr-pm doctor` reports
registry/list inconsistencies.

## Popup

Open with `herdr-pm`, the configured key, or the `julienpoirou.herdr-plugin-manager.open`
action.

| Area | Keys |
|---|---|
| Navigation | arrows / `j` / `k`, `/` filter |
| Mouse | every key shown in a footer or dialog is clickable, its label included; `←×` at the top right go back and close; click a row to select it, again to act; wheel scrolls; click a tab to switch views |
| Updates | `u` selected, `U` all, each with its own review; `A` auto-update on/off (marked `⟳`) |
| State | `e` enable/disable, `x` remove (`u`) or delete completely (`d`), `X` pick removed plugins whose leftovers to delete |
| Versions | `p` version picker (pinned plugins show `📌`), `s` switch ref, `z` rollback, `t` take a GitHub plugin over from Herdr |
| Discovery | `m` or Tab, `/` search, `s` sort, `f` source, arrows for pages |
| Other | `o` open homepage, `C` settings, `E` config.toml in your editor, `r` refresh, `?` help (scrolls), `q` quit |

Keyboard and mouse work together: a click sends exactly the key it shows, so
both run the same code. The `?` help lists every key; its lines are
clickable too, which reaches the keys the footer has no room for. Inside
Herdr with `ui.mouse_capture = false` (keyboard-only), the popup does not
capture the mouse and shows no `×`, `←` or buttons.

Install, update (one or all), removal, rollback, pinning another version and
switching run inside the popup. The trust preview appears as a dialog (`y`
yes, `n` no, `a` yes to the rest of an update-all, arrows to scroll); the work
then runs in the background behind a progress dialog: the current step, a bar
(`plugin k/n` for update-all), and the command output on `l`. It closes by
itself on success; on failure it stays open with the output. GitHub
shorthands get the same preview from a mirror of the repository before Herdr
installs them. Only the external editor (`E`) takes over the terminal.

`C` opens the settings: the general values, the auto-update strategy and one
per plugin, the marketplace sources (on/off) and the aliases. `↵` types a value
or switches it, `←`/`→` steps through numbers and strategies, `d` deletes an
alias or puts a value back to its default. Each change is checked and written
to `config.toml` at once, comments kept; a refused value shows why and leaves
the file alone. Adding a marketplace source or a token is done in the file
(`E`).

Two settings change the system and are off by default; turning either on or
off asks first, then runs behind the progress dialog:

- `cli_on_path` puts `herdr-pm` on your PATH: a link in
  `%LOCALAPPDATA%\herdr-plugin-manager\bin` (Windows) or `~/.local/bin`
  pointing at the manager's current installation (a symbolic link when the
  system allows it, otherwise a hard link or a copy), and that directory on
  PATH. The link moves after every update of the manager, at Herdr startup
  and when the popup opens. `herdr-pm path enable|disable` does the same from
  a shell.
- `agent_skill` links the `herdr-pm` [agent skill](skills/herdr-pm/SKILL.md)
  into `~/.agents/skills` (Codex, Pi, Cline, Zed…) and into the skills
  directory of each coding agent found (Claude Code, OMP, Cursor, Gemini CLI,
  GitHub Copilot, OpenCode, Droid, Windsurf, Kiro…), and turns `cli_on_path`
  on, since agents run `herdr-pm`. The links follow updates the same way.
  `herdr-pm skill enable|disable` from a shell; `herdr-pm --skill` prints the
  skill of the installed version; `npx skills add julienpoirou/herdr-plugin-manager
  --skill herdr-pm -g` installs it without the manager.

Both take effect at once, without restarting Herdr. New Herdr panes inherit
the environment of a server started earlier, so the PATH also goes through a
marked block in your shell profiles: the PowerShell profiles on Windows (in
addition to the user PATH; skipped, with a note, for a shell whose execution
policy runs no profile), and `~/.profile` plus `~/.bashrc`, `~/.bash_profile`
and `~/.zshrc` when they exist elsewhere. The block only adds the directory
when it is missing, and disabling removes it, keeping the rest of the file.
New agent sessions read the skill links immediately. A process cannot change
the environment of another one: shells already open keep their PATH, and
agents already running keep the skills they loaded. Herdr's live handoff
does not help here: it only replaces the Herdr server during
`herdr update --handoff`, and Windows servers do not support it.

The skill tells agents to show the `--dry-run --yes` preview and wait for
your approval before any `--yes` change.

`icons` (on by default) shows the project or owner picture next to the
details in Installed and Marketplace, only in terminals that draw images:
Kitty, Ghostty, WezTerm, iTerm2, foot, Konsole…, and inside Herdr when its
outer terminal speaks the Kitty protocol (Herdr's `terminal.kitty_graphics`).
Pictures come from GitHub, GitLab and Gitea/Forgejo and are cached for a week.
The former `logos` preference is migrated to `icons`, preserving its value.

The detail panel shows the installed version as `branch@commit`
(`main@8cda521`, `v1.4.0@8cda521`), or `@8cda521 (pinned)`.

`p` lists `latest (unpin)` first, then the repository's tags (one
`git ls-remote`), newest version first:

- `latest (unpin)` follows the default branch again;
- the installed version pins it;
- another version is installed first, with its preview, then pinned.

A git-managed plugin is pinned without reinstalling. A plugin Herdr installed
is reinstalled at the commit (with its preview), even the one it runs now:
Herdr only keeps a commit it installed itself, and `herdr plugin update` would
move anything else. Pins recorded in `registry.json` by older versions are
still read; when Herdr moved such a plugin the popup shows `⚠ moved by herdr`
and `u` (or `herdr-pm update`) reinstalls it at the pin.

`x` asks what to do: `u` removes the plugin (a local link is only unlinked)
and keeps its config and data for a reinstall; `d` deletes it completely:
installations, Herdr's GitHub checkouts and their lock files, the plugin's
config and data directories, the cached repository (unless another plugin uses
it), its auto-update and pin settings, and a local link's directory, after you
type the plugin id. Nothing is left for `X` afterwards.

`X` lists the plugins removed earlier that left something behind, whichever
way they were removed, with the size and kind of what remains: config and
state directories, Herdr's old GitHub checkouts and their lock files, and this
manager's auto-update/pin settings. `Space` checks plugins, `a` checks them
all, `↵` reviews the paths of the checked ones (or of the highlighted one) and
deletes them after `y`. `herdr-pm clean [ids…]` does the same from the shell,
for every removed plugin when no id is given. Installed plugins and the
manager's own data are never touched.

Auto-update is a per-plugin switch, independent of enable/disable. It updates
every GitHub- or git-installed plugin with the switch on, **without the trust
preview**, disabled plugins included. Pinned plugins stay where they are; local
links cannot use it. Turn it on only for repositories you trust to run new
commands unattended.

When it runs depends on `[auto_update] trigger`:

| Trigger | Runs |
|---|---|
| `startup` (default) | when the Herdr server starts (also when a new server takes over during a Herdr update; not when a client attaches or config reloads) |
| `popup` | when the popup opens, behind the progress dialog |
| `min-interval` | at most once per `min_interval` (`30m`, `6h`, `1d`, `1h30m`; default `6h`), checked when the server starts, when the popup opens and when a tab or pane is created |
| `manual` | only `herdr-pm auto-update` |

Herdr runs the manager's `[[startup]]` hook and its `tab.created` /
`pane.created` event hooks; each calls `herdr-pm auto-update --hook …`, which
exits at once unless the trigger says an update is due. Herdr has no timers
for plugins and asks startup hooks not to stay running, hence the checks on
activity rather than a clock. `herdr-pm auto-update` without `--hook` always
runs.

Where it moves a plugin depends on the strategy:

| Strategy | Moves to |
|---|---|
| `latest` (default) | the newest commit of the branch or tag the plugin follows |
| `release` | the newest version tag (`v2.0.0`), skipping untagged commits and pre-releases |
| `minor` | the newest version tag with the same major version (`1.3.0` → `1.9.2`, not `2.0.0`) |
| `patch` | the newest version tag with the same minor version (`1.3.0` → `1.3.4`) |

```toml
[auto_update]
strategy = "release"

[auto_update.plugins]
"examples.agent-telegram-notify" = "patch"
```

With a tag strategy a git-installed plugin keeps following its branch (`u`
still offers the newest commit); a GitHub-installed plugin is reinstalled at
the tag, which Herdr then follows until `p` → `latest (unpin)`.

## CLI

```text
herdr-pm                       open the interactive manager
herdr-pm list [--check] [--json]
herdr-pm install <source> [--ref R] [--subdir D] [--disabled] [--git] [--replace] [-y]
herdr-pm update [ids…] [-y]    herdr-pm outdated [--json]
herdr-pm uninstall <id> [--purge] herdr-pm enable|disable <id>
herdr-pm clean [ids…] [-y]     delete leftovers of removed plugins
herdr-pm auto-update [enable|disable <id>] [--hook startup|event]
herdr-pm take-over <id> [-y]   move a Herdr GitHub install to the Git pipeline
herdr-pm info <id> [--json]
herdr-pm logs [id] [--limit N] [--format text|jsonl|ecs] [-f|--follow]
herdr-pm rollback <id>         herdr-pm pin <id> [ref]
herdr-pm unpin <id> [ref]      herdr-pm switch <id> <ref>
herdr-pm search [words…] [--source S] [--sort stars|updated|name] [--refresh] [--json]
herdr-pm sources
herdr-pm index build (--repo URL… | --repos-file F | --scan-dir DIR [--url-base URL]) [-o F]
herdr-pm index check <file>
herdr-pm push [-y]             herdr-pm pull [-y]
herdr-pm migrate <checkout> [--binary path] [-y]
herdr-pm doctor                herdr-pm config path|init|show
herdr-pm path enable|disable   herdr-pm skill enable|disable
herdr-pm --skill               print the agent skill
```

Global `--dry-run` (or `HERDR_PM_DRY_RUN=1`) reports changes without applying
them.

### Logs

`herdr-pm logs` shows the last 30 manager changes, oldest first. These are
install/update/pin/removal records, not the command-output logs from
`herdr plugin log`. There is no `history` command alias.

```sh
herdr-pm logs
herdr-pm logs demo.plugin --limit 100
herdr-pm logs -f                       # or --follow; Ctrl+C stops it
herdr-pm logs --format jsonl           # one native JSON object per line
herdr-pm logs --format ecs --follow    # one ECS JSON document per line
herdr-pm logs -f --limit 0             # only new changes
```

In `C` → Settings → General → `history`, use Enter or left/right to select
`jsonl` (default), `ecs`, or `off`. This controls **new records on disk**:
`history.jsonl` for native fields, `history.ecs.ndjson` for Elastic Common
Schema 8.11.0, or no writes. Turning it off does not delete old records.
Changing formats keeps both files readable; `logs` merges their history and
follows appends to either file, including a format change while it runs.
The `--format` option controls stdout independently of this setting.

`log_dir` in Settings chooses an absolute writable directory; `d` restores
the default. Existing log files are not moved when changing directories.
New installations default to `%LOCALAPPDATA%\herdr\plugins\julienpoirou.herdr-plugin-manager`
on Windows and `$XDG_STATE_HOME/herdr/plugins/julienpoirou.herdr-plugin-manager`
(or `~/.local/state/herdr/plugins/julienpoirou.herdr-plugin-manager`) on Linux.
`/var/log/herdr-plugin-manager` is an optional administrator-provisioned
directory, not the unprivileged default.

ECS records contain `@timestamp` in UTC, `event.action`, `event.outcome`,
`event.category: ["package"]`, `package.name`, and `package.reference` when
a destination commit is known. Original source, before/after commits and
details live under `herdr_plugin_manager`; failed records also have
`error.message`. Host/user names come from the emitting process's environment
when available; converting old native records cannot recover their original
host or user.

Point an Elastic Agent/Filebeat NDJSON input or a Wazuh agent JSON
`localfile` input at the selected file. Collection configuration and Wazuh
alert rules remain external; these integrations are not installed by the
manager.


## Configuration

`herdr-pm config init` creates `config.toml` in the plugin's Herdr config
directory; `C` edits it in the popup and `E` opens it in your editor. Every
key is optional:

```toml
keep_installs = 3       # versions kept for rollback
market_ttl = 600        # seconds before the marketplace is refreshed
check_concurrency = 6   # parallel update checks in the popup
use_gh_cli = true       # use `gh auth token` for GitHub
github_via_git = true   # owner/repo through the Git pipeline, not Herdr's installer
icons = true            # pictures where the terminal draws images
history = "jsonl"       # jsonl (default), ecs or off
# log_dir = "/absolute/writable/log/directory" # omitted: manager state directory
cli_on_path = false     # herdr-pm on your PATH (asks in C)
agent_skill = false     # the herdr-pm skill for coding agents (asks in C)
editor = "code --wait"  # for E (VISUAL, EDITOR, then vi/notepad otherwise)

[backup]
enabled = false        # opt-in continuous LOCAL snapshots; no automatic push
branch = "main"
plugins = true         # publish/apply the plugin list (plugins.toml)
files = []             # exact plaintext approvals, selected in Settings
# remote = "ssh://git@git.example/my-herdr.git"

[auto_update]
strategy = "latest"     # latest, release, minor or patch
trigger = "startup"     # startup, popup, min-interval or manual
min_interval = "6h"     # for min-interval

[auto_update.plugins]
"some.plugin" = "patch"

[aliases]
corp = "ssh://git@git.corp.example/"
lab = "https://gitlab.corp.example/{path}.git"
```

herdr.dev, gitlab.com and codeberg.org are built in. A `[[marketplace]]`
block adds a source; one named like a built-in overrides the fields it sets,
so `name = "codeberg.org"` with `enabled = false` hides it.

```toml
[[marketplace]]
name = "ghe"
kind = "github"
url = "https://ghe.corp.example/api/v3"
token_env = "GH_ENTERPRISE_TOKEN"

[[marketplace]]
name = "corp"
kind = "gitea"                 # also Forgejo
url = "https://git.corp.example"
token_env = "CORP_GITEA_TOKEN"
clone = "ssh"                  # https (default) or ssh

[[marketplace]]
name = "lab"
kind = "gitlab"
url = "https://gitlab.corp.example"
token_cmd = ["pass", "show", "gitlab/token"]

[[marketplace]]
name = "internal"
kind = "git-index"
url = "ssh://git@git.corp.example/herdr/index.git"
path = "herdr-index.json"
ref = "main"                  # optional

[[marketplace]]
name = "team"
kind = "index"
url = "https://intranet.corp.example/herdr-index.json"
```

The `herdr.dev` source downloads the catalog behind herdr.dev/plugins (every
public GitHub repository tagged `herdr-plugin`, one entry per manifest,
monorepo plugins included); `gitlab.com` and `codeberg.org` list the
projects tagged `herdr-plugin` there. Every source except `github` lists its
whole catalog once; searching filters the cached copy locally. The popup shows
the cached marketplace immediately at any age, refreshes it in the background
once it is older than `market_ttl`, and reports what changed (`r` forces a
refresh). Static indexes are revalidated with `If-None-Match` /
`If-Modified-Since`, so an unchanged catalog costs a 304. GitHub topic search
is capped at 1000 repositories by the GitHub API.

GitHub authentication uses an explicit source token, `GH_TOKEN`,
`GITHUB_TOKEN`, then `gh auth token`. GHE additionally prefers
`GH_ENTERPRISE_TOKEN` and passes its hostname to `gh auth token --hostname`.
`HERDR_PM_NO_TOKEN=1` disables all GitHub API authentication. The documented
GitHub API version is `2022-11-28`, which remains valid and is also understood
by GHES versions supported by this configuration.

## GitOps: the same setup on every machine

In Settings (`C`), configure **Backup**: the Git remote, the branch, and the
exact configuration files you approve. Enablement asks for confirmation: files
are **plaintext** and an independent local worker will observe changes every
two seconds. The approval list starts empty. The local snapshot repository is
kept by the manager in its data directory (`backup/`); setting a remote does
not grant access to additional files.

```sh
herdr-pm push                  # review the diff, commit, then push
herdr-pm pull                  # fetch, review, install/apply; stop on conflict
herdr-pm pull --dry-run --yes  # preview without changing live state/repository
```

On each additional machine, configure its own local Backup settings and
approve the files it may apply, then pull. Git handles authentication through
your normal SSH agent or credential helper; no Git credentials are embedded
in the snapshot format.

```text
backup/
├── plugins.toml
├── herdr/config.toml                # only when explicitly approved
├── manager/config.toml              # only when explicitly approved
└── plugins/<encoded-plugin-id>/...  # exact approved custom files
```

`plugins.toml` is a deterministic TOML document, not an event stream:
`schema`, approved `files`, `[[plugins]]` records and `[[local_only]]`
notices. Each reproducible plugin records its source URL, subdirectory,
exact commit, displayed version, followed ref, optional pin, enabled state
and auto-update flag. Local hand-linked code is reported as local-only;
it is never silently presented as a portable code backup.

The plugin list can be turned off per machine (`plugins.toml` at the top of
the Backup approvals in Settings, or `plugins = false`). That machine then
neither publishes nor applies plugins: its pushes keep the plugin entries
already in the snapshot unchanged, and its pulls install, update and
uninstall nothing, while approved files still sync. Turning it back on makes
the next pull reconcile plugins like a first sync (a differing installed
plugin is a conflict, missing ones are installed after review, extra local
ones are kept); push refuses until that pull has run.

The worker updates only the local snapshot. **It never commits or contacts a
remote.** Push shows a diff, stages only snapshot-owned paths, commits using
your Git identity when needed, and performs a normal fast-forward push.
No automatic merge, reset, or force-push.

Any machine can push and pull. Pull compares each plugin and each selected
file with the last sync: changes made only on the remote are applied
(including removals: an uninstalled plugin is uninstalled here too, keeping
its configuration files, after the review), changes made only on this
machine are kept and published by the next push, and only an item changed
differently on both sides stops the pull, before anything is applied. When
push says the remote has changes this machine has not pulled, run
`herdr-pm pull` then `herdr-pm push` again. Pull still refuses divergent Git
history and manual or staged edits in the local snapshot repository. On the
very first pull, a selected local file or an installed plugin that differs
from the remote is a conflict: move the file aside or align the plugin, then
pull again. Plugins the remote never knew about are kept.

Different machines may approve different files. Unselected remote
configuration remains in the snapshot but is not applied to live files.
Only an explicitly selected file that is absent locally authorizes its
deletion on the next push. A remote snapshot cannot expand the local approval
list. The manager's `[backup]` section and absolute `log_dir` stay local even
when `manager/config.toml` is synchronized.

Only explicitly selected UTF-8 files up to 4 MiB are supported. Machine-local
authority roots are resolved first (including OS directory aliases); symlink
traversal within selected config/snapshot trees, unsafe paths, runtime state,
caches, logs and binaries are refused.
No encryption or automatic secret removal: approving a file may publish its
tokens/passwords and their Git history. Use deliberate selections and a
trusted private repository. Plugin source repositories must remain available;
the snapshot records commits, not copies of plugin code or build tools.
Arbitrary machine-specific paths inside custom configuration cannot be made
portable automatically.

Pull registers new plugin versions for **new invocations**, reloads Herdr
configuration when changed, and refreshes enabled CLI/skill links immediately.
It does not replace existing plugin processes, rewrite open shells' PATH, or
reload an agent's already-loaded skill context. A Herdr server handoff is not
performed: that experimental mechanism is for replacing Herdr itself, not
synchronizing configuration.

The worker starts when Backup is enabled or the manager next runs, replaces
itself after manager updates, and stops cooperatively when disabled. When it
fails (Git missing, unreadable file…), Settings shows a `problem` line in the
Backup section until it recovers by itself; the history is kept in
`backup-watch.log` in the manager's data directory. Backup does not include
terminal sessions, running processes or terminal history.

## A marketplace without a forge API

Build an index from bare repositories and publish it in Git:

```sh
REPO_ROOT=/srv/git URL_BASE=ssh://git@git.corp.example/srv/git \
INDEX_REPO=/srv/git/herdr/index.git sh examples/publish-index.sh
```

The generated JSON records root and monorepo plugins, versions, platforms,
minimum Herdr versions, refs and commits. `herdr-pm index check FILE` validates
the format. See `examples/publish-index.sh` for incremental publication.

## State and offline behavior

herdr-plugin-manager stores its data under Herdr's state directory:

```text
~/.local/state/herdr/plugins/julienpoirou.herdr-plugin-manager/
├── registry.json       # remotes, followed refs, current and previous installs
├── installs/           # independent checkouts
├── cache/              # bare mirrors and marketplace responses
├── trash/              # deferred cleanup
├── history.jsonl       # native records (default)
└── history.ecs.ndjson  # records written while history = "ecs"
```

On Windows the default root is `%LOCALAPPDATA%\herdr`. `HERDR_PM_HOME` and
`HERDR_PM_CONFIG_DIR` override the manager's directories.

Enable/disable first uses Herdr's CLI. If the server is stopped, the manager
updates Herdr's `plugins.json` under `.plugins.lock`, preserves unknown JSON
fields and follows registry symlinks, including a dangling final target.

## Environment

| Variable | Meaning |
|---|---|
| `HERDR_PM_DRY_RUN=1` | Report mutations without applying them |
| `HERDR_PM_HERDR_BIN` | Herdr executable (otherwise `HERDR_BIN_PATH`, then `herdr`) |
| `HERDR_PM_HOME` / `HERDR_PM_CONFIG_DIR` | Override state/config directories |
| `HERDR_PM_EDITOR` | Editor before `VISUAL` and `EDITOR` |
| `HERDR_PM_NO_TOKEN=1` | Never authenticate GitHub API requests |
| `HERDR_PM_BUILD_FROM_SOURCE=1` | Ignore a matching prebuilt binary |
| `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` | Forge HTTP proxy behavior, including domain suffixes |

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --locked && sh tests/e2e.sh   # requires herdr on PATH
```

The E2E suite uses real Herdr and temporary repositories. It covers local,
`file://` and `git://` installs, monorepos, builds, updates/diffs, rollback,
pin/switch, `--replace`, offline and live-server operations, identity migration,
continuous approved-file backup, marketplace indexes, hostile Git transports,
terminal escapes and invalid paths.

## Français

**Herdr Plugin Manager** installe des plugins depuis GitHub, GitLab,
Gitea/Forgejo, un dépôt nu, `git daemon` ou un chemin local. Le binaire
`herdr-pm` fournit une interface Ratatui et une CLI scriptable.

Ses fonctions principales sont les monorepos (`depot.git//plugins/outil`), les
alias de serveurs, l'aperçu des commandes avant confirmation, les diffs de
mise à jour, les installations indépendantes avec rollback, pin/unpin/switch,
marketplaces (herdr.dev, gitlab.com et codeberg.org par défaut), les index JSON
hébergés dans Git, la synchronisation GitOps (`push`/`pull`) aux commits exacts,
`doctor`, les logs et le mode hors ligne.

```sh
herdr plugin install julienpoirou/herdr-plugin-manager
herdr-pm install git@git.corp.example:outils/notify.git
herdr-pm install /srv/git/mono.git//plugins/foo --ref v1.4.0
```

Raccourci conseillé : `prefix+m` avec la commande
`julienpoirou.herdr-plugin-manager.open` (exemple TOML ci-dessus). Dans le popup, la souris
fonctionne aussi : molette, clic sur une ligne ou sur un onglet. `herdr-pm config
init` crée la configuration, `C` la modifie dans le popup (réglages, stratégies,
sources, alias), `E` l'ouvre dans votre éditeur et `?` affiche l'aide du popup.

**Mise à jour automatique** : `A` dans « Installed » l'active ou la désactive
pour un plugin, qu'il soit activé ou non (`⟳` dans la liste). Au démarrage de
Herdr (ou à l'ouverture du popup, ou au plus une fois par intervalle, selon
`trigger`), `herdr-pm auto-update` met ces plugins à jour **sans aperçu** ; les
plugins épinglés ne bougent pas et les liens locaux ne sont pas concernés.
La stratégie choisit la cible : `latest` (défaut, dernier commit), `release`
(dernier tag de version), `minor` (même version majeure) ou `patch` (même
version mineure), globalement dans `[auto_update]` ou par plugin dans
`[auto_update.plugins]`.

Les raccourcis `owner/repo` passent par le pipeline Git (`github_via_git`) :
`herdr plugin update` ne les touche pas. `t` (ou `herdr-pm take-over <id>`)
convertit un plugin installé auparavant par Herdr.

**Sécurité** : les transports `ext::`/remote-helpers sont refusés, les chemins
`..` sont rejetés, les caractères de contrôle sont neutralisés, et aucun build
n'est lancé avant confirmation. Le manifeste est comparé avant et après le
build. Les plugins restent du code exécuté avec vos droits : lisez l'aperçu et
épinglez le commit vérifié.

`--replace` fonctionne dans le pipeline Git. Pour remplacer un lien local par
une source GitHub, utilisez `owner/repo --git --replace` : le programme Herdr
natif ne propose pas cette opération de remplacement.

**Releases et paquets** : chaque release GitHub publie des archives
`herdr-pm` (Linux musl x86_64/arm64, macOS x86_64/arm64, Windows x86_64) avec
leurs sommes SHA-256, la crate `herdr-plugin-manager`
(`cargo install herdr-plugin-manager --locked`) et l'image
`ghcr.io/julienpoirou/herdr-plugin-manager` pour les tâches d'index. Les
versions sont préparées par release-please à partir des Conventional Commits ;
fusionner la pull request de release déclenche la publication.

## License

MIT — see [LICENSE](LICENSE).
