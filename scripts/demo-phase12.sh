#!/usr/bin/env bash
# Phase 12 demo: encrypted connections and a sandbox per lane.
#
# 1. A server has a certificate of its own; people get its fingerprint
#    with their token.
# 2. A client that is given another fingerprint does not connect.
# 3. What travels between a client and the server cannot be read: the same
#    conversation is listened to on a server that does not encrypt, and on
#    one that does.
# 4. The checks of an integration run code a lane wrote. A lane whose check
#    tries to read another repository of the server, the server's own
#    files and the daemon's key, to write outside its copy and to call
#    out, gets nowhere.
# 5. A command an agent runs in its lane is confined to the lane.
#
# Needs python3 (the listener between client and server) and git.
#
#   cargo build --release -p kitod -p kito-cli -p kito-server
#   scripts/demo-phase12.sh
set -euo pipefail

export KITO_NO_CLAUDE=1
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release"
export PATH="$BIN:$PATH"
export KITO_AUTHOR="${KITO_AUTHOR:-the server}"
DEMO="$(mktemp -d)"
# The servers each client connects to are kept here, not in your own Kito directory.
export KITO_CONFIG_DIR="$DEMO/config"
SERVER="$DEMO/server"
PIDS=()
stop() {
  for dir in ada mallory eve; do kito --repo "$DEMO/$dir" daemon stop >/dev/null 2>&1 || true; done
  for pid in "${PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done
  for repo in shop payroll; do kito --repo "$SERVER/repos/$repo" daemon stop >/dev/null 2>&1 || true; done
  rm -rf "$DEMO"
}
trap stop EXIT
cd "$DEMO"

run() { printf '\n$ %s\n' "$*"; "$@"; }
pause() { read -r -t "$1" _ < /dev/zero || true; }
tidy() { sed "s|$DEMO|/demo|g; s|kito_[0-9a-f]\{64\}|kito_<token>|g"; }
as() { local who="$1"; shift; (cd "$DEMO/$who" && printf '\n(%s) $ %s\n' "$who" "$*" && "$@"); }
serve() { # extra arguments; sets ADDR
  : > "$DEMO/serving"
  kito-server --dir "$SERVER" serve --listen 127.0.0.1:0 "$@" > "$DEMO/serving" 2>/dev/null &
  PIDS+=($!)
  until [ -s "$DEMO/serving" ]; do pause 0.05; done
  ADDR="$(sed -n 's/^kito-server: listening on \([^,]*\),.*/\1/p' "$DEMO/serving")"
}
source_repo() { # name, file, content
  mkdir -p "$DEMO/$1-src/src"
  printf '%s\n' "$3" > "$DEMO/$1-src/src/$2"
  git -C "$DEMO/$1-src" init -q -b main
  git -C "$DEMO/$1-src" add -A
  git -C "$DEMO/$1-src" -c user.name=Grace -c user.email=grace@example.com commit -q -m "start"
}

# Somebody between a client and the server: passes on what it hears, and
# keeps a copy of what the client said.
cat > "$DEMO/listen.py" <<'PY'
import socket, sys, threading
server, heard = sys.argv[1], sys.argv[2]
host, port = server.rsplit(":", 1)
door = socket.socket(); door.bind(("127.0.0.1", 0)); door.listen(8)
print(door.getsockname()[1], flush=True)
def carry(src, dst, keep):
    try:
        while True:
            data = src.recv(65536)
            if not data: break
            if keep:
                with open(heard, "ab") as out: out.write(data)
            dst.sendall(data)
    except OSError: pass
    for s in (src, dst):
        try: s.close()
        except OSError: pass
while True:
    client, _ = door.accept()
    there = socket.create_connection((host, int(port)))
    threading.Thread(target=carry, args=(client, there, True), daemon=True).start()
    threading.Thread(target=carry, args=(there, client, False), daemon=True).start()
PY
listen_between() { # the file to keep what is heard in; sets THROUGH
  : > "$DEMO/listening"
  python3 "$DEMO/listen.py" "$ADDR" "$1" > "$DEMO/listening" &
  PIDS+=($!)
  until [ -s "$DEMO/listening" ]; do pause 0.05; done
  THROUGH="127.0.0.1:$(cat "$DEMO/listening")"
}
heard() { # file, the token: what a listener makes of it
  python3 - "$1" "$2" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
token = sys.argv[2].encode()
text = "".join(chr(b) if 32 <= b < 127 else "." for b in data)
text = text.replace(sys.argv[2], "kito_<the token, in full>")[:150]
print(f"  {len(data)} bytes heard from the client. They begin:")
print("  " + text[:75]); print("  " + text[75:150])
print("  the token is in them:", "YES" if token in data else "no")
PY
}

source_repo shop shop.py 'def total(prices):
    return sum(prices)'
source_repo payroll salaries.py 'SALARIES = {"ada": 5200, "grace": 6100}'

