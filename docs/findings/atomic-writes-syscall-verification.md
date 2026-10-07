# Atomic, durable writes: syscall-level verification

What this branch changes, observed on the real binary rather than inferred from
the code: the syscalls a `gfs commit` makes, what a crash at the worst moment leaves
behind, what concurrent readers see, and what it costs.

Everything below was **run** for this record on 2026-10-07 unless it says
**relayed**. Unit tests cannot show a crash; the strace injection in section 2 is
the decisive evidence, the stress counts in section 3 are supporting.

`atomic_write.rs` is a verbatim port of commit `823fcaa` from the unmerged branch
`feat/env-record-and-resolution` (tag `extracted/atomic-write-823fcaa`); it is kept
byte-identical so that branch merges clean, and `durable_write.rs` carries the
hardening. The extraction record itself belongs on that branch's side.

## Setup

| | baseline | fixed |
| --- | --- | --- |
| commit | `ee4197e` (`origin/main`) | `80d2691` (`fix/atomic-writes`) |
| VM binary sha256 | `1c09772f…a70850` | `eb1bd74e…a8de7c9` |
| Mac binary sha256 | `4e9f516f…2c124a` | `c7f0a048…ae06f9` |

- **Linux VM**: multipass `guepard-dev-dp`, Ubuntu 24.04.5, kernel 6.8.0-142-generic
  aarch64, 4 vCPU, root filesystem ext4 (`rw,relatime,discard,commit=30`) on a
  virtual disk. strace 6.8, Python 3.12.3. rustc 1.93.1 (01f6ddf75), installed for
  this run under `/home/ubuntu/gfs-atomic-writes/` (no toolchain existed on the VM;
  removed afterwards).
- **Mac**: Apple M3 Pro, macOS 26.5.1, internal SSD, APFS. rustc 1.93.1 (01f6ddf75),
  forced with `PATH=<toolchain>/bin:$PATH RUSTC=… RUSTDOC=…` because Homebrew's
  rustc 1.96.1 shadows the pinned toolchain on this machine; the `rustc/01f6ddf75`
  commit string is embedded in both Mac binaries and the target directory's
  `.rustc_info.json` reports `release: 1.93.1`.
- Both: `cargo build -p gfs-cli`, **debug** profile, same compiler for both
  binaries on each machine. SQLite provider (`gfs init --database-provider sqlite
  --database-version 3`), no container; `DOCKER_HOST` pointed at a socket that
  cannot exist. Database writes are made with Python's `sqlite3`.
- The scripts are not committed; each section quotes the commands that matter.

## 1. What a commit does to `.gfs`

```
strace -f -o <log> -y -e trace=openat,open,write,pwrite64,fsync,fdatasync,rename,renameat,renameat2,unlink,unlinkat,link,linkat \
  gfs commit -m c2 --path <R>          # after one INSERT, on a repo with one prior commit
```

All `.gfs` writes happen on one thread. Whole-process counts: baseline **0 fsync, 0
rename**; fixed **10 fsync, 5 rename**.

