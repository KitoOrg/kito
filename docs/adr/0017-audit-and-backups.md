# 0017 · The server's audit log, and its backups

Status: accepted
Date: 2026-11-22

## Context

A Kito server holds a team's repositories, its people and their access.
Nothing said afterwards who added whom, who changed which rule, who
signed in or was refused, and there was no way to keep a copy of the
whole server and put it back. The person asked on 2026-11-22 for an
audit log of people and access, rules and settings, repositories and
every request, read by admins and made so that a change to it is found;
and for backups of the whole server, encrypted, made on a schedule, kept
in a folder and in storage that speaks S3; from `kito-server`, the web
client and a connected directory.

## Decision

- **The audit log** is in the server's directory, `audit/`:
  `log.jsonl` keeps what changes the server (every event the server
  tells: people, roles, grants, settings, plugins, repositories,
  sign-ins and refusals, sessions, backups), forever;
  `requests-<day>.jsonl` keeps each request (who, which repository,
  which method, answered or refused), for 90 days.
- **Each line is chained**: it carries the hash (SHA-256) of the line
  before it in its file and its own. `kito-server audit --verify` walks
  the chains and names the first line taken out or changed.
- **Admins and owners read it**: `kito-server audit` on the server's
  machine, `server.audit` and `server.audit_verify` through the server
  (`kito audit`, the web client).
- **A backup is the whole server** in one archive: every repository, the
  people, the settings, what plugins keep; without the indexes a
  repository rebuilds, the locks, the backup key and the archives
  themselves. It is made while the server runs: a repository's store is
  append-only and checks every record, so a copy is what a crash would
  have left, which it opens from.
- **Archives are compressed and encrypted** (zstd, then
  ChaCha20-Poly1305 in chunks of a mebibyte, each with its number and
  whether it is the last): a changed, moved or cut chunk is refused. The
  key is the server's (`backup.key`, for its owner), made with the first
  backup and printed once; `kito-server backup key` prints it again on
  the server's machine. A restore elsewhere needs it.
- **Where and when**: in a folder (`backups/` by default) and, when said,
  in S3-compatible storage (signed with AWS Signature Version 4, keys
  kept for the server's owner or in the environment); every 24 hours
  while the server serves, the last 14 kept, all said in `[backup]` of
  `server.toml` (`kito-server backup schedule`, `backup s3`).
- **Restore** puts an archive back into an empty directory, from a file
  or from S3: the server is then served from there.
- Commands: `kito-server audit [--requests] [--who] [--kind] [--repo]
  [--since] [--verify]`, `kito-server backup [run | list | key |
  schedule | s3]`, `kito-server restore <archive> --to <dir> [--key]`;
  `kito audit`, `kito backup [now]` for admins in a connected directory.

## Consequences

- A change to the log is found, not prevented: whoever controls the
  machine can rewrite a whole chain. Sending the log elsewhere is a later
  step.
- Backups hold everything the server holds, people's token hashes
  included: the key is what protects them.
- A restore is into a new directory; replacing a server in place is
  stopping it, moving directories and starting it again.
