# 0002 · Kito on Windows

Status: accepted
Date: 2026-11-20

## Context

Kito runs on Linux and macOS; on Windows only through WSL. People who work
in Windows itself (editors, agents and terminals that are Windows
programs, repositories under `C:\`) cannot use it: a repository under
`/mnt/c` is slow in WSL and its file events are unreliable.

A build for Windows (`cargo check --target x86_64-pc-windows-gnu`) fails
in 46 places, in `kitod` above all. They come down to:

1. **The daemon's socket** is a Unix socket (`tokio::net::UnixListener`,
   `std::os::unix::net::UnixStream` in the client and in `kito-mcp`).
2. **What a file is on disk** (`FileStat`): device, inode, mtime in
   seconds and nanoseconds, and the mode, read through
   `std::os::unix::fs::MetadataExt`.
3. **Permissions**: the executable bit when a file is written or captured;
   `0600` on keys and the socket's key file.
4. **Processes**: commands in a process group of their own, stopped with
   it (`process_group`, `kill_process_group`); the daemon started detached
   from its terminal; SIGTERM to stop it.
5. **The sandbox** is Landlock: Linux only (macOS has none either).
6. **Links**: the shared paths of a lane (`node_modules`) are symbolic
   links, and so are files the tree says are links.
7. **Paths**: a path in a tree is a `PathBuf`. Made from a Windows path it
   would hold `\`; the store, the git export and every client expect `/`.
8. **The checks** run with `sh -c`.

## Decision

**Paths in Kito's data are `/`-separated UTF-8 on every platform.** A path
read from disk is converted where it enters (scan, capture, file events,
the hooks' arguments) and converted back where it leaves (writing a file,
showing a path to open). A path that is not valid UTF-8 on Windows does
not exist (Windows paths are UTF-16; the conversion is lossless for valid
ones). Nothing already stored changes: Unix paths are already this.

**The daemon listens on a named pipe on Windows**,
`\\.\pipe\kito-<hash of the repository's root>`, with the same JSON lines
and the same key (`socket.key`) as the Unix socket. The client opens the
pipe as a file. A small `ipc` module in `kitod` hides which of the two it
is.

**A file's identity on disk** is its size, its modification time to the
nanosecond, its creation time, and the file index (`GetFileInformationByHandle`
through `std::os::windows::fs::MetadataExt` where stable, else size and
times only). It is compared only on the machine that took it.

**The executable bit** does not exist on Windows: a file written keeps the
mode the tree gives it in the tree, and a file captured keeps the mode it
had (a new file is regular). Keys are left to the user profile's ACLs,
which already keep them from other users.

**Processes**: a command for a lane or a check is started in a new
process group (`CREATE_NEW_PROCESS_GROUP`) and stopped with its children by
`taskkill /T /F /PID <pid>`. The daemon is started with `DETACHED_PROCESS`
and stopped through its pipe (`shutdown`) or Ctrl-C, not a signal.

**The checks and lane commands** run with `sh -c` where a `sh` is on the
`PATH` (Git for Windows has one), else with `cmd /C`. The configuration's
commands are the user's: what shell they are written for is theirs to know.

**The daemon holds nothing of whoever started it.** A Windows process
inherits every inheritable handle of the one that starts it, its standard
pipes included, whatever it is given as its own: a daemon started by
`kito` would keep open the pipes of the program that ran `kito`, which
would wait for them for ever. Before starting it, `kito` marks its
standard handles as not inherited (`SetHandleInformation`). It is the one
`unsafe` call outside `kito-store` (`kitod/src/detach.rs`): Rust has no
stable way to say which handles a child inherits.

**No sandbox on Windows** for now: `[sandbox] mode = "auto"` runs unconfined
and says so; `"required"` refuses, as on macOS.

**Links**: a shared directory of a lane is a directory junction (no
privileges needed); a link in a tree is written as a file holding its
target, as git does with `core.symlinks = false`, and read back as a link
only if the tree already said it was one.

**File events** (ReadDirectoryChangesW, through `notify`) have no
close-after-write either: Windows has the limit macOS has, written down.

## Consequences

- The data is the same on every platform: a repository moved from Linux
  to Windows reads as it was.
- Two ways to reach the daemon to keep working; the `ipc` module and tests
  on a Windows runner keep them apart and both tested.
- A Windows release (`windows-x86_64.zip`) and `install.ps1`; CI runs the
  tests on `windows-latest`. Tests that need a Unix shell or Unix links
  say why they do not run there, as the Linux-only ones do on macOS.
- Not done: a sandbox on Windows (AppContainer or a job object with
  restricted tokens), `arm64` Windows builds.
