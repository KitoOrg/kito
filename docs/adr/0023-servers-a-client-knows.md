# 0023 · The servers a client knows

Status: accepted
Date: 2026-11-23

## Context

A directory connected to a repository on a server kept the whole
connection in its own `.kito/remote.toml`: how the server is reached,
how it is known to be the one meant, and the person's token or session.
So a person signed in once per directory: a session that ended was
renewed in each directory, a new token was written in one and the others
had to be connected again, and nothing could ask a server without a
directory connected to one of its repositories. The person asked on
2026-11-23 for a way to sign in to a server by naming it alone, kept for
the client and not for a directory; for several servers to switch
between; for `connect` to hold the connection to a server, not to a
repository; and for a repository of their own, worked on locally, to be
sent to a server from where they are. They decided: a directory of a
repository is `kito clone shop`, and `kito connect <server>/<repo>`
keeps doing both steps; a current server is switched with `kito server
use`, `--server` names another for one command, and a connected
directory always uses its own; `kito repos add <name> --push .` inside
the local repository connects it in place, its local history kept aside.

## Decision

One rule holds all through: **a directory never changes who it acts
as, and nothing a person did not ask for becomes their current server.**

- **`servers.toml`, in the person's Kito directory** (where their plugins
  are: `$KITO_CONFIG_DIR`, then `$XDG_CONFIG_HOME/kito`, `~/.config/kito`,
  `%APPDATA%\kito`). For each server, by name: `address` (or `command`
  for SSH), `fingerprint` / `authority` / `plain` as before, `user` as
  the server last said, `token` (the person's, or a session's) and
  `sign_in` (when the token is a session's: how another is got); and
  `current`. On Unix it is readable by its owner only (0600); on Windows
  it has the permissions of the directory it is in, the user profile's
  by default (ADR 0002). It is written whole to a file beside it and
  renamed over it, and changed under a lock (`servers.lock`): two
  commands at once take turns. One that cannot be read is said, and
  never written over. What runs confined for a lane (the checks, `kito
  lane run`) does not read it, as it does not read `.kito`, where the
  tokens were before.
- **Every program of a person sees the same Kito directory**: their
  shell, the hooks, the directory's daemon (started with the environment
  of the command that started it) and the MCP server. One that sees
  another (`KITO_CONFIG_DIR` set for one and not the others) finds no
  entry for the name a directory gives, and says that the server is not
  one of the person's servers.
