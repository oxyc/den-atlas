#!/usr/bin/env bash
# Cut a release, and refuse to cut a broken one.
#
#   scripts/release.sh 0.37.0
#
# Tagging by hand is how three releases in this fleet built nothing. The failure is quiet in a specific way:
# CI gates on `cargo fmt --all --check`, a tag that fails it produces NO image, and `den-update` then reports
# "already at <digest>" — which reads as "nothing to deploy" rather than "your build broke". The tag exists,
# the deploy command succeeds, and the box keeps serving the old binary.
#
# So this checks first and tags second, and then waits for the build rather than assuming it.
set -euo pipefail

version="${1:-}"
[ -n "$version" ] || { echo "usage: scripts/release.sh <version>   e.g. 0.37.0" >&2; exit 1; }
case "$version" in
    v*) echo "error: give the version without the leading v ($version -> ${version#v})" >&2; exit 1 ;;
    *.*.*) ;;
    *) echo "error: '$version' is not a semver version" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."
repo="${DEN_ATLAS_REPO:-oxyc/den-atlas}"

git diff --quiet || { echo "error: working tree has uncommitted changes" >&2; exit 1; }
git fetch --quiet origin
# Someone else may have released while you were working; this repo is shared between sessions.
git merge-base --is-ancestor origin/main HEAD \
    || { echo "error: origin/main has moved — rebase onto it before releasing" >&2; exit 1; }
git rev-parse "v$version" >/dev/null 2>&1 && { echo "error: tag v$version already exists" >&2; exit 1; }

# FORMATTING, THE WAY CI CHECKS IT. `cargo fmt` is unavailable on some machines here (no rustfmt component,
# no rustup), and "command not found" reads as "not applicable" — which is how this was skipped. nix runs the
# real thing against this repo's own rustfmt.toml.
echo "==> formatting"
if cargo fmt --all --check 2>/dev/null; then
    :
elif command -v nix >/dev/null 2>&1; then
    # shellcheck disable=SC2046
    nix run nixpkgs#rustfmt -- --edition 2021 --check $(git ls-files '*.rs')
else
    echo "error: no rustfmt. Install the component, or install nix and re-run" >&2
    exit 1
fi

echo "==> tests"
cargo test --quiet

echo "==> version"
current="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
[ "$current" != "$version" ] || { echo "error: Cargo.toml is already $version" >&2; exit 1; }
sed -i.bak "0,/^version = \"$current\"/s//version = \"$version\"/" Cargo.toml && rm -f Cargo.toml.bak
cargo build --quiet   # refresh Cargo.lock's own version entry
git add Cargo.toml Cargo.lock
git commit --quiet -m "den-atlas $version"

echo "==> pushing"
git push --quiet origin HEAD:main
git tag -a "v$version" -m "den-atlas $version"
git push --quiet origin "v$version"

# WAIT FOR THE IMAGE. Without this the script would end exactly where the old mistake began: a pushed tag,
# and no idea whether anything was built from it.
if ! command -v gh >/dev/null 2>&1; then
    echo "note: gh not found — check the build yourself: https://github.com/$repo/actions"
    exit 0
fi
echo "==> waiting for docker-publish"
for _ in $(seq 1 80); do
    read -r status conclusion <<<"$(gh run list --repo "$repo" --limit 1 --json status,conclusion \
        --jq '.[0] | "\(.status) \(.conclusion // "-")"')"
    [ "$status" = "completed" ] && break
    sleep 15
done
if [ "$status" != "completed" ]; then
    echo "note: still building after 20 minutes — check: gh run list --repo $repo"
    exit 0
fi
if [ "$conclusion" != "success" ]; then
    echo "error: the build for v$version FAILED ($conclusion) — no image exists, so there is nothing to" >&2
    echo "       deploy. den-update will say the box is already at the previous digest; that is this," >&2
    echo "       not a no-op. See: gh run view --repo $repo --log-failed" >&2
    exit 1
fi
echo "v$version built. Deploy with:"
echo "  ssh root@pve 'incus exec den -- env TUF_ROOT=/var/lib/den/sigstore den-update den-atlas'"
