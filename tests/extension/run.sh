#!/bin/bash
# End-to-end test of the browser extension against an isolated habitd.
# Runs a headless Firefox-based browser with a throwaway HOME and profile.
#
#   tests/extension/run.sh                 # uses $BROWSER, zen or firefox
#   BROWSER=/usr/bin/firefox tests/extension/run.sh
#   EXTENSION=/path/to/other/extension tests/extension/run.sh
set -euo pipefail

HERE=$(dirname "$(readlink -f "$0")")
ROOT=$(readlink -f "$HERE/../..")
BROWSER=${BROWSER:-$(command -v /opt/zen-browser-bin/zen zen-browser firefox 2>/dev/null | head -n1)}
EXTENSION=${EXTENSION:-$ROOT/extension}
PORT=9333

cargo build -q --manifest-path "$ROOT/Cargo.toml" -p habitd -p hf
HF=$ROOT/target/debug/hf

# Short path: unix socket paths are limited to ~108 bytes.
W=$(mktemp -d /tmp/hf-ext.XXXX)
PIDS=()
cleanup() {
  kill "${PIDS[@]}" 2>/dev/null || true
  sleep 0.5
  kill -9 "${PIDS[@]}" 2>/dev/null || true
  rm -rf "$W"
}
trap cleanup EXIT

cat > "$W/config.toml" <<CFG
[general]
notifications = false
[groups.social]
name = "Social"
domains = ["blocked.test"]
requires = ["walk"]
[habits.walk]
name = "Walk"
kind = "manual"
reward = { groups = ["social"], duration = "1m" }
[habits.quick]
name = "Quick"
target = "1s"
reward = { groups = ["social"], duration = "10m" }
[habits.long]
name = "Long"
target = "1h"
reward = { groups = ["social"], duration = "1m" }
CFG

export HOME=$W/home
export HABITFOCUS_CONFIG=$W/config.toml HABITFOCUS_SOCKET=$W/hf.sock XDG_STATE_HOME=$W/state
mkdir -p "$HOME/.mozilla" "$HOME/.zen"
"$HF" install-native-host 2>/dev/null >/dev/null

env -u NIRI_SOCKET "$ROOT/target/debug/habitd" > "$W/habitd.log" 2>&1 & PIDS+=($!)
python3 "$HERE/slow_server.py" & PIDS+=($!)
sleep 1
"$HF" start quick > /dev/null
sleep 2.5

mkdir -p "$W/profile"
cat > "$W/profile/user.js" <<PREFS
user_pref("network.dns.localDomains", "blocked.test");
user_pref("network.trr.mode", 5);
user_pref("xpinstall.signatures.required", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
PREFS
"$BROWSER" --headless --no-remote --profile "$W/profile" --remote-debugging-port $PORT -remote-allow-system-access > "$W/browser.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 40); do
  curl -s "http://127.0.0.1:$PORT/json/version" > /dev/null 2>&1 && break
  sleep 0.5
done

echo "browser: $BROWSER"
node "$HERE/harness.mjs" $PORT "$EXTENSION" "$HF"
