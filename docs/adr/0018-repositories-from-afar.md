# 0018 · Repositories added, archived, renamed and deleted from afar

Status: accepted
Date: 2026-11-22

## Context

A server's repositories were added only on the server's machine
(`kito-server repo add`), and nothing took one away or renamed it. The
person asked on 2026-11-22 for admins to add repositories from a
connected directory and from the web client: empty, cloned from a git
address (a private one with a token used once), or sent from their own
machine; to take one out of use without losing it at once; and to rename
one without breaking the directories connected to it.

## Decision

- **Admins and owners add** a repository through the server
  (`server.repo_add`); whoever adds it maintains it.
- **From where**: empty; a git address of another machine (`https://`,
  `http://`, `ssh://`, `git://`, `user@host:path`), never a path on the
  server's own disk; or sent from the client's machine in pieces
  (`server.repo_upload_begin`, `_chunk`, `_end`). A Kito repository is
  sent whole: its store, less what it rebuilds, and the files of its
  main lane, in the archive of backups (ADR 0017) with a key made for
  that one upload. A git repository is sent as a bundle of all its
  history.
- **A token** for a private address is given to git once, in the address
  of the clone, and kept nowhere: the clone's `origin` is set back to the
  address without it, and an error that names it shows `<token>`.
- **Archive, then delete**: a repository archived moves to `archived/`
  with who did it and when; nobody reaches it; it is deleted 30 days
  later by the serving server. Owners bring it back
  (`server.repo_unarchive`) or delete it now (`server.repo_delete`,
  with its name again). Admins archive.
- **Rename** moves the repository, the access given to it, its settings'
  history and the kitchens that name it; the old name is kept in
  `[renamed]` of `server.toml`, so a directory connected with it still
  reaches the repository.
- Commands: `kito repos all | add <name> [--from <url> [--token-file
  F]] [--push <dir>] | archive | unarchive | delete --confirm | rename`
  in a connected directory; `kito-server repo archive | unarchive |
  delete --confirm | rename` on the server's machine.

## Consequences

- An admin may have the server fetch any address it can reach on the
  network; only its own disk is closed to them.
- An upload is held whole on the server's disk until it ends; one never
  ended stays in `uploads/` until the server's keeper removes it.
- The backups made before a delete still have the repository.
