#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DESKTOP="$ROOT/apps/desktop"
SKIP_INSTALL=0
UNIVERSAL=0
BUMP=""

usage() {
  cat <<'EOF'
Uso: ./scripts/release-macos.sh [major|minor|patch|keep] [opzioni]

Segmento versione (opzionale, incrementa apps/desktop/package.json):
  major | minor | patch   incrementa il segmento indicato
  keep                    lascia la versione invariata
  (nessuno)               chiede interattivamente; in non-interattivo = keep

Opzioni:
  --skip-install  riutilizza node_modules senza eseguire npm ci
  --universal     crea un DMG universale (Apple Silicon + Intel)
  -h, --help      mostra questo aiuto

Senza --universal viene creato un DMG nativo per il Mac in uso.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    major|minor|patch|keep)
      BUMP="$1"
      ;;
    --skip-install)
      SKIP_INSTALL=1
      ;;
    --universal)
      UNIVERSAL=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf '%s\n' "Opzione sconosciuta: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

# I .command aperti dal Finder possono ricevere un PATH più ridotto.
PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export PATH

for TOOL in node npm cargo rustc xcrun; do
  if ! command -v "$TOOL" >/dev/null 2>&1; then
    printf '%s\n' "Prerequisito mancante: $TOOL" >&2
    exit 1
  fi
done

node -e '
const [major, minor] = process.versions.node.split(".").map(Number);
const supported = (major === 20 && minor >= 19) || (major === 22 && minor >= 12) || major > 22;
if (!supported) {
  console.error(`Node.js ${process.versions.node} non supportato: installa Node.js 20.19+, 22.12+ o più recente (con nvm: nvm install 22; nvm alias default 22).`);
  process.exit(1);
}
'

if ! xcrun --sdk macosx --show-sdk-path >/dev/null 2>&1; then
  printf '%s\n' "Xcode Command Line Tools mancanti. Installa con: xcode-select --install" >&2
  exit 1
fi

if [ "$UNIVERSAL" -eq 1 ]; then
  if command -v rustup >/dev/null 2>&1; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
  else
    RUST_SYSROOT=$(rustc --print sysroot)
    for TARGET in aarch64-apple-darwin x86_64-apple-darwin; do
      if [ ! -d "$RUST_SYSROOT/lib/rustlib/$TARGET" ]; then
        printf '%s\n' \
          "Target Rust $TARGET mancante. Installa rustup oppure esegui senza --universal." >&2
        exit 1
      fi
    done
  fi
fi

# Usa automaticamente la chiave updater locale, se presente. In sua assenza
# disabilita solo gli artifact dell'updater: il DMG viene creato comunque.
UNSIGNED_CONFIG=
BUNDLES=dmg
SIGNING_KEY_FOUND=0

if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  DEFAULT_KEY_PATH="${HOME}/.tauri/msnnext-updater.key"
  if [ -n "${TAURI_SIGNING_PRIVATE_KEY_PATH:-}" ]; then
    TAURI_SIGNING_PRIVATE_KEY="$TAURI_SIGNING_PRIVATE_KEY_PATH"
    export TAURI_SIGNING_PRIVATE_KEY
    SIGNING_KEY_FOUND=1
    printf '%s\n' "Firma updater: percorso chiave configurato."
  elif [ -f "$DEFAULT_KEY_PATH" ]; then
    TAURI_SIGNING_PRIVATE_KEY="$DEFAULT_KEY_PATH"
    export TAURI_SIGNING_PRIVATE_KEY
    SIGNING_KEY_FOUND=1
    printf '%s\n' "Firma updater: chiave locale rilevata."
  fi
else
  SIGNING_KEY_FOUND=1
fi

if [ "$SIGNING_KEY_FOUND" -eq 1 ] && [ "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD+x}" != x ]; then
  if [ -t 0 ]; then
    printf '%s' "Password chiave updater (solo Invio per creare soltanto il DMG): "
    STTY_STATE=$(stty -g)
    trap 'stty "$STTY_STATE"' EXIT HUP INT TERM
    stty -echo
    IFS= read -r TAURI_SIGNING_PRIVATE_KEY_PASSWORD
    stty "$STTY_STATE"
    trap - EXIT HUP INT TERM
    printf '\n'
    if [ -n "$TAURI_SIGNING_PRIVATE_KEY_PASSWORD" ]; then
      export TAURI_SIGNING_PRIVATE_KEY_PASSWORD
    else
      SIGNING_KEY_FOUND=0
      unset TAURI_SIGNING_PRIVATE_KEY
    fi
  else
    printf '%s\n' "Firma updater: password non configurata; creo soltanto il DMG."
    SIGNING_KEY_FOUND=0
    unset TAURI_SIGNING_PRIVATE_KEY
  fi
fi

if [ "$SIGNING_KEY_FOUND" -eq 1 ]; then
  BUNDLES=app,dmg
else
  UNSIGNED_CONFIG='{"bundle":{"createUpdaterArtifacts":false}}'
  printf '%s\n' "Updater non firmato: il DMG viene creato comunque."
fi

if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  APPLE_SIGNING_IDENTITY=-
  export APPLE_SIGNING_IDENTITY
fi

cd "$DESKTOP"

if [ "$SKIP_INSTALL" -eq 0 ]; then
  npm ci
fi

node "$ROOT/scripts/bump-version.mjs" $BUMP

npm run check

if [ "$UNIVERSAL" -eq 1 ]; then
  if [ -n "$UNSIGNED_CONFIG" ]; then
    npm run tauri -- build --target universal-apple-darwin --bundles "$BUNDLES" --config "$UNSIGNED_CONFIG"
  else
    npm run tauri -- build --target universal-apple-darwin --bundles "$BUNDLES"
  fi
  OUTPUT_DIR="$ROOT/target/universal-apple-darwin/release/bundle"
else
  if [ -n "$UNSIGNED_CONFIG" ]; then
    npm run tauri -- build --bundles "$BUNDLES" --config "$UNSIGNED_CONFIG"
  else
    npm run tauri -- build --bundles "$BUNDLES"
  fi
  OUTPUT_DIR="$ROOT/target/release/bundle"
fi

printf '\n%s\n' "Release pronta in: $OUTPUT_DIR"
printf '%s\n' "Nota: il DMG locale non è notarizzato da Apple."
