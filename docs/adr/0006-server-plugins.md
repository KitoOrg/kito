# 0006 · Plugins for the server

Status: accepted
Date: 2026-11-21

## Context

A repository's plugins (ADR 0005) add commands, gates, event handlers
and tools for agents, where the repository is. A `kito-server` has
things of its own that people want changed, and none of them is a
repository's:

- **Who a person is.** The server knows people by tokens it hands out
  (`kito-server user add`). A team already has identities: GitHub, a
  company's sign-on. Nobody wants one more secret a person.
- **A web client for a team.** `kito web` serves one person on their own
  machine. A team wants one address, and to sign in.
- **What happens on the server**: a repository added, a person removed,
  somebody signing in. For an audit log, a message in a channel.
- **What an administrator runs there**: backups, imports, reports.

Kito keeps having no interface and no identity provider of its own:
these are plugins too (decided with the person on 2026-11-21, with: a
sign-in plugin says who somebody is and the server says what they may
do; GitHub first; pages reached through the server's own address).

This adds an HTTP listener to `kito-server` and a second way to be known
by it, which the ADR rules ask to write down first.

## Decision

### 1. The same plugins, enabled by a server

A server plugin is a plugin as ADR 0005 has it: a program `kito-<name>`
with a `kito-plugin.toml`, installed for the user the server runs as
(`kito-server plugin add --path <dir>`, or `kito plugin add`: the same
place). What it does for a server is in sections of its
own, and one plugin may have both kinds (the web client has a command
for a repository and pages for a server):

```toml
name = "github"
version = "0.1.0"
api = 1

[sign_in]                            # says who somebody is
method = "github"
about = "Sign in with GitHub"

[pages]                              # serves HTTP under a path
mount = "/"

[[server_event]]                     # run for these, one at a time
kinds = ["signed_in", "user_removed", "repo_added"]

[[server_command]]                   # `kito-server audit ...`
name = "audit"
about = "What happened on this server, and who did it"
```

`kito-server plugin enable <name>` writes `[plugins.<name>]` in
`server.toml`, with what the plugin was given; `plugin disable`, `plugin
list`. Nothing of a plugin runs on a server that did not enable it. A
plugin keeps what is its own (the secret of an OAuth application, a
log) in `plugins/<name>/` under the server's directory, readable by the
server's user only.

What a server gives a plugin, besides a repository's `read`, `write`,
`integrate` and `admin`:

| Permission | What it lets the plugin do |
| --- | --- |
| `sign_in` | Say who somebody is. Nothing else: not what they may do. |
| `pages` | Be reached through the server's HTTP address. |
| `server_events` | Be told what happens on the server. |
| `server_admin` | Add commands to `kito-server`, with what an administrator may do. |

### 2. Signing in: the plugin says who, the server says what

A sign-in plugin proves an identity and nothing more:

```json
{ "provider": "github", "subject": "583231", "login": "bob", "name": "Bob", "email": "bob@example.com" }
```

- **Roles and repositories stay the server's.** A person is still added
  by an administrator: `kito-server user add bob --identity github:bob
  --role write`. The server keeps `provider` and `subject` (the number
  that does not change when a login is renamed; the plugin resolves the
  login when the person is added). An identity no user has is refused,
  and said in the events (`sign_in_refused`).
- **Tokens stay.** `user add` without `--identity` gives a token as
  today; scripts and agents on other machines go on using them.
- **The plugin answers questions; the server keeps the state.** It is
  run for each step, with JSON on its input and its output, and holds
  nothing between two steps but what the server hands back to it:
  - `sign-in begin` (told the `kind`: `device` for a command line,
    `browser` with the address to come back to) → what the person must
    do: `{ "open": url, "code": "ABCD-1234", "state": …, "interval":
    seconds }` (a code to enter at an address: nothing of the server
    need be reachable by the provider), or for a browser `{ "open": url,
    "state": … }`, where the server sends it;
  - `sign-in finish` (with `state`, and for a browser the query it came
    back with) → the identity, `{ "wait": seconds }`, or a refusal with
    its reason;
  - `sign-in lookup <login>` → the identity of a login, for `user add`.
- **From the command line**: `kito connect <server> <repo> --sign-in
  github` shows the address and the code, waits, and keeps what the
  server gives it: a **session token**, made by the server, which ends
  after a time the server sets (`[sign_in] days = 30`) and is told apart
  from the tokens of `user add`. `kito sign-in` again when it has ended.
- **In a browser**: the server's HTTP address (section 3) has `/-/
  sign-in/<method>`, which goes to the provider and comes back to a
  session cookie.
- The plugin never sees a token or a cookie of the server, and never
  the roles. The server never sees the provider's secret.

### 3. Pages: through the server's own address

`kito-server serve` answers HTTP when told to (`--http <address>`, or
`http = "0.0.0.0:7443"` in `server.toml`), encrypted with the
certificate it already has (phase 12); in the clear (`--plain`) only on
the machine itself.

Who asks is known by a token (`Authorization: Bearer`, the one `user
add` printed or the one of a session, section 2) or by a cookie: `POST
/-/session` with a token opens a session and sets it (`HttpOnly`,
`SameSite=Strict`, `Secure` when encrypted), `DELETE /-/session` ends
it.

- **The server serves no pages of its own.** Under `/-/` it has what
  only it can answer: `/-/me` (who asks, and the repositories they may
  use), `/-/session`, `/-/sign-in/…` (section 2), and
  `/-/api/<repository>` (below). Everything else goes to the plugin
  whose `[pages]` mount is the longest that fits; with none, the address
  says so in a line of text.
- **A pages plugin is a program that stays up.** The server starts
  `kito-<name> serve-pages` with an address on the machine itself to
  listen on (`KITO_PAGES_LISTEN`) and a secret (`KITO_PAGES_SECRET`),
  and passes requests on to it, each with the secret (`X-Kito-Secret`),
  where it is mounted (`X-Kito-Mount`) and who asks (`X-Kito-User`, when
  somebody is signed in). What the browser sent under those names is not
  passed on, nor the person's token, nor the cookie of their session: a
  plugin is told who asks, and cannot be them. It may set cookies of its
  own, not the server's. The server holds the plugin's standard input
  open: when that closes, the server is gone, and the plugin stops. It
  is started again when it stops; a request to one that is down says so.
- **A page asks about a repository as the person who is looking at
  it.** `/-/api/<repository>` takes a method of the daemon's API
  (`docs/api.md`), as `{ "method": …, "params": … }`, and answers as the
  daemon would for that person: what their role allows. A browser asks
  with its session cookie, and is then held to what the plugins that
  serve pages were given (`read`, `write`, `integrate`); and its request
  must carry the header `X-Kito`, which a page of another site cannot
  set. With a token of their own (a script, the command line) a person
  is held to their role alone. No page can do what the person looking at
  it could not do with `kito`.
- The web client (`kito-web`) is the same client in both places: served
  by `kito web` to one person, or by `kito-web serve-pages` behind a
  server to those who sign in.

### 4. What happens on the server, and commands for it

- **Server events**: `repo_added`, `repo_changed` (its rules),
  `user_added`, `user_removed`, `role_changed`, `signed_in`,
  `sign_in_refused`, `session_ended`, `plugin_failed`. A plugin is run
  for the kinds it lists (`kito-<name> server-event`, the event on its
  input), one at a time and in order, after the fact. What an
  administrator's command did is told by that command before it leaves;
  what happens while serving, by the server (D-154).
- **Commands**: `kito-server <name> […]` for a name the server does not
  have runs the enabled plugin that declares it (`kito-<plugin> <name>
  […]`), with the server's directory in its environment
  (`KITO_SERVER_DIR`).

## Consequences

- A server with no plugin enabled behaves as before: tokens, the
  encrypted socket of phase 8, no HTTP.
- **The server speaks HTTP**, which it did not. It is one more thing to
  keep right (who is signed in, which site a request comes from). It
  answers JSON and redirects only; what is shown comes from plugins.
- **Two kinds of token**: those an administrator hands out, which last
  until the person is removed, and session tokens, which end. Removing a
  person ends both.
- A sign-in plugin that is wrong lets the wrong person in as somebody
  the server knows: it is trusted with that, and with nothing else. What
  that somebody may do is still the server's word.
- Groups of a provider (a GitHub team) giving roles by themselves is
  not done: a person is added by an administrator. It can come as
  identities with groups, mapped in the server's configuration, without
  changing what a plugin is.
- Not done here: plugins in a directory connected to a server (a plugin
  still runs where the repository is, or on the server); a server that
  signs people in for `ssh` sessions; a registry of plugins.

## The plugins this is for

In repositories of their own:

- **`github`** (`KitoOrg/kito-github`): sign-in with GitHub, by a code
  typed at github.com from the command line and by a redirect in a
  browser; optionally only for members of an organization.
- **`web`** (`KitoOrg/kito-web`): `serve-pages`, with signing in and
  the lanes of every repository a person may use.

## Order of work

One phase (20), released as 0.21.0, in three parts:

1. server plugins enabled in `server.toml`, their commands and the
   server's events;
2. the HTTP address, `/-/api`, and pages passed on to a plugin;
3. signing in: identities on users, session tokens, `kito connect
   --sign-in`, `/-/sign-in`.

Then `kito-github`, and `kito-web` behind a server.

## Amendment, 2026-11-22: pages without a restart

The person asked that enabling a plugin take effect at once, not when
the server starts again. The serving server now brings the plugins that
serve pages in line with its configuration when it starts, right after
a change from the browser, and every two seconds for a change made on
its machine: one enabled is started, one disabled is stopped (its input
closed, then its process ended), one updated or given other permissions
is started again. Ways to sign in were already read for each request.

## Amendment, 2026-11-22: a way that is not set up is not offered

A plugin that signs people in was listed as soon as it was enabled,
before anybody had given it the provider's application: the sign-in
page offered a button that could only fail. The server now asks each
one `sign-in ready` when it lists them (told the `kind`, as `begin` is:
`browser`), and says the answer with the way in `/-/sign-in`:
`"ready": true`, or `"ready": false` with `"why"`. A client does not
offer a way that is not ready. The plugin answers `{ "ready": true }` or
`{ "ready": false, "why": … }`; one that does not know the question
(it fails, or answers something else) is taken to be ready, so the
plugins that exist keep working as they did.
