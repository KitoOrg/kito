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
