#!/usr/bin/env bash
# Offline production-path tests for fetch-dataset.sh. A loopback release and a cosign verifier shim keep
# the trust, interruption and retry cases deterministic without GitHub, Fulcio or Rekor access.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/fetch-dataset.sh"
ROOT="$(mktemp -d)"
REL="$ROOT/release"
WORK="$ROOT/work"
SHIMS="$ROOT/shims"
mkdir -p "$REL" "$WORK/data" "$SHIMS"
SERVER_PID=""
trap 'if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi; rm -rf "$ROOT"' EXIT

python3 - "$REL" "$ROOT/port" <<'PY' &
import http.server, os, socketserver, sys
os.chdir(sys.argv[1])
class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        if self.path.startswith("/dataset.meta.json.sha256-") and os.path.exists(".proof-status-500"):
            self.send_error(500)
            return
        super().do_GET()
server = socketserver.TCPServer(("127.0.0.1", 0), Quiet)
open(sys.argv[2], "w").write(str(server.server_address[1]))
server.serve_forever()
PY
SERVER_PID=$!
i=0
while [ ! -s "$ROOT/port" ]; do
  i=$((i + 1)); [ "$i" -lt 100 ] || break
  sleep 0.02
done
[ -s "$ROOT/port" ] || { echo "test server did not start" >&2; exit 1; }
BASE="http://127.0.0.1:$(cat "$ROOT/port")"

cat > "$SHIMS/cosign" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[ "$1" = verify-blob ]; shift
bundle="" identity="" issuer="" meta=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --bundle) bundle="$2"; shift 2 ;;
    --certificate-identity) identity="$2"; shift 2 ;;
    --certificate-oidc-issuer) issuer="$2"; shift 2 ;;
    *) meta="$1"; shift ;;
  esac
done
[ "$identity" = "https://github.com/oxyc/den-dataset/.github/workflows/daily.yml@refs/heads/main" ]
[ "$issuer" = "https://token.actions.githubusercontent.com" ]
read -r signed_sha verdict < "$bundle"
[ "$verdict" = valid ]
[ "$signed_sha" = "$(shasum -a 256 "$meta" | cut -d' ' -f1)" ]
EOF
chmod +x "$SHIMS/cosign"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok   - $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL - $1" >&2; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$3], got [$2]"; fi; }

clear_release() { find "$REL" -mindepth 1 -delete; }
write_release() {
  local version="$1" body="$2" sum
  clear_release
  printf '%s' "$body" > "$REL/store.bin"
  sum="$(shasum -a 256 "$REL/store.bin" | cut -d' ' -f1)"
  printf '{"datasetVersion":"%s","storeFile":"store.bin","storeSha256":"%s"}' "$version" "$sum" > "$REL/dataset.meta.json"
}
manifest_sha() { shasum -a 256 "$REL/dataset.meta.json" | cut -d' ' -f1; }
sign_immutable() {
  local digest="${1:-$(manifest_sha)}" verdict="${2:-valid}"
  printf '%s %s\n' "$digest" "$verdict" > "$REL/dataset.meta.json.sha256-$(manifest_sha).bundle"
}
sign_fixed() { printf '%s %s\n' "${1:-$(manifest_sha)}" "${2:-valid}" > "$REL/dataset.meta.json.bundle"; }
seed_old() {
  rm -rf "$WORK/data"; mkdir -p "$WORK/data"
  printf 'old-store' > "$WORK/data/store-old.bin"
  printf '{"datasetVersion":"old","storeFile":"store-old.bin"}' > "$WORK/data/dataset.meta.json"
}
run_fetch() {
  RC=0
  (cd "$WORK" && PATH="$SHIMS:$PATH" DEN_COSIGN="$SHIMS/cosign" DEN_DATASET_BASE="$BASE" \
    bash "$SCRIPT") >"$ROOT/out" 2>"$ROOT/err" || RC=$?
}
preserved_old() {
  [ "$(cat "$WORK/data/store-old.bin" 2>/dev/null)" = old-store ] &&
    grep -q '"datasetVersion":"old"' "$WORK/data/dataset.meta.json"
}

# Valid immutable proof: authenticate, verify and publish the matching generation.
seed_old; write_release v1 new-store; sign_immutable; run_fetch
check "valid immutable proof succeeds" "$RC" 0
check "valid release installs its store" "$(cat "$WORK/data/store.bin" 2>/dev/null)" new-store

# Migration compatibility is deliberately narrow: only an absent digest proof falls back to the fixed name.
seed_old; write_release v2 fixed-proof; sign_fixed; run_fetch
check "missing immutable proof uses the fixed migration proof" "$RC" 0
check "fixed proof installs its matching release" "$(cat "$WORK/data/store.bin" 2>/dev/null)" fixed-proof

# The verifier receives the pinned identity, and rejects a proof whose certificate verdict is wrong.
seed_old; write_release v3 wrong-identity; sign_immutable "$(manifest_sha)" wrong; run_fetch
check "wrong workflow identity is rejected" "$RC" 1
preserved_old && ok "wrong identity preserves last-good data" || bad "wrong identity changed last-good data"

# A transient/error response is not absence: fixed-name fallback is allowed only for an actual 404.
seed_old; write_release v3b server-error; sign_fixed; touch "$REL/.proof-status-500"; run_fetch
check "immutable proof server error does not use migration fallback" "$RC" 1
preserved_old && ok "proof fetch error preserves last-good data" || bad "proof fetch error changed last-good data"

# A proof for earlier bytes cannot authenticate an altered fixed-name manifest.
seed_old; write_release v4 original; original_sha="$(manifest_sha)"; sign_immutable
printf ' ' >> "$REL/dataset.meta.json"
mv "$REL/dataset.meta.json.sha256-$original_sha.bundle" \
  "$REL/dataset.meta.json.sha256-$(manifest_sha).bundle"
run_fetch
check "altered manifest is rejected" "$RC" 1
preserved_old && ok "altered manifest preserves last-good data" || bad "altered manifest changed last-good data"

# No immutable or migration proof is an authenticated-publication failure, not an unsigned upgrade.
seed_old; write_release v5 unsigned; run_fetch
check "missing proof is rejected" "$RC" 1
preserved_old && ok "missing proof preserves last-good data" || bad "missing proof changed last-good data"

# A proof can arrive before every named blob. Keep the old generation, then accept the same manifest safely
# when the interrupted publication completes on a later invocation.
seed_old; write_release v6 completed-later; sign_immutable
mv "$REL/store.bin" "$ROOT/held-store.bin"
run_fetch
check "interrupted generation is rejected" "$RC" 1
preserved_old && ok "interrupted generation preserves last-good data" || bad "interruption changed last-good data"
mv "$ROOT/held-store.bin" "$REL/store.bin"
run_fetch
check "completed generation retries successfully" "$RC" 0
check "retry installs the authenticated generation" "$(cat "$WORK/data/store.bin" 2>/dev/null)" completed-later

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
