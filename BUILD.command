#!/bin/zsh

# Avviabile con doppio clic dal Finder oppure da Terminale.
ROOT_DIR="${0:A:h}"

# Carica la versione Node predefinita di NVM anche quando Finder usa un PATH
# ereditato da una sessione precedente.
if [[ -s "$HOME/.nvm/nvm.sh" ]]; then
  export NVM_DIR="$HOME/.nvm"
  source "$NVM_DIR/nvm.sh"
  nvm use --silent default >/dev/null 2>&1 || true
fi

"$ROOT_DIR/scripts/release-macos.sh" "$@"
STATUS=$?

echo
if [[ $STATUS -eq 0 ]]; then
  echo "Build macOS completata."
else
  echo "Build macOS non riuscita (codice $STATUS)."
fi

if [[ -t 0 ]]; then
  read -r "?Premi Invio per chiudere..."
fi

exit $STATUS
