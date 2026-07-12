use futures::StreamExt;
use libp2p::{
    identity::Keypair,
    request_response::{self, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent},
    Multiaddr, PeerId, StreamProtocol,
};
use msnnext_protocol::{
    resolve_emoticons, validate_text_message, validate_triggers, ChatEvent, Emoticon,
    EmoticonOffer, Envelope, Mime, Nudge, NudgeRateLimit, TextMessage, Trigger, PROTOCOL_VERSION,
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncBufReadExt, BufReader};

// ponytail: one bounded request; replace with manifests/chunks when Milestone 2 adds resume.
const MAX_EMOTICON_BYTES: usize = 350_000;
const MAX_EMOTICON_SIDE: usize = 512;

#[derive(NetworkBehaviour)]
struct Behaviour {
    chat: request_response::cbor::Behaviour<Envelope, ()>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    let identity = load_or_create_identity(&args.identity)?;
    let local_peer_id = PeerId::from(identity.public());
    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_quic()
        .with_behaviour(|_| Behaviour {
            chat: request_response::cbor::Behaviour::new(
                [(
                    StreamProtocol::new("/msnnext/chat/1"),
                    ProtocolSupport::Full,
                )],
                request_response::Config::default(),
            ),
        })?
        .build();

    swarm.listen_on(args.listen)?;
    if let Some(address) = args.connect {
        swarm.dial(address)?;
    }

    println!("peer: {local_peer_id}");
    println!("comandi: text <messaggio> | emote <trigger> <file> | nudge | quit");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut peers = HashSet::new();
    let mut triggers = load_triggers(&args.emotes)?;
    let mut nudge_limits = HashMap::<PeerId, NudgeRateLimit>::new();
    let mut incoming_nudge_limits = HashMap::<PeerId, NudgeRateLimit>::new();
    let mut sent_numbers = HashMap::<PeerId, u64>::new();
    let mut nudge_counter = 0_u64;

    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(line) if line == "quit" => break,
                Some(line) if line == "nudge" => {
                    nudge_counter += 1;
                    let now = now_ms();
                    let mut id = [0; 16];
                    id[..8].copy_from_slice(&now.to_be_bytes());
                    id[8..].copy_from_slice(&nudge_counter.to_be_bytes());
                    if peers.is_empty() { eprintln!("nessun peer collegato"); }
                    for peer in &peers {
                        match nudge_limits.entry(*peer).or_default().try_acquire(now) {
                            Ok(()) => send_event(&mut swarm, *peer, local_peer_id, &mut sent_numbers, ChatEvent::Nudge(Nudge { id, intensity: 1, timestamp_ms: now })),
                            Err(wait_ms) => eprintln!("trillo limitato per {peer}: riprova tra {}s", wait_ms.div_ceil(1_000)),
                        }
                    }
                }
                Some(line) if line.starts_with("text ") => {
                    let text = line[5..].to_owned();
                    let emoticons = resolve_emoticons(&text, &triggers)
                        .map_err(|error| format!("trigger emoticon non valido: {error:?}"))?;
                    let event = ChatEvent::Text(TextMessage { text, emoticons });
                    broadcast(&mut swarm, &peers, local_peer_id, &mut sent_numbers, event);
                }
                Some(line) if line.starts_with("emote ") => match parse_emote_command(&line, &args.emotes, &mut triggers) {
                    Ok(offer) => {
                        println!("emoticon salvata: {}", offer.metadata.name);
                        broadcast(&mut swarm, &peers, local_peer_id, &mut sent_numbers, ChatEvent::EmoticonOffer(offer));
                    }
                    Err(error) => eprintln!("emoticon rifiutata: {error}"),
                },
                Some(_) => eprintln!("comando non valido"),
                None => break,
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => println!("ascolto: {address}/p2p/{local_peer_id}"),
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    peers.insert(peer_id);
                    println!("connesso: {peer_id}");
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    peers.remove(&peer_id);
                    nudge_limits.remove(&peer_id);
                    incoming_nudge_limits.remove(&peer_id);
                    println!("disconnesso: {peer_id}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Chat(request_response::Event::Message { peer, message, .. })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        match validate_envelope(peer, local_peer_id, &request) {
                            Ok(()) => receive_event(peer, &request.event, &args.emotes, &mut triggers, &mut incoming_nudge_limits),
                            Err(error) => eprintln!("{peer}: evento rifiutato: {error}"),
                        }
                        swarm.behaviour_mut().chat.send_response(channel, ()).ok();
                    }
                    request_response::Message::Response { .. } => {}
                },
                SwarmEvent::OutgoingConnectionError { error, .. } => eprintln!("connessione fallita: {error}"),
                _ => {}
            }
        }
    }
    Ok(())
}

