# 0020 · A server's plugins that work on its repositories on their own

Status: accepted
Date: 2026-11-22

## Context

A plugin of a server acted only when it was asked: as a person (a page
asking through `/-/api` with that person's cookie), or for a repository
where it ran (ADR 0005). The person asked on 2026-11-22 for the GitHub
plugin to keep a repository in step with a GitHub repository, both ways,
told by GitHub's webhooks and checking by itself when they do not come.
Nobody is there to ask: the plugin has to work on the repository by
itself, with what an administrator gave it, and say when to pull and
push.

## Decision

- **A plugin that serves pages asks the server as itself.** The server
  already gives it a secret (`KITO_PAGES_SECRET`) to recognise requests
  passed on to it; the plugin sends the same secret back in
  `x-kito-plugin` to `/-/api/<repository>`. The server answers with what
  the plugin was given in a repository (`read`, `write`, `integrate`,
  `admin`), in every repository of the server; a wrong secret is
  nobody. Each request is in the audit log as `plugin:<name>`.
- **Where to reach it**: `KITO_SERVER_API`, the server's HTTP on this
  machine. A server that serves HTTPS also listens in the clear on
  127.0.0.1 for this: its certificate names the server, not 127.0.0.1.
- **Pages are told the person's role on the server** (`x-kito-role`),
  beside who they are, so that a plugin keeps its own setup pages for
  administrators.
- **`git_sync`** (needs `integrate`): the daemon brings in what the git
  remote has and pushes, now rather than after the next integration, as
  it does after each one: the remote's commits come in through a lane,
  merged and checked like any (`pull`), then the main lane and the
  permanent lanes' branches are pushed.

## Consequences

- A plugin given `admin` may change a repository's settings, in every
  repository: it is given that by an administrator who enables it.
- Only plugins that serve pages have a secret, so only they can work on
  their own; others still act when asked.
