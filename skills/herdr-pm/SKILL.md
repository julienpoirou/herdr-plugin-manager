---
name: herdr-pm
description: Manage Herdr plugins with the herdr-pm CLI — list, search the marketplaces, install from GitHub or any git URL, update, pin, roll back, enable/disable, remove, clean leftovers, and configure auto-update. Use whenever the user asks about Herdr plugins or wants one installed, updated or removed.
---

# herdr-pm

`herdr-pm` manages Herdr plugins. Run it as a normal shell command; it talks
to Herdr itself. If `herdr-pm` is not found, tell the user to turn on
`cli_on_path` (`C` in the manager popup, `prefix+m` in Herdr).

## Rules

1. **Never install or change a plugin without the user's approval.** Plugins
   run code on their machine. First show what would run:
   `herdr-pm install <source> --dry-run --yes` (same for `update`, `pin`,
   `switch`, `rollback`, `take-over`, `push`, `pull`, `migrate`). Show the preview to the user, wait for
   a yes, then run the same command with `--yes` and without `--dry-run`.
2. You have no terminal for prompts: commands that change things need `--yes`,
   or they stop with "pass --yes".
3. Read state with `--json` when it exists, not by parsing tables.
4. `uninstall --purge` and `clean` delete files: name what goes, ask first.
5. Backup is plaintext and opt-in. Never enable it, expand its exact file
   approvals, or push configuration without explicit user approval. A private
   Git repository does not remove secrets from selected files or Git history.
   Pull conflicts must be resolved explicitly, never bypassed with force/reset.

## Read

| Need | Command |
|---|---|
| Installed plugins | `herdr-pm list --json` (`--check` adds update status) |
| Updates waiting | `herdr-pm outdated --json` |
| One plugin | `herdr-pm info <id> --json` |
| Find plugins | `herdr-pm search <words> --json` (`--source herdr.dev\|gitlab.com\|codeberg.org`, `--sort stars\|updated\|name`) |
| Marketplaces | `herdr-pm sources` |
| Past changes | `herdr-pm logs [id] --format jsonl [--limit N]` |
| Live changes | `herdr-pm logs [id] -f --format jsonl` (`--follow`; stop when finished) |
| Health | `herdr-pm doctor` |
| Config | `herdr-pm config show`, `herdr-pm config path` |

`logs --format ecs` emits Elastic Common Schema documents. Settings (`C`)
or `history = "jsonl" | "ecs" | "off"` in config select how new changes
are recorded, not the stdout format. Off preserves existing records.

## GitOps

`herdr-pm push` captures the current plugin state and approved configuration,
shows the Git diff, commits and pushes. `herdr-pm pull` fetches and previews
exact plugin/configuration changes, stopping on local conflicts. Both support
`--dry-run --yes` for previews and require approval before applying with `--yes`.
There are no export/import commands.

Set up `[backup]` through Settings (`C`): local directory, remote, branch, and
exact plaintext file approvals (empty by default). Its enabled worker captures
local changes every two seconds; it never commits or pushes automatically.
`plugins.toml` records source/commit/ref/version/pin/enabled/auto-update.
Only locally approved config files are applied. `[backup]` and `log_dir` stay
machine-local; local-only code, binaries, caches, logs and sessions are not backed up.
Errors appear in the worker log path shown in Settings.


## Change (preview with `--dry-run --yes` first)

| Need | Command |
|---|---|
| Install | `herdr-pm install <source> [--ref R] [--subdir D] [--disabled] --yes` |
| Update | `herdr-pm update [ids…] --yes` |
| Hold a version | `herdr-pm pin <id> [tag\|commit] --yes`, release: `herdr-pm unpin <id> --yes` |
| Follow a branch/tag | `herdr-pm switch <id> <ref> --yes` |
| Previous version | `herdr-pm rollback <id> --yes` (git-managed plugins) |
| Herdr's GitHub install → git-managed | `herdr-pm take-over <id> --yes` |
| Enable / disable | `herdr-pm enable <id>`, `herdr-pm disable <id>` |
| Remove | `herdr-pm uninstall <id> --yes` (`--purge` also deletes config and data) |
| Leftovers of removed plugins | `herdr-pm clean [ids…] --yes` |
| Auto-update per plugin | `herdr-pm auto-update enable\|disable <id>` |
| Run auto-update now | `herdr-pm auto-update` |

Sources: `owner/repo[/subdir]` (GitHub), `https://…`, `ssh://…`,
`git@host:path`, `git://…`, `file://…`, a local path, or `alias:path` from
the config. `repo.git//plugins/foo` picks a plugin inside a repository.

## Config

`config.toml` (`herdr-pm config path`) holds `keep_installs`, `market_ttl`,
`github_via_git`, `[auto_update] strategy` (`latest`, `release`, `minor`,
`patch`) and `trigger` (`startup`, `popup`, `min-interval`, `manual`) with
`min_interval`, per-plugin strategies in `[auto_update.plugins]`, `[aliases]`
and extra `[[marketplace]]` sources. `icons` controls pictures; `log_dir` sets
the change-log directory. Edit only when asked; the user can use `C` in the popup.

## Good to know

- Plugin ids look like `owner.name`; commands accept the id or the source.
- The manager id is `julienpoirou.herdr-plugin-manager`. Legacy installations
  require the fresh binary's `migrate <checkout> --binary <binary>` preview
  before applying migration with approval.
- `--dry-run` (or `HERDR_PM_DRY_RUN=1`) works with every command.
- New or changed plugin commands take effect without restarting Herdr;
  startup hooks run at the next Herdr server start.
- Pull refreshes CLI/skill links immediately. It cannot rewrite an already-open
  shell's PATH or an agent's loaded context; do not restart Herdr via handoff
  just to synchronize configuration.
- Exit code 1 means at least one plugin failed; the output says which.
