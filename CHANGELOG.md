# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- feat: Native Qwery support — the installer now detects Qwery (`~/.qwery` or the `qwery`/`qwery-tui` CLI) and injects the GFS skills into `~/.qwery/skills/` and the Qwery agent into `~/.qwery/agents/`
 - feat: add SQLite database provider to `gfs-compute-docker` (file-backed provider, import/export/schema support)

### Bug Fixes

- fix: Windows snapshot/clone use `robocopy /E /COPY:DAT` instead of `/COPYALL` so commits do not require copying audit (SACL) information, which failed on Windows 11 ([issue #34](https://github.com/Guepard-Corp/gfs/issues/34))
- fix: Correct opencode.json MCP configuration format to use command array with type: local for proper OpenCode integration

## [0.2.0] - 2026-03-23

Binaries for this release are available on [GitHub Releases](https://github.com/Guepard-Corp/gfs/releases/tag/v0.2.0).

### Features

- feat: improve Docker connection error messages with actionable hints when Docker is not running or the user lacks permission

### Bug Fixes

- fix: recreate compute when container has been manually removed from Docker

### Chores

- chore: add --port flag to gfs init and gfs compute config db.port

## [0.1.13] - 2026-03-14

Binaries for this release are available on [GitHub Releases](https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.13).

### Bug Fixes

- fix: release telemetry
- fix: version exit code
- fix: use workspace-relative paths for Docker mounts and add export directory defaults
- fix: workspace-relative Docker mounts and export directory defaults 2
- fix: workspace-relative Docker mounts and export directory defaults 3
- fix: better error management
- fix: fix tests

### Documentation

- docs: rewrite README to emphasize AI coding agents use case
- docs: switch licence to MIT

### Chores

- chore: update changelog and fix typos
- chore: bump version to 0.1.13

### CI

- ci: better pr workflow
- ci: add changelog generation

## [0.1.12] - 2026-03-05

Binaries for this release are available on [GitHub Releases](https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.12).

## [0.1.10] - 2026-03-02

Binaries for this release are available on [GitHub Releases](https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.10).

## [0.1.4] - 2026-02-25

Binaries for this release are available on [GitHub Releases](https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.4).

## [0.1.3] - 2026-02-25

Binaries for this release are available on [GitHub Releases](https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.3).

[Unreleased]: https://github.com/Guepard-Corp/gfs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Guepard-Corp/gfs/compare/v0.1.13...v0.2.0
[0.1.13]: https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.13
[0.1.12]: https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.12
[0.1.10]: https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.10
[0.1.4]: https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.4
[0.1.3]: https://github.com/Guepard-Corp/gfs/releases/tag/v0.1.3