printf '# 1. A server with a certificate of its own\n'
run kito-server init "$SERVER" | tidy
kito-server --dir "$SERVER" repo add shop --from "$DEMO/shop-src" > /dev/null
kito-server --dir "$SERVER" repo add payroll --from "$DEMO/payroll-src" > /dev/null
printf '\n$ kito-server --dir /demo/server user add ada --repo shop\n'
kito-server --dir "$SERVER" user add ada --repo shop > "$DEMO/ada.txt"
tidy < "$DEMO/ada.txt"
ADA="$(grep '^kito_' "$DEMO/ada.txt")"
MALLORY="$(kito-server --dir "$SERVER" user add mallory --repo shop | grep '^kito_')"
PRINT="$(kito-server --dir "$SERVER" fingerprint)"
serve
printf '\n$ kito-server --dir /demo/server serve --listen 127.0.0.1:0 &\n'
tidy < "$DEMO/serving"

printf '\n\n# 2. A client that is given another fingerprint does not connect\n'
WRONG="sha256:$(printf 'ab%.0s' $(seq 32))"
printf '\n$ kito connect %s shop eve --token kito_<token> --fingerprint %s\n' "$ADDR" "$WRONG"
kito connect "$ADDR" shop eve --token "$ADA" --fingerprint "$WRONG" 2>&1 | tidy | fold -s -w 100 || true
printf '\n$ kito connect %s shop ada --token kito_<token> --fingerprint %s\n' "$ADDR" "$PRINT"
kito connect "$ADDR" shop ada --token "$ADA" --fingerprint "$PRINT" | tidy | fold -s -w 100 | head -4

printf '\n\n# 3. What somebody between the two can read\n'
printf '\n(a listener sits between a client and the server, and the client asks for the lanes)\n'
listen_between "$DEMO/heard-encrypted"
mkdir -p "$DEMO/eve/.kito"
printf 'server = "%s"\nrepo = "shop"\ntoken = "%s"\nfingerprint = "%s"\n' "$THROUGH" "$ADA" "$PRINT" > "$DEMO/eve/.kito/remote.toml"
(cd "$DEMO/eve" && kito lanes > /dev/null 2>&1)
printf '\nthis server:\n'
heard "$DEMO/heard-encrypted" "$ADA"
kito --repo "$DEMO/eve" daemon stop > /dev/null 2>&1 || true

printf '\nthe same on a server started with --plain, as every server was before this version:\n'
kill "${PIDS[0]}" 2>/dev/null || true; pause 0.3
serve --plain
listen_between "$DEMO/heard-plain"
printf 'server = "%s"\nrepo = "shop"\ntoken = "%s"\nplain = true\n' "$THROUGH" "$ADA" > "$DEMO/eve/.kito/remote.toml"
(cd "$DEMO/eve" && kito lanes > /dev/null 2>&1)
heard "$DEMO/heard-plain" "$ADA" | tidy
kito --repo "$DEMO/eve" daemon stop > /dev/null 2>&1 || true
kill "${PIDS[@]}" 2>/dev/null || true; PIDS=(); pause 0.3
kito --repo "$DEMO/ada" daemon stop > /dev/null 2>&1 || true

printf '\n\n# 4. The checks of a lane are confined\n'
run kito-server --dir "$SERVER" repo set shop --check "sh check.sh" | tidy
serve
printf 'server = "%s"\nrepo = "shop"\ntoken = "%s"\nfingerprint = "%s"\n' "$ADDR" "$ADA" "$PRINT" > "$DEMO/ada/.kito/remote.toml"
kito connect "$ADDR" shop mallory --token "$MALLORY" --as mallory > /dev/null
as mallory kito status | grep -A1 '^Checks' | tidy
printf '\n(mallory may use `shop`, not `payroll`. Her lane brings a check of its own.)\n'
as mallory kito lane open agent-m --intent "a check for the shop" --scope check.sh | head -3 | tidy
W="$DEMO/mallory/.kito/lanes/1/work"
cat > "$W/check.sh" <<SH
echo "payroll, another repository of this server:"
cat $SERVER/repos/payroll/src/salaries.py
echo "who may connect to this server:"
cat $SERVER/server.toml
echo "the key of this repository's daemon:"
cat $SERVER/repos/shop/.kito/socket.key
echo "writing in the main directory of the shop:"
echo 'def total(prices): return 0' > $SERVER/repos/shop/src/shop.py
echo "calling out:"
python3 -c 'import socket; socket.create_connection(("example.com", 80), 3)' 2>&1 | tail -1
echo "the check itself, in its own copy:"
echo ok > result.txt && cat result.txt
exit 1
SH
printf '\n(mallory) $ cat check.sh\n'; tidy < "$W/check.sh"
as mallory kito integrate 1 2>&1 | tidy | fold -s -w 100 || true
printf '\n$ cat /demo/server/repos/shop/src/shop.py     # as it was\n'
cat "$SERVER/repos/shop/src/shop.py"

printf '\n\n# 5. A command run in a lane is confined to the lane\n'
as mallory kito lane run 1 -- sh -c 'echo "print(41 + 1)" > answer.py && python3 answer.py'
printf '\n(mallory) $ kito lane run 1 -- sh -c "echo x > ../../../../hacked.txt; cat ~/.config/kito/servers.toml"\n'
(cd "$DEMO/mallory" && kito lane run 1 -- sh -c "echo x > ../../../../hacked.txt; cat $KITO_CONFIG_DIR/servers.toml" 2>&1 | tidy) || true
printf '\n(the tokens of the servers are in servers.toml: the command could not read it)\n'