Baseline, last lines (`<R>` is the repository; pid and the fd's path annotation shortened, otherwise verbatim):

```
openat(AT_FDCWD, "<R>/.gfs/objects/0c/dc3000582ff2d00ac75f21a975e25832f99c9818caa7e37022b9948ef4e13e", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 12
openat(AT_FDCWD, "<R>/.gfs/objects/b7/9f0b278c45db10a6b7aa35bb6d52ff724b9de2c88abcfb43f1bbfaa2e77a22", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 12
openat(AT_FDCWD, "<R>/.gfs/refs/heads/main", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 12<<R>/.gfs/refs/heads/main>
write(12<<R>/.gfs/refs/heads/main>, "b79f0b278c45db10a6b7aa35bb6d52ff"..., 64) = 64
```

`O_TRUNC` directly on `refs/heads/main`, then the write; no fsync, no rename.

Fixed, the commit object and the ref (same shortening):

```
openat(AT_FDCWD, "<R>/.gfs/objects/28/.40ddc722210972d18c2b77c657c485298fa9fc3caf85b4dc12733f386c2e52.tmp.150661.3", O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC, 0666) = 12
write(12<…/.40ddc722…tmp.150661.3>, "{\n  \"hash\": \"2840ddc722210972d18"..., 1033) = 1033
fsync(12<…/.40ddc722…tmp.150661.3>) = 0
renameat(AT_FDCWD, "<R>/.gfs/objects/28/.40ddc722…tmp.150661.3", AT_FDCWD, "<R>/.gfs/objects/28/40ddc722210972d18c2b77c657c485298fa9fc3caf85b4dc12733f386c2e52") = 0
fsync(12<<R>/.gfs/objects/28>) = 0
openat(AT_FDCWD, "<R>/.gfs/refs/heads/.main.tmp.150661.4", O_WRONLY|O_CREAT|O_EXCL|O_CLOEXEC, 0666) = 12<<R>/.gfs/refs/heads/.main.tmp.150661.4>
write(12<<R>/.gfs/refs/heads/.main.tmp.150661.4>, "2840ddc722210972d18c2b77c657c485"..., 64) = 64
fsync(12<<R>/.gfs/refs/heads/.main.tmp.150661.4>) = 0
renameat(AT_FDCWD, "<R>/.gfs/refs/heads/.main.tmp.150661.4", AT_FDCWD, "<R>/.gfs/refs/heads/main") = 0
fsync(12<<R>/.gfs/refs/heads>) = 0
```

A dotted temp in the same directory (`O_EXCL`), the write, `fsync` of that fd, the
rename onto `refs/heads/main`, then `fsync` of the directory (opened `O_RDONLY` just
before). The object is durable before the ref that makes it reachable. The schema
files and the files object follow the same sequence.

### fsync and rename count per operation (VM, strace)

| operation | baseline fsync / rename | fixed fsync / rename |
| --- | --- | --- |
| `init` | 0 / 0 | 7 / 4 (HEAD, ref, WORKSPACE durable; `config.toml` via `write_atomic`, no dir sync) |
| `commit` (first, schema unchanged, schema changed) | 0 / 0 | 10 / 5 (schema.json, schema.sql, files object, commit object, ref) |
| `checkout <hash>`, `checkout main` | 0 / 0 | 6 / 3 (HEAD, WORKSPACE, `.needs-repair`) |
| `checkout -b x` | 0 / 0 | 8 / 3 (ref via `create_durable`, plus the three above) |
| `branch y` (create) | 0 / 0 | 2 / 0 (`link(2)`, not rename) |
| `branch -d y`, `branch --restore y` | 0 / 1 | 0 / 1 (unchanged; see "not covered") |
| `log`, `status`, `branch` | 0 / 0 | 0 / 0 |

With `GFS_FSYNC=off` the fixed binary's commit makes **0 fsync and 5 renames**.

## 2. Crash injection: SIGKILL at the worst moment

strace delivers `SIGKILL` on syscall entry, so the syscall itself never runs.
`-P <path>` restricts both tracing and injection to syscalls touching that path,
and injection counts are per syscall per thread, so `when=1` with `-P` hits the
first matching call. Every result below was read back from the strace log, which
shows the killed syscall as `= ?`.

| case | command | killed syscall (from the log) |
| --- | --- | --- |
| baseline, at the ref write | `strace -f -y -P <R>/.gfs/refs/heads/main -e trace=openat,write -e inject=write:signal=KILL:when=1 gfs commit …` | `write(12<…/refs/heads/main>, "53899b0b…", 64) = ?`, after `openat(… O_WRONLY\|O_CREAT\|O_TRUNC …)` |
| fixed, at the ref rename | `… -P <R>/.gfs/refs/heads/main -e trace=renameat,renameat2 -e inject=renameat,renameat2:signal=KILL:when=1 …` | `renameat(…, ".main.tmp.150831.4", …, "refs/heads/main") = ?` |
| fixed, at the ref temp's fsync | `… -e trace=fsync,renameat,renameat2 -e inject=fsync:signal=KILL:when=9 …` (fsync #9 of a commit is the ref temp's, per section 1) | `fsync(12<…/refs/heads/.main.tmp.150953.4>) = ?` |

What each left behind, and what the next commands did:

