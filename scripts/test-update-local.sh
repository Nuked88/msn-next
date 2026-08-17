#!/bin/sh
# End-to-end updater test on macOS WITHOUT publishing to GitHub.
#
# It builds the *new* version as a signed updater artifact, generates a local
# latest.json pointing at http://localhost:PORT, and serves it. You then run an
# "old" build whose updater endpoint is overridden to that local server and
# watch it detect + install the new version.
#
# Requires the same updater signing key as the release scripts
# (TAURI_SIGNING_PRIVATE_KEY[_PATH] or ~/.tauri/msnnext-updater.key).
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DESKTOP="$ROOT/apps/desktop"
PORT=1421
PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"; export PATH

usage() {
  cat <<EOF
Uso: ./scripts/test-update-local.sh [--old | --new]

  --new   (default) costruisce la versione nuova firmata, genera latest.json
          locale e avvia il server HTTP su :$PORT.
  --old   costruisce ed esegue il client "vecchio" con endpoint updater
          reindirizzato a http://localhost:$PORT (in un'altra shell).

Flusso tipico:
  1) In un terminale:  ./scripts/test-update-local.sh --new
  2) In un altro:      ./scripts/test-update-local.sh --old
     Il client vecchio deve rilevare l'aggiornamento e installarlo.
EOF
}

MODE=new
case "${1:-}" in
  --old) MODE=old ;;
  --new|"") MODE=new ;;
  -h|--help) usage; exit 0 ;;
  *) echo "Opzione sconosciuta: $1" >&2; usage >&2; exit 2 ;;
esac

# --- signing key (same resolution as release-macos.sh) ---
resolve_key() {
  [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ] && return 0
  if [ -n "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]; then
    TAURI_SIGNING_PRIVATE_KEY="$TAURI_SIGNING_PRIVATE_KEY_PATH"; export TAURI_SIGNING_PRIVATE_KEY; return 0
  fi
  DEFAULT="${HOME}/.tauri/msnnext-updater.key"
  [ -f "$DEFAULT" ] && { TAURI_SIGNING_PRIVATE_KEY="$DEFAULT"; export TAURI_SIGNING_PRIVATE_KEY; return 0; }
  echo "Chiave updater mancante: imposta TAURI_SIGNING_PRIVATE_KEY o metti ~/.tauri/msnnext-updater.key" >&2
  exit 1
}

CUR=$(node -p "require('$DESKTOP/package.json').version")

if [ "$MODE" = new ]; then
  resolve_key
  : "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:=}"; export TAURI_SIGNING_PRIVATE_KEY_PASSWORD
  NEW=$(node -e "const [a,b,c]='$CUR'.split('.').map(Number);console.log(\`\${a}.\${b}.\${c+1}\`)")
  echo "Versione attuale $CUR -> versione di test $NEW"
  node "$ROOT/scripts/bump-version.mjs" patch
  cd "$DESKTOP"
  APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"; export APPLE_SIGNING_IDENTITY
  npm run tauri -- build --bundles app

  BUNDLE="$ROOT/target/release/bundle/macos"
  TAR=$(ls "$BUNDLE"/*.app.tar.gz | head -n1)
  SIG=$(ls "$BUNDLE"/*.app.tar.gz.sig | head -n1)
  [ -f "$TAR" ] && [ -f "$SIG" ] || { echo "Artifact updater non trovati in $BUNDLE (build firmata?)" >&2; exit 1; }

  ARCH=$(uname -m); case "$ARCH" in arm64) PLAT=darwin-aarch64 ;; x86_64) PLAT=darwin-x86_64 ;; *) PLAT="darwin-$ARCH" ;; esac
  TARNAME=$(basename "$TAR")
  # Copia gli artifact in una dir dedicata: la build del client "vecchio"
  # sovrascriverebbe target/release/bundle/macos e romperebbe il server.
  SERVE="$ROOT/target/update-test"
  rm -rf "$SERVE"; mkdir -p "$SERVE"
  cp "$TAR" "$SERVE/$TARNAME"
  node -e "
    const fs=require('fs');
    const sig=fs.readFileSync('$SIG','utf8').trim();
    const j={version:'$NEW',notes:'Local update test',pub_date:new Date().toISOString(),
      platforms:{'$PLAT':{signature:sig,url:'http://localhost:$PORT/$TARNAME'}}};
    fs.writeFileSync('$SERVE/latest.json',JSON.stringify(j,null,2));
    console.log('latest.json ->','$SERVE/latest.json');
  "
  # restore working version so the repo isn't left bumped
  (cd "$DESKTOP" && node -e "const p=require('./package.json');p.version='$CUR';require('fs').writeFileSync('package.json',JSON.stringify(p,null,2)+'\n')")

  echo "Servo $SERVE su http://localhost:$PORT  (Ctrl+C per fermare)"
  echo "Ora, in un'altra shell:  ./scripts/test-update-local.sh --old"
  cd "$SERVE"
  exec python3 -m http.server "$PORT"
fi

# --- old client: build current version with updater endpoint -> localhost, then run ---
OVERRIDE=$(printf '{"bundle":{"createUpdaterArtifacts":false},"plugins":{"updater":{"endpoints":["http://localhost:%s/latest.json"]}}}' "$PORT")
cd "$DESKTOP"
APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:--}"; export APPLE_SIGNING_IDENTITY
echo "Costruisco client vecchio ($CUR) con endpoint -> localhost:$PORT"
npm run tauri -- build --bundles app --config "$OVERRIDE"
APP=$(ls -d "$ROOT/target/release/bundle/macos"/*.app | head -n1)
echo "Avvio $APP  (Impostazioni > Aggiornamenti > Controlla ora)"
exec open -n "$APP"
