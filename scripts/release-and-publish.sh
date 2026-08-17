#!/bin/sh
# Rilascio "un click": incrementa versione, committa, push e avvia la CI che
# builda firmato e pubblica la release (che i client vecchi scaricano).
#
# NON builda in locale: la build firmata la fa GitHub Actions.
# Per una build locale di test (senza toccare git/CI) usa BUILD.command.
#
# Uso: ./scripts/release-and-publish.sh [major|minor|patch]   (default: patch)
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DESKTOP="$ROOT/apps/desktop"
PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"; export PATH

SEG="${1:-patch}"
case "$SEG" in
  major|minor|patch) ;;
  *) printf '%s\n' "Segmento non valido: $SEG (usa major|minor|patch)" >&2; exit 2 ;;
esac

for TOOL in node git gh; do
  command -v "$TOOL" >/dev/null 2>&1 || { printf '%s\n' "Prerequisito mancante: $TOOL" >&2; exit 1; }
done

# La CI pubblica come "latest" solo dal branch di default (main).
BRANCH=$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" != "main" ]; then
  printf '%s\n' "Sei su '$BRANCH', ma la CI pubblica solo da 'main'. Passa a main e riprova." >&2
  exit 1
fi

# Solo il bump di versione deve finire nel commit: il codice va già committato.
if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  printf '%s\n' "Working tree non pulito. Committa le modifiche al codice prima di rilasciare." >&2
  git -C "$ROOT" status --short >&2
  exit 1
fi

CUR=$(node -p "require('$DESKTOP/package.json').version")
NEW=$(node -e "const [a,b,c]='$CUR'.split('.').map(Number);const s='$SEG';console.log(s==='major'?\`\${a+1}.0.0\`:s==='minor'?\`\${a}.\${b+1}.0\`:\`\${a}.\${b}.\${c+1}\`)")

printf 'Rilascio v%s -> v%s su %s, con push e avvio CI. Procedere? [y/N] ' "$CUR" "$NEW" "$BRANCH"
read -r ANS
case "$ANS" in y|Y|yes|si|s) ;; *) printf '%s\n' "Annullato."; exit 1 ;; esac

node "$ROOT/scripts/bump-version.mjs" "$SEG"
git -C "$ROOT" add apps/desktop/package.json
git -C "$ROOT" commit -m "release: v$NEW"
git -C "$ROOT" push
gh workflow run build-release.yml --ref "$BRANCH" -R "$(gh repo view --json nameWithOwner -q .nameWithOwner)"

printf '\n%s\n' "Rilascio v$NEW avviato. Stato: gh run watch  (oppure GitHub > Actions)"
printf '%s\n' "Quando la CI finisce, i client vecchi rilevano v$NEW entro 5h o con 'Controlla ora'."
