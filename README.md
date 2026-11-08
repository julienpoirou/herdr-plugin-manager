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
| Reproducibility | JSON export/import at exact exported commits |
| Operations | Enable/disable (including offline), uninstall, history, doctor, dry-run |

Remote-helper transports such as `ext::` are deliberately rejected. This also
protects imports: an imported URL cannot select a Git helper or a command-line
option before the review.

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
command = "herdr-plugin-manager.open"
description = "Open the plugin manager"
```

Then run `herdr server reload-config`. Put `bin/herdr-pm` on `PATH` (or use
`cargo install --path . --locked`) to use the CLI directly.

On native Windows, the manifest uses `scripts/build.ps1`; Git for Windows,
PowerShell and Rust/MSVC must be available.

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
ref.

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

Open with `herdr-pm`, the configured key, or the `herdr-plugin-manager.open`
action.

| Area | Keys |
|---|---|
| Navigation | arrows / `j` / `k`, `/` filter |
| Mouse | wheel moves the selection, click selects a row, click a tab to switch views |
| Updates | `u` selected, `U` all, each with its own review; `A` auto-update on/off (marked `⟳`) |
| State | `e` enable/disable, `x` remove (`u`) or delete completely (`d`), `X` pick removed plugins whose leftovers to delete |
| Versions | `p` version picker (pinned plugins show `📌`), `s` switch ref, `z` rollback, `t` take a GitHub plugin over from Herdr |
| Discovery | `m` or Tab, `/` search, `s` sort, `f` source, arrows for pages |
| Other | `o` open homepage, `C` settings, `E` config.toml in your editor, `r` refresh, `?` help (scrolls), `q` quit |

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
herdr-pm info <id> [--json]    herdr-pm history [id]
herdr-pm rollback <id>         herdr-pm pin <id> [ref]
herdr-pm unpin <id> [ref]      herdr-pm switch <id> <ref>
herdr-pm search [words…] [--source S] [--sort stars|updated|name] [--refresh] [--json]
herdr-pm sources
herdr-pm index build (--repo URL… | --repos-file F | --scan-dir DIR [--url-base URL]) [-o F]
herdr-pm index check <file>
herdr-pm export [-o file]      herdr-pm import <file> [ids…]
herdr-pm doctor                herdr-pm config path|init|show
```

Global `--dry-run` (or `HERDR_PM_DRY_RUN=1`) reports changes without applying
them.

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
editor = "code --wait"  # for E (VISUAL, EDITOR, then vi/notepad otherwise)

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
~/.local/state/herdr/plugins/herdr-plugin-manager/
├── registry.json       # remotes, followed refs, current and previous installs
├── installs/           # independent checkouts
├── cache/              # bare mirrors and marketplace responses
├── trash/              # deferred cleanup
└── history.jsonl
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
pin/switch, `--replace`, offline and live-server operations, import/export,
marketplace indexes, malicious imports, terminal escapes and invalid paths.

## Français

**Herdr Plugin Manager** installe des plugins depuis GitHub, GitLab,
Gitea/Forgejo, un dépôt nu, `git daemon` ou un chemin local. Le binaire
`herdr-pm` fournit une interface Ratatui et une CLI scriptable.

Ses fonctions principales sont les monorepos (`depot.git//plugins/outil`), les
alias de serveurs, l'aperçu des commandes avant confirmation, les diffs de
mise à jour, les installations indépendantes avec rollback, pin/unpin/switch,
marketplaces (herdr.dev, gitlab.com et codeberg.org par défaut), les index JSON hébergés dans Git, l'export/import aux
commits exacts, `doctor`, l'historique et le mode hors ligne.

```sh
herdr plugin install julienpoirou/herdr-plugin-manager
herdr-pm install git@git.corp.example:outils/notify.git
herdr-pm install /srv/git/mono.git//plugins/foo --ref v1.4.0
```

Raccourci conseillé : `prefix+m` avec la commande
`herdr-plugin-manager.open` (exemple TOML ci-dessus). Dans le popup, la souris
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
