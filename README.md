# herdr-plugin-manager

Manage [Herdr](https://herdr.dev) plugins from a popup or the command line —
from GitHub **or any git server**: Gitea, Forgejo, GitLab, a bare repository
over SSH, `git daemon`, dumb HTTP, a shared folder.

`herdr plugin install` only takes GitHub `owner/repo`. This plugin adds the
rest: it clones from any remote, shows you everything the plugin will run,
builds it, links it into Herdr, and keeps earlier versions for rollback. It
also browses several marketplaces at once, including ones you host yourself
with nothing but git.

Written in Rust, one ~5 MB binary, macOS / Linux / Windows.

[Français ↓](#français)

```
 herdr plugins  Installed 5 · ↑1   Marketplace                          herdr 0.9.5
╭───────────────────────────────────────────────────────────────────────────────────╮
│ 1 ● Hello            1.1.0    git     ssh://git@git.corp.local/tools/hello  ↑ 1.2.0  ›│
│ 2 ● Notify           0.4.2    git     ssh://git@git.corp.local/tools/notify     ✓  ⌄│
│         ↳ send               Send a test notification                  prefix+n │
│ 3 ● Reviewr          1.2.0    github  persiyanov/herdr-reviewr                 ✓  ›│
│ 4 ○ Scratch          0.3.0    local   /home/me/src/herdr-scratch           local  ›│
╰───────────────────────────────────────────────────────────────────────────────────╯
 tools.hello  enabled · git
 source   ssh://git@git.corp.local/tools/hello.git
 ref      default branch @ 7790da5  · installed 2d ago · 2 kept for rollback
 update   1.1.0 → 1.2.0 (419ae66) — press u
 ↵ actions  u update  U all  e toggle  x remove  i install  m market  ? help  q quit
```

## What it does

Everything [speardragon/herdr-plugin-manager](https://github.com/speardragon/herdr-plugin-manager) does — popup with installed plugins, update
indicators, update one / all, enable/disable, uninstall, action accordion
with key bindings, run an action, open the repo, edit `plugins.json`,
marketplace with search, sort and paging, dry-run mode — plus:

| | |
|---|---|
| **Any git remote** | `https://`, `ssh://`, `git@host:path`, `git://`, `file://`, local paths, dumb HTTP. No forge API needed. |
| **Monorepos** | `repo.git//plugins/foo` or `--subdir`; a repository with a single plugin is found automatically. |
| **Aliases** | `corp = "ssh://git@git.corp.local/"` → `herdr-pm install corp:tools/notify.git`. |
| **Its own trust preview** | Source, ref → commit, commit author/date, every command (build, startup, actions, events, panes, link handlers), platform and Herdr-version checks — before anything runs. Control characters are escaped so a manifest cannot disguise its commands. |
| **Update diffs** | Updates list the new commits and **what changed in what the plugin runs** (`+ startup #1 …`, `~ build #1 a → b`). |
| **Rollback** | Each install goes into a fresh directory; the previous ones are kept (`keep_installs`, default 3). `z` / `herdr-pm rollback` switches back instantly, no rebuild. |
| **Pin / unpin / switch** | Follow a branch, a tag, or hold an exact commit. |
| **Multi-source marketplace** | GitHub topic search, Gitea/Forgejo, GitLab, a static index over HTTP or a file, and a **git-index**: a catalog stored in any git repository. |
| **Catalog builder** | `herdr-pm index build --scan-dir /srv/git` turns a directory of bare repos into a catalog — a marketplace for a plain git server. |
| **Export / import** | Reproduce your plugin set on another machine at the exact commits. |
| **Doctor & history** | Checks herdr, git, config, manifests, dangling key bindings, marketplace reachability; logs every change. |
| **Works offline** | Enable/disable works with no Herdr server running (Herdr's CLI refuses that). |
| **Proxies done right** | `HTTPS_PROXY`, `NO_PROXY` (with domain suffixes), system CA store for internal certificates. |

GitHub `owner/repo` sources still go through `herdr plugin install`, so Herdr
keeps owning those checkouts and `herdr plugin update` keeps working. Pass
`--git` to fetch a GitHub repository with git instead (private repos over SSH).

## Install

The plugin builds from source on install, so it needs a Rust toolchain and a
C compiler on the machine (`rustup` + Xcode CLT / build-essential / MSVC).
A checkout that already contains a matching `bin/herdr-pm` skips the build.

**From GitHub** (once the repository is published there):

```sh
herdr plugin install <owner>/herdr-plugin-manager
```

**From your own git server**, bootstrap once with the CLI, then let the
manager manage itself:

```sh
git clone ssh://git@git.corp.local/tools/herdr-plugin-manager.git
cd herdr-plugin-manager && cargo install --path . --locked     # puts herdr-pm on PATH
herdr-pm install ssh://git@git.corp.local/tools/herdr-plugin-manager.git
```

**For development**: `sh scripts/build.sh && herdr plugin link "$PWD"`.

Bind a key in `~/.config/herdr/config.toml` and reload:

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "herdr-plugin-manager.open"
description = "plugin manager"
```

```sh
herdr server reload-config
```

To use the CLI from a shell as well, put the binary on your `PATH`
(`cargo install --path .`, or symlink `bin/herdr-pm` from the plugin
directory shown by `herdr plugin list`).

## Popup keys

**Installed**

| key | |
|---|---|
| `↵` `→` `←` | fold/unfold a plugin's actions · run the selected action (the popup closes first, as Herdr requires) |
| `u` / `U` | update the selected plugin / every outdated one (each still shows its preview) |
| `e` | enable ↔ disable |
| `x` | uninstall (hand-linked plugins are only unlinked) |
| `i` | install: `owner/repo`, any git URL, `alias:path`, `… --ref v2 --subdir dir` |
| `p` | pin to the current commit / unpin |
| `s` | switch to another branch, tag or commit |
| `z` | roll back to the previous installation |
| `o` | open the source in a browser |
| `L` | recent commands Herdr ran for the plugin |
| `c` / `C` | edit Herdr's `plugins.json` / this manager's `config.toml` |
| `/` · `r` | filter · refresh and re-check |

**Marketplace** (`m` or `Tab`)

| key | |
|---|---|
| `↵` | install the selection (confirmation, then the full preview) |
| `/` | search all sources |
| `s` · `f` | cycle sort (stars, updated, name) · cycle source |
| `←` `→` | previous / next page |
| `o` · `r` | open in browser · refresh (ignore cache) |

Status column: `✓` up to date · `↑ 1.2.0` update available · `pinned` · `local`
(hand-linked) · `! check failed` (details below the list).

Install, update, uninstall, rollback and pin run on the plain terminal with
the same preview and prompts as the CLI, then return to the popup.

## Sources

```sh
herdr-pm install owner/repo[/subdir]                      # GitHub, via herdr
herdr-pm install https://gitea.corp/tools/notify.git      # any https remote
herdr-pm install git@gitlab.corp:tools/notify.git         # scp-style ssh
herdr-pm install ssh://git@host:2222/srv/git/mono.git//plugins/foo
herdr-pm install git://git.lan/notify.git                 # git daemon
herdr-pm install /srv/git/notify.git                      # local bare repo
herdr-pm install corp:tools/notify.git                    # alias from config
herdr-pm install <source> --ref v1.4.0                    # branch, tag or commit
herdr-pm install <source> --replace                       # take over a plugin installed another way
```

Credentials are plain git: your SSH agent and keys, `~/.netrc`, credential
helpers, `GIT_SSH_COMMAND`. Background update checks never prompt; installs
and updates do.

## CLI

```
herdr-pm                       open the interactive manager
herdr-pm list [--check] [--json]
herdr-pm install <source> [--ref R] [--subdir D] [--disabled] [--git] [--replace] [-y]
herdr-pm update [ids…] [-y]    herdr-pm outdated [--json]
herdr-pm uninstall <id>        herdr-pm enable|disable <id>
herdr-pm info <id> [--json]    herdr-pm history [id]
herdr-pm rollback <id>         herdr-pm pin <id> [ref]   herdr-pm unpin <id> [ref]
herdr-pm switch <id> <ref>
herdr-pm search [words…] [--source S] [--sort stars|updated|name] [--refresh] [--json]
herdr-pm sources
herdr-pm index build (--repo URL… | --repos-file F | --scan-dir DIR [--url-base URL]) [-o F]
herdr-pm index check <file>
herdr-pm export [-o file]      herdr-pm import <file> [ids…]
herdr-pm doctor                herdr-pm config path|init|show
```

`--dry-run` (or `HERDR_PM_DRY_RUN=1`) prints changes instead of making them.

## Configuration

`herdr-pm config init` writes a commented file to
`$(herdr plugin config-dir herdr-plugin-manager)/config.toml`:

```toml
keep_installs = 3        # earlier installations kept for rollback
market_ttl = 600         # seconds marketplace listings are cached
check_concurrency = 6    # parallel update checks

[aliases]
corp = "ssh://git@git.corp.local/"
lab  = "https://gitlab.lab.local/{path}.git"   # {path} = template

[[marketplace]]
name = "github"
kind = "github"          # repositories tagged herdr-plugin (herdr.dev/plugins)

[[marketplace]]
name = "corp"
kind = "gitea"           # also Forgejo
url = "https://git.corp.local"
token_env = "CORP_GITEA_TOKEN"   # or token_cmd = ["pass", "show", "gitea"]
clone = "ssh"            # install over ssh instead of https

[[marketplace]]
name = "lab"
kind = "gitlab"
url = "https://gitlab.lab.local"

[[marketplace]]
name = "internal"
kind = "git-index"       # plain git server: a catalog file in a repo
url = "ssh://git@git.internal/srv/git/herdr/index.git"
path = "herdr-index.json"

[[marketplace]]
name = "team"
kind = "index"           # catalog over http(s) or from a file
url = "https://intranet.example/herdr-index.json"
```

Forge sources list repositories carrying the `topic` (default
`herdr-plugin`), skipping forks and archived ones, like herdr.dev. For GitHub
Enterprise use `kind = "github"` with `url = "https://ghe.example/api/v3"`.
GitHub requests use `GH_TOKEN`, `GITHUB_TOKEN` or `gh auth token`
(`HERDR_PM_NO_TOKEN=1` disables that).

## A marketplace on a plain git server

No web UI, no API: build a catalog from the repositories and publish it in a
git repository. [`examples/publish-index.sh`](examples/publish-index.sh) does
it from cron and only commits when something changed:

```sh
REPO_ROOT=/srv/git URL_BASE=ssh://git@git.internal/srv/git \
INDEX_REPO=/srv/git/herdr/index.git sh examples/publish-index.sh
```

The index records every plugin found (monorepos included, with their
subdirectories), its version, platforms and minimum Herdr version. The format
is a small JSON file (`"format": "herdr-plugin-index"`), easy to generate from
anything else too.

## How it works

* Remotes are mirrored into a bare cache (`…/cache/repos`), so installs,
  version lookups and update previews work the same on every transport.
* An install checks out the commit into
  `…/installs/<plugin>/<commit>-<time>/`, runs the manifest's `[[build]]`
  steps for this platform there (argv, no shell, no Herdr runtime variables,
  output shown live), verifies the manifest did not change, then
  `herdr plugin link`s it. Re-linking replaces the registration atomically;
  running processes keep their old files.
* Herdr only sees a linked directory, so the manager keeps its own record
  (`registry.json`) of remote, followed ref, commit and earlier installations.
  Data lives in Herdr's state directory for the plugin
  (`~/.local/state/herdr/plugins/herdr-plugin-manager/`).
* Update checks are one `git ls-remote` per plugin, in parallel; versions of
  available updates are read from the mirror (or raw.githubusercontent.com for
  Herdr-managed GitHub plugins).

## Trust

Plugins are code that runs as you. Listings are not reviewed — by Herdr or
by this manager. Read the preview; pin what you reviewed (`--ref <commit>` or
`p`). Nothing runs before you confirm, and non-interactive use needs `--yes`.

## Environment

| variable | |
|---|---|
| `HERDR_PM_DRY_RUN=1` | print changes instead of making them |
| `HERDR_PM_HERDR_BIN` | herdr binary (default `HERDR_BIN_PATH`, then `herdr`) |
| `HERDR_PM_CONFIG_DIR` / `HERDR_PM_HOME` | override config / data directories |
| `HERDR_PM_EDITOR` | editor for `c`/`C` (then `VISUAL`, `EDITOR`) |
| `HERDR_PM_NO_TOKEN=1` | never send a GitHub token |
| `HERDR_PM_BUILD_FROM_SOURCE=1` | rebuild even if a prebuilt `bin/herdr-pm` matches |

## Development

```sh
cargo test                      # unit tests
cargo build && sh tests/e2e.sh  # end-to-end against a real herdr on PATH
HERDR_PM_DRY_RUN=1 cargo run    # popup UI in any terminal, nothing changes
```

The end-to-end suite builds a throwaway git server (bare repos, `file://`,
`git daemon`), and exercises install, subdirectories, updates with diffs,
rollback, pin, refusals, aliases, the git-index marketplace, export/import
and a live Herdr server.

---

## Français

Gestionnaire de plugins [Herdr](https://herdr.dev) en Rust, en popup ou en
ligne de commande, qui installe depuis **n'importe quel serveur git** :
Gitea, Forgejo, GitLab, dépôt nu en SSH, `git daemon`, HTTP « dumb », dossier
partagé. `herdr plugin install` n'accepte que `owner/repo` GitHub ; ici toute
URL git fonctionne.

Il reprend toutes les fonctions de speardragon/herdr-plugin-manager et
ajoute : alias de serveurs (`corp:outils/notif.git`), monorepos
(`depot.git//plugins/foo`), aperçu de confiance complet avant toute
exécution, **diff de ce que le plugin exécute** à chaque mise à jour,
**rollback** instantané, pin/unpin/switch, marketplace multi-sources (GitHub,
Gitea/Forgejo, GitLab, index statique, **index dans un dépôt git** pour un
serveur git nu), générateur de catalogue, export/import, `doctor`,
historique, gestion du proxy et de `NO_PROXY`.

**Installation sur votre serveur git** :

```sh
git clone ssh://git@git.corp.local/outils/herdr-plugin-manager.git
cd herdr-plugin-manager && cargo install --path . --locked
herdr-pm install ssh://git@git.corp.local/outils/herdr-plugin-manager.git
```

Puis un raccourci dans `~/.config/herdr/config.toml` :

```toml
[[keys.command]]
key = "prefix+p"
type = "plugin_action"
command = "herdr-plugin-manager.open"
```

**Marketplace interne sans API** : lancez
[`examples/publish-index.sh`](examples/publish-index.sh) en cron sur le
serveur, puis déclarez-la chez les clients :

```toml
[[marketplace]]
name = "interne"
kind = "git-index"
url = "ssh://git@git.corp.local/srv/git/herdr/index.git"
```

`herdr-pm config init` crée un fichier de configuration commenté ; touche
`?` dans le popup pour l'aide.

## License

MIT