| | baseline, killed at the ref write | fixed, killed at the rename | fixed, killed at the temp's fsync |
| --- | --- | --- | --- |
| `refs/heads/main` | **empty, 0 bytes** | previous tip, 64 bytes | previous tip, 64 bytes |
| leftover | none | `refs/heads/.main.tmp.<pid>.4` | `refs/heads/.main.tmp.<pid>.4` |
| `gfs log -n 3` | **rc=1** `error: repository error: revision not found: ''` | rc=0, shows the previous commit | rc=0 |
| `gfs branch` | rc=0, `* main` with **no tip** | rc=0, `* main <prev> c1` | rc=0 |
| `gfs status` | rc=0, HEAD blank | rc=0 | rc=0 |
| next `gfs commit` | rc=0, but records **`parents: ['']`** | rc=0, parent = previous tip | rc=0, parent = previous tip |
| `gfs log` after that | **rc=1, still** `revision not found: ''` | rc=0 | rc=0 |
| leftover after the next commit | n/a | still there, ignored by every command | still there, ignored |

The baseline result is worse than an empty ref: the next commit succeeds and
writes a commit whose parent is the empty string, so the branch's earlier history
is cut off permanently and `gfs log` keeps failing. On the fixed binary the
interrupted commit simply did not happen; the repository is exactly as it was
before it, plus one ignored temp (and an unreachable commit object).

Not run separately: a kill at the temp's `openat`. Before that call nothing has
been written, so it is the trivially safe case; the two kills above bracket the
window in which the temp exists.

What this does **not** show: a power cut. A SIGKILL leaves the page cache intact,
so it proves atomicity against a process crash, not durability against power loss.
The durability claim rests on the `fsync` calls being present in the right order
(section 1), not on an observed power failure.

## 3. Concurrent readers under a writer (VM ext4)

`stress.py <repo> <gfs> 200`: 200 `gfs commit` subprocesses while one thread inserts
into the database continuously, one thread reads `refs/heads/main` in a tight loop,
and one thread runs `gfs log -n 1 --json` in a loop.

| | commits ok | ref reads | empty reads | short / other | `gfs log` runs | non-zero exit | JSON parse failures |
| --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | 200 | 255,499 | **222** | 0 / 0 | 715 | 0 | 0 |
| fixed | 200 | 577,740 | **0** | 0 / 0 | 1,396 | 0 | 0 |

A repeat on the `18c323d` binary gave 194 empty of 268,278 (baseline) and 0 of
591,138 (fixed). `gfs log` never hit the window in either: it reads the ref once,
early, and the truncate-to-write gap is a few microseconds.

## 4. Edge cases (VM, pass/fail)

