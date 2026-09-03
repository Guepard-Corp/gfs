# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- feat: SQLite provider — a file-based database versioned without any container runtime. `gfs init --database-provider sqlite` provisions nothing; commit, checkout, branching, schema capture, export and import all run against the SQLite amalgamation linked into the binary, so the recorded engine version is reproducible rather than inherited from whatever `sqlite3` the host has. Snapshots are quiesced through SQLite's own write lock, which is what a container could not do for it — the writer is the user's application, not anything GFS controls
- feat: Native Qwery support — the installer now detects Qwery (`~/.qwery` or the `qwery`/`qwery-tui` CLI) and injects the GFS skills into `~/.qwery/skills/` and the Qwery agent into `~/.qwery/agents/`

### Bug Fixes

- fix: a concurrent `gfs commit` and `gfs checkout` could lose a commit. A commit reads HEAD's commit as its parent before the snapshot and reads HEAD's branch after it, so a checkout landing in between made the commit advance the branch it moved *to*, with a parent from the branch it moved *from* — leaving that branch's previous tip unreachable from any ref, with both commands reporting success. Checkout now takes the same repository lock commit takes, waiting for a running commit rather than interleaving with it
- fix: `gfs export --format sql` produced a dump that corrupted data when replayed. All DDL was written before the rows, so every `AFTER INSERT` trigger fired for every replayed row; and virtual-table content was omitted entirely, because `CREATE VIRTUAL TABLE` builds an empty index. The dump now emits tables, then rows, then views/triggers/indexes, and names its insert columns so generated and hidden columns cannot misalign
- fix: `gfs import` reported "import failed" and discarded the underlying cause, so a missing file, an unsupported format and an invalid script were indistinguishable
- fix: MCP tools all built a container client before deciding whether they needed one, so with no reachable Docker daemon every tool failed — including `init --database-provider sqlite`, which starts no container. The MCP `query` tool also required a runtime section that an embedded repository never writes
- fix: `gfs user` on an embedded provider advised `gfs compute start`, which then reported no `container_name` in the repo config. Neither command could succeed; both now explain that an embedded database is a file with no server, no roles and nothing to start
- fix: a column made unique by a partial index whose predicate is `<column> IS NOT NULL` is now reported unique. Such a predicate cannot exclude a row the index would otherwise constrain, since a unique index already permits any number of NULLs
- fix: Windows snapshot/clone use `robocopy /E /COPY:DAT` instead of `/COPYALL` so commits do not require copying audit (SACL) information, which failed on Windows 11 ([issue #34](https://github.com/Guepard-Corp/gfs/issues/34))
- fix: Correct opencode.json MCP configuration format to use command array with type: local for proper OpenCode integration

### Chores

- chore: `scripts/gfs-reclaim-orphan-snapshots.py` lists and removes snapshot trees no commit refers to. A commit killed between taking its snapshot and writing its commit object leaves one behind, and there is no `gfs gc`
- chore: `scripts/gfs-commit-checkout-race.py` reproduces the concurrent commit/checkout interleaving, so the fix above can be re-checked rather than believed

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
