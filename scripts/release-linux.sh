#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT/apps/desktop"

# Uso: ./scripts/release-linux.sh [major|minor|patch|keep] [--skip-install]
BUMP=""
SKIP_INSTALL=0
for arg in "$@"; do
  case "$arg" in
    major|minor|patch|keep) BUMP="$arg" ;;
    --skip-install) SKIP_INSTALL=1 ;;
  esac
done

if [ "$SKIP_INSTALL" -eq 0 ]; then
  npm ci
fi
node "$ROOT/scripts/bump-version.mjs" $BUMP
npm run check
npm run release:linux

printf '%s\n' "Release pronta in target/release e target/release/bundle"