fn broadcast(
    swarm: &mut libp2p::Swarm<Behaviour>,
    peers: &HashSet<PeerId>,
    local_peer_id: PeerId,
    sent_numbers: &mut HashMap<PeerId, u64>,
    event: ChatEvent,
) {
    if peers.is_empty() {
        eprintln!("nessun peer collegato");
    }
    for peer in peers {
        send_event(swarm, *peer, local_peer_id, sent_numbers, event.clone());
    }
}

fn send_event(
    swarm: &mut libp2p::Swarm<Behaviour>,
    peer: PeerId,
    local_peer_id: PeerId,
    sent_numbers: &mut HashMap<PeerId, u64>,
    event: ChatEvent,
) {
    let number = sent_numbers.entry(peer).or_default();
    *number += 1;
    let envelope = Envelope {
        protocol_version: PROTOCOL_VERSION,
        conversation_id: conversation_id(local_peer_id, peer),
        sender_device_id: device_id(local_peer_id),
        message_number: *number,
        previous_message_number: number.saturating_sub(1),
        event,
    };
    swarm.behaviour_mut().chat.send_request(&peer, envelope);
}

fn validate_envelope(
    peer: PeerId,
    local_peer_id: PeerId,
    envelope: &Envelope,
) -> Result<(), &'static str> {
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err("versione protocollo non supportata");
    }
    if envelope.sender_device_id != device_id(peer) {
        return Err("identità mittente non valida");
    }
    if envelope.conversation_id != conversation_id(local_peer_id, peer) {
        return Err("conversazione non valida");
    }
    if envelope.message_number == 0
        || envelope.previous_message_number.checked_add(1) != Some(envelope.message_number)
    {
        return Err("sequenza messaggi non valida");
    }
    match &envelope.event {
        ChatEvent::Text(message) => {
            validate_text_message(message).map_err(|_| "messaggio di testo non valido")
        }
        ChatEvent::Nudge(nudge) if nudge.intensity != 1 => Err("intensità trillo non valida"),
        _ => Ok(()),
    }
}

fn device_id(peer: PeerId) -> [u8; 16] {
    blake3::hash(&peer.to_bytes()).as_bytes()[..16]
        .try_into()
        .expect("16 bytes")
}

fn conversation_id(first: PeerId, second: PeerId) -> [u8; 32] {
    let mut peers = [first.to_bytes(), second.to_bytes()];
    peers.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"msnnext/conversation/1");
    hasher.update(&peers[0]);
    hasher.update(&peers[1]);
    *hasher.finalize().as_bytes()
}

fn receive_event(
    peer: PeerId,
    event: &ChatEvent,
    store: &Path,
    triggers: &mut Vec<Trigger>,
    nudge_limits: &mut HashMap<PeerId, NudgeRateLimit>,
) {
    match event {
        ChatEvent::Text(message) => println!(
            "{peer}: {} [{} emoticon]",
            message.text,
            message.emoticons.len()
        ),
        ChatEvent::Nudge(_) => match nudge_limits.entry(peer).or_default().try_acquire(now_ms()) {
            Ok(()) => println!("{peer}: *** TRILLO ***"),
            Err(_) => eprintln!("{peer}: trillo ricevuto ignorato per rate limit"),
        },
        ChatEvent::EmoticonOffer(offer) => match save_offer(store, offer, triggers) {
            Ok(path) => println!("{peer}: emoticon salvata in {}", path.display()),
            Err(error) => eprintln!("{peer}: emoticon rifiutata: {error}"),
        },
    }
}

