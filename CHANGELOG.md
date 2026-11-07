# Changelog

## [0.5.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.4.1...v0.5.0) (2026-11-07)


### Features

* Per-plugin auto-update, toggled with A in the popup ([#14](https://github.com/julienpoirou/herdr-plugin-manager/issues/14)) ([b807f55](https://github.com/julienpoirou/herdr-plugin-manager/commit/b807f55cfecfd5ec8b25e8a705b591f09bca7327))

## [0.4.1](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.4.0...v0.4.1) (2026-11-07)


### Bug Fixes

* **tui:** Drop the Herdr version from the popup header ([#12](https://github.com/julienpoirou/herdr-plugin-manager/issues/12)) ([72c9105](https://github.com/julienpoirou/herdr-plugin-manager/commit/72c9105731bf8ce27d28e50d0eb96eb017d7cfac))

## [0.4.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.3.0...v0.4.0) (2026-11-06)


### Features

* **market:** List the full herdr.dev catalog from a background-refreshed cache ([#10](https://github.com/julienpoirou/herdr-plugin-manager/issues/10)) ([cf4fc28](https://github.com/julienpoirou/herdr-plugin-manager/commit/cf4fc28fa8f41dd55e6c5f51a1bc26f99918cc72))

## [0.3.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.2.0...v0.3.0) (2026-11-06)


### ⚠ BREAKING CHANGES

* the plugin id is now herdr-plugin-manager. Uninstall the gitbridge plugin and install this one again, bind the popup to herdr-plugin-manager.open, and move plugins/gitbridge under Herdr's state directory to plugins/herdr-plugin-manager to keep git-managed installs. Exports must use the herdr-plugin-manager-export format.

### Features

* Rename the plugin to herdr-plugin-manager ([#8](https://github.com/julienpoirou/herdr-plugin-manager/issues/8)) ([11f93bb](https://github.com/julienpoirou/herdr-plugin-manager/commit/11f93bbae5072096f3fd72967c00a78875fe29cc))
* **tui:** Remove the per-plugin actions menu ([11f93bb](https://github.com/julienpoirou/herdr-plugin-manager/commit/11f93bbae5072096f3fd72967c00a78875fe29cc))


### Bug Fixes

* **tui:** Request mouse reports from the terminal on Windows ([11f93bb](https://github.com/julienpoirou/herdr-plugin-manager/commit/11f93bbae5072096f3fd72967c00a78875fe29cc))

## 0.2.0 — 2026-11-04

- Publish releases: release-please release pull requests, prebuilt
  `herdr-pm` archives for Linux (static musl), macOS and Windows with
  SHA-256 checksums, the `ghcr.io/julienpoirou/herdr-plugin-manager`
  container image, and the `herdr-plugin-manager` crate on crates.io when
  `CARGO_REGISTRY_TOKEN` is set.
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
