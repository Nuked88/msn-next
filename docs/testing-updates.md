# Testare l'auto-aggiornamento

L'app usa il plugin Tauri updater. Il client controlla l'endpoint in
`apps/desktop/src-tauri/tauri.conf.json` (di default il `latest.json` della
release GitHub), confronta la versione, scarica l'artifact firmato e lo
installa. L'endpoint **non** è sovrascrivibile a runtime: per puntarlo altrove
serve una build con override di config.

## Metodo A — locale, senza pubblicare (rapido)

`scripts/test-update-local.sh` fa tutto il giro su macOS senza GitHub.

Serve la chiave di firma updater (come per le release):
`TAURI_SIGNING_PRIVATE_KEY[_PATH]` oppure `~/.tauri/msnnext-updater.key`.

```sh
# Terminale 1: costruisce la versione NUOVA firmata, genera latest.json
# locale e la serve su http://localhost:1421
./scripts/test-update-local.sh --new

# Terminale 2: costruisce ed esegue il client VECCHIO con endpoint
# reindirizzato a localhost, poi lo avvia
./scripts/test-update-local.sh --old
```

Nel client vecchio: **Impostazioni → Aggiornamenti → Controlla ora**.
Deve rilevare la versione N+1, scaricarla e riavviarsi aggiornato.

Note:
- La versione di test è `patch+1` di quella attuale; lo script ripristina la
  versione nel `package.json` a fine build.
- Le app buildate localmente non sono notarizzate: l'updater le sostituisce
  comunque perché non hanno quarantena Gatekeeper.
- Il controllo automatico ha un intervallo di 5 ore; usa il pulsante manuale
  per forzarlo subito.

## Metodo B — reale, via GitHub Release (fedele alla produzione)

1. Installa e avvia la versione attuale (il "client vecchio").
2. Incrementa la versione e pubblica una release firmata con gli artifact
   updater + `latest.json` (lo fa la CI `tauri-action`, oppure a mano con
   `./scripts/release-macos.sh` e caricando gli artifact su GitHub Releases).
3. Avvia il client vecchio: entro l'intervallo di controllo (o via pulsante
   manuale) rileva la nuova versione e si aggiorna.

Questo è il percorso identico a quello degli utenti finali: verifica anche
endpoint, firma e formato di `latest.json` reali.