fn default_triggers() -> Vec<Trigger> {
    vec![
        Trigger {
            text: ":)".into(),
            asset_id: [1; 32],
            case_sensitive: true,
        },
        Trigger {
            text: ":-)".into(),
            asset_id: [2; 32],
            case_sensitive: true,
        },
    ]
}

fn parse_emote_command(
    line: &str,
    store: &Path,
    triggers: &mut Vec<Trigger>,
) -> Result<EmoticonOffer, Box<dyn Error>> {
    let mut parts = line.splitn(3, ' ');
    parts.next();
    let trigger = parts.next().ok_or("manca il trigger")?;
    let path = parts.next().ok_or("manca il file")?;
    let bytes = fs::read(path)?;
    if bytes.len() > MAX_EMOTICON_BYTES {
        return Err(format!("massimo {MAX_EMOTICON_BYTES} byte").into());
    }
    let mime = detect_mime(&bytes).ok_or("formato non supportato")?;
    let size = imagesize::blob_size(&bytes)?;
    if size.width > MAX_EMOTICON_SIDE || size.height > MAX_EMOTICON_SIDE {
        return Err(format!("dimensioni massime {MAX_EMOTICON_SIDE}x{MAX_EMOTICON_SIDE}").into());
    }
    let asset_id = *blake3::hash(&bytes).as_bytes();
    let offer = EmoticonOffer {
        metadata: Emoticon {
            asset_id,
            mime,
            width: size.width as u16,
            height: size.height as u16,
            animated: mime == Mime::Gif
                || mime == Mime::Webp && bytes.windows(4).any(|part| part == b"ANIM"),
            suggested_triggers: vec![trigger.to_owned()],
            name: Path::new(path)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("emoticon")
                .to_owned(),
        },
        bytes,
    };
    save_offer(store, &offer, triggers)?;
    Ok(offer)
}

fn save_offer(
    store: &Path,
    offer: &EmoticonOffer,
    triggers: &mut Vec<Trigger>,
) -> Result<PathBuf, Box<dyn Error>> {
    validate_offer(offer)?;
    fs::create_dir_all(store)?;
    let hash = blake3::Hash::from_bytes(offer.metadata.asset_id)
        .to_hex()
        .to_string();
    let asset_path = store.join(format!("{hash}.{}", extension(offer.metadata.mime)));
    if !asset_path.exists() {
        fs::write(&asset_path, &offer.bytes)?;
    }

    if let Some(text) = offer.metadata.suggested_triggers.first() {
        let trigger = Trigger {
            text: text.clone(),
            asset_id: offer.metadata.asset_id,
            case_sensitive: true,
        };
        if let Some(existing) = triggers
            .iter()
            .find(|existing| existing.text == trigger.text)
        {
            if existing.asset_id != trigger.asset_id {
                eprintln!(
                    "trigger {} già assegnato: asset salvato senza scorciatoia",
                    trigger.text
                );
                return Ok(asset_path);
            }
        } else {
            validate_triggers(std::slice::from_ref(&trigger))
                .map_err(|error| format!("trigger non valido: {error:?}"))?;
            fs::write(store.join(format!("{hash}.trigger")), &trigger.text)?;
            triggers.push(trigger);
        }
    }
    Ok(asset_path)
}

fn validate_offer(offer: &EmoticonOffer) -> Result<(), Box<dyn Error>> {
    if offer.bytes.len() > MAX_EMOTICON_BYTES {
        return Err("emoticon troppo grande".into());
    }
    if *blake3::hash(&offer.bytes).as_bytes() != offer.metadata.asset_id {
        return Err("hash BLAKE3 non valido".into());
    }
    if detect_mime(&offer.bytes) != Some(offer.metadata.mime) {
        return Err("MIME non corrispondente ai byte".into());
    }
    let size = imagesize::blob_size(&offer.bytes)?;
    if size.width > MAX_EMOTICON_SIDE
        || size.height > MAX_EMOTICON_SIDE
        || size.width != offer.metadata.width as usize
        || size.height != offer.metadata.height as usize
    {
        return Err("dimensioni non valide".into());
    }
    Ok(())
}

