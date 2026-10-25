# GitBridge for Herdr

<img src="assets/logo.png" alt="GitBridge logo" width="112" align="right">

[![CI](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/ci.yml)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-a78bfa)](https://www.rust-lang.org/)
[![Herdr 0.7.4+](https://img.shields.io/badge/herdr-0.7.4%2B-2dd4bf)](https://herdr.dev/)
[![MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**GitBridge connects Herdr to the Git servers you actually use.** It installs
plugins from GitHub, GitLab, Gitea/Forgejo, a company Git appliance, a bare
repository, or a directory on disk. The same `herdr-pm` binary provides a
Ratatui popup and a scriptable CLI.

The repository remains `herdr-plugin-manager`; the Herdr plugin id and Cargo
package are **`gitbridge`**. The user-facing name is **GitBridge for Herdr**.

[English](#why-gitbridge) · [Français](#français)

## Why GitBridge

Herdr's own installer accepts GitHub `owner/repo`. GitBridge adds the other
transports without asking a forge API for a checkout:

| Capability | Behavior |
|---|---|
| Git transports | HTTPS, HTTP, SSH, scp-style SSH, `git://`, `file://`, absolute or relative repository paths |
| Monorepos | `repo.git//plugins/tool` or `--subdir plugins/tool`; a single manifest is discovered automatically |
| Reviewed execution | Resolved commit and every build/startup/action/event/pane command are shown before confirmation |
| Version history | Separate installations, rollback, pin/unpin and branch/tag/commit switching |
| Update context | New commits and changes to the executable surface, not only a version bump |
| Discovery | GitHub/GHE, Gitea/Forgejo, GitLab, static JSON indexes and indexes stored in Git |
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

Bind the popup (recommended: `prefix+g`):

```toml
[[keys.command]]
key = "prefix+g"
type = "plugin_action"
command = "gitbridge.open"
description = "Open GitBridge"
```

Then run `herdr server reload-config`. Put `bin/herdr-pm` on `PATH` (or use
`cargo install --path . --locked`) to use the CLI directly.

On native Windows, the manifest uses `scripts/build.ps1`; Git for Windows,
PowerShell and Rust/MSVC must be available.

## Install sources

```sh
herdr-pm install owner/repo[/subdir]                       # native Herdr/GitHub
herdr-pm install owner/repo --git                          # same repo through GitBridge
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
new build succeeds. Herdr 0.7.4's native GitHub installer has no equivalent
replacement operation for an existing local link. Therefore
`herdr-pm install owner/repo --replace` fails explicitly; use
`herdr-pm install owner/repo --git --replace` when that behavior is wanted.

## Trust and build model

For Git-managed installations, GitBridge:

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

A hard crash between Herdr's registration and GitBridge's `registry.json`
update can require manual recovery. Avoid running multiple mutating
`herdr-pm` commands for the same plugin concurrently; `herdr-pm doctor` reports
registry/list inconsistencies.

## Popup

Open with `herdr-pm`, the configured key, or the `gitbridge.open` action.

| Area | Keys |
|---|---|
| Navigation | arrows / `j` / `k`, mouse, `/` filter |
| Actions | Enter folds actions; Enter on an action closes the popup then invokes it |
| Updates | `u` selected, `U` all, each with its own review |
| State | `e` enable/disable, `x` uninstall, `p` pin/unpin |
| Versions | `s` switch ref, `z` rollback |
| Discovery | `m` or Tab, `/` search, `s` sort, `f` source, arrows for pages |
| Other | `o` open homepage, `L` logs, `c`/`C` edit configuration, `r` refresh, `?` help, `q` quit |

Install, update, rollback, pin and uninstall suspend the popup and use the same
terminal review paths as the CLI.

## CLI

```text
herdr-pm                       open the interactive manager
herdr-pm list [--check] [--json]
herdr-pm install <source> [--ref R] [--subdir D] [--disabled] [--git] [--replace] [-y]
herdr-pm update [ids…] [-y]    herdr-pm outdated [--json]
herdr-pm uninstall <id>        herdr-pm enable|disable <id>
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

`herdr-pm config init` creates a commented `config.toml` in the plugin's Herdr
config directory.

```toml
keep_installs = 3        # previous installations retained for rollback
market_ttl = 600         # marketplace cache lifetime in seconds
check_concurrency = 6    # parallel background update checks

[aliases]
corp = "ssh://git@git.corp.example/"
lab = "https://gitlab.corp.example/{path}.git"

[[marketplace]]
name = "github"
kind = "github"
topic = "herdr-plugin"

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

GitBridge stores its data under Herdr's state directory:

```text
~/.local/state/herdr/plugins/gitbridge/
├── registry.json       # remotes, followed refs, current and previous installs
├── installs/           # independent checkouts
├── cache/              # bare mirrors and marketplace responses
├── trash/              # deferred cleanup
└── history.jsonl
```

On Windows the default root is `%LOCALAPPDATA%\herdr`. `HERDR_PM_HOME` and
`HERDR_PM_CONFIG_DIR` override GitBridge's directories.

Enable/disable first uses Herdr's CLI. If the server is stopped, GitBridge
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

**GitBridge for Herdr** installe des plugins depuis GitHub, GitLab,
Gitea/Forgejo, un dépôt nu, `git daemon` ou un chemin local. Le binaire
`herdr-pm` fournit une interface Ratatui et une CLI scriptable.

Ses fonctions principales sont les monorepos (`depot.git//plugins/outil`), les
alias de serveurs, l'aperçu des commandes avant confirmation, les diffs de
mise à jour, les installations indépendantes avec rollback, pin/unpin/switch,
plusieurs marketplaces, les index JSON hébergés dans Git, l'export/import aux
commits exacts, `doctor`, l'historique et le mode hors ligne.

```sh
herdr plugin install julienpoirou/herdr-plugin-manager
herdr-pm install git@git.corp.example:outils/notify.git
herdr-pm install /srv/git/mono.git//plugins/foo --ref v1.4.0
```

Raccourci conseillé : `prefix+g` avec la commande
`gitbridge.open` (exemple TOML ci-dessus). `herdr-pm config init`
crée une configuration commentée et `?` affiche l'aide du popup.

**Sécurité** : les transports `ext::`/remote-helpers sont refusés, les chemins
`..` sont rejetés, les caractères de contrôle sont neutralisés, et aucun build
n'est lancé avant confirmation. Le manifeste est comparé avant et après le
build. Les plugins restent du code exécuté avec vos droits : lisez l'aperçu et
épinglez le commit vérifié.

`--replace` fonctionne dans le pipeline Git. Pour remplacer un lien local par
une source GitHub, utilisez `owner/repo --git --replace` : le programme Herdr
natif ne propose pas cette opération de remplacement.

## License

MIT — see [LICENSE](LICENSE).
