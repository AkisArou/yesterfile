# yesterfile

AI coding agents, formatters, generators, and scripts can rewrite a project
while its editor is closed. Editor undo history cannot see those changes, and
normal Git only preserves the states you deliberately commit. Yesterfile fills
that recovery gap with an always-on, local timeline of settled filesystem
changes.

Yesterfile keeps event-driven snapshots of Git worktrees without touching their
branches, index, or object database. Watchman reports changed paths and a small
Rust helper commits each resulting state to a private bare Git repository.
Rapid write bursts may be coalesced into one settled snapshot, which avoids
half-written states and process-per-event overhead.

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
cargo install --path .
yesterfile init
~~~

Edit the generated config, inspect the result, and perform the first
registration:

~~~sh
yesterfile discover
yesterfile daemon --once
~~~

Then install the service appropriate for the platform from [contrib](contrib).

## Configuration

The generated configuration is:

~~~json
{
  "$schema": "https://raw.githubusercontent.com/AkisArou/yesterfile/main/yesterfile.schema.json",
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

The "$schema" property gives JSON-aware editors validation, completion, and
hover documentation from [yesterfile.schema.json](yesterfile.schema.json).

An optimized real-world config normally omits inherited defaults:

~~~json
{
  "$schema": "https://raw.githubusercontent.com/AkisArou/yesterfile/main/yesterfile.schema.json",
  "discovery_roots": ["~/Projects"],
  "repositories": [
    "~/dotfiles",
    {
      "path": "~/Projects/project-that-keeps-build",
      "ignore_directories": {
        "remove": ["build"],
        "add": ["generated"]
      },
      "max_file_size_mb": 25
    }
  ],
  "exclude_repositories": ["~/Projects/archive/**"]
}
~~~

Missing properties receive their documented defaults, so a small config is
valid. A repository object inherits global settings: "add" and "remove"
modify the global directory ignore list for that worktree, and
"max_file_size_mb" replaces the global size limit. The remove list does not
override that repository's own Git ignore rules. Repository exclusions accept
glob patterns after ~ expansion.

### Platform paths

"store_dir": null selects the platform-local user data directory:

| Platform | Config | Snapshot data | Runtime state |
| --- | --- | --- | --- |
| Linux | $XDG_CONFIG_HOME/yesterfile/config.json | $XDG_DATA_HOME/yesterfile | $XDG_STATE_HOME/yesterfile |
| Linux fallback | ~/.config/yesterfile/config.json | ~/.local/share/yesterfile | ~/.local/state/yesterfile |
| macOS | ~/Library/Application Support/yesterfile/config.json | ~/Library/Application Support/yesterfile | snapshot data under state/ |
| Windows | %APPDATA%\yesterfile\config.json | %LOCALAPPDATA%\yesterfile | snapshot data under state\ |

Run "yesterfile paths" to see the resolved paths. Snapshot databases are
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
Linux. yesterfile never creates or modifies .watchmanconfig, because doing
so would dirty source repositories.

Yesterfile's config intentionally does not proxy arbitrary Watchman root
settings. Those settings affect every Watchman client sharing the root, are
loaded only when the watch is established, and require removing and re-adding
the watch after a change. Keep them explicit and repository-owned in
.watchmanconfig; Yesterfile generates only its own saved-trigger query.

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
yesterfile capture ~/Projects/example

# Machine-readable history affecting one file
yesterfile list --file ~/Projects/example/src/main.rs --json

# Recover a selected version
yesterfile show \
  --file ~/Projects/example/src/main.rs \
  --revision <commit>

# Verify platform locations
yesterfile paths
~~~

Deleting a repository does not delete its private history database. Removing a
repository from configuration makes the daemon remove its Watchman trigger,
while retaining its stored snapshots for manual recovery.

## Service examples

### Linux/systemd

~~~sh
install -Dm644 contrib/systemd/yesterfile.service \
  ~/.config/systemd/user/yesterfile.service
systemctl --user daemon-reload
systemctl --user enable --now yesterfile.service
~~~

### macOS/launchd

Replace __YESTERFILE_BIN__ in the supplied plist with the absolute binary
path, copy it to ~/Library/LaunchAgents/, then bootstrap it:

~~~sh
launchctl bootstrap gui/"$(id -u)" \
  ~/Library/LaunchAgents/io.github.akisArou.yesterfile.plist
~~~

### Windows

Run [contrib/windows/install-task.ps1](contrib/windows/install-task.ps1) in
PowerShell after yesterfile.exe and watchman.exe are available on PATH. It
creates a per-user task that starts the daemon at logon.

## Security and backup

History may contain uncommitted credentials or deleted text. Data directories
are created for the current user, but their protection ultimately follows the
parent directory and operating-system account permissions.

This is local history, not an off-machine backup. Back up the data directory
separately if it must survive disk loss.
