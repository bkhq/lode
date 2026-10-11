#!/usr/bin/env bash
# Smoke test of the cross-platform surface: lode-cli and `lode <args>` passthrough.
# Runs on Linux, macOS and Windows (Git Bash). Usage: cli.sh <dir with lode + lode-cli>
set -euo pipefail

bin="$(cd "$1" && pwd)"
release_keys="${SMOKE_RELEASE_KEYS:+$(cd "$(dirname "$SMOKE_RELEASE_KEYS")" && pwd)/$(basename "$SMOKE_RELEASE_KEYS")}"
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) exe=".exe" windows=1 ;;
  *) exe="" windows=0 ;;
esac
lode="$bin/lode$exe" cli="$bin/lode-cli$exe"
export NO_COLOR=1

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

fail() {
  echo "smoke: FAIL: $*" >&2
  exit 1
}
step() { echo "smoke: $*"; }
# expect <pattern> <command...>: the command must succeed and print <pattern>.
expect() {
  local pattern="$1" out
  shift
  out="$("$@" 2>&1)" || fail "$* exited $?: $out"
  echo "$out" | grep -q -- "$pattern" || fail "$* did not print \"$pattern\": $out"
}

step "version"
expect '^lode-cli ' "$cli" --version
expect '^lode ' "$lode" --version

step "keygen, sign, verify"
"$cli" keygen --out key >keygen.out
public="$(awk '/^public:/ {print $2}' keygen.out)"
printf 'payload\n' >asset.bin
"$cli" sign asset.bin --app demo --version 1.0.0 --key key.key >sign.out
sig="$(awk '/^sig:/ {print $2}' sign.out)"
expect 'signature: OK' "$cli" verify asset.bin --app demo --version 1.0.0 --pubkey "$public" --sig "$sig"
if "$cli" verify asset.bin --app demo --version 1.0.1 --pubkey "$public" --sig "$sig" | grep -q 'signature: OK'; then
  fail "a signature for 1.0.0 verified as 1.0.1"
fi
if "$cli" verify asset.bin --app other --version 1.0.0 --pubkey "$public" --sig "$sig" | grep -q 'signature: OK'; then
  fail "a signature for the app demo verified for the app other"
fi

step "manifest"
"$cli" manifest asset.bin --app demo --version 1.0.0 --url https://example.com/asset.bin \
  --key key.key --into manifest.json
grep -q '"lode/v1"' manifest.json || fail "manifest schema"
"$cli" manifest-sign --into manifest.json --key key.key

step "seed, versions, status, rollback, restart"
if [ "$windows" = 1 ]; then
  app="app.cmd"
  printf '@echo off\r\necho app %%LODE_ACTIVE_VERSION%% args=%%*\r\nexit /b 7\r\n' >"$app"
else
  app="app.sh"
  printf '#!/bin/sh\necho "app $LODE_ACTIVE_VERSION args=$*"\nexit 7\n' >"$app"
  chmod +x "$app"
fi
data="$work/data"
expect 'installed 1.0.0' "$cli" --dir "$data" seed "$app" --version 1.0.0
expect 'activated 1.1.0' "$cli" --dir "$data" seed "$app" --version 1.1.0
expect '^\* 1.1.0' "$cli" --dir "$data" versions
expect '"current": "1.1.0"' "$cli" --dir "$data" status
expect 'target set to 1.0.0' "$cli" --dir "$data" rollback --version 1.0.0
expect 'restart_nonce=1' "$cli" --dir "$data" restart

step "passthrough"
set +e
out="$("$lode" --dir "$data" hello world)"
code=$?
set -e
[ "$code" = 7 ] || fail "passthrough exit status $code, want 7"
echo "$out" | grep -q 'app 1.1.0 args=hello world' || fail "passthrough output: $out"

step "publish refuses to run without a token"
set +e
LODE_REGISTRY_TOKEN= "$cli" publish --package acme/app --version 1.0.0 asset.bin >publish.out 2>publish.err
code=$?
set -e
[ "$code" = 1 ] || fail "publish exit status $code, want 1"
grep -q 'LODE_REGISTRY_TOKEN is not set' publish.err || fail "publish error: $(cat publish.err)"
[ ! -s publish.out ] || fail "publish wrote to stdout"

# SMOKE_RELEASE_TAG names the lode release to install.
if [ -n "${SMOKE_GITHUB_TOKEN:-}" ] && [ -n "${SMOKE_RELEASE_TAG:-}" ]; then
  step "update from GitHub Releases over HTTPS, signature enforced"
  release_key="$(grep -v '^#' "$release_keys" | grep . | head -1)"
  "$cli" --dir "$work/remote" --app lode --github bkhq/lode --asset lode-linux-x64.tar.gz \
    --pin "$SMOKE_RELEASE_TAG" --run ./lode --require-signature enforce --trusted-keys "$release_key" \
    --header "Authorization: Bearer $SMOKE_GITHUB_TOKEN" update | tee update.out
  [ -f "$work/remote/versions/${SMOKE_RELEASE_TAG#v}/lode" ] || fail "the release asset was not extracted"
  grep -q "\"current\": \"${SMOKE_RELEASE_TAG#v}\"" "$work/remote/state.json" || fail "state.json does not record ${SMOKE_RELEASE_TAG#v}"
