# RFC 002: Config (CLI)

## Overview

The **config** command lets you read and write configuration, similar to `git config`. The main use is configuring **user.name** and **user.email**, which are used as the default author and author email for every commit (see [002-cli-commit](002-cli-commit.md)).

GFS supports two config scopes:

| Scope | File | Flag |
|-------|------|------|
| **Repository** | `.gfs/config.toml` (inside the repo) | *(default)* |
| **Global** | `~/.gfs/config.toml` (in the user's home dir) | `--global` / `-g` |

This RFC defines the **command interface**, **behaviour**, and **storage** of `gfs config`.

---

## Command interface

### Get

```
gfs config user.name
gfs config user.email

gfs config --global user.name
gfs config --global user.email
```

### Set

```
gfs config user.name "<name>"
gfs config user.email "<email>"

gfs config --global user.name "<name>"
gfs config --global user.email "<email>"
```

### Full syntax

```
gfs config [--global | -g] [--path <dir>] <key> [<value>]
```

| Flag | Required | Description |
|------|----------|-------------|
| `--global`, `-g` | no | Operate on the global `~/.gfs/config.toml` instead of the repo-local `.gfs/config.toml`. |
| `--path` | no | Path to the GFS repository root (directory that contains `.gfs/`). Defaults to the current working directory. Ignored when `--global` is set. |
| `key` | **yes** | Configuration key. Supported: `user.name`, `user.email`. |
| `value` | for set | Value to write. Omit to read. |

Semantics match git: one argument after the key means **get**; two arguments (key + value) means **set**.

---

## Behaviour

### Repository-local (default)

- **Get**: Read the value for the given key from `.gfs/config.toml`. If the key or the `[user]` section is missing, print nothing and exit 0. Otherwise print the value to stdout (single line, no extra output).
- **Set**: Ensure `.gfs/` and `config.toml` exist (init if needed), then update or create the `[user]` section with the given key/value. Other keys in `config.toml` (e.g. `mount_point`, `environment`, `runtime`) are preserved. Write the updated TOML back to `.gfs/config.toml`.

### Global (`--global`)

- **Get**: Read the value from `~/.gfs/config.toml`. If the file or key is missing, print nothing and exit 0.
- **Set**: Create `~/.gfs/` and `~/.gfs/config.toml` if they do not exist, then update or create the `[user]` section. Write the updated TOML back.

Only `user.name` and `user.email` are supported. Other keys are out of scope and may be rejected.

---

## Storage

### Repository config: `.gfs/config.toml`

Stored at the repository root. User identity is under a `[user]` section:

```toml
[user]
name = "Alice"
email = "alice@example.com"
```

This file also contains other sections (e.g. `mount_point`, `environment`, `runtime`). The config command only touches the `[user]` section.

### Global config: `~/.gfs/config.toml`

Stored in the user's home directory. Same `[user]` format:

```toml
[user]
name = "Alice"
email = "alice@example.com"
```

The file is created on first `gfs config --global … <value>` invocation.

---

## Integration with commit

Each commit records **author** and **author email** (see [002-cli-commit](002-cli-commit.md)). Those values are resolved in this order:

1. CLI flags `--author` / `--author-email` if present.
2. `user.name` / `user.email` from `.gfs/config.toml` (repo-local).
3. `user.name` / `user.email` from `~/.gfs/config.toml` (global).
4. `user.name` / `user.email` from `git config` (local → global → system).
5. For name only: fallback to `"user"` if still unset; email remains unset.

---

## Output

- **Get (value present)**: Print the value, no trailing newline required (one line to stdout).
- **Get (value or key missing)**: Print nothing; exit 0.
- **Set**: No output on success; exit 0.
- **Error**: Message to stderr, non-zero exit (e.g. not a repo for local get; invalid key; write failure).

---

## Examples

```sh
# Set global identity (applies to all repos, like git config --global)
gfs config --global user.name "Alice"
gfs config --global user.email "alice@example.com"

# Set repo-local identity (overrides global for this repo)
gfs config user.name "Alice"
gfs config user.email "alice@example.com"

# Read back
gfs config user.name    # → Alice
gfs config user.email   # → alice@example.com

gfs config --global user.name   # → Alice (from ~/.gfs/config.toml)

# In a specific repo
gfs config --path /data/my-repo user.email "bob@example.com"
```

---

## Domain / adapters

- **Reading repo-local**: The repository port exposes `get_user_config(repo) -> Option<UserConfig>`. The CLI loads `GfsConfig` from `.gfs/config.toml` and reads `config.user`.
- **Reading global**: `GlobalSettings::load()` reads `~/.gfs/config.toml` directly.
- **Reading git fallback**: `repo_layout::get_git_user_config()` shells out to `git config user.name` / `git config user.email`.
- **Writing**: The CLI loads the relevant config struct, updates or creates the `[user]` section, and saves it. No new port is required.

---

## Out of scope

- Other config keys (e.g. `core.*`, `runtime.*`) may be added in a later RFC.
- Removing a key (e.g. `gfs config --unset user.email`) is not required for this RFC.
