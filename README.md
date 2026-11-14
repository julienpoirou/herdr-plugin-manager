# Herdr Plugin Manager

[![CI](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/ci.yml)
[![CodeQL](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/codeql.yml/badge.svg?branch=main)](https://github.com/julienpoirou/herdr-plugin-manager/actions/workflows/codeql.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/julienpoirou/herdr-plugin-manager/badge)](https://scorecard.dev/viewer/?uri=github.com/julienpoirou/herdr-plugin-manager)
[![Release](https://img.shields.io/github/v/release/julienpoirou/herdr-plugin-manager)](https://github.com/julienpoirou/herdr-plugin-manager/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Herdr 0.7.4+](https://img.shields.io/badge/Herdr-0.7.4%2B-2dd4bf)](https://herdr.dev/)

<p align="center">
  <img src="logo.svg" alt="Herdr Plugin Manager logo" width="160" height="160">
</p>

Install and manage Herdr plugins from Git repositories with a terminal popup or CLI.

## Requires

- [Herdr](https://herdr.dev/) **0.7.4+** and Git.
- Linux, macOS or Windows.
- For source builds: Rust **1.89+** with Cargo and a C compiler/linker.
  A matching prebuilt binary in the checkout skips compilation.
- Native Windows: Git for Windows and PowerShell; source builds also need
  Rust's MSVC toolchain and Visual Studio C++ Build Tools.

## Install

```sh
herdr plugin install julienpoirou/herdr-plugin-manager
```

For the standalone CLI:

```sh
cargo install herdr-plugin-manager --locked
```

Or download an archive and its SHA-256 checksum from
[Releases](https://github.com/julienpoirou/herdr-plugin-manager/releases)
and place `herdr-pm` on your `PATH`.

For a source checkout:

```sh
git clone https://github.com/julienpoirou/herdr-plugin-manager.git
cd herdr-plugin-manager
sh scripts/build.sh
herdr plugin link "$PWD"
```

On native Windows, replace the last two commands with:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1
herdr plugin link "$($PWD.Path)"
```

See the guide for [packages](docs/guide.md#releases-and-packages) and
[migration from the old plugin identity](docs/guide.md#upgrading-the-old-plugin-identity).

## Usage

Bind the popup in your Herdr configuration:

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "julienpoirou.herdr-plugin-manager.open"
description = "Open the plugin manager"
```

Run `herdr server reload-config`, then press `prefix+m`.
The popup supports keyboard and mouse; `?` opens help and `C` opens settings.

To use the CLI after a plugin-only install, enable `cli_on_path` in settings
or run the installed `bin/herdr-pm` (`bin\herdr-pm.exe` on Windows):

```sh
herdr-pm
herdr-pm install git@git.example.com:team/notify.git
herdr-pm install https://git.example.com/team/mono.git//plugins/tool --ref v1.4.0
herdr-pm list --check
herdr-pm update
```

## Key capabilities

- GitHub, GitLab, Gitea/Forgejo, SSH/HTTPS, bare repositories and local paths.
- Monorepo subdirectories, forge aliases and discovery via herdr.dev and gitlab.com.
- Reviewed commands and commit changes before installation or updates.
- Independent installations, rollback, pin/unpin and branch/tag/commit switching.
- Enable/disable, cleanup, logs, diagnostics and dry-run previews.
- Opt-in GitOps snapshots at exact commits with explicitly approved config files.

## Security and trust

Plugin code runs with your user privileges, not in a sandbox. In the reviewed
installation flow, build and plugin commands execute only after you review and
approve the source, resolved commit and declared commands. Review the repository
too; pin trusted commits where appropriate.

Opt-in auto-update skips the per-update trust preview: enable it only for
repositories you trust to execute new code unattended. `--yes` also supplies
approval without an interactive prompt.

GitOps is opt-in and **plaintext, not encrypted**. Approved files can contain
secrets that remain in Git history; select files deliberately and use a trusted
private repository. See the [trust model](docs/guide.md#trust-and-build-model)
and [GitOps guide](docs/guide.md#gitops-the-same-setup-on-every-machine).

## Roadmap

- Improve GitOps security.
- Add `herdr-pm audit`.
- Add support for modules and layouts in Herdr App.

## License

[MIT](LICENSE) © 2026 Julien Poirou.
