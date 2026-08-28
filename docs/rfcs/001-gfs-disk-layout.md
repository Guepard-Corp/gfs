## GFS Repository Layout

```
~/.gfs/
├─ config.toml            ← TOML file containing mount_point, user information, and environment information
├─ HEAD
│    (e.g. "refs/heads/master")
├─ refs/
│    └─ heads/
│        ├─ master        ← "<commit-hash1>", example: 8d46fa9c76820d12c277de6e44a4936b7dba7cffbe2c04af4c255b88156ebc27
│        └─ develop       ← "<commit-hash2>"
├─ objects/
│    ├─ ab/
│    │    └─ cdef1234…    ← any object whose hash begins "ab…"
│    │       (JSON: commit object — message, snapshot_hash, parents, author/committer, files_ref, etc.)
│    ├─ 8d/
│    │    └─ 46fa9c76…    ← example commit hash of master
│    │       (JSON: same commit schema; hash is filename, not stored inside)
│    ├─ 3f/
│    │    └─ 45…          ← etc.
│    └─ …                (flat shard by first two hex chars)
├─ workspaces/
│    └─ <branch>/         ← or "detached" when HEAD is a direct commit hash
│        └─ <short_commit_id>/   ← prefix of full hash (e.g. 12 chars) to keep paths short on disk; refs/HEAD store full hash
│            └─ data/     ← database and working data for this branch at this commit; container mounts here
├─ snapshots/
│    ├─ xx/              ← shard by first two hex chars of snapshot hash
│    │    └─ <snapshot_hash>  ← immutable COW folder via storage impl. Empty on init.
│    └─ …
└─ …
```

### Object format (commit objects)

Objects under `objects/<2-char>/<rest-of-hash>` are **commit objects** only. Each file is JSON with the commit schema:

- **Identity**: `hash` (optional in JSON), `message`, `timestamp`, `author`, `author_email`, `committer`, `committer_email`, `author_date`, `committer_date`
- **DAG**: `parents` (array of commit hashes)
- **Snapshot**: `snapshot_hash` (points to directory under `snapshots/<2-char>/<rest>`)
- **Metadata**: `schema`, `database_provider`, `database_version`
- **Stats**: `files_added`, `files_deleted`, `files_modified`, `files_renamed`, `blocks_*`, `db_objects_*`
- **Files reference**: `files_ref` — 64-char hex hash pointing to a separate object (see below)

**Current behaviour**: When committing, the implementation compares the current snapshot directory to the parent snapshot (if any) via a recursive file walk. It sets `files_added`, `files_deleted`, `files_modified` and writes the file list to a **binary object** under `objects/<2>/<62>`, then stores its content hash in `files_ref`. If the current snapshot path does not exist at commit time, `files_ref` remains unset.

### Files list object (binary)

The `files_ref` field in a commit points to an object stored under `objects/<2>/<62>` (same layout as commit objects). The content is **binary** (bincode-serialized `Vec<FileEntry>`), content-addressed by SHA-256 of that blob. Each `FileEntry` has:

| Field | Description |
|-------|-------------|
| `relative_path` | Path relative to the workspace `data/` folder |
| `file_size` | Size in bytes |
| `owner` | Owner (e.g. uid or username) |
| `group` | Group (e.g. gid or group name) |
| `permissions` | File mode / permissions (e.g. octal or string) |
| `file_attributes` | Optional platform-specific attributes (e.g. extended attributes, flags) |

To resolve the file list for a commit, read the object at `objects/<2>/<62>` (where the path is derived from `files_ref`) and deserialize with bincode.
