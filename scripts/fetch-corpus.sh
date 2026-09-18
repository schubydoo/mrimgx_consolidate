#!/usr/bin/env bash
# Fetch the public test corpus into testdata/.
#
# The corpus is the sample backup set that Macrium publishes in the
# linux-contrib-tidy branch of macrium/mrimgx_file_layout. The tests in
# tests/corpus.rs skip when testdata/ is absent, so a fresh clone stays green
# without it. Run this script to make those tests run.
#
# The clone is pinned to one commit, so the numbers the tests assert stay
# valid. The same clone carries the reference extractor sources that
# scripts/build-refextract.sh compiles.
#
# Usage:
#   ./scripts/fetch-corpus.sh
#
# Environment:
#   CORPUS   where to clone the upstream repository (default /tmp/mrimgx-corpus)
#   DEST     where to copy the backup files (default <repo>/testdata)

set -euo pipefail

UPSTREAM=https://github.com/macrium/mrimgx_file_layout.git
COMMIT=8751d217ebfedf199dd77608534b8a9c065dd629

CORPUS=${CORPUS:-/tmp/mrimgx-corpus}
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEST=${DEST:-"$REPO_ROOT/testdata"}

if [ ! -d "$CORPUS/.git" ]; then
    echo "Cloning $UPSTREAM into $CORPUS" >&2
    git clone --filter=blob:none --no-checkout "$UPSTREAM" "$CORPUS"
fi

# One sparse set for both users of this clone: the backup files for the tests,
# and the reference extractor sources for the oracle.
git -C "$CORPUS" sparse-checkout set --no-cone \
    'contrib/extract-to-img/Backup-Files/*' \
    'contrib/extract-to-img/libs/*' \
    'contrib/extract-to-img/src/*' \
    'contrib/extract-to-img/dependencies/*'
git -C "$CORPUS" fetch --filter=blob:none origin "$COMMIT"
git -C "$CORPUS" checkout --detach "$COMMIT"

mkdir -p "$DEST"
cp -r "$CORPUS"/contrib/extract-to-img/Backup-Files/* "$DEST/"

echo "corpus in $DEST:" >&2
du -sh "$DEST" >&2
