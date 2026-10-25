# Changelog


## Unreleased

- Rename the user-facing interface and Herdr plugin id to GitBridge
  (`gitbridge`), add an original logo, rewrite the documentation, and expand
  community/CI configuration.
- Reject Git remote-helper transports and option-shaped URLs at the Git
  boundary; validate import sources and `--subdir` before fetching.
- Sanitize every trust-preview and popup field, including event names, link
  handlers, marketplace URLs and refs.
- Normalize abbreviated commit installs to full ids for subsequent update
  checks; import GitHub entries at their exported commit.
- Abort installation on submodule failures; preserve the previous Herdr
  registration when replacing a GitHub install through Git.
- Report a corrupt private registry instead of treating managed plugins as
  local links; preserve dangling registry symlinks during offline changes.
- Match Herdr's Windows-safe config path encoding, avoid `cmd /c start` for
  URLs, and select GitHub Enterprise tokens by hostname.
- Upgrade Ratatui to 0.30.2 and `lru` to 0.18.5, removing the two memory
  safety advisories affecting the previous dependency version.

## 0.1.0 — 2026-10-25

First release.

- Popup and CLI: list, install, update one/all, enable/disable, uninstall,
  run actions (with their key bindings), open source, edit plugins.json.
- Install from any git remote (https, ssh, scp-style, git://, file://, local
  paths, dumb HTTP), monorepo subdirectories (`repo//dir`, `--subdir`) and
  server aliases. GitHub shorthands still go through `herdr plugin install`.
- Trust preview with every command, control characters escaped; update
  previews list new commits and the diff of what the plugin runs.
- Versioned installations with rollback, pin/unpin, switch.
- Marketplace from GitHub, Gitea/Forgejo, GitLab, static indexes and
  git-hosted indexes; `index build` / `index check` to publish catalogs.
- Export/import, history, doctor, dry-run, NO_PROXY-aware HTTP.
