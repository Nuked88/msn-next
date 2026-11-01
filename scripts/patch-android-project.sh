#!/usr/bin/env bash
# Adatta il progetto Gradle generato da `tauri android init` (gitignored):
# - permesso REQUEST_INSTALL_PACKAGES, per aprire l'installer di sistema
#   sull'APK di aggiornamento scaricato dall'app;
# - cache-path nel FileProvider del template, perché l'APK scaricato sta
#   nella cache dell'app.
# Idempotente: si può rilanciare dopo ogni `android init`.
# Uso: scripts/patch-android-project.sh [apps/desktop/src-tauri/gen/android]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PROJECT="${1:-$ROOT_DIR/apps/desktop/src-tauri/gen/android}"
MAIN="$PROJECT/app/src/main"
MANIFEST="$MAIN/AndroidManifest.xml"
PATHS="$MAIN/res/xml/file_paths.xml"

if [[ ! -f "$MANIFEST" ]]; then
  echo "AndroidManifest.xml non trovato in $MAIN (lancia prima 'tauri android init')" >&2
  exit 1
fi

if ! grep -q 'android.permission.REQUEST_INSTALL_PACKAGES' "$MANIFEST"; then
  perl -0pi -e 's|(\n\s*<application)|\n    <uses-permission android:name="android.permission.REQUEST_INSTALL_PACKAGES" />$1|' "$MANIFEST"
  grep -q 'android.permission.REQUEST_INSTALL_PACKAGES' "$MANIFEST" || {
    echo "Impossibile aggiungere REQUEST_INSTALL_PACKAGES a $MANIFEST" >&2
    exit 1
  }
fi

if ! grep -q 'androidx.core.content.FileProvider' "$MANIFEST"; then
  echo "Il manifest non dichiara il FileProvider del template Tauri" >&2
  exit 1
fi

if [[ ! -f "$PATHS" ]]; then
  mkdir -p "$(dirname "$PATHS")"
  cat > "$PATHS" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<paths xmlns:android="http://schemas.android.com/apk/res/android">
    <cache-path name="msnnext_cache" path="." />
</paths>
XML
elif ! grep -q '<cache-path' "$PATHS"; then
  perl -0pi -e 's|</paths>|    <cache-path name="msnnext_cache" path="." />\n</paths>|' "$PATHS"
fi

echo "Progetto Android pronto per gli aggiornamenti APK: $PROJECT"