fi

if [ -n "${SMOKE_RUNTIME:-}" ]; then
  step "runtime: download bun, place it, pin its version, run it through passthrough"
  if [ "$windows" = 1 ]; then bun_zip="bun-windows-x64.zip"; else bun_zip="bun-linux-x64.zip"; fi
  rt="$work/rt"
  mkdir -p "$rt"
  cat >"$rt/lode.toml" <<TOML
[global]
app = "rt"
[update]
policy = "off"
[command]
exec = "bun"
[runtime]
runtime = "bun"
download = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/$bun_zip"
version = "1.4.3"
TOML
  expect 'activated 1.0.0' "$cli" --dir "$rt" seed asset.bin --version 1.0.0
  expect '^1\.4\.3' "$lode" --dir "$rt" -- --version
fi

if command -v node >/dev/null 2>&1; then
  step "supervise: start, switch version, restart, single instance, stop"
  # wait_for <description> <command...>: poll until the command succeeds.
  wait_for() {
    local what="$1" i
    shift
    for i in $(seq 1 150); do
      "$@" >/dev/null 2>&1 && return 0
      sleep 0.2
    done
    echo "--- lode.log" >&2
    cat "$work/lode.log" >&2 || true
    ls "$marks" >&2 || true
    fail "timed out waiting for $what"
  }
  marks="$work/marks"
  svc="$work/svc"
  mkdir -p "$marks" "$svc"
  if [ "$windows" = 1 ]; then marks_native="$(cygpath -m "$marks")"; else marks_native="$marks"; fi
  cat >app.js <<'JS'
const fs = require("fs");
const path = require("path");
const version = process.env.LODE_ACTIVE_VERSION;
const mark = name => fs.writeFileSync(path.join(process.env.MARK_DIR, name), "");
const stop = () => { mark(`stopped-${version}-${process.pid}`); process.exit(0); };
for (const signal of ["SIGTERM", "SIGINT", "SIGBREAK"]) process.on(signal, stop);
mark(`started-${version}-${process.pid}`);
setInterval(() => {}, 1000);
JS
  cat >"$svc/lode.toml" <<TOML
[global]
app = "svc"
[update]
policy = "off"
[command]
run = "node app.js"
[supervise]
health_grace = 2
stop_timeout = 10
[env]
MARK_DIR = "$marks_native"
TOML
  expect 'installed 1.0.0' "$cli" --dir "$svc" seed app.js --version 1.0.0
  expect 'activated 1.1.0' "$cli" --dir "$svc" seed app.js --version 1.1.0
  count() { find "$marks" -name "$1" | wc -l | tr -d ' '; }
  # has <pattern> <n>: exactly n marker files match.
  has() { [ "$(count "$1")" = "$2" ]; }

  "$lode" --dir "$svc" >"$work/lode.log" 2>&1 &
  lode_pid=$!
  wait_for "1.1.0 to start" has 'started-1.1.0-*' 1
  wait_for "status running" grep -q '"status": "running"' "$svc/state.json"

  # A second supervisor on the same directory is refused.
  if "$lode" --dir "$svc" >second.out 2>&1; then fail "a second instance started"; fi
  grep -q 'already running' second.out || fail "second instance: $(cat second.out)"

  # Switching versions stops the old one gracefully and commits the new one.
  expect 'target set to 1.0.0' "$cli" --dir "$svc" rollback --version 1.0.0
  wait_for "1.1.0 to stop gracefully" has 'stopped-1.1.0-*' 1
  wait_for "1.0.0 to start" has 'started-1.0.0-*' 1
  wait_for "1.0.0 to become current" grep -q '"current": "1.0.0"' "$svc/state.json"
  wait_for "1.0.0 to be committed" grep -q '"last_good": "1.0.0"' "$svc/state.json"

  expect 'restart_nonce=1' "$cli" --dir "$svc" restart
  wait_for "1.0.0 to stop for the restart" has 'stopped-1.0.0-*' 1
  wait_for "1.0.0 to start again" has 'started-1.0.0-*' 2

  if [ "$windows" = 1 ]; then
    # Bash cannot send a console event, so lode is killed outright. The job
    # object must take the app down with it.
    app_pid="$(sed -n 's/.*"pid": \([0-9]*\).*/\1/p' "$svc/state.json")"
    kill -9 "$lode_pid" 2>/dev/null || true
    wait "$lode_pid" 2>/dev/null || true
    gone() { ! tasklist //FI "PID eq $app_pid" //NH | grep -q " $app_pid "; }
    wait_for "the app to die with lode" gone
  else
    kill -TERM "$lode_pid"
    set +e
    wait "$lode_pid"
    code=$?
    set -e
    [ "$code" = 0 ] || fail "lode exited $code after SIGTERM"
    [ "$(count 'stopped-1.0.0-*')" = 2 ] || fail "the app was not stopped gracefully on shutdown"
    [ ! -e "$svc/lode.pid" ] || fail "lode.pid was left behind"
  fi
else
  step "supervise: skipped (node not found)"
fi

echo "smoke: OK"
