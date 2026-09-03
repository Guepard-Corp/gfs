# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

- feat: SQLite provider — a file-based database versioned without any container runtime. `gfs init --database-provider sqlite` provisions nothing; commit, checkout, branching, schema capture, export and import all run against the SQLite amalgamation linked into the binary, so the recorded engine version is reproducible rather than inherited from whatever `sqlite3` the host has. Snapshots are quiesced through SQLite's own write lock, which is what a container could not do for it — the writer is the user's application, not anything GFS controls
- feat: Native Qwery support — the installer now detects Qwery (`~/.qwery` or the `qwery`/`qwery-tui` CLI) and injects the GFS skills into `~/.qwery/skills/` and the Qwery agent into `~/.qwery/agents/`

### Bug Fixes

- fix: `gfs checkout` of a commit whose snapshot tree is missing reported success and handed over an **empty database**. The restore treated "this commit records no snapshot" and "this commit's snapshot is gone" as the same case; `gfs schema show` still printed the real DDL, so only a query noticed, and a commit taken from that state recorded the emptiness as a legitimate breaking change. It now refuses and names the missing tree. The Kubernetes runtime, where the real restore is a PVC VolumeSnapshot and no filesystem snapshot is expected, is unaffected
- fix: a symlinked database — or a symlinked directory containing one — was committed as a success with **zero data** in the snapshot. The refusal existed but only on the conventional-name path; the discovery path's symlink check was guarded by `DirEntry::metadata()`, which does not follow links on Unix, so it never once fired. The snapshot held a dangling link, the schema was empty, and a later checkout returned whatever the outside file said at that moment
- fix: `gfs export` truncated any TEXT value at an embedded NUL byte. `quote()` is C-string based, so `x'610062'` dumped as `'a'` and restored as `'a'`, exit 0. Such values now go out as a blob cast
- fix: `gfs export` omitted `sqlite_sequence`, so a restored database reused a deleted AUTOINCREMENT id — voiding the one guarantee that column type exists for. A source whose next id was 4 handed out 3
- fix: a failed `gfs import` echoed the entire un-executed remainder of the script; a 4 MB dump produced a 4 MB error message
- fix: `gfs init --database-provider sqlite3` reported "GFS was not able to connect to Docker/Podman" and a list of daemon troubleshooting steps. The provider name was validated only after the container client had been built
- fix: `gfs init` accepted any `--database-version`, so a repository could permanently record "sqlite 4" while running against the linked 3.53.2
- fix: `gfs schema extract` and `gfs query` replaced the provider's actionable message ("the workspace holds 2 SQLite databases (a.db, b.db) … set GFS_SQLITE_DB_PATH to choose") with "schema extraction failed" and "failed to build query command"
- fix: `gfs status` advertises a connection string in its help and showed none for an embedded provider, which has no compute section to carry one. The MCP `status` tool likewise returned two fields where its skill file promised current branch, HEAD commit and connection information
- fix: a concurrent `gfs commit` and `gfs checkout` could lose a commit. A commit reads HEAD's commit as its parent before the snapshot and reads HEAD's branch after it, so a checkout landing in between made the commit advance the branch it moved *to*, with a parent from the branch it moved *from* — leaving that branch's previous tip unreachable from any ref, with both commands reporting success. Checkout now takes the same repository lock commit takes, waiting for a running commit rather than interleaving with it
- fix: `gfs export --format sql` produced a dump that corrupted data when replayed. All DDL was written before the rows, so every `AFTER INSERT` trigger fired for every replayed row; and virtual-table content was omitted entirely, because `CREATE VIRTUAL TABLE` builds an empty index. The dump now emits tables, then rows, then views/triggers/indexes, and names its insert columns so generated and hidden columns cannot misalign
- fix: `gfs import` reported "import failed" and discarded the underlying cause, so a missing file, an unsupported format and an invalid script were indistinguishable
- fix: MCP tools all built a container client before deciding whether they needed one, so with no reachable Docker daemon every tool failed — including `init --database-provider sqlite`, which starts no container. The MCP `query` tool also required a runtime section that an embedded repository never writes
- fix: `gfs user` on an embedded provider advised `gfs compute start`, which then reported no `container_name` in the repo config. Neither command could succeed; both now explain that an embedded database is a file with no server, no roles and nothing to start
- fix: a column made unique by a partial index whose predicate is `<column> IS NOT NULL` is now reported unique. Such a predicate cannot exclude a row the index would otherwise constrain, since a unique index already permits any number of NULLs
- fix: `gfs export` silently destroyed a contentless or external-content FTS5 index. Such a table stores nothing retrievable of its own, so repopulating it from its own rows wrote NULLs: the dump restored without error, kept the row count, and returned nothing for every `MATCH`. Export now refuses, naming the table and pointing at `sqlite3 .dump`, which writes the index's shadow tables directly
- fix: MCP `init` accepted a `database_version` the provider does not support and wrote it to the repo config permanently, and reported a Docker daemon failure for an unknown provider name — both of which the CLI already refused. The check now lives in `InitRepositoryUseCase`, which every caller reaches, rather than in one caller
- fix: a column made unique by an expression index — `CREATE UNIQUE INDEX ix ON t(lower(a))` — is now reported unique, while one whose key spans two columns (`t(a || b)`) is correctly not. The two are indistinguishable through SQLite's pragmas, so this reads the index DDL; the reader is written so that every case it cannot parse under-claims rather than over-claims
- fix: Windows snapshot/clone use `robocopy /E /COPY:DAT` instead of `/COPYALL` so commits do not require copying audit (SACL) information, which failed on Windows 11 ([issue #34](https://github.com/Guepard-Corp/gfs/issues/34))
- fix: Correct opencode.json MCP configuration format to use command array with type: local for proper OpenCode integration

### Chores

- chore: a test over the provider catalogue asserts that every registered provider is exactly one of container-backed or embedded. The trait documented it and nothing enforced it: a provider implementing neither accessor would report `requires_compute() == false` while offering no embedded path to run on
- chore: `scripts/gfs-reclaim-orphan-snapshots.py` lists and removes snapshot trees no commit refers to. A commit killed between taking its snapshot and writing its commit object leaves one behind, and there is no `gfs gc`
- chore: `scripts/gfs-commit-checkout-race.py` reproduces the concurrent commit/checkout interleaving, so the fix above can be re-checked rather than believed
- chore: the SQLite end-to-end suites now run on Linux as well as macOS (`cfg(unix)` rather than `cfg(target_os = "macos")`), which is where the snapshot guard matters most — `cp --reflink=auto` degrades to a deep copy on ext4. Adds a concurrency test that commits repeatedly against a live writer and asserts every snapshot is structurally sound, holds a row count the database passed through, and contains only whole transactions
- chore: `scripts/sqlite-snapshot-torture.py` bounds its writer at `GFS_TORTURE_MAX_ROWS` (default 2,000,000, about 90 MB). Unbounded, twelve rounds wrote 59.8M rows and left a 7.5 GB repository, because each round runs for as long as the growing commit takes

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
