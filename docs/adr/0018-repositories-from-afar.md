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

## Amendment, 2026-11-22: a repository starts with something

A repository added empty took no lane: a lane starts from what the main
lane has, and `open_lane` answered that there was nothing a lane could
start from. On a server the main lane is changed through lanes, so
nobody connected to such a repository could put a first file in it: it
was a dead end until somebody wrote files in its directory on the
server's machine and made a checkpoint there.

A repository added with no git address and no upload now starts with a
first version: files written in its main directory, recorded, and a
first checkpoint, "first version". Lanes open in it at once. It is not
protected, as an empty one was not.

- **Given nothing**, it starts with a `README.md` that holds `# <name>`.
  `kito-server repo add <name>` and `kito repos add <name>` give it
  nothing.
- **Given files**, they are its first version, and there is no
  `README.md` unless it is one of them. `server.repo_add` takes them in
  `files`, a list of `{ "path": "kitchen.toml", "text": "…" }`; a list
  of nothing is nothing given. The answer is the same, `{ "name": … }`,
  and the request is still for admins.
- **What may be given**: at most 32 files, each text at most 256 KiB. A
  path is relative and written with `/`, with no empty part, no `.` or
  `..` and no control character; it is not under `.kito` or `.git`,
  whatever the case of the letters; none is given twice, or under a path
  given as a file. A file that a `.gitignore` or `.kitoignore` among
  them leaves out is refused as well: it would be on the server's disk
  and on no lane. What is refused is said with its path, and nothing is
  made.
- **One way to start at a time**: `files` together with `from`, or with
  an upload (`server.repo_upload_end`), is refused.

The first version is recorded as any file saved in a main directory is:
by whoever the server runs as. Who added the repository is in the audit
log, with how many files were given. Over HTTP the request is the same
(`POST /-/people`); a request there holds at most 2 MiB in all, so the
files of one request together stay under that, while a client connected
to the server's own port may give all 32 at their largest.

## Amendment, 2026-11-23: what is deleted is the repository named

Found when 0.39.3 was reviewed a second time; there since 0.36.0. A
repository was deleted by its name, and the one in use was preferred to
the archived one. `shop` archived, somebody adds a new `shop`, and when
the archived one's 30 days were over the purge deleted the new one, its
lanes and all; the archived one went at its next pass.

- **The purge deletes the archived repository it is about**, and
  nothing else: a repository in use under its name is another
  repository.
- **Deleting says which**: `server.repo_delete { name, confirm,
  archived: true|false }`, `kito-server repo delete <name> --confirm
  <name> [--archived | --live]`, `kito repos delete <name> --confirm
  <name> [--archived | --live]`. With nothing said, the only one of that
  name is deleted; when there are two, nothing is, and whoever asked is
  told to say which. The answer says `archived: true|false`, and the
  audit log `shop, archived` for the archived one.
- **A deletion is done under one lock, in one order**: which repository
  it is; what the people were given of it and its old names, written;
  then its directory moved aside, beside where it was
  (`repos/.deleted-<name>-…` for one in use, `archived/.deleted-<name>-…`
  for an archived one: on the same disk, so the move is a rename), and
  what is left of its name taken away (below), and its event kept and
  queued. The lock is let go and the directory removed. Nothing is moved
  back: a directory moved aside is one whose deletion was written, and
  the server sweeps those when it starts, whatever their age. Backups
  leave them out. When the move fails, the configuration is written all
  the same; the audit log says that nobody is given the repository any
  more and that its directory is still there (`repo_not_deleted`), and
  whoever asked is told so (`…its directory could not be moved (…): ask
  for the deletion again`).
- **When no repository in use has the name any more**, nothing is left
  of it: its history of settings goes, and no repository has it as its
  kitchen (`kitchen.path` is taken out where it names it; a failure there
  is said on the server's standard error, and undoes nothing). This is
  done with the lock held, before a repository can be added under the
  name.
- **Archiving is never due by accident.** When and by whom is written,
  whole, before the directory is moved, with the lock held; if the move
  fails it is taken away. An archived repository whose date cannot be
  read says none (`deleted_at_ms: null`) and is never due: the purge
  dates it then, and says so, and it has its 30 days from that day. The
  purge's list is made before the lock is taken, and each it deletes is
  looked at again with the lock held: one that is not due then (another
  archived under the name meanwhile) is not deleted.
- **A client asks which only of a server that hears it.** `kito repos
  delete … --archived | --live` is refused before anything is sent when
  the server did not say, as it greeted the client (`server` in the
  answer to hello), that it is 0.39.5 or later: an older server would not
  hear `archived` and would delete the one in use (`this server is older
  than 0.39.5 and would delete the repository in use: delete it on the
  server's machine`).
- **A name is one repository's, an archived one's too.** What people are
  given, a repository's old names, its history of settings and the
  kitchens that name it are said under its name: a repository added
  under the name of an archived one would have them all. Adding one so is
  refused (`an archived repository is called `shop`: bring it back,
  delete it, or give this one another name`).
- **A repository is made aside, and takes its name when it is whole.**
  Added empty, with files, from a git address or sent from a client, it
  is made in `repos/.adding-<name>-<random>` and, once complete and
  closed, takes its name in one step with the lock held: refused when
  anything has the name by then (in `repos/`, whatever it is, or among
  the archived), else renamed into place. Whatever fails takes away the
  directory made aside, and nothing else: nothing that has the name is
  ever removed by an addition that failed. No name of a repository starts
  with a dot, so nothing being made is listed, reached, archived,
  renamed or deleted by name; the server sweeps what was made aside and
  never given its name once it is a day old. Nothing Kito or git keeps
  names the directory a repository was made in.
- **A rename that is not written is put back with the lock held**,
  before anybody could take the old name; when it cannot be, that is
  said on the server's standard error with both paths, and to whoever
  asked by the names. Two of one name exist only on a
  server that made them before 0.39.5; there, what people were given
  under the name counts for the one in use. An operator finds such a
  pair in `server.repos_all` (`kito repos all`): a name both among
  `repos` and among `archived`. What to look at then: who may use the one
  in use (they were given it as the archived one, perhaps), and which
  repositories have it as their kitchen.
