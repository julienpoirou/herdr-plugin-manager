# Changelog

## [0.18.3](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.18.2...v0.18.3) (2026-11-14)


### Corrections 🐛

* Remove failing marketplace and harden project tooling ([#64](https://github.com/julienpoirou/herdr-plugin-manager/issues/64)) ([4d31c24](https://github.com/julienpoirou/herdr-plugin-manager/commit/4d31c24bb9f34e97ec5006289ba9815282e7eedd))

## [0.18.2](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.18.1...v0.18.2) (2026-11-13)


### Bug Fixes

* **tui:** The selected marketplace entry is highlighted across both lines ([#62](https://github.com/julienpoirou/herdr-plugin-manager/issues/62)) ([f5380cf](https://github.com/julienpoirou/herdr-plugin-manager/commit/f5380cf76acf347c0d0e22180954c37d30a27dd3))

## [0.18.1](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.18.0...v0.18.1) (2026-11-13)


### Bug Fixes

* **tui:** Larger back and close buttons with a margin on their left ([#60](https://github.com/julienpoirou/herdr-plugin-manager/issues/60)) ([a7df419](https://github.com/julienpoirou/herdr-plugin-manager/commit/a7df419f59968e71a134f578d0878c171bf4ca62))

## [0.18.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.17.1...v0.18.0) (2026-11-12)


### Features

* **gitops:** Choose per machine whether the plugin list is backed up ([#58](https://github.com/julienpoirou/herdr-plugin-manager/issues/58)) ([d50800d](https://github.com/julienpoirou/herdr-plugin-manager/commit/d50800daaefc456f490e32a652b7dc273ef1fe38))


### Bug Fixes

* **tui:** Settings, the version picker and the leftovers list only scroll when the selection leaves the window, so a click on a visible line no longer makes the list jump. ([d50800d](https://github.com/julienpoirou/herdr-plugin-manager/commit/d50800daaefc456f490e32a652b7dc273ef1fe38))

## [0.17.1](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.17.0...v0.17.1) (2026-11-12)


### Bug Fixes

* **tui:** Back and close buttons side by side at the top right, unstyled footer ([#56](https://github.com/julienpoirou/herdr-plugin-manager/issues/56)) ([fa6f483](https://github.com/julienpoirou/herdr-plugin-manager/commit/fa6f483f20ac381cfc6bedc1471f8b4490fee11a))

## [0.17.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.16.1...v0.17.0) (2026-11-12)


### Features

* **tui:** Every shortcut is also clickable, with close and back buttons ([#55](https://github.com/julienpoirou/herdr-plugin-manager/issues/55)) ([e051881](https://github.com/julienpoirou/herdr-plugin-manager/commit/e051881f5cb7505b32a8a4736ef6aa533643c7d9))


### Bug Fixes

* **gitops:** Pull and push from any machine with per-item three-way reconciliation ([#53](https://github.com/julienpoirou/herdr-plugin-manager/issues/53)) ([d943248](https://github.com/julienpoirou/herdr-plugin-manager/commit/d943248507cef1a9f05896d5c26170aec7d1d602))

## [0.16.1](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.16.0...v0.16.1) (2026-11-11)


### Bug Fixes

* Put herdr-pm on PATH for new Herdr panes without restarting Herdr ([#51](https://github.com/julienpoirou/herdr-plugin-manager/issues/51)) ([8a21531](https://github.com/julienpoirou/herdr-plugin-manager/commit/8a2153175a2b2ebc57ce818d670a2913e38d0b9d))

## [0.16.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.15.0...v0.16.0) (2026-11-11)


### ⚠ BREAKING CHANGES

* export/import and the history alias are removed. Existing manager installations must migrate to the namespaced plugin id using a freshly built release checkout.

### Features

* GitOps backup and sync, namespaced identity and settings ([#49](https://github.com/julienpoirou/herdr-plugin-manager/issues/49)) ([81e5ecd](https://github.com/julienpoirou/herdr-plugin-manager/commit/81e5ecd7704b44c488c2fa0101d2b5f352a8550e))

## [0.15.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.14.2...v0.15.0) (2026-11-08)


### Features

* Add followable logs and configurable JSONL or ECS history ([#47](https://github.com/julienpoirou/herdr-plugin-manager/issues/47)) ([3bcf7bb](https://github.com/julienpoirou/herdr-plugin-manager/commit/3bcf7bb815764dbc1f07e5febd0ec927ed6a9f45))

## [0.14.2](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.14.1...v0.14.2) (2026-11-08)


### Bug Fixes

* A copied config.toml with cli_on_path on puts the link directory on PATH at the next refresh ([a35b912](https://github.com/julienpoirou/herdr-plugin-manager/commit/a35b9122294f3dbb1ca47b1b19231da39cb926a5))
* Exports carry auto-update and GitHub entries import following their branch ([#45](https://github.com/julienpoirou/herdr-plugin-manager/issues/45)) ([a35b912](https://github.com/julienpoirou/herdr-plugin-manager/commit/a35b9122294f3dbb1ca47b1b19231da39cb926a5))

## [0.14.1](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.14.0...v0.14.1) (2026-11-08)


### Bug Fixes

* Turning cli_on_path off also turns agent_skill off ([#43](https://github.com/julienpoirou/herdr-plugin-manager/issues/43)) ([11736aa](https://github.com/julienpoirou/herdr-plugin-manager/commit/11736aae96a2e1495113ba4142e61418f545d29e))

## [0.14.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.13.0...v0.14.0) (2026-11-08)


### Features

* Herdr-pm on PATH, an agent skill, and project pictures ([#41](https://github.com/julienpoirou/herdr-plugin-manager/issues/41)) ([73822d3](https://github.com/julienpoirou/herdr-plugin-manager/commit/73822d38e1e61bc6320619301a5057004c2fe044))

## [0.13.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.12.0...v0.13.0) (2026-11-08)


### Features

* Show GitHub remotes as owner/repo/subdir and let the manager take itself over ([#39](https://github.com/julienpoirou/herdr-plugin-manager/issues/39)) ([8e352e0](https://github.com/julienpoirou/herdr-plugin-manager/commit/8e352e09887f52925bc6dd3b9f136a244424a9fa))

## [0.12.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.11.0...v0.12.0) (2026-11-08)


### Features

* Choose when auto-update runs and keep plugins out of herdr plugin update ([#37](https://github.com/julienpoirou/herdr-plugin-manager/issues/37)) ([92fbbb3](https://github.com/julienpoirou/herdr-plugin-manager/commit/92fbbb367e35968c495ae6546e5df90dc83451d5))

## [0.11.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.10.0...v0.11.0) (2026-11-08)


### Features

* **tui:** Edit config.toml in a settings dialog ([#35](https://github.com/julienpoirou/herdr-plugin-manager/issues/35)) ([913c860](https://github.com/julienpoirou/herdr-plugin-manager/commit/913c8600462bb1090711f52a63dd6ea9b7266a12))

## [0.10.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.9.0...v0.10.0) (2026-11-08)


### Features

* Auto-update strategies and gitlab.com/codeberg.org marketplaces ([#33](https://github.com/julienpoirou/herdr-plugin-manager/issues/33)) ([ea7c3cf](https://github.com/julienpoirou/herdr-plugin-manager/commit/ea7c3cfe4e4f62cc5c685b032d6dda1f5ed89e69))


### Bug Fixes

* Delete a plugin's data directory and Herdr lock files with x then d ([ea7c3cf](https://github.com/julienpoirou/herdr-plugin-manager/commit/ea7c3cfe4e4f62cc5c685b032d6dda1f5ed89e69))
* **tui:** Spell out labels in the detail panels and the footer, label the description, drop the marketplace hint line and the L log listing ([ea7c3cf](https://github.com/julienpoirou/herdr-plugin-manager/commit/ea7c3cfe4e4f62cc5c685b032d6dda1f5ed89e69))

## [0.9.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.8.0...v0.9.0) (2026-11-08)


### Features

* Choose which removed plugins to clean ([#31](https://github.com/julienpoirou/herdr-plugin-manager/issues/31)) ([13f2864](https://github.com/julienpoirou/herdr-plugin-manager/commit/13f2864bc1b8358429add4f6c96c257a404c16d7))

## [0.8.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.7.0...v0.8.0) (2026-11-07)


### Features

* Clean leftovers of removed plugins with X or herdr-pm clean ([#28](https://github.com/julienpoirou/herdr-plugin-manager/issues/28)) ([6d1d006](https://github.com/julienpoirou/herdr-plugin-manager/commit/6d1d006e8bdbc194c182388f11de8f469106cbf6))


### Bug Fixes

* **tui:** List latest (unpin) first in the version picker ([6d1d006](https://github.com/julienpoirou/herdr-plugin-manager/commit/6d1d006e8bdbc194c182388f11de8f469106cbf6))

## [0.7.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.6.0...v0.7.0) (2026-11-07)


### Features

* **tui:** Run installs, updates and removals inside the popup ([#26](https://github.com/julienpoirou/herdr-plugin-manager/issues/26)) ([4e063af](https://github.com/julienpoirou/herdr-plugin-manager/commit/4e063af89158a40ff7dc1b75f2ac24447f1ffbfc))


### Bug Fixes

* **tui:** Keep backslashes of Windows paths at the install prompt and in the editor command ([4e063af](https://github.com/julienpoirou/herdr-plugin-manager/commit/4e063af89158a40ff7dc1b75f2ac24447f1ffbfc))

## [0.6.0](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.5.3...v0.6.0) (2026-11-07)


### Features

* Delete a plugin completely with `x` then `d`, or `uninstall --purge` ([630fbf9](https://github.com/julienpoirou/herdr-plugin-manager/commit/630fbf9672f81d7d01bec97e58d72059302a5d6a))
* **tui:** Pick a version to pin from the repository tags ([#22](https://github.com/julienpoirou/herdr-plugin-manager/issues/22)) ([630fbf9](https://github.com/julienpoirou/herdr-plugin-manager/commit/630fbf9672f81d7d01bec97e58d72059302a5d6a))


### Bug Fixes

* **tui:** Explain on activation that auto-update installs without review ([630fbf9](https://github.com/julienpoirou/herdr-plugin-manager/commit/630fbf9672f81d7d01bec97e58d72059302a5d6a))
* **tui:** One shortcut per line in a scrollable help overlay ([630fbf9](https://github.com/julienpoirou/herdr-plugin-manager/commit/630fbf9672f81d7d01bec97e58d72059302a5d6a))
* **tui:** Show the followed ref as `branch@commit`, or `@commit (pinned)` ([630fbf9](https://github.com/julienpoirou/herdr-plugin-manager/commit/630fbf9672f81d7d01bec97e58d72059302a5d6a))

## [0.5.3](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.5.2...v0.5.3) (2026-11-07)


### Bug Fixes

* **git:** Check out committed bytes regardless of core.autocrlf ([#20](https://github.com/julienpoirou/herdr-plugin-manager/issues/20)) ([6fdf58a](https://github.com/julienpoirou/herdr-plugin-manager/commit/6fdf58af5114bf519c72a692e28f48e09d633997))

## [0.5.2](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.5.1...v0.5.2) (2026-11-07)


### Bug Fixes

* **tui:** Clearer footer and help labels ([#18](https://github.com/julienpoirou/herdr-plugin-manager/issues/18)) ([2551f17](https://github.com/julienpoirou/herdr-plugin-manager/commit/2551f1739c2223ce8fd129a66cbe1cd0ed09cba3))

## [0.5.1](https://github.com/julienpoirou/herdr-plugin-manager/compare/v0.5.0...v0.5.1) (2026-11-07)


### Bug Fixes

* **market:** Keep the star icon off the star count ([#16](https://github.com/julienpoirou/herdr-plugin-manager/issues/16)) ([3649b5b](https://github.com/julienpoirou/herdr-plugin-manager/commit/3649b5b43c3e49f31241e071f6fff1a330e02afc))

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
