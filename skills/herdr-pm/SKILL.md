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
   `switch`, `rollback`, `take-over`). Show the preview to the user, wait for
   a yes, then run the same command with `--yes` and without `--dry-run`.
2. You have no terminal for prompts: commands that change things need `--yes`,
   or they stop with "pass --yes".
3. Read state with `--json` when it exists, not by parsing tables.
4. `uninstall --purge` and `clean` delete files: name what goes, ask first.

## Read

| Need | Command |
|---|---|
| Installed plugins | `herdr-pm list --json` (`--check` adds update status) |
| Updates waiting | `herdr-pm outdated --json` |
| One plugin | `herdr-pm info <id> --json` |
| Find plugins | `herdr-pm search <words> --json` (`--source herdr.dev\|gitlab.com\|codeberg.org`, `--sort stars\|updated\|name`) |
| Marketplaces | `herdr-pm sources` |
| Past changes | `herdr-pm logs [id] --format jsonl [--limit N]` (`history` is an alias) |
| Live changes | `herdr-pm logs [id] -f --format jsonl` (`--follow`; stop when finished) |
| Health | `herdr-pm doctor` |
| Config | `herdr-pm config show`, `herdr-pm config path` |

`logs --format ecs` emits Elastic Common Schema documents. Settings (`C`)
or `history = "jsonl" | "ecs" | "off"` in config select how new changes
are recorded, not the stdout format. Off preserves existing records.

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
and extra `[[marketplace]]` sources. Edit it only when asked; the user can
also use `C` in the popup.

## Good to know

- Plugin ids look like `owner.name`; commands accept the id or the source.
- `--dry-run` (or `HERDR_PM_DRY_RUN=1`) works with every command.
- New or changed plugin commands take effect without restarting Herdr;
  startup hooks run at the next Herdr server start.
- Exit code 1 means at least one plugin failed; the output says which.
