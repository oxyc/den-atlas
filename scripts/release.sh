#!/usr/bin/env bash
# Publish a release without rebuilding or retesting it. Main CI has already built, behavior-probed, and scanned an
# immutable candidate image for its exact SHA; the vX.Y.Z workflow only verifies, signs and promotes that digest.
#
#   scripts/release.sh --prepare 0.94.0  # open the VERSION pull request
#   scripts/release.sh 0.94.0            # after that pull request is merged
#
# Only VERSION changes (Cargo.toml stays put so the cached Rust build survives). Protected main means that change
# must pass through a pull request. Preparation creates that version-only PR; publishing waits for the merged
# commit's exact main CI and tags it only after that succeeds, so a red build leaves no tag that builds nothing.
set -euo pipefail

mode=publish
if [ "${1:-}" = --prepare ]; then
    mode=prepare
    shift
fi
version="${1:-}"
[ -n "$version" ] && [ "$#" -eq 1 ] || {
    echo "usage: scripts/release.sh [--prepare] <version>   e.g. 0.94.0" >&2
    exit 1
}
case "$version" in
    v*) echo "error: give the version without the leading v ($version -> ${version#v})" >&2; exit 1 ;;
    *.*.*) ;;
    *) echo "error: '$version' is not a semver version" >&2; exit 1 ;;
esac
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || { echo "error: '$version' is not X.Y.Z" >&2; exit 1; }

cd "$(dirname "$0")/.."
# From the remote, so nothing here names a repo.
slug="$(git remote get-url origin | sed -e 's#.*[:/]\([^/]*/[^/]*\)$#\1#' -e 's/\.git$//')"
[ -z "$(git status --porcelain)" ] || { echo "error: working tree has uncommitted changes" >&2; exit 1; }
git fetch --quiet origin
main="$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD | sed 's#^origin/##' || echo main)"
[ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/$main")" ] \
    || { echo "error: checkout is not the latest origin/$main — update it before releasing" >&2; exit 1; }
git rev-parse "v$version" >/dev/null 2>&1 && { echo "error: tag v$version already exists" >&2; exit 1; }
command -v gh >/dev/null 2>&1 || {
    echo "error: gh is required to prove exact-main CI before tagging" >&2; exit 1; }

current="$(cat VERSION)"
if [ "$mode" = publish ] && [ "$current" != "$version" ]; then
    echo "error: VERSION is $current; protected $main requires a version pull request first" >&2
    echo "       run: scripts/release.sh --prepare $version" >&2
    echo "       after it merges, update $main and rerun: scripts/release.sh $version" >&2
    exit 1
fi

if [ "$mode" = prepare ]; then
    [ "$current" != "$version" ] || {
        echo "error: VERSION is already $version; update $main after the version PR merges, then run:" >&2
        echo "       scripts/release.sh $version" >&2
        exit 1
    }
    branch="release/$version"
    existing_pr="$(gh pr list --repo "$slug" --head "$branch" --state open --limit 1 --json url --jq '.[0].url // empty')"
    if [ -n "$existing_pr" ]; then
        echo "release preparation already exists: $existing_pr"
        exit 0
    fi
    git show-ref --verify --quiet "refs/heads/$branch" && {
        echo "error: local branch $branch already exists; inspect or remove it before retrying" >&2
        exit 1
    }
    git show-ref --verify --quiet "refs/remotes/origin/$branch" && {
        echo "error: origin/$branch exists without an open PR; inspect it before retrying" >&2
        exit 1
    }
fi

# Formatting goes through nix when this machine has no rustfmt: treating "no such command" as success is how
# broken formatting reached earlier releases.
run_rustfmt() {
    if cargo fmt --all --check 2>/dev/null; then return 0; fi
    command -v nix >/dev/null 2>&1 \
        || { echo "error: no rustfmt, and no nix to borrow one" >&2; return 1; }
    # shellcheck disable=SC2046
    nix run nixpkgs#rustfmt -- --edition 2021 --check $(git ls-files '*.rs')
}
echo "==> formatting"; run_rustfmt
echo "==> tests";      cargo test --locked --workspace --quiet

if [ "$mode" = prepare ]; then
    echo "==> version $current -> $version"
    git switch --quiet -c "$branch"
    printf '%s\n' "$version" > VERSION
    [ "$(cat VERSION)" = "$version" ] || { echo "error: the version bump did not apply" >&2; exit 1; }
    git add VERSION
    git commit --quiet -m "${slug#*/} $version"
    git push --quiet --set-upstream origin "$branch"
    body="$(printf '%s\n\n%s\n' \
        "Prepare ${slug#*/} $version." \
        "After this merges and exact-main CI passes, publish it with \`scripts/release.sh $version\`.")"
    if ! pr_url="$(gh pr create --repo "$slug" --base "$main" --head "$branch" \
        --title "Release $version" --body "$body")"; then
        echo "error: $branch was pushed, but its pull request could not be created" >&2
        echo "       retry: gh pr create --repo $slug --base $main --head $branch" >&2
        exit 1
    fi
    echo "release preparation opened: $pr_url"
    echo "after it merges, update $main and run: scripts/release.sh $version"
    exit 0
fi
sha="$(git rev-parse HEAD)"

echo "==> waiting for exact-main candidate $sha"
status=""; conclusion=""
for _ in $(seq 1 120); do
    read -r status conclusion <<<"$(gh run list --repo "$slug" --workflow ci.yml --branch "$main" --commit "$sha" \
        --event push --limit 1 --json status,conclusion --jq '.[0] | "\(.status // "-") \(.conclusion // "-")"')"
    [ "$status" = completed ] && break
    sleep 15
done
if [ "$status" != completed ]; then
    echo "error: exact-main CI did not finish within 30 minutes" >&2
    exit 1
fi
if [ "$conclusion" != success ]; then
    echo "error: exact-main CI for $sha finished $conclusion; no release tag was created" >&2
    exit 1
fi

# Do not quietly release an older generation when another session merged while CI ran. Rerunning against the new
# head needs no second version commit: VERSION already carries the intended release and its own candidate will win.
git fetch --quiet origin
[ "$sha" = "$(git rev-parse "origin/$main")" ] || {
    echo "error: origin/$main moved while CI ran — update this checkout and rerun; nothing was tagged" >&2
    exit 1
}

git tag -a "v$version" -m "${slug#*/} $version"
git push --quiet origin "v$version"

echo "==> promoting the tested candidate"
status=""; conclusion=""
for _ in $(seq 1 80); do
    read -r status conclusion <<<"$(gh run list --repo "$slug" --workflow docker-publish.yml --branch "v$version" \
        --limit 1 --json status,conclusion --jq '.[0] | "\(.status // "-") \(.conclusion // "-")"')"
    [ "$status" = completed ] && break
    sleep 15
done
if [ "$status" != completed ]; then
    echo "note: promotion is still running after 20 minutes — gh run list --repo $slug"; exit 0
fi
[ "$conclusion" = success ] || {
    echo "error: v$version promotion failed ($conclusion); :latest was not moved" >&2
    echo "       logs: gh run view --repo $slug --log-failed" >&2
    exit 1
}
echo "v$version is signed and published. Deploy with:"
echo "  ssh root@pve 'incus exec den -- env TUF_ROOT=/var/lib/den/sigstore den-update ${slug#*/}'"
