# 0013 · Connecting by one address, signed in in the browser

Status: accepted
Date: 2026-11-22

## Context

Connecting a directory to a server took the server, the repository, a
token or the name of a way to sign in, and the fingerprint of the
server's certificate: four things to copy. The person asked on
2026-11-22 for `kito connect <url of repo>`, with signing in in the
browser or a token, opened by itself, as GitHub's tools do; and for
`kito clone` as the same command.

## Decision

- **One address**: `kito connect host[:port]/<repository>` or
  `kito://host[:port]/<repository>`; 7421, the server's own default, when
  no port is said. The old form, the server and then the repository,
  stays, and is the only one for `ssh://`. An `http(s)://` address is the
  web client's, and is refused with the right form.
- **`kito clone`** is another name of `kito connect`.
- **Who the server is**, when nothing is said: a server on this machine
  by the certificate it presents (as before); one whose certificate a
  public authority vouches for, by its name; otherwise, at a terminal,
  the fingerprint is shown and the person says whether it is the server
  meant, once, as ssh asks of a host it does not know; it is kept. With
  nobody at the terminal it is refused, as before, with the flag to give.
- **Who the person is**, when no token and no `--sign-in` is given:
  Kito asks the server which ways to sign in it offers. At a terminal the
  person picks one, or pasting a token (read without showing it); with
  nobody at the terminal, the only way there is is taken, and more than
  one, or none, is refused with what to say.
- **The browser opens by itself** on the page a way to sign in gives
  (`$BROWSER`, `open`, `start`, `wslview` or `xdg-open`), when somebody is
  at the terminal; the page and the code are printed as well.
  `KITO_NO_BROWSER=1` opens nothing. `kito sign-in` does the same.
- `kito-server user add` prints the line in the new form.

## Consequences

- Connecting is `kito connect kito.example.com/shop`, then a browser.
- Trusting a fingerprint at the terminal is the person's word, as with
  ssh: the line printed by `user add` still carries it, for whoever wants
  it said.
- No change to what a connected directory keeps (`remote.toml`).
