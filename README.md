# msnnext

Messenger desktop P2P ispirato a MSN Messenger. L'obiettivo è mantenere testo, trilli, emoticon personali e scambio multimediale senza account centrale: identità, chiavi, contatti e cronologia appartengono agli utenti.

> Stato attuale: **alpha di sviluppo**. La GUI si avvia ed è utilizzabile per provare collegamento e messaggi, ma non è ancora una release affidabile. Emoticon e allegati hanno ora conferme collegate al risultato del core, ma richiedono ancora collaudo end-to-end tra due applicazioni installate.

## Dove siamo arrivati

### Applicazione desktop

- applicazione unica Tauri 2 + Svelte con core Rust integrato, senza processo CLI separato;
- interfaccia ispirata a MSN con lista contatti online/offline, chat, ricerca e non letti;
- temi Chiaro, Scuro e Sistema persistenti;
- onboarding, aggiunta contatto tramite link `msnnext://add/...`, QR grafico e scansione QR da immagine;
- selettore file nativo;
- conversazioni separate per Peer ID;
- contatti, cronologia e identità persistenti dopo il riavvio;
- bridge tipizzato tra GUI e core tramite comandi ed eventi Tauri.

### Rete e sicurezza

- identità Ed25519 persistente;
- trasporto libp2p QUIC e TCP;
- discovery LAN tramite mDNS;
- Identify, Kademlia DHT, AutoNAT, DCUtR e Circuit Relay v2;
- link contatto firmati e verifica della corrispondenza tra chiave pubblica e Peer ID;
- handshake applicativo ibrido X25519 + ML-KEM-768;
- messaggi applicativi cifrati con XChaCha20-Poly1305 e ratchet simmetrico;
- protezione da replay e supporto limitato ai messaggi fuori ordine;
- reconnessione con backoff dopo la perdita del collegamento;
- timeout inattivo della connessione portato oltre il precedente limite errato di 60 secondi.

### Dati locali e protocollo

- envelope CBOR versionati e legati a dispositivo e conversazione;
- cronologia SQLite con contenuto cifrato localmente;
- contatti persistenti;
- trasferimento core a chunk con hash BLAKE3 e ripresa dei chunk mancanti;
- limiti e validazione per emoticon PNG, JPEG, GIF e WebP;
- trilli cifrati e sottoposti a rate limit.

## Cosa funziona oggi

- avvio della GUI e creazione dell'identità;
- aggiunta di un contatto tramite link o QR;
- collegamento diretto sulla stessa macchina o LAN nelle condizioni già provate;
- negoziazione del canale sicuro;
- invio e ricezione dei messaggi di testo;
- cronologia testuale e lista contatti dopo il riavvio;
- trillo di base con animazione della finestra web;
- generazione degli installer Windows MSI e NSIS.

La stabilità della reconnessione è stata migliorata e provata manualmente con due core locali, compreso un riavvio, ma deve ancora essere collaudata a lungo con due applicazioni installate e su reti reali diverse.

## Problemi noti

### Emoticon personalizzate

Il flusso è presente nel core e nella GUI, con creazione, salvataggio, rinomina della scorciatoia ed eliminazione:

- invio, anteprima, salvataggio sul destinatario e riutilizzo tramite scorciatoia devono ancora essere collaudati end-to-end tra due applicazioni installate;
- gli span delle emoticon non vengono conservati nella cronologia, quindi dopo il riavvio il messaggio può tornare a mostrare solo il trigger testuale;
- i conflitti tra scorciatoie vengono rifiutati, ma la GUI non propone ancora automaticamente una scorciatoia alternativa.

Il risultato richiesto da `GROUND.md` resta: scegliere un'immagine o GIF, assegnare una combinazione, vederla nel messaggio del destinatario e permettergli di salvarla in pochi secondi.

### Immagini, video e file

Il protocollo Rust a chunk esiste e la GUI mostra completamento o errore reali; l'esperienza resta incompleta:

- manca una verifica end-to-end affidabile tra due applicazioni desktop;
- non ci sono anteprima inline di immagini o video;
- non ci sono richiesta di accettazione o rifiuto, avanzamento, velocità, annullamento e ritentativo visibili;
- non è ancora disponibile una barra di avanzamento per i singoli chunk;
- mancano drag-and-drop e apertura sicura del file ricevuto dalla conversazione.

