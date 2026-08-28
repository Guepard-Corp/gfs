# RFC 003: Storage

This RFC describes storage backends for GFS: volume and directory-level operations (mount, snapshot, clone, quota) and how they map to different filesystems.

## Filesystem comparison

| Feature        | APFS (macOS)     | Btrfs (Linux)     | ZFS (Linux/BSD)   |
|----------------|------------------|-------------------|-------------------|
| **COW**        | Yes              | Yes               | Yes               |
| **Snapshot**   | Yes (volume)     | Yes (subvolume)   | Yes (dataset)     |
| **Clone**      | Yes (clonefile)  | Yes (reflink)     | Yes (clone from snapshot) |
| **Dedup**      | No               | Yes (optional)    | Yes (optional, RAM-heavy) |
| **Compression**| Yes (transparent)| Yes (lzo/zstd/…)  | Yes (lz4/gzip/…)  |

- **APFS:** Clone via `cp -c` / clonefile(2); no built-in block-level dedup.
- **Btrfs:** Snapshot via `btrfs subvolume snapshot`; clone via `cp --reflink`; dedup via external tools (e.g. duperemove) or btrfs dedup.
- **ZFS:** Snapshot via `zfs snapshot`; clone via `zfs clone` (from a snapshot); dedup is optional and can require large amounts of RAM.

---

## Current implementation: APFS adapter

The storage adapter for macOS uses **APFS** and relies on **copy-on-write (COW)** for snapshots and clones so that large directory trees are duplicated instantly without doubling disk usage.

### How it works

- **Mechanism:** Directory-level `snapshot` and `clone` operations run `cp -cRp` (copy with `-c` = clone, `-R` = recursive, `-p` = preserve attributes). On APFS, `cp -c` uses the **clonefile(2)** syscall under the hood.
- **Effect:** The destination is a **clone** of the source: same logical content and size, but physical blocks are shared. No extra space is used until either the original or the clone is modified; then only the changed blocks are allocated for the modified copy.
- **Where it’s used in .gfs:** Commit creates a snapshot by COW-copying the workspace data directory (e.g. `.gfs/workspaces/main/0/data`) to a path under `.gfs/snapshots/<2>/<62>/`. Clone can copy from that snapshot (or from another source) to a new target path.

### Practical notes

- **Size reporting:** Tools like `du` and Finder report **logical** size (file lengths). A cloned snapshot will therefore “look” like a full copy (e.g. 62 MB). The real disk impact is visible only at the volume level (e.g. `df`); after a snapshot, volume “used” does not increase by the snapshot’s logical size.
- **Deleting the original:** If the source directory is removed, the snapshot (or any other clone) still references the shared blocks, so the data remains and space is not freed until every reference is gone.

---

## Immutable snapshots: single-file bundle

Today a snapshot is a **directory tree** under `.gfs/snapshots/<2>/<62>/`. It is a normal, writable directory: anyone with filesystem access can change or delete files inside it. To make a snapshot **immutable**, it can be stored as a **single file** that is then made read-only at the filesystem level. The bundle format must be **universal**: the same technology and file format on macOS, Linux, and any supported platform, so that snapshots can be created, stored, and restored consistently everywhere.

### Bundle format comparison

| Approach | Universal? | Description | Immutability |
|----------|------------|-------------|--------------|
| **Tar (optionally compressed)** | Yes | POSIX tar; same `tar` on macOS and Linux. Produce one `.tar` or `.tar.gz` / `.tar.zst`, then `chmod 444`. | Read-only file; contents unchanged until extracted. |
| **Zip** | Yes | Single `.zip`; `zip`/`unzip` available on all platforms. `chmod 444` the file. | Same as tar. |
| **Read-only disk image** | No | macOS: `hdiutil` + `.dmg`. Linux: different tools/formats. Not one format across OSes. | Contents immutable by format where supported. |
| **ZFS / Btrfs** | N/A | Native snapshots are immutable by design; no bundle file. | Native read-only snapshot. |

### Recommended: universal tar bundle

Use **tar** as the single-file bundle format so that snapshot creation and consumption use the same commands and format on every platform.

1. Create the snapshot directory as today (COW copy of `workspaces/.../data` to `snapshots/<2>/<62>/`).
2. Create a tar bundle from it (from the parent of the snapshot dir so paths inside the archive are stable), e.g.  
   `tar -cf .gfs/snapshots/82/4ca61....tar -C .gfs/snapshots/82 4ca61...`  
   Optional compression (same on all platforms): `tar -cf - -C .gfs/snapshots/82 4ca61... | gzip -n > .gfs/snapshots/82/4ca61....tar.gz`
3. Make the bundle read-only: `chmod 444 .gfs/snapshots/82/4ca61....tar[.gz]`
4. Optionally remove the writable directory and keep only the bundle so the only representation of the snapshot is the immutable file.

To use the snapshot (clone or restore): extract the bundle to a temporary or target directory, then copy from there (or, if the platform supports COW, copy from the extracted tree). Same commands on macOS and Linux: `tar -xf snapshot.tar -C <dest>` (or `gunzip -c snapshot.tar.gz | tar -xf - -C <dest>`).

### Trade-offs

- **Space:** The bundle is a serialized copy (no COW inside the file). Compression (gzip/zstd) reduces size at the cost of CPU. Keeping both the directory and the bundle doubles space; keeping only the bundle avoids a writable directory but requires extract before clone/restore.
- **Speed:** Creating the tar is I/O-bound; compression adds CPU. Extract is needed to use the snapshot.
- **Portability:** A single `.tar` or `.tar.gz` is universal and easy to copy, backup, or move across platforms.
