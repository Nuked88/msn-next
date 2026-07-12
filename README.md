# msnnext

Messenger P2P senza proprietario: identità, chiavi, cronologia ed emoticon appartengono agli utenti. I futuri nodi bootstrap e relay aiutano la connessione, ma non possono leggere i contenuti.

## Milestone 1 — completata

Due client CLI possono ora collegarsi direttamente e scambiare:

- identità Ed25519 persistenti e connessioni QUIC cifrate;
- envelope CBOR versionati, legati a dispositivo e conversazione;
- testo con span delle emoticon risolti in `asset_id`;
- trilli limitati in invio e ricezione;
- emoticon statiche o animate verificate con BLAKE3, salvate e subito riutilizzabili.

```powershell
cargo test
```

## Client CLI P2P

Il primo client usa identità Ed25519 persistenti, CBOR e connessioni dirette libp2p su QUIC.

Primo terminale:

```powershell
cargo run -p msnnext -- --listen /ip4/0.0.0.0/udp/4040/quic-v1 --identity .msnnext/alice.key
```

Secondo terminale:

```powershell
cargo run -p msnnext -- --listen /ip4/0.0.0.0/udp/0/quic-v1 --connect /ip4/127.0.0.1/udp/4040/quic-v1 --identity .msnnext/bob.key
```

Comandi interattivi:

```text
text ciao :-)
emote :risata: C:\immagini\risata.gif
nudge
quit
```

`emote` accetta PNG, JPEG, GIF e WebP fino a 350 KB e 512×512 pixel. Il destinatario verifica formato, dimensioni e hash BLAKE3, salva una sola copia in `.msnnext/emoticons` e rende immediatamente utilizzabile il trigger suggerito. Usa `--emotes <cartella>` per cambiare archivio.

QUIC cifra già il trasporto. La cifratura applicativa ibrida e il ratchet non sono ancora implementati, quindi questo prototipo non va presentato come post-quantum o pronto per conversazioni sensibili.

Il prototipo HTML/CSS/JS in `prototypes/web` serve soltanto a validare l'esperienza di chat e non contiene la futura rete o crittografia di produzione.

## Milestone 2 — completata

Il comando `file <percorso>` trasferisce immagini, video o file generici fino a 25 MB in chunk da 256 KB. Manifest e chunk hanno hash BLAKE3; rilanciando lo stesso comando dopo un'interruzione, il destinatario richiede soltanto i chunk mancanti. Usa `--downloads <cartella>` per cambiare destinazione.

La cronologia locale usa SQLite, ma i contenuti delle righe sono cifrati con XChaCha20-Poly1305. Il comando `history` mostra gli ultimi 20 eventi. La chiave è derivata dall'identità locale: la protezione tramite key store del sistema operativo arriverà con il crate storage definitivo.

Le notifiche desktop si attivano con `--notify true`. Il prototipo web supporta anche il drag-and-drop di immagini e video sulla conversazione; la futura UI Tauri richiamerà lo stesso backend Rust usato da `file`.

## Milestone 3 — completata

Il client trova e collega automaticamente gli altri peer msnnext nella LAN tramite mDNS. All'apertura della connessione scambia nome e presenza; la chiusura dell'ultima connessione porta il contatto offline.

```text
contact qr
contact export alice.contact
contact import alice.contact
contact import-link msnnext://add/...
```

La scheda CBOR contiene nome locale, Peer ID, chiave pubblica classica e fino a quattro indirizzi iniziali. Import e link verificano crittograficamente che chiave e Peer ID corrispondano. `contact qr` mostra nel terminale il QR del link verificabile.

La discovery Internet usa una DHT Kademlia isolata nel protocollo `/msnnext/kad/1`. Identify condivide gli indirizzi, AutoNAT rileva la raggiungibilità e DCUtR tenta il passaggio da relay a connessione diretta. Bootstrap e relay sono sempre configurati dall'utente: il binario non contiene infrastruttura pubblica predefinita.

Avvia un nodo bootstrap/relay e annota il Peer ID stampato:

```powershell
cargo run -p msnnext -- --name Relay --identity .msnnext/relay.key --listen /ip4/0.0.0.0/udp/4001/quic-v1 --listen-tcp /ip4/0.0.0.0/tcp/4001 --relay-server
```

Poi avvia due client sostituendo `<RELAY_PEER_ID>`:

```powershell
cargo run -p msnnext -- --name Alice --identity .msnnext/alice.key --listen /ip4/0.0.0.0/udp/0/quic-v1 --bootstrap /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER_ID> --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER_ID>
cargo run -p msnnext -- --name Bob --identity .msnnext/bob.key --listen /ip4/0.0.0.0/udp/0/quic-v1 --bootstrap /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER_ID> --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER_ID>
```

Importando la scheda contatto, il client prova in ordine indirizzi diretti, DHT e relay. Il collaudo reale del hole punching richiede due reti NAT distinte; una singola macchina verifica soltanto bootstrap, prenotazione relay e chat locale.

## Milestone 4 — in corso

I peer collegati negoziano una chiave di sessione effimera con X25519 + ML-KEM-768 sul protocollo `/msnnext/handshake/1`. L'integrazione della chiave nel ratchet e le firme ibride sono i prossimi incrementi.
