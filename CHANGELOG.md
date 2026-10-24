# Changelog

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
