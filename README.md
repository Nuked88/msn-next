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

## Milestone 2 — prossima

Aggiungere file transfer a chunk con manifest, hash per blocco e resume. Gli algoritmi post-quantum arriveranno dietro un crate `crypto` isolato: niente primitive inventate e nessuna promessa “quantum-resistant” prima di test e revisione.

Poi: cronologia locale, drag-and-drop e notifiche desktop.
