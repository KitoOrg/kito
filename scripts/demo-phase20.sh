#!/usr/bin/env bash
# Phase 20 demo: plugins for the server.
#
# The plugin is `kito-hello`, the smallest there is (an example of
# kito-cli). On a server it is a command, a listener, a page, and a
# provider that does not exist: whoever a file names has signed in.
#
# 1. A server enables the plugin, with what a server gives.
# 2. Its command is a command of kito-server.
# 3. A person is added by who the plugin says they are: no token.
# 4. They connect by signing in, and the command line waits for them.
# 5. A browser signs in the same way, and gets a page from the plugin.
# 6. The plugin was told what happened.
#
# The server here is in the clear, on this machine (--plain): a real one
# encrypts both of its addresses with its certificate.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   cargo build --release -p kito-cli --example kito-hello
#   scripts/demo-phase20.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="${KITO_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release}"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-the server}"
DEMO="$(mktemp -d)"
SERVER="$DEMO/server"
DESK="$DEMO/desk"
export KITO_CONFIG_DIR="$DEMO/config"
SERVING=""
stop() {
  [ -n "$SERVING" ] && kill "$SERVING" 2>/dev/null || true
  kito --repo "$DESK" daemon stop >/dev/null 2>&1 || true
  kito --repo "$SERVER/repos/shop" daemon stop >/dev/null 2>&1 || true
  rm -rf "$DEMO"
}
trap stop EXIT

# The demo's own directories, ports and secrets are not part of what it shows.
tidy() {
  sed -e "s#$DEMO#/demo#g" -e "s#127\.0\.0\.1:[0-9]*#127.0.0.1:PORT#g" \
    -e "s#kitos_[0-9a-f]*#kitos_…#g" -e "s#sha256:[0-9a-f:]*#sha256:…#g"
}
run() { printf '\n$ %s\n' "$*" | tidy; "$@" | tidy; }
# A command that is expected to be refused.
refused() { printf '\n$ %s\n' "$*" | tidy; "$@" 2>&1 | tidy || true; }

# The plugin as a download would leave it: its program and its manifest.
mkdir -p "$DEMO/kito-hello"
cp "$BIN/examples/kito-hello" "$DEMO/kito-hello/"
cat > "$DEMO/kito-hello/kito-plugin.toml" <<'TOML'
name = "hello"
version = "0.6.0"
api = 1
permissions = ["read", "sign_in", "pages", "server_events", "server_admin"]

[sign_in]
method = "hello"
about = "Sign in with hello"

[pages]
mount = "/"

[[server_event]]
kinds = ["user_added", "signed_in", "sign_in_refused", "session_ended"]

[[server_command]]
name = "greet"
about = "Says where it runs"
TOML

# A project for the server to keep.
mkdir -p "$DEMO/src"
printf 'export const price = 1;\n' > "$DEMO/src/shop.ts"
git -C "$DEMO/src" init -q -b main
git -C "$DEMO/src" add -A
git -C "$DEMO/src" -c user.name=ada -c user.email=ada@example.com commit -q -m start

kito-server init "$SERVER" >/dev/null
kito-server --dir "$SERVER" repo add shop --from "$DEMO/src" >/dev/null

echo "== 1. A server enables the plugin"
run kito-server --dir "$SERVER" plugin add --path "$DEMO/kito-hello" | tidy
run kito-server --dir "$SERVER" plugin enable hello | tidy
run kito-server --dir "$SERVER" plugin list | tidy

echo
echo "== 2. Its command is a command of kito-server"
run kito-server --dir "$SERVER" greet everybody | tidy

echo
echo "== 3. A person is who the plugin says they are"
run kito-server --dir "$SERVER" user add bob --identity hello:bob --role write
refused kito-server --dir "$SERVER" user add ghost --identity hello:nobody
run kito-server --dir "$SERVER" user list

kito-server --dir "$SERVER" serve --plain --listen 127.0.0.1:0 --http 127.0.0.1:0 > "$DEMO/serving.log" 2>&1 &
SERVING=$!
for _ in $(seq 1 100); do
  [ "$(grep -c '^kito-server: ' "$DEMO/serving.log" || true)" -ge 2 ] && break
  sleep 0.1
done
ADDRESS="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving.log")"
HTTP="$(sed -n 's#^kito-server: \(http://[^,]*\),.*#\1#p' "$DEMO/serving.log")"

echo
echo "== 4. Connecting by signing in"
# bob does what he is asked, two seconds later.
(sleep 2; mkdir -p "$SERVER/plugins/hello"; echo bob > "$SERVER/plugins/hello/sign-in") &
printf '\n$ kito connect 127.0.0.1:PORT shop desk --plain --sign-in hello\n'
kito connect "$ADDRESS" shop "$DESK" --plain --sign-in hello 2>&1 | tidy
wait $!
run kito --repo "$DESK" repos
printf '\n$ grep -v token desk/.kito/remote.toml\n'
grep -v '^token' "$DESK/.kito/remote.toml" | tidy

echo
echo "== 5. A browser signs in the same way"
printf '\n$ curl http://127.0.0.1:PORT/-/sign-in\n'
curl -s "$HTTP/-/sign-in"; echo
printf '\n$ curl -i http://127.0.0.1:PORT/-/sign-in/hello\n'
curl -s -i "$HTTP/-/sign-in/hello" | tr -d '\r' | grep -iE '^(HTTP|location|set-cookie)' | tidy
# The provider sends the browser back, with what it left with.
printf '\n$ curl -i --cookie kito_sign_in=left-1 "http://127.0.0.1:PORT/-/sign-in/hello/callback?code=bob&state=left-1"\n'
BACK="$(curl -s -i --cookie kito_sign_in=left-1 "$HTTP/-/sign-in/hello/callback?code=bob&state=left-1" | tr -d '\r')"
echo "$BACK" | grep -iE '^(HTTP|set-cookie: kito_session)' | tidy
SESSION="$(echo "$BACK" | sed -n 's/^set-cookie: kito_session=\([^;]*\);.*/\1/p')"
printf '\n$ curl --cookie kito_session=kitos_… http://127.0.0.1:PORT/-/me\n'
curl -s --cookie "kito_session=$SESSION" "$HTTP/-/me"; echo
printf '\n$ curl --cookie kito_session=kitos_… http://127.0.0.1:PORT/\n'
curl -s --cookie "kito_session=$SESSION" "$HTTP/" | tidy; echo
# Somebody the provider knows and the server does not.
printf '\n$ curl --cookie kito_sign_in=left-1 "http://127.0.0.1:PORT/-/sign-in/hello/callback?code=eve&state=left-1"\n'
curl -s --cookie kito_sign_in=left-1 "$HTTP/-/sign-in/hello/callback?code=eve&state=left-1"; echo
printf '\n$ curl -X DELETE --cookie kito_session=kitos_… http://127.0.0.1:PORT/-/session\n'
curl -s -X DELETE --cookie "kito_session=$SESSION" "$HTTP/-/session"; echo

echo
echo "== 6. The plugin was told"
run cat "$SERVER/plugins/hello/events.log"
