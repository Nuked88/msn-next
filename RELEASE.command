#!/bin/zsh

# Rilascio "un click": bump versione + commit + push + avvio CI (build firmata
# e pubblicazione). NON builda in locale. Per il test locale usa BUILD.command.
# Avviabile con doppio clic dal Finder oppure da Terminale.
# Uso: ./RELEASE.command [major|minor|patch]   (default: patch)
ROOT_DIR="${0:A:h}"

if [[ -s "$HOME/.nvm/nvm.sh" ]]; then
  export NVM_DIR="$HOME/.nvm"
  source "$NVM_DIR/nvm.sh"
  nvm use --silent default >/dev/null 2>&1 || true
fi

"$ROOT_DIR/scripts/release-and-publish.sh" "$@"
STATUS=$?

echo
if [[ $STATUS -eq 0 ]]; then
  echo "Rilascio avviato."
else
  echo "Rilascio non riuscito (codice $STATUS)."
fi

if [[ -t 0 ]]; then
  read -r "?Premi Invio per chiudere..."
fi

exit $STATUS
