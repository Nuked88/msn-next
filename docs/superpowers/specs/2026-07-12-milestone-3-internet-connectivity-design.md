# Milestone 3: connettività Internet P2P

## Obiettivo

Completare il percorso di rete della Milestone 3 senza modificare il protocollo
chat o anticipare la crittografia post-quantum. Due client devono potersi trovare
tramite bootstrap configurabili, usare un relay sostituibile quando non sono
raggiungibili direttamente e tentare automaticamente una connessione diretta.

## Perimetro

- mantenere QUIC diretto e mDNS;
- aggiungere Identify, Kademlia, AutoNAT, DCUtR e Circuit Relay;
- accettare bootstrap e relay esclusivamente dalla configurazione CLI;
- permettere allo stesso eseguibile di fungere da relay per sviluppo e test;
- non includere nodi pubblici predefiniti;
- non modificare UI, formato degli eventi o crittografia applicativa.

Il rendering QR rimane un incremento separato della Milestone 3. Il collaudo
reale del hole punching tra due NAT richiederà due reti distinte e non fa parte
del test locale automatizzato.

## Architettura

Il `NetworkBehaviour` esistente resta nel client CLI e viene esteso, senza
estrarre ancora un crate `network`. Identify raccoglie indirizzi e protocolli
supportati. Kademlia usa i bootstrap espliciti per trovare un Peer ID. AutoNAT
classifica la raggiungibilità del nodo. Circuit Relay offre il percorso di
fallback e DCUtR tenta di sostituirlo con una connessione diretta.

Ordine del percorso:

1. indirizzo diretto noto o scoperto;
2. ricerca Kademlia;
3. connessione tramite relay;
4. tentativo DCUtR verso QUIC diretto.

Il guasto di un sottosistema non deve disabilitare i percorsi già funzionanti:
connessione manuale e mDNS continuano a operare.

## Configurazione

La CLI aggiunge opzioni ripetibili per bootstrap e relay e un'opzione esplicita
per abilitare il servizio relay locale. Ogni indirizzo deve includere il Peer ID
quando necessario; configurazioni malformate causano un errore leggibile
all'avvio. Nessun indirizzo infrastrutturale viene compilato nel binario.

## Flusso dati e stato

Gli eventi Identify aggiornano gli indirizzi conosciuti da Kademlia. Dopo il
bootstrap, il client cerca i Peer ID importati dalle schede contatto. Gli eventi
AutoNAT aggiornano soltanto lo stato di raggiungibilità e la modalità di
pubblicazione. Se un contatto non è raggiungibile direttamente, il client usa
un indirizzo relay configurato; quando DCUtR riesce, libp2p mantiene il percorso
diretto.

Messaggi, allegati, presenza e trilli continuano a passare dal protocollo
request-response esistente, indipendentemente dal trasporto scelto.

## Errori e sicurezza

- bootstrap o relay irraggiungibili vengono registrati senza terminare il client;
- input CLI e indirizzi remoti vengono validati prima dell'uso;
- i relay non sono considerati fidati e non ricevono chiavi applicative;
- non viene dichiarata resistenza post-quantum: QUIC protegge ancora soltanto il
  trasporto;
- limiti e validazioni esistenti per eventi e allegati restano invariati.

## Verifica

Lo sviluppo segue cicli test-first:

1. parsing e validazione della nuova configurazione;
2. transizioni dello stato di scoperta e scelta del percorso;
3. test dell'integrazione dei behaviour;
4. collaudo multiprocesso locale con un bootstrap/relay e due client;
5. `cargo fmt`, Clippy con warning negati e test dell'intero workspace.

Il collaudo locale dimostra bootstrap, discovery e trasporto relay. Il successo
di AutoNAT e hole punching attraverso CGNAT non può essere dimostrato in una
singola macchina e sarà verificato separatamente su due reti reali.