| case | baseline | fixed |
| --- | --- | --- |
| `refs/heads` read-only (`chmod a-w`), then commit | commit **succeeds** (it rewrites the existing file in place) | commit **fails**, rc=1, `Permission denied (os error 13)`; ref unchanged, no temp left, `gfs log` works. A behaviour change: a ref directory the user cannot write to is now an error. The message reads "IO error while searching for repository", which is the existing wording of `RepoError::IoError` and misleading here |
| disk full for real: a 64 KB tmpfs (and, separately, a 2 MB one), filled with `dd` after the first commit | rc=1 at the snapshot copy (`/bin/cp: error copying …`), ref unchanged, `gfs log` works | same: rc=1 at the snapshot copy, ref unchanged, no temp, `gfs log` works |
| `ENOSPC` injected exactly at the ref write (baseline: `write` on the ref, `-P`; fixed: the ref temp's `fsync`, `when=9`) | rc=1, ref **empty (0 bytes)**, `gfs log` fails | rc=1, ref unchanged, temp removed, `gfs log` works |
| a temp from another pid already present (`.main.tmp.1.4`) | commit ok | commit ok, planted file untouched, ref a 64-hex tip |
| the exact names this process will use already present (pid pinned with `exec`, `.main.tmp.<pid>.0` … `.15` planted) | commit ok | commit ok via the first free name; all 16 planted files still hold their own bytes |
| leftover temps in `refs/heads/`, `refs/heads/team/`, `objects/<2>/` and `refs/deleted/<ms>/`; then `log -n 3`, `log --graph --all`, `branch`, `branch --deleted`, `status`, `log --from <prefix>`, `checkout <prefix>`, `checkout main` | all rc=0, but the temps **appear** as refs in 5 of 8 outputs | all rc=0, temps appear in **0** of 8 |
| `GFS_FSYNC=off` | n/a | commit ok, 0 fsync, 5 renames, log shows both commits |
| Windows | out of scope: not built, not run | |

A real full disk never reached the ref on either binary: the snapshot copy runs
first and fails first, which is why the exact-point `ENOSPC` row exists. The
pinned-pid case found a real defect in the first version of
`durable_write` (it deleted a pre-existing file with its temp's name after
`create_new` failed); `80d2691` fixes it, and this table is from the fixed code.

## 5. Benchmark

Method (`bench.py`): every sample is one `gfs` process, wall clock around
`subprocess.run`, so process start counts equally for every variant. Three variants
per case: **baseline**, **fixed** (fsync on, the default), and **fixed with
`GFS_FSYNC=off`** to separate the cost of the syscalls' existence from the cost of
the syncs. Variants are **alternated inside every iteration, with the order rotated
each iteration**; nothing is batched. 2 warmup + 15 measured iterations unless
stated; median and p95 (linear interpolation). Prep (database writes, pruning old
snapshots to keep disk flat) is not timed. hyperfine was installed on the Mac but
not used, because it batches all runs of one command before the next.

Machines, filesystems and runs:

- **VM ext4**: the VM's root filesystem (full-copy snapshot path).
- **VM tmpfs**: a 1.5 GB tmpfs mounted for the run at `/tmp/gfs-aw-tmpfs` and
  unmounted after; fsync is a no-op there, so this isolates CPU cost. Case 3 not run
  (memory).
- **Mac APFS**: internal SSD, clonefile snapshot path. On macOS Rust's `sync_all` is
  `F_FULLFSYNC`, a full device cache flush.
- The VM runs on the Mac, so the two were never run at the same time.

### Mac, APFS (rustc 1.93.1)

| case | baseline median / p95 ms | fixed median / p95 | fixed delta | `GFS_FSYNC=off` median / p95 | off delta |
| --- | --- | --- | --- | --- | --- |
| 1 first commit, fresh repo | 33.39 / 40.19 | 63.10 / 128.01 | +29.71 (+89%) | 34.65 / 68.12 | +1.26 |
| 2 commit, ~1 MB db | 33.64 / 43.89 | 67.25 / 77.34 | +33.61 (+100%) | 33.94 / 49.85 | +0.30 |
| 3 commit, ~200 MB db (5 runs) | 31.54 / 33.84 | 83.61 / 85.70 | +52.07 (+165%) | 33.06 / 34.18 | +1.52 |
| 5 checkout `<hash>` | 14.90 / 16.55 | 33.36 / 43.85 | +18.46 (+124%) | 15.75 / 16.55 | +0.85 |
| 5 checkout main | 14.51 / 16.10 | 32.29 / 37.60 | +17.78 (+123%) | 14.89 / 18.67 | +0.38 |
| 6 checkout -b | 13.23 / 14.58 | 36.41 / 40.01 | +23.18 (+175%) | 14.20 / 14.81 | +0.97 |
| 6 branch -d | 10.39 / 11.16 | 10.66 / 11.20 | +0.27 | 10.29 / 11.53 | -0.10 |
| 6 branch --restore | 8.20 / 8.81 | 8.20 / 8.81 | 0.00 | 8.19 / 9.01 | -0.01 |
| 7 log -n 5 --json | 8.16 / 9.04 | 8.02 / 8.58 | -0.14 | 7.65 / 8.35 | -0.51 |
| 7 log --graph --all | 25.24 / 27.51 | 24.98 / 26.13 | -0.26 | 25.08 / 26.62 | -0.16 |
| 7 status --json | 8.40 / 9.29 | 8.31 / 8.74 | -0.09 | 8.31 / 9.01 | -0.09 |
| 7 branch | 7.63 / 8.59 | 6.99 / 7.82 | -0.64 | 7.30 / 8.16 | -0.33 |
| 8 log --from `<4-char prefix>`, clean | 7.42 / 8.82 | 7.32 / 8.17 | -0.10 | 7.29 / 8.63 | -0.13 |
| 8 same, 20 planted temps | 7.98 / 8.73 | 7.79 / 8.41 | -0.19 | 7.62 / 8.67 | -0.36 |
| 8 branch, 20 planted temps | 7.84 / 9.07 | 7.00 / 7.89 | -0.84 | 7.40 / 8.29 | -0.44 |
| 9 commit, 5,000 extra files | 841.91 / 888.32 | 938.99 / 977.46 | +97.08 (+11.5%) | 844.88 / 877.52 | +2.97 |
| 9 checkout `<hash>`, 5,000 files | 1021.13 / 1087.12 | 1105.65 / 1143.57 | +84.52 (+8.3%) | 1026.24 / 1078.52 | +5.11 |
| 9 checkout main, 5,000 files | 1026.79 / 1078.72 | 1050.59 / 1092.39 | +23.80 (+2.3%) | 1021.64 / 1090.74 | -5.15 |
| 10 branch, 500 branches | 36.39 / 38.84 | 36.67 / 39.31 | +0.28 | 37.37 / 40.05 | +0.98 |
| 10 commit, 500 branches | 31.38 / 36.14 | 58.92 / 64.02 | +27.54 (+88%) | 32.42 / 35.09 | +1.04 |
| 11 checkout -b a/b/c/d/e`<i>` | 12.94 / 15.84 | 34.12 / 39.00 | +21.18 (+164%) | 13.23 / 14.48 | +0.29 |
| 11 commit on it | 30.44 / 35.55 | 57.39 / 60.87 | +26.95 (+89%) | 31.61 / 33.70 | +1.17 |
| 11 branch -d | 10.42 / 11.33 | 10.34 / 11.65 | -0.08 | 10.32 / 10.90 | -0.10 |
| 11 branch --restore | 8.41 / 9.22 | 8.15 / 10.05 | -0.26 | 8.18 / 9.80 | -0.23 |

Case 4, 200 sequential commits: baseline 6.27 s (median 31.0 ms, first-20 median
31.46, last-20 30.94); fixed 12.93 s (+106%, median 65.07, first-20 65.79, last-20
66.16); `GFS_FSYNC=off` 6.40 s (+2.1%).

### VM, ext4

| case | baseline median / p95 ms | fixed median / p95 | fixed delta | `GFS_FSYNC=off` median / p95 | off delta |
| --- | --- | --- | --- | --- | --- |
| 1 first commit, fresh repo | 6.82 / 11.82 | 16.68 / 23.20 | +9.86 | 8.04 / 16.68 | +1.22 |
| 2 commit, ~1 MB db | 8.34 / 11.50 | 18.09 / 20.02 | +9.75 | 9.66 / 12.69 | +1.32 |
| 3 commit, ~200 MB db (5 runs) | 78.74 / 80.16 | 211.17 / 248.61 | +132.43 | 53.81 / 63.41 | -24.93 |
| 5 checkout `<hash>` | 5.77 / 8.07 | 12.09 / 16.34 | +6.32 | 6.17 / 7.40 | +0.40 |
| 5 checkout main | 5.18 / 6.81 | 11.68 / 13.85 | +6.50 | 5.99 / 8.33 | +0.81 |
| 6 checkout -b | 4.92 / 5.78 | 14.05 / 15.83 | +9.13 | 5.19 / 5.96 | +0.27 |
| 6 branch -d | 3.07 / 3.76 | 3.47 / 4.30 | +0.40 | 3.16 / 4.29 | +0.09 |
| 6 branch --restore | 2.46 / 3.18 | 2.81 / 3.14 | +0.35 | 2.66 / 3.00 | +0.20 |
| 7 log -n 5 --json | 2.60 / 3.00 | 3.00 / 3.82 | +0.40 | 2.73 / 3.68 | +0.13 |
| 7 log --graph --all | 11.33 / 13.01 | 12.13 / 13.23 | +0.80 | 11.75 / 13.27 | +0.42 |
| 7 status --json | 2.60 / 3.27 | 2.78 / 3.20 | +0.18 | 2.72 / 3.38 | +0.12 |
| 7 branch | 2.18 / 3.10 | 2.26 / 2.63 | +0.08 | 2.20 / 2.95 | +0.02 |
| 8 log --from prefix, clean | 2.30 / 3.17 | 2.29 / 2.80 | -0.01 | 2.28 / 2.93 | -0.02 |
| 8 same, 20 planted temps | 2.72 / 4.36 | 3.20 / 4.10 | +0.48 | 2.73 / 3.70 | +0.01 |
| 8 branch, 20 planted temps | 2.53 / 3.74 | 2.57 / 3.76 | +0.04 | 2.45 / 5.27 | -0.08 |
| 9 commit, 5,000 extra files | 129.99 / 143.89 | 142.26 / 158.45 | +12.27 (+9.4%) | 131.12 / 137.71 | +1.13 |
| 9 checkout `<hash>`, 5,000 files | 314.24 / 352.59 | 350.54 / 394.03 | +36.30 (+11.6%) | 311.89 / 389.12 | -2.35 |
| 9 checkout main, 5,000 files | 309.18 / 335.32 | 343.03 / 403.66 | +33.85 (+10.9%) | 312.33 / 361.57 | +3.15 |
| 10 branch, 500 branches | 20.33 / 34.41 | 21.67 / 34.26 | +1.34 | 22.64 / 34.35 | +2.31 |
| 10 commit, 500 branches | 8.63 / 13.38 | 20.03 / 31.26 | +11.40 | 10.10 / 18.25 | +1.47 |
| 11 checkout -b a/b/c/d/e`<i>` | 6.98 / 11.01 | 16.51 / 32.24 | +9.53 | 6.99 / 13.08 | +0.01 |
| 11 commit on it | 8.88 / 17.74 | 20.41 / 39.52 | +11.53 | 10.67 / 17.91 | +1.79 |
| 11 branch -d | 3.47 / 8.59 | 5.35 / 8.38 | +1.88 | 4.30 / 8.47 | +0.83 |
| 11 branch --restore | 3.15 / 6.88 | 3.68 / 7.20 | +0.53 | 3.38 / 7.33 | +0.23 |

Case 4: baseline 1.55 s (median 7.56 ms, first-20 8.30, last-20 7.29); fixed 3.83 s
(+147%, median 18.65, first-20 19.44, last-20 18.83); `GFS_FSYNC=off` 1.70 s.

### VM, tmpfs (no-op fsync: the CPU cost alone)

Every fixed-vs-baseline delta is within the run's noise, which this run puts at
about ±0.5 ms on 2-3 ms commands (the read-only commands in case 7 moved by up to
+0.6 ms with no code path changed for them). Commit +1.3 to +2.2 ms, but
`GFS_FSYNC=off` moved by the same amount (+1.4 to +2.4 ms); checkout +0.4 to +0.5 ms;
5,000-file commit +0.5 ms; 200 commits 1.64 s baseline, 1.77 s fixed, 1.83 s off. The full table
is in the run's JSON and is not repeated here.

### Reading the numbers

- **The cost is the syncs, nothing else.** With `GFS_FSYNC=off` the fixed binary is
  within noise of the baseline on every case on every machine; the temp, the rename
  and the walker filters cost nothing measurable.
- **Each fsync costs about 1 ms on the VM's ext4 and about 3 ms on the Mac**
  (`F_FULLFSYNC`); a durable write makes two. A commit makes 10 fsyncs: **+10 ms on the VM (8 → 18 ms) and +27
  to +34 ms on the Mac (31 → 58-67 ms)**, roughly doubling a commit of a small
  database. Checkout pays 6 fsyncs (+6 ms VM, +18 ms Mac), `checkout -b` 8.
- **Where the copy dominates, it is a smaller fraction**: +9 to +12% with 5,000 extra
  files. The 200 MB case did not behave as "noise": on ext4 the fixed commit took
  211 ms against 79 ms. The likely mechanism is that the ref's fsync forces an ext4
  journal commit, which in `data=ordered` mode also writes back the 200 MB snapshot
  just copied into the page cache; this is a hypothesis, not measured. An earlier run
  of the same case on the `18c323d` binary gave medians of 71 vs 73 ms with a fixed
  p95 of 192 ms, so this case is also highly variable (5 runs).
- **Readers and walkers: no regression.** `log`, `status`, `branch` and prefix
  resolution are unchanged within noise with 200 commits, with 500 branches, and
  with 20 planted temps. The planted temps showed up in the baseline's output in
  every run (34 of 34 samples) and in the fixed binary's in none.
- **No growth trend** over 200 commits: first-20 and last-20 medians match on every
  machine.
- A first full VM run on the `18c323d` binary agreed in shape (commit +7.8 ms, checkout
  +5.1 to +5.3 ms, readers within ±0.4 ms); the difference between the two runs is
  host noise, since the code paths measured did not change.

What would reduce the cost, not done here: skip the per-object directory fsync and
issue one directory sync per shard before the ref moves (objects are unreachable
until then), which would take a commit from 10 fsyncs to about 6; or use plain
`fsync` instead of `F_FULLFSYNC` on macOS, trading power-loss durability on that
platform for speed, as git's default does.

## 6. Unit tests, each seen failing first (Mac, rustc 1.93.1)

| test | sabotage | failure seen | restored |
| --- | --- | --- | --- |
| `durable_write::a_concurrent_reader_never_sees_a_partial_file` | `write_durable` replaced by `std::fs::write` | `read a partial file: 0 bytes` | sha256 equal, passes |
| `repo_layout::a_concurrent_reader_never_sees_a_torn_branch_ref` | `update_branch_ref` back to `fs::write` | `read a torn branch ref after 2 reads: ""` (3 runs of 3) | sha256 equal, passes |
| six walker tests (`list_branches`, `get_refs_pointing_to`, `list_deleted_branch_refs`, `find_commits_by_prefix`, `is_commit`, CLI `list_branch_tips`) and `a_segment_starting_with_a_dot_is_refused` | the filters removed | all 7 fail (e.g. CLI: `left: [("team/.alpha.tmp.9.1", …), (".main.tmp.123.0", ""), ("main", …)]`) | sha256 equal, pass |
| `durable_write::a_taken_temp_name_is_skipped_and_never_removed` | old behaviour (remove the taken name, return the error) | `File exists (os error 17)` | sha256 equal, passes |

Gates at `80d2691`: `cargo fmt --all -- --check` clean; `cargo clippy -p gfs-domain
-p gfs-cli --all-targets -- -D warnings` clean; `cargo test -p gfs-domain` 337
passed (doctests 0 passed, 2 ignored); `cargo test -p gfs-cli --lib` 20 passed;
`--test e2e_sqlite` 14, `--test e2e_init` 1, `--test json_output` 10, all passed.

## 7. Not covered, by design or not yet

- **Power loss** was not simulated (see section 2).
- **Directory creation** (`objects/<2>/`, nested `refs/heads/a/b/`) is not fsynced in
  its parent; a power cut can lose a freshly created shard directory. ext4's single
  journal makes this unlikely in practice; not verified.
- **`branch -d` and `--restore`** move refs with `rename` and no directory fsync,
  unchanged here; they belong with the compare-and-swap ref work.
- **Snapshot data** is not fsynced; a power cut can leave a snapshot whose ref and
  commit object are durable but whose files are not. Out of scope; the 200 MB case
  above suggests where that cost would land.
- `init`'s `config.toml` and `new` marker, `.gfs/commit.lock`, and every write outside
  `.gfs` are unchanged.
- Windows: not built, not run.

## 8. rerere pre-training for the later merge

On a scratch branch cut from `feat/env-record-and-resolution` (`6d06306`), the
hardening and routing commits were cherry-picked. Three conflict sets came up:

- `repo_utils/mod.rs` (both sides add module lines): resolved, recorded.
- `repo_layout.rs`, 6 hunks, every one the expected shape: the branch changed the
  PATH to `RepoPaths`, `main` changed the WRITE to `write_durable`. Resolved by that
  rule, recorded.
- `gfs_repository.rs`, 3 hunks: the import (trivial), but also `checkout`, where the
  one-line marker change sits inside a region `main` rewrote for unrelated reasons,
  and `create_branch`, where the branch predates `main`'s no-clobber fix. That is the
  general divergence, not this work, so it was **not** resolved and **not** recorded.

Result: **2 resolutions recorded** (`mod.rs`, `repo_layout.rs`), 1 preimage left
without a resolution (`gfs_repository.rs`). A recorded resolution replays only when
the conflict text matches exactly, and the real merge brings 122 other commits of
divergence, so check every replayed hunk. The scratch worktree and branch were
removed; `feat/env-record-and-resolution` was `6d06306` before and after.

## Cleanup

The VM build directory `/home/ubuntu/gfs-atomic-writes/` (clone, both target
directories, the toolchain and every test repository) was removed after these runs,
and the tmpfs mounts were unmounted. Nothing else on the VM was touched.
