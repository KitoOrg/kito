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

## Amendment, 2026-11-23: how a person signs in can be changed

A person added with a token could not be given an identity later, and
one added by identity had no token: they were removed and added again,
and lost what they had been given. The person asked that it can be
changed, and decided how: an identity is linked to somebody who has a
token, or unlinked; a new token is issued, and the old one stops; a
token is taken away; a person may have a token and identities at once;
admins change it for others (only an owner for an admin or an owner, as
for roles), and each person for themselves.

- **A person has a token, identities, or both: never nothing.** What
  would leave somebody with no way to sign in is refused, and says what
  to do first (`bob would have no way to sign in: give them a token
  first`). An identity counts while the server offers its way: with the
  plugin that signs people in with `github` disabled, `github:bob` signs
  nobody in, and bob's token is not taken away for it (`github:bob is
  all they would have, and this server offers that way no more`). One
  identity is one person's, and a person has at most one of each way to
  sign in (`unlink it first`).
- **On the server's machine**: `kito-server user link <name>
  <method:login>` (the plugin says who the login is, as for `user add
  --identity`), `user unlink <name> <method>`, `user token <name>` (a
  new one, printed once) and `user token <name> --remove`. `user list`
  says how each person signs in, `[token, github:bob]`, never a secret.
- **Requests**, in the family of the server's people (ADR 0014): asked
  through a client's connection and through `POST /-/people` alike, the
  person named by `user` as in the others.

  ```text
  server.people_link   { user, identity: "github:bob" }  →  { user, token: true|false, logins: […] }
  server.people_unlink { user, method: "github" }        →  the same
  server.people_token  { user }                          →  { user, token: "kito_…" }
  server.people_token  { user, remove: true }            →  { user, token: null }
  ```

  The new token is said this once, to whoever asked: no other request
  answers a token, and none is ever logged. `server.people` says `token:
  true|false` for each person, and `/-/me` says `signs_in: { token,
  logins }` to whoever asks. A plugin that asks as itself is refused all
  three, as the rest of the family (ADR 0020): asked with nobody named,
  it would be the token of whoever is called as the plugin is.
- **Who may ask.** For somebody else, the rule of roles: an admin for a
  member, an owner for anybody. For oneself (`user` left out, or one's
  own name), anybody asks for a new token, takes their token away and
  unlinks an identity, whatever their role: whoever holds what a person
  signs in with is that person already. **Nobody links themselves by
  naming a login**, unless they could change that person anyway. A
  login named proves nothing: whoever held bob's token for a minute
  would attach an account of their own to bob, and be bob for good.
  (They can all the same, by signing in to it from a browser with a
  session that token opened: see "a token that leaked", below.)
- **Who asks is looked at again where the change is made**, on the
  configuration as it is then (ADR 0014, amended): what the request
  came with is theirs still (their token was not changed or taken away,
  their session has not ended, they were not removed), and they still
  may change that person. A request that began a moment before its
  token was taken away gets no new token for it, and a link by name is
  not made for an admin when the person became an owner while the
  plugin was asked who the login is.
- **An account of one's own is linked with the provider's word.** `GET
  /-/sign-in/<method>?link=1`, from somebody signed in in that browser
  (the session cookie), starts the flow of signing in. What is kept for
  the way back is in the state cookie, after the plugin's state: a
  value made for this link and no other, 32 random bytes as a session's
  token is. The server keeps what the value hashes to, with the session
  the link was asked from, for the ten minutes the cookie lasts. No
  page reads or writes that cookie (`HttpOnly`). At the callback the
  value is hashed and looked up, and names the session: it is taken out
  as it is looked up, so that it brings a browser back once, whatever
  follows; it ends with its session; and a link asked again forgets the
  one before. What the server keeps on its disk, and so what every
  backup has, brings nobody back: not what a session's token hashes to,
  not what the value does. The identity that comes back is attached to
  the person of that session, if the session is there still; no session
  is opened; a small page says so and goes on to `/`, as the one of
  signing in does. The way back starts on the provider's site, so the
  session's own cookie (`SameSite=Strict`) may not come with it: the
  session is known by the value, and a session the browser does send
  must be that one. An identity that is theirs already changes nothing,
  and goes on. When it cannot be linked (no link waits for the value;
  it is another person's; they have one of that way; they are no longer
  signed in; they said no at the provider), nothing changes, and a page
  says why in words, with a link to `/`: a person reads it. Without
  `link=1` the flow is what it was; a `link=1` from nobody signed in is
  refused.
- **The sign-in cookie is this host's own when the server is served
  securely.** It is then named `__Host-kito_sign_in`, which a browser
  takes only with `Secure`, for every path (`Path=/`) and with no
  `Domain`: a host beside the server under the same domain cannot plant
  one. In the clear (this machine, `--plain`) it keeps its name,
  `kito_sign_in`, for `/-/sign-in`. It is set, read and forgotten under
  the one name in each case. Sent for every path, it would reach the
  pages too: the server passes none of its own cookies on to a plugin,
  the session's as before and this one, and lets a plugin set neither.
- **What a change ends.** Linking ends nothing, and neither does a first
  token: nothing was taken away. A new token in place of one, a token
  taken away and an identity unlinked each end, at once:
  - every session of that person (browsers, `kito sign-in`) but the one
    the request came through. A session does not say what it was opened
    with, and one opened with what was just taken away, perhaps by
    somebody else, must not go on;
  - every connection of a client opened with a token that is no longer
    theirs. A connection remembers what it was opened with, as what it
    hashes to, and is refused at its next request (`bob's token was
    changed: connect again with the new one`). One opened with a session
    ends with its session. The connection that asked for its own new
    token is told it, and goes on; one that SSH opened was opened with
    no token, and goes on.

  The sessions and the configuration are changed under one lock, by
  one writer at a time (ADR 0014, amended). **The sessions end first,
  and the configuration is written after.** When the sessions cannot
  be ended nothing changes: the token is the one it was, and whoever
  asked is told. When the sessions ended and the configuration could
  not be written, the person signs in again with what they had: nobody
  is locked out, and no session goes on that should not. A session
  opens only if what proved who somebody is, is theirs still when the
  session is written, under the same lock: one opened at the same
  moment is ended by the change, or is not opened. What stays possible
  is a token that was written and never told: the server stopped, or
  the connection was lost, between the write and the answer. The server
  keeps no token, so it cannot say it again; an admin gives another.
- **A person is not removed while their sessions could not be ended**:
  they end in the same change, before the person goes, so that none is
  left for a later person of that name. And when what the server keeps
  cannot be held for a change (something holds it and does not let go),
  that is said to whoever asked, never passed over: a sign-out that
  could not be made answers so, and the browser keeps its cookie.
- **A person has at most 64 sessions**: opening one more ends their
  oldest. The sessions are written whole at every change, and are not
  one person's to make slow for everybody.
- **Events and the log**: `identity_linked` (with the `identity`),
  `identity_unlinked`, `token_changed` (with `replaced`) and
  `token_removed`, each with the `user`, `what`, and who did it (`by`)
  when it was not done on the server's machine. Then `session_ended`,
  once for each session that ended. Never a token. Each is kept in the
  audit log before whoever asked is answered, and told to the plugins
  that listen once the answer is on its way, in the order it happened:
  a plugin that is slow holds no token back. The same holds for the
  token of somebody just added (`user_added`); on the server's machine
  a token is printed before any plugin hears of it. A server run for
  one client (`stdio`, as SSH runs it) tells the plugins before it
  leaves.
- **What cannot be checked is refused.** On the port of the clients a
  connection is held at every request to the configuration as it is
  then. When `server.toml` cannot be read, an open connection is not
  answered on what it knew, and a new one is not let in on what was
  read when the server started: `the server's configuration cannot be
  read just now: try again`.
- **From a connected directory**: `kito people link <name>
  <method:login>`, `kito people unlink [<name>] <method>`, `kito people
  token [<name>] [--remove]`; with no name, oneself. A new token of
  one's own is written in the directory when a token is what it connects
  with (not a session), so that it keeps working, and its daemon is
  started again. It is printed once either way: the other directories
  connected with the old one need it too, and are connected again with
  it (`kito connect`, which starts the daemon of a directory that had
  one again).
- **How a client connects is said to whoever is signed in.** `/-/me`
  carries `clients: { port, plain, fingerprint }`: the port the server
  listens on for clients, whether it encrypts, and the fingerprint its
  certificate is known by (none when it does not encrypt, or when an
  authority vouches for it). They are the three facts `server.people_add`
  answers to an admin, and none is a secret: the port answers anybody,
  and the fingerprint is of the certificate the server presents to
  whoever connects. A page needs them to write a `kito connect` line
  for somebody who is not an admin, with the token they just asked for.

What follows from it:

- Whoever holds a person's token can change it, take it away and unlink
  that person's identities. A token was that person already; an admin,
  or the server's machine, gives a new one.
- A change made for oneself from a browser keeps that browser signed
  in, even when its session was opened with what is taken away: the one
  who asks is trusted to be the one who is cleaning up.
- Anybody now causes `server.toml` to be written, with their own token.
  Every change of it is made alone, on the file as it is at that
  moment: nobody who writes brings back what was changed since they
  read it, a token taken away least of all. That is a rule of the
  server's configuration, not only of signing in: ADR 0014, amended
  2026-11-23, states it.
- A link is finished in the browser it was begun in, within ten
  minutes, and once. A browser that comes back with a value that was
  used starts again at `/-/sign-in/<method>?link=1`.
- The plugins hear of a change of how somebody signs in once it was
  made and answered: after the fact, as every event is told, and for
  these after the answer too. The audit log has it before either.

## Amendment, 2026-11-23: a token that leaked

Found when 0.39.3 was reviewed a second time. The amendment above says
that nobody linking themselves by naming a login stops "whoever held
bob's token for a minute". It does not: a token is the person. Whoever
holds bob's token opens a session with it in a browser, goes to
`/-/sign-in/<method>?link=1`, signs in to an account of their own, and
is told that whoever signs in as it is bob from now on (an identity of
that way bob had, they unlink first: that is bob's to ask). When bob's
token is then replaced, the sessions end and the identity stays, and
the stranger signs in as bob. A person who has only a token must be able
to link an account, so the flow stays. What is true, and what changes:

- **A token is the person, and so is an account linked with it.**
  Replacing a token does not detach what was attached with it.
- **A new token says how the person signs in afterwards**, wherever it
  is asked: `kito-server user token` and `kito people token` print it as
  `link` and `unlink` do (`bob signs in with a token or as
  github:mallory.`), and while an identity remains they add a line: if
  the token was replaced because somebody else had it, look at each
  identity and unlink what is not theirs, or give a token alone.
  `server.people_token` answers `logins` beside `token`.
- **A token alone, for one that leaked**: `kito-server user token
  <name> --alone`, `kito people token [<name>] --alone`,
  `server.people_token { user, alone: true }`, for whoever may give the
  person a new token. A new token; every identity of the person
  unlinked, whoever linked it; every session of theirs ended but the one
  the request came through: one change. It is refused together with
  `--remove` (`remove: true`).

  ```text
  server.people_token  { user, alone: true }  →  { user, token: "kito_…", logins: [], unlinked: ["github:mallory"] }
  ```

  Kept and told: `token_changed` with `alone: true` and `unlinked`, whose
  line names them (`bob, alone: github:mallory unlinked`); then
  `identity_unlinked` for each, and `session_ended` for each session.
- **A link made in a browser says through what it came.** A session
  keeps what it was opened with (`a token`, `github:bob`), when, and the
  address it was asked from. `identity_linked` from a browser keeps in
  the audit log through which person's session the link came, how and
  when that session was opened, and where the browser came back from:
  `bob: github:mallory, in a browser at 198.51.100.7, through a session
  of bob opened with a token on 2026-11-23 10:12:05 UTC from
  203.0.113.9`. The plugins hear `what` as before, and in `how` the same
  without the addresses, which stay in the log. A
  session opened before 0.39.5 says it was `opened before sessions said
  how`. The address is the connection's: behind a proxy, the proxy's.

What follows from it: whoever replaces a token because it leaked gives
it alone, or reads the identities the answer names and unlinks what is
not the person's. And since a token is the person, whoever holds it can
ask for a token alone first, through a session they opened with an
identity of their own, and lock the person out: an admin puts that
right with `--alone` too, which gives the person a token of their own
again and takes the stranger's identity away. The audit log tells, afterwards, that an account was
linked through a session opened with a token, and when.
