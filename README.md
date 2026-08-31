# local-history

local-history keeps event-driven snapshots of Git worktrees without touching
their branches, index, or object database. Watchman reports changed paths and a
small Rust helper commits the resulting tree to a private bare Git repository.

There are no editor hooks. Changes made by Neovim, Codex, formatters, scripts,
or other editors follow the same Watchman path, including while Neovim is
closed.

## Requirements

- Git 2.25 or newer
- [Watchman](https://facebook.github.io/watchman/docs/install)
- Linux, macOS, or Windows 10 64-bit and newer

Watchman uses inotify on Linux, FSEvents on macOS, and native filesystem
notifications on Windows.

## Install

~~~sh
cargo install --path . --root ~/.local
local-history init
~~~

Edit the generated config, inspect the result, and perform the first
registration:

~~~sh
local-history discover
local-history daemon --once
~~~

Then install the service appropriate for the platform from [contrib](contrib).

## Configuration

The generated configuration is:

~~~json
{
  "discovery_roots": [],
  "repositories": [],
  "exclude_repositories": [],
  "ignore_directories": [
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".direnv",
    ".cache",
    "dist",
    "build",
    "target",
    "coverage",
    ".next",
    ".turbo",
    ".gradle",
    "__pycache__"
  ],
  "store_dir": null,
  "max_file_size_mb": 10,
  "discovery_max_depth": 4,
  "discovery_interval_seconds": 60
}
~~~

"discovery_roots" are parent directories such as ~/Projects. Repositories
beneath them are found automatically up to "discovery_max_depth". Once a Git
worktree is found, discovery does not crawl through it looking for nested
repositories.

"repositories" contains explicit worktrees. It is always additive. When
"discovery_roots" is empty, only "repositories" is used.

For example:

~~~json
{
  "discovery_roots": ["~/Projects"],
  "repositories": ["~/dotfiles"],
  "exclude_repositories": ["~/Projects/archive/**"]
}
~~~

Missing properties receive their documented defaults, so a small config is
valid. Repository exclusions accept glob patterns after ~ expansion.

### Platform paths

"store_dir": null selects the platform-local user data directory:

| Platform | Config | Snapshot data | Runtime state |
| --- | --- | --- | --- |
| Linux | $XDG_CONFIG_HOME/local-history/config.json | $XDG_DATA_HOME/local-history | $XDG_STATE_HOME/local-history |
| Linux fallback | ~/.config/local-history/config.json | ~/.local/share/local-history | ~/.local/state/local-history |
| macOS | ~/Library/Application Support/local-history/config.json | ~/Library/Application Support/local-history | snapshot data under state/ |
| Windows | %APPDATA%\local-history\config.json | %LOCALAPPDATA%\local-history | snapshot data under state\ |

Run "local-history paths" to see the resolved paths. Snapshot databases are
user data; trigger registrations and other reconstructible bookkeeping use the
state directory.

Each worktree receives:

~~~text
<data>/repos/<path-hash>/
├── history.git/       private bare repository
├── index              private staging index
├── lock
└── project.json
~~~

## Ignore behavior

Three filters apply before content enters history:

1. Watchman trigger expressions reject "ignore_directories" at any depth.
2. The source repository's .gitignore, .git/info/exclude, and global Git
   excludes reject untracked files. Tracked files remain eligible.
3. "max_file_size_mb" rejects large regular files.

The default list avoids dependency, build, cache, and VCS traffic. If one of
those directories was previously captured and later becomes ignored, the next
event removes it from the private history tree.

Watchman trigger expressions prevent snapshot work but do not prevent Watchman
from maintaining its own directory watch. For maximum performance, especially
on Linux, projects can additionally place an
[ignore_dirs](https://facebook.github.io/watchman/docs/config#ignore_dirs)
list in .watchmanconfig:

~~~json
{
  "settle": 20,
  "ignore_dirs": ["node_modules", "dist", "build", "target", ".cache"]
}
~~~

Watchman applies these top-level exclusions at the OS notification layer on
Linux. local-history never creates or modifies .watchmanconfig, because doing
so would dirty source repositories.

## How snapshots are written

Watchman saved triggers batch settled filesystem events and pass JSON path
records over stdin. One trigger process runs at a time per worktree. While a
capture is active, Watchman retains later changes and invokes it again when the
first process exits.

The capture process:

1. locks that worktree's history store;
2. stages only changed/deleted paths in the private index;
3. writes a Git tree;
4. skips the commit when the tree is unchanged;
5. creates a commit with git commit-tree; and
6. atomically advances refs/heads/history.

Git immediately deduplicates identical content. Normal Git packing can
delta-compress similar versions. Large compressed binaries should remain
excluded.

All snapshot commits are currently retained. Automatic Git maintenance packs
objects but does not delete reachable history. Removing a worktree from the
configuration stops future captures without deleting its existing data.

## CLI

~~~sh
# Full snapshot now
local-history capture ~/Projects/example

# Machine-readable history affecting one file
local-history list --file ~/Projects/example/src/main.rs --json

# Recover a selected version
local-history show \
  --file ~/Projects/example/src/main.rs \
  --revision <commit>

# Verify platform locations
local-history paths
~~~

Deleting a repository does not delete its private history database. Removing a
repository from configuration makes the daemon remove its Watchman trigger,
while retaining its stored snapshots for manual recovery.

## Service examples

### Linux/systemd

~~~sh
install -Dm644 contrib/systemd/local-history.service \
  ~/.config/systemd/user/local-history.service
systemctl --user daemon-reload
systemctl --user enable --now local-history.service
~~~

### macOS/launchd

Replace __LOCAL_HISTORY_BIN__ in the supplied plist with the absolute binary
path, copy it to ~/Library/LaunchAgents/, then bootstrap it:

~~~sh
launchctl bootstrap gui/"$(id -u)" \
  ~/Library/LaunchAgents/io.github.akisArou.local-history.plist
~~~

### Windows

Run [contrib/windows/install-task.ps1](contrib/windows/install-task.ps1) in
PowerShell after local-history.exe and watchman.exe are available on PATH. It
creates a per-user task that starts the daemon at logon.

## Security and backup

History may contain uncommitted credentials or deleted text. Data directories
are created for the current user, but their protection ultimately follows the
parent directory and operating-system account permissions.

This is local history, not an off-machine backup. Back up the data directory
separately if it must survive disk loss.