- **A server's name** is its host as typed, without the port when it is
  the usual one (7421); `user@host` for SSH. `--as <name>` gives another:
  two accounts on one server, or two servers on one host. A name is one
  server's and one person's there:
  - taken by another server (another address or command: `kito connect
    host` and `kito connect ssh://host/srv/kito` are both `host`): with
    `--as` it is refused, saying what has the name; without, the new one
    is kept under the first free `host-2`, `host-3`…, or the one that is
    already this server, and that is said;
  - the same server, another person (a `--token` or a sign-in that the
    server answers as somebody else than the entry's `user`): refused
    before anything is kept, `` you are alice on `host`: to be bob there
    too, add `--as <name>`; to stop being alice there, `kito server
    remove host` first ``. A session opened for it is ended. The same
    person again (a new token, a new session) replaces what was kept.
    When the entry does not say who it is, the server is asked with what
    is kept; when that no longer lets anybody in, the entry is replaced.
- **`kito connect <server>`** (`host[:port]`, `kito://host[:port]`,
  `ssh://user@host/path`) checks the server and signs in exactly as
  before (`--fingerprint`, `--authority`, `--plain`; `--token`,
  `$KITO_TOKEN`, a token pasted, `--sign-in`), keeps it, makes it
  current, and says who the person is there and which repositories they
  may use. It makes no directory. When the server is known under that
  name and what is kept still lets the person in, nobody signs in again;
  when it no longer does, the person signs in again, the server known by
  what was checked the first time. With a repository too
  (`<server>/<repo> [dir]`, `<server> <repo> [dir]`, the SSH form and a
  repository), it then does what `kito clone` does.
- **`kito clone <repo> [dir] [--server <name>]`** makes the directory:
  what `connect` did after signing in, into the directory, whose files
  are left as they are. `clone` is no longer another name of `connect`;
  given an address (`kito clone kito.example.com/shop`, as before), it is
  `kito connect` with it.
- **A connected directory names its server**: `remote.toml` holds
  `server = "<name>"` and `repo`, nothing secret. `Remote::load` fills
  the rest from the entry of that name, so what reads a connection (the
  CLI, the directory's daemon, the hooks, the MCP server) is unchanged,
  and a session renewed once serves every directory of the server. The
  directory's daemon reads it again before each new connection to the
  server: a session renewed or a token replaced is taken without a
  restart. A name the person has no entry for is reached nowhere, and
  says so.
- **A directory connected before 0.40** (an address and a token in its
  own `remote.toml`) works as it did. The first command of the CLI run in
  it moves it: an entry is used only when it is the very same connection
  (the same address or command, token, way to sign in, fingerprint,
  authority and plain); otherwise its own connection is added under its
  host's name, or the first free `host-2`… when that is taken, and the
  line said on the standard error tells why (another account of that
  server, or that server known another way, is kept under the host's
  name). Nothing becomes current. `user` is taken from an entry of the
  same token when there is one, and otherwise filled by the first command
  of the CLI that reaches the server with that entry. Then `remote.toml`
  is rewritten to name the entry, and the directory's daemon, if one
  runs, is stopped: it may be of the version before. `servers.toml` is
  written first, both whole: a machine that stops in between leaves the
  directory as it was, and the next command finds the entry. The
  daemon, the hooks and the MCP server only read; they never rewrite.
- **`kito server`** (`list`, the default; `use <name>`; `remove <name>`)
  lists the name, where the server is, who the person is there and which
  is current, never a token. `remove` forgets one; when it holds a
  person's token it says first that Kito keeps it nowhere else and that
  only an admin of that server can give another; when it holds a
  session, it ends it on the server (`server.sign_out`, new; a server
  that does not know it leaves the session to run out, and that is
  said).
- **`--server <name>`** on `clone`, `repos` (every subcommand), `people`,
  `audit`, `backup` and `sign-in`: in a connected directory they ask its
  server unless `--server` says another; anywhere else, the current one.
  With no server known they say to `kito connect <server>` first; with
  servers known and none current, `` no current server: `kito server use
  <name>` (you have: a, b) ``. A server becomes current only by `kito
  connect` and `kito server use`. `members`, `grant` and `revoke` stay
  about the directory's repository.
- **`kito sign-in [method] [--server <name>]`** signs in again to the
  directory's server or the current one, and keeps the session in
  `servers.toml`. **`kito people token`**, for a person whose entry holds
  their token (not a session), keeps the new one there: every directory
  of that server uses it already.
- **`kito repos add <name> --push <dir>`** works anywhere; a `<dir>`
  inside a repository of this machine is that repository. What is saved
  in it is recorded before anything is packed. When it is the repository
  the command runs in, then once the server has it the directory is
  connected to it in place: before anything is sent, a repository with
  open lanes is refused (close or integrate them, or `--keep-local`);
  its permanent lanes go with it and are named; its daemon stops, and
  when it does not, nothing is moved and that is said; `.kito` is moved
  aside, whole, to `.kito-local-<UTC date-time>`, named in what is
  printed, ignored as a file of the project and hidden from what runs
  confined; the directory names the new repository and the main lane's
  files come into it as `clone` brings them, leaving every file there as
  it is. What fails after the move puts `.kito` back, and says the
  repository is on the server and that `kito clone <name>` makes a
  directory of it. `--keep-local` only sends, as before.

## Consequences

- A person signs in once per server, not once per directory, and asks a
  server from anywhere: `kito connect kito.example.com`, then `kito
  repos`, `kito clone shop`.
- The person is who the person's Kito directory says, not who the
  directory says: two people who share a directory on one machine each
  need the server among their own (each daemon is started as whoever
  started it, as before). Two accounts on one server, on one machine,
  are two names (`--as`): ada, rose and bob each under their own.
- A token is no longer in any directory: copying or archiving a project
  carries none. A project unpacked from somebody else with an old
  `remote.toml` acts as that somebody, as it did before 0.40, under a
  name of its own; it never becomes the current server.
- Moving an old directory needs a person's Kito directory it can write;
  when it cannot, the directory keeps its connection as it was, and says
  why.
- A daemon of 0.39 that is running when its directory is moved keeps
  the connection it read when it started, and would not find it in the
  new `remote.toml` if started again by 0.39; it is stopped by the move,
  and the next command of 0.40 starts the new one.

## Considered and left out

- **The system keychain** (Keychain, Secret Service, Credential Manager)
  for the tokens: another dependency on each system, and a prompt in the
  middle of a script or an agent's run. The file is the person's alone,
  as `remote.toml` was; the keychain can come later behind the same
  entries.
- **Several identities on one server under one name**, chosen per
  directory: what `--as` gives already, with a name a person can read in
  `kito server list`.
- **Moving every directory at once**: Kito does not know where they are;
  each is moved the first time it is used.
- **Asking the server who the person is when an old directory is
  moved**: moving makes no connection; `user` is filled by the first
  command that reaches the server with the entry.
- **Reusing an entry of the same address when an old directory is
  moved**, as first written: it made a directory act as another person,
  and lost a token that was kept nowhere else.
