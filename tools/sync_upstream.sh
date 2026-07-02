#!/usr/bin/env bash
#
# Sync flow's `deno` mirror branch to a denoland/deno release and (optionally)
# start the reconciliation branch for an upgrade.
#
# flow is a detached downstream of denoland/deno:
#   - `deno`  : a pristine mirror of upstream. It only ever fast-forwards to an
#               upstream release commit; it must never contain flow commits.
#   - `main`  : flow's development line (the edge port on top of upstream).
#   - upgrade/<version> : where a new upstream release is reconciled with flow
#                         before merging into `main`.
#
# We deliberately do NOT import upstream tags (denoland ships hundreds, and a
# tag named `v2.9.1` would collide with an `upgrade/2.9.1` branch). Instead we
# fetch only the tag's commit into the `deno` branch.
#
# Usage:
#   tools/sync_upstream.sh <version> [--upgrade]
#
#   <version>    e.g. 2.9.1 or v2.9.1
#   --upgrade    after updating `deno`, create upgrade/<version> off main and
#                merge `deno` into it so you can resolve conflicts, build, test,
#                then `git switch main && git merge upgrade/<version>`.
#
# Example:
#   tools/sync_upstream.sh 2.9.1 --upgrade
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: tools/sync_upstream.sh <version, e.g. 2.9.1> [--upgrade]" >&2
  exit 2
fi

RAW_VERSION="$1"
VERSION="${RAW_VERSION#v}"          # strip a leading v if present
TAG="v${VERSION}"
DO_UPGRADE="${2:-}"

UPSTREAM_REMOTE="upstream"
UPSTREAM_URL="https://github.com/denoland/deno.git"

# Refuse to run on a dirty tree — merges/branch resets need a clean state.
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "ERROR: working tree is dirty. Commit or stash first." >&2
  exit 1
fi

# Ensure the upstream remote exists (idempotent).
if ! git remote get-url "$UPSTREAM_REMOTE" >/dev/null 2>&1; then
  echo "Adding '$UPSTREAM_REMOTE' remote -> $UPSTREAM_URL"
  git remote add "$UPSTREAM_REMOTE" "$UPSTREAM_URL"
fi

# Fetch ONLY the tag's commit into FETCH_HEAD, without creating a local tag ref.
echo "Fetching $TAG from $UPSTREAM_REMOTE ..."
git fetch --no-tags "$UPSTREAM_REMOTE" "refs/tags/${TAG}"
TARGET="$(git rev-parse FETCH_HEAD)"

# `deno` must exist and be a pristine mirror: it can only move forward to a newer
# upstream commit (i.e. current deno is an ancestor of the target). If it isn't,
# someone put non-upstream commits on it — bail rather than rewrite them.
if ! git show-ref --verify --quiet refs/heads/deno; then
  echo "ERROR: no local 'deno' branch. Create it at your current upstream base first." >&2
  exit 1
fi
if [ "$(git rev-parse deno)" = "$TARGET" ]; then
  echo "'deno' is already at ${TAG} (${TARGET}). Nothing to sync."
else
  if ! git merge-base --is-ancestor deno "$TARGET"; then
    echo "ERROR: 'deno' is not an ancestor of ${TAG}." >&2
    echo "       It may contain non-upstream commits, or ${TAG} is older than deno." >&2
    echo "       Refusing to move the mirror. Inspect: git log --oneline deno..${TARGET}" >&2
    exit 1
  fi
  # Fast-forward the mirror to the release commit and publish it.
  git branch -f deno "$TARGET"
  echo "deno -> ${TAG} (${TARGET})"
  git push origin deno
fi

if [ "$DO_UPGRADE" = "--upgrade" ]; then
  BRANCH="upgrade/${VERSION}"
  if git show-ref --verify --quiet "refs/heads/${BRANCH}"; then
    echo "ERROR: branch ${BRANCH} already exists." >&2
    exit 1
  fi
  echo "Creating ${BRANCH} off main and merging deno (${TAG}) into it ..."
  git switch -c "${BRANCH}" main
  if git merge --no-edit deno; then
    echo "Merge clean. Build + test, then: git switch main && git merge ${BRANCH}"
  else
    echo ""
    echo "Merge has conflicts (expected). Resolve them, then:"
    echo "  git commit           # finish the merge"
    echo "  # build + test flow, then:"
    echo "  git switch main && git merge ${BRANCH}"
    echo "See the merge-deno-upstream skill for guidance."
  fi
fi
