#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT/apps/desktop"

if [ "${1:-}" != "--skip-install" ]; then
  npm ci
fi
npm run check
npm run release:linux

printf '%s\n' "Release pronta in target/release e target/release/bundle"