fn detect_mime(bytes: &[u8]) -> Option<Mime> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Mime::Png)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(Mime::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(Mime::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(Mime::Webp)
    } else {
        None
    }
}

fn extension(mime: Mime) -> &'static str {
    match mime {
        Mime::Png => "png",
        Mime::Jpeg => "jpg",
        Mime::Gif => "gif",
        Mime::Webp => "webp",
    }
}

fn load_triggers(store: &Path) -> Result<Vec<Trigger>, Box<dyn Error>> {
    let mut triggers = default_triggers();
    if !store.exists() {
        return Ok(triggers);
    }
    for entry in fs::read_dir(store)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("trigger") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(hash) = blake3::Hash::from_hex(stem) else {
            continue;
        };
        let trigger = Trigger {
            text: fs::read_to_string(path)?.trim().to_owned(),
            asset_id: *hash.as_bytes(),
            case_sensitive: true,
        };
        if !triggers
            .iter()
            .any(|existing| existing.text == trigger.text)
        {
            triggers.push(trigger);
        }
    }
    validate_triggers(&triggers)
        .map_err(|error| format!("archivio trigger non valido: {error:?}"))?;
    Ok(triggers)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn load_or_create_identity(path: &Path) -> Result<Keypair, Box<dyn Error>> {
    if path.exists() {
        let bytes = fs::read(path)?;
        return Ok(Keypair::from_protobuf_encoding(&bytes)?);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let identity = Keypair::generate_ed25519();
    fs::write(path, identity.to_protobuf_encoding()?)?;
    Ok(identity)
}

struct Args {
    listen: Multiaddr,
    connect: Option<Multiaddr>,
    identity: PathBuf,
    emotes: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut listen = "/ip4/0.0.0.0/udp/4040/quic-v1".parse()?;
        let mut connect = None;
        let mut identity = PathBuf::from(".msnnext/identity.key");
        let mut emotes = PathBuf::from(".msnnext/emoticons");
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("manca il valore per {flag}"))?;
            match flag.as_str() {
                "--listen" => listen = value.parse()?,
                "--connect" => connect = Some(value.parse()?),
                "--identity" => identity = value.into(),
                "--emotes" => emotes = value.into(),
                _ => return Err(format!("opzione sconosciuta: {flag}").into()),
            }
        }
        Ok(Self {
            listen,
            connect,
            identity,
            emotes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_tampered_emoticon() {
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
        let mut offer = EmoticonOffer {
            metadata: Emoticon {
                asset_id: *blake3::hash(&bytes).as_bytes(),
                mime: Mime::Png,
                width: 1,
                height: 1,
                animated: false,
                suggested_triggers: vec![":x:".into()],
                name: "x".into(),
            },
            bytes,
        };
        assert!(validate_offer(&offer).is_ok());
        offer.bytes.push(1);
        assert!(validate_offer(&offer).is_err());
    }

    #[test]
    fn envelope_is_bound_to_sender_and_conversation() {
        let alice = PeerId::from(Keypair::generate_ed25519().public());
        let bob = PeerId::from(Keypair::generate_ed25519().public());
        let mut envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            conversation_id: conversation_id(alice, bob),
            sender_device_id: device_id(bob),
            message_number: 1,
            previous_message_number: 0,
            event: ChatEvent::Text(TextMessage {
                text: "ciao".into(),
                emoticons: vec![],
            }),
        };
        assert_eq!(validate_envelope(bob, alice, &envelope), Ok(()));
        envelope.conversation_id[0] ^= 1;
        assert_eq!(
            validate_envelope(bob, alice, &envelope),
            Err("conversazione non valida")
        );
    }
}
