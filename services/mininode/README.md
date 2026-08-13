# msnnext mininode

Il mininodo offre bootstrap e Circuit Relay v2. Non conserva messaggi, allegati
o cronologia degli utenti: inoltra stream già cifrati soltanto mentre i client
sono collegati.

## Avvio sulla VPS

Apri nel firewall della VPS le porte `4001/TCP` e `4001/UDP`, poi dalla radice
del repository esegui:

```bash
docker compose -f services/mininode/compose.yml up -d --build
docker compose -f services/mininode/compose.yml logs mininode
```

Nei log copia il valore mostrato dopo `peer:`. L'indirizzo da inserire nelle
impostazioni di msnnext è uno dei seguenti:

```text
/dns4/relay.example.com/tcp/4001/p2p/PEER_ID
/ip4/203.0.113.10/udp/4001/quic-v1/p2p/PEER_ID
```

Il volume `mininode-data` mantiene stabile l'identità e quindi il Peer ID dopo
aggiornamenti e riavvii. Non cancellarlo finché i client usano quell'indirizzo.

Il traffico inoltrato, inclusi gli allegati, passa dalla VPS e ne consuma la
banda. Restano attivi i limiti libp2p su circuiti concorrenti e richieste.
