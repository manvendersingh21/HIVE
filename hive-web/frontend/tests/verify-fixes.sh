#!/usr/bin/env bash
# Mechanical proof that the four F-05..F-08 tests fail before their fixes and
# pass after. Reviewers and CI can run this instead of taking the claim on
# trust: it builds the unfixed base commit with nothing but the new test file
# copied in, and requires all four to fail there and pass on the branch.
#
#   hive-web/frontend/tests/verify-fixes.sh [base-ref]
#
# base-ref defaults to 852f946, the commit this branch was cut from.
set -euo pipefail

BASE="${1:-852f946}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FRONTEND="$(cd "$HERE/.." && pwd)"
REPO="$(cd "$FRONTEND/../.." && pwd)"
TEST="tests/navigationLayout.spec.ts"
BEFORE="$(mktemp -d "${TMPDIR:-/tmp}/hive-before.XXXXXX")"

cleanup() {
  git -C "$REPO" worktree remove --force "$BEFORE" >/dev/null 2>&1 || rm -rf "$BEFORE"
  rm -rf "$BEFORE"
}
trap cleanup EXIT

echo "== base commit: $(git -C "$REPO" rev-parse --short "$BASE")"

# An unfixed tree with only the new test file: every fix is absent.
git -C "$REPO" worktree add --detach "$BEFORE" "$BASE" >/dev/null
cp "$FRONTEND/$TEST" "$BEFORE/hive-web/frontend/$TEST"

changed=0
for file in app/globals.css app/page.tsx app/session/page.tsx components/Nav.tsx; do
  if ! diff -q "$FRONTEND/$file" "$BEFORE/hive-web/frontend/$file" >/dev/null; then
    changed=$((changed + 1))
  fi
done
if [ "$changed" -ne 4 ]; then
  echo "FAIL: expected 4 unfixed files at $BASE, found $changed" >&2
  exit 1
fi
echo "== all 4 fixes absent at base, test file present"

cd "$BEFORE/hive-web/frontend"
npm ci >/dev/null 2>&1
npm run build >/dev/null 2>&1
if npx playwright test "$TEST" --workers=1 >/dev/null 2>&1; then
  echo "FAIL: the tests passed on unfixed $BASE, so they prove nothing" >&2
  exit 1
fi
echo "== before: 4 tests fail without the fixes"

cd "$FRONTEND"
npm ci >/dev/null 2>&1
npx tsc --noEmit
npm run build >/dev/null 2>&1
npx playwright test "$TEST" --workers=1
echo "== after: 4 tests pass with the fixes"