### Collegamento e presenza

- la reconnessione automatica necessita ancora di test prolungati su due PC e reti differenti;
- hole punching e fallback relay non sono stati collaudati su due NAT reali;
- non esiste infrastruttura bootstrap/relay pubblica preconfigurata;
- non esistono messaggi offline: almeno uno dei dispositivi deve essere raggiungibile;
- avatar, nome personale, rinomina e rimozione dei contatti sono disponibili; stato personale, gruppi e blocco restano incompleti.

### Trilli e UX

- manca il suono configurabile del trillo;
- l'animazione attuale muove il contenuto della webview, non ancora la finestra nativa come MSN;
- mancano impostazioni, accessibilità e controllo completo delle notifiche.

### Sicurezza ancora da completare

- firme identitarie ibride Ed25519 + ML-DSA;
- verifica utente tramite fingerprint o QR della sessione;
- salvataggio della chiave del database nel key store del sistema operativo;
- Content Security Policy più restrittiva per la webview;
- audit crittografico e di sicurezza indipendente;
- protocollo di migrazione/versionamento delle primitive crittografiche.

## Prossime priorità

1. Riprodurre e correggere il flusso completo delle emoticon personalizzate tra due GUI.
2. Completare immagini, video e file con stato, avanzamento, accettazione, annullamento e anteprima.
3. Eseguire test prolungati di collegamento, disconnessione e reconnessione con due applicazioni installate.
4. Completare trillo nativo, suono, presenza, avatar e gestione contatti.
5. Collaudare bootstrap, relay e hole punching su reti reali.
6. Rafforzare key storage, CSP, firme post-quantum e verifica delle identità.

## Struttura

```text
apps/cli                 core Rust e client CLI
apps/desktop             GUI Svelte
apps/desktop/src-tauri   backend desktop Tauri
crates/protocol          eventi e formati condivisi
prototypes/web           vecchio prototipo, non applicazione di produzione
```

## Avvio desktop

Prerequisiti: Rust, Node.js, npm e dipendenze di sistema richieste da Tauri 2.

```powershell
cd apps/desktop
npm install
npm run desktop
```

## Verifiche

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

cd apps/desktop
npm run check
npm run build
npx tauri build
```

La suite standard corrente passa con 47 test; un test con due socket reali è escluso dalla suite automatica perché il teardown libp2p può bloccare l'harness su Windows. Il suo scenario deve essere sostituito da un collaudo end-to-end desktop deterministico.

Gli installer vengono generati in:

```text
target/release/bundle/msi/msnnext_0.1.0_x64_en-US.msi
target/release/bundle/nsis/msnnext_0.1.0_x64-setup.exe
```

## Creazione release e installer

Su Windows, dalla cartella principale:

```powershell
.\scripts\release-windows.ps1
```

Produce l'eseguibile release e il setup NSIS. Per riutilizzare le dipendenze già installate: `.\scripts\release-windows.ps1 -SkipInstall`.

Il formato MSI usa il vecchio toolset WiX e può essere richiesto con `.\scripts\release-windows.ps1 -Msi`; NSIS resta il setup Windows predefinito e più affidabile.

Su Linux:

```sh
./scripts/release-linux.sh
```

Produce il binario release, un pacchetto DEB e un'AppImage. Va eseguito direttamente su Linux con le dipendenze di sistema richieste da Tauri; per riutilizzare `node_modules`, passare `--skip-install`.

Le release Windows e Linux vanno costruite sui rispettivi sistemi operativi. Android non è ancora incluso: richiede l'inizializzazione del progetto mobile Tauri e l'adattamento delle funzionalità desktop native.

## Definizione di “usabile”

msnnext non sarà considerato usabile finché due persone non potranno, senza riavvii manuali:

1. aggiungersi tramite QR o link;
2. collegarsi e restare collegate;
3. scambiare testo e trilli;
4. creare, inviare e salvare emoticon statiche o animate;
5. inviare e ricevere immagini, video e file con feedback chiaro;
6. chiudere e riaprire l'app ritrovando contatti e conversazioni.
