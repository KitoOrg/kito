# 0014 · Roles in each repository, and roles of the server

Status: accepted
Date: 2026-11-22

## Context

A person on a server had one role (`read`, `write` or `integrate`,
`integrate` when nothing was said), a list of repositories (`*` for all),
and roles of their own in some of them; and an `admin` flag that only let
them manage plugins from a browser. Adding people and repositories, and
changing the rules of a repository, was done on the server's machine
only. The person asked on 2026-11-22 that permissions be per repository,
that the server have roles of its own, and that the commands be simple.

## Decision

- **A person has a role in each repository they may use, and nothing in
  the others.** No role for everywhere, no `*`. The roles, each with what
  the one before it has:
  - `read`: see, and follow lanes;
  - `write`: open lanes and work in them; their integrations wait;
  - `integrate`: integrate lanes, decide for any lane;
  - `maintain`: change the repository's rules (protection, checks,
    approvals, git push, kitchen) and who may use it, from a connected
    directory too.
- **A person has a role on the server**:
  - `member` (the default): the repositories they were given;
  - `admin`: may use every repository as `maintain`, add and remove
    people, give and take repositories, manage the server's plugins;
  - `owner`: as `admin`, and names admins and owners.
  On the server's machine, whoever runs `kito-server` is an owner.
- **Commands** — on the server's machine:

  ```text
  kito-server user add <name> [--repo shop[:role]]… [--role member|admin|owner] [--email] [--identity]
  kito-server user role <name> <member|admin|owner>
  kito-server user list | remove <name>
  kito-server grant <name> <repository> <read|write|integrate|maintain>
  kito-server revoke <name> <repository>
  ```

  `--repo shop` alone is `write`. In a connected directory:

  ```text
  kito members                    who may use this repository, and as what
  kito grant <name> <role>        maintainers and admins
  kito revoke <name>
  kito people                     admins: everybody, and their role on the server
  kito people add <name> [--role] [--email] [--identity]   prints the token once
  kito people remove <name> | role <name> <role>
  ```

  `kito-server user admin` is gone: `user role <name> admin`.
- **What a maintainer may ask through the server** besides what anyone
  who integrates may: `protect` and `configure`.
- **A server that was there before** is read as it was meant once, and
  written back in the new form: each repository a person had (`*`: every
  repository the server has then) with the role they had in it; `admin`
  becomes the `admin` role.

## Consequences

- Giving somebody a new repository is one command, and does not change
  what they may do elsewhere.
- A person added with no `--repo` may use nothing until granted; before,
  they could use every repository as `integrate`.
- Adding and removing repositories stays on the server's machine: it
  clones and imports, which is long and runs as the server.
- What a person may do was read when they connected; since 0.30 it is
  read at every request (see below).

## Amended 2026-11-22: the people page

Asked for with the people page of the web client:

- **A change counts at once**: an open connection reads the person's role
  at every request, and one removed, or whose repository was taken away,
  is stopped there.
- **Who uses a repository** (`server.members`) is for its maintainers
  and the server's admins, as `kito members` is.
- **When each person was last here** is kept, a file each in `seen/`,
  written at most once a minute, and given with the members and the
  people.
- **The server's people over HTTP**: `POST /-/people` answers
  `server.people`, `_add`, `_remove` and `_role` as whoever is signed in;
  `/-/api/<repository>` answers `server.members` and `server.grant`.
  Adding somebody answers the port the server listens on and whether it
  encrypts, for the line they connect with.

## Amended 2026-11-22: the settings page

Asked for with the settings page of the web client:

- **The checks are the admins'**: they run commands on the server's
  machine, so through the server only its admins change them. A
  maintainer sees them, and changes the other rules.
- **A kitchen is another repository of the server**, given as
  `../<name>`: nothing else is taken through the server, so no folder of
  the server's machine can be named.
- **Who changed which rule, and to what**, is kept by the server
  (`settings/<repository>.jsonl`, the last 200), from a client or from
  `kito-server repo set` on its machine, and given to maintainers and
  admins (`server.settings_history`).

## Amended 2026-11-23: a change of the server's configuration is made alone

Found when 0.39.3 was reviewed. Everything that wrote `server.toml`
wrote the copy it had read when its request began: a change made in
between was undone by whoever wrote later, and nobody was told. With
people changing their own tokens (ADR 0006, amended), what was undone
could be a revocation. A repository added from a git address, a clone
of a minute, wrote back a token an owner had taken away meanwhile. Two
people who asked for a new token at once wrote over each other: one held
a token the server did not know, and the one they had replaced worked.
Anybody writing in a loop brought back whoever an admin had just
removed, or the role they had just lost.

- **Every change is made alone, on the configuration as it is on the
  disk at that moment.** One way, for everything that writes
  `server.toml`: the lock is taken, the file is read again, the change
  checks and changes what was just read, that is written, the lock is
  let go. What a change checks, it checks there: the person exists, the
  name is free, the role rule holds, they keep a way to sign in.
- **Nothing slow is done while the lock is held**: no plugin is asked,
  nothing is cloned, uploaded or sent over the network. What is slow is
  done first; then the lock is taken, and what was checked before is
  checked again. When a repository is deleted, what the people were
  given of it leaves the configuration first, and its files after.
- **Who asked is looked at again in the change.** A request that takes
  its time (a clone, a plugin installed or asked) changes nothing for
  somebody who, when the change is made, was removed, came with a token
  or a session that is no longer theirs, or is no longer the admin or
  the owner the request is for: `ada is no longer an admin of this
  server: nothing was changed`. What was made before that stays (a
  repository that was cloned), for the server's admins.
- **One lock, for the configuration and for the sessions**, taken once
  for a change: `.lock` in the server's directory. A change of one
  often ends or reads the other, and with one lock there is no order to
  take two in. It is not the audit log's, which has its own and is
  taken while a line is written. In the code a change is a value of its
  own: what writes is a method of it, and nothing of it begins a change
  inside another, so the lock is not taken twice.
- **A lock says whose it is**, and is removed by its holder only. One
  older than five seconds is taken over, since a crash must not block
  the server for ever; whoever it was taken from finds out before it
  writes, and writes nothing. When the lock cannot be had within two
  seconds, whoever asked is told so, and nothing is changed.
- **A net under it**: a server remembers what `server.toml` hashed to
  when it read it, and writes over nothing else (`the server's
  configuration was changed meanwhile: nothing was written; ask
  again`). With the first point in place this does not happen. It is
  there so that a writer nobody converted refuses, instead of undoing
  somebody's change.
- **What a server stopped in the middle of a write left staged** beside
  `server.toml` (`server.toml.<pid>-<n>.new`, with what tokens hash to)
  is swept when the server starts, once it is older than a minute.

What follows from it:

- A change waits for the one before it, which is short. One that cannot
  have the lock is refused, rather than made on what was read earlier.
- `server.toml` edited by hand while the server runs is not written
  over without a word: a change that read it a moment before the edit
  is refused, and the next one reads the edit.
- A window stays that a lock file cannot close: a writer that held the
  lock for more than five seconds, was taken over, and lost it between
  its last look and the moment it puts its file in place. Nothing holds
  the lock that long: what is slow is outside it.
