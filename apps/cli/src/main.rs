mod attachments;
mod connectivity;
mod contacts;
mod crypto;
mod history;

use attachments::{build_manifest, read_chunk, Receiver};
use connectivity::{split_peer_address, FallbackPlanner, Recovery};
use crypto::{
    accepts_inbound, needs_outbound_handshake, respond as respond_hybrid, HybridInitiator,
    HybridResponse, RatchetMessage, RatchetSession, SessionKey,
};
use futures::StreamExt;
use history::History;
use libp2p::{
    autonat, dcutr, identify,
    identity::Keypair,
    kad, mdns,
    multiaddr::Protocol,
    noise, ping, relay,
    request_response::{self, ProtocolSupport},
    swarm::{behaviour::toggle::Toggle, dial_opts::DialOpts, NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm,
};
use msnnext_protocol::{
    resolve_emoticons, validate_text_message, validate_triggers, AttachmentManifest, ChatEvent,
    Emoticon, EmoticonOffer, Envelope, Mime, Nudge, NudgeRateLimit, PresenceUpdate,
    ProtocolResponse, TextMessage, Trigger, PROTOCOL_VERSION,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncBufReadExt, BufReader};

// Emoticons stay tiny and atomic; regular attachments use the resumable chunk protocol.
const MAX_EMOTICON_BYTES: usize = 350_000;
const MAX_EMOTICON_SIDE: usize = 512;
const MAX_REQUEST_RETRIES: u8 = 2;

#[derive(NetworkBehaviour)]
struct Behaviour {
    chat: request_response::cbor::Behaviour<Envelope, ProtocolResponse>,
    secure_chat: request_response::cbor::Behaviour<RatchetMessage, ProtocolResponse>,
    handshake: request_response::cbor::Behaviour<crypto::HybridClientHello, HybridResponse>,
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    autonat: autonat::Behaviour,
    relay_client: relay::client::Behaviour,
    relay_server: Toggle<relay::Behaviour>,
    dcutr: dcutr::Behaviour,
    ping: ping::Behaviour,
}

struct PendingOffer {
    peer: PeerId,
    path: PathBuf,
    manifest: AttachmentManifest,
    retries: u8,
}

struct PendingTransfer {
    peer: PeerId,
    path: PathBuf,
    manifest: AttachmentManifest,
    remaining: VecDeque<u32>,
    current: u32,
    retries: u8,
}

struct Incoming<'a> {
    emotes: &'a Path,
    triggers: &'a mut Vec<Trigger>,
    nudge_limits: &'a mut HashMap<PeerId, NudgeRateLimit>,
    attachments: &'a mut Receiver,
    history: &'a History,
    notifications: bool,
    peer_names: &'a mut HashMap<PeerId, String>,
}

fn build_swarm(
    identity: Keypair,
    relay_server_enabled: bool,
) -> Result<Swarm<Behaviour>, Box<dyn Error>> {
    let local_peer_id = PeerId::from(identity.public());
    let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;

    Ok(libp2p::SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(move |key, relay_client| {
            let kad = kad::Behaviour::with_config(
                local_peer_id,
                kad::store::MemoryStore::new(local_peer_id),
                kad::Config::new(StreamProtocol::new("/msnnext/kad/1")),
            );
            Behaviour {
                chat: request_response::cbor::Behaviour::new(
                    [(
                        StreamProtocol::new("/msnnext/chat/1"),
                        ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                ),
                secure_chat: request_response::cbor::Behaviour::new(
                    [(
                        StreamProtocol::new("/msnnext/chat/2"),
                        ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                ),
                handshake: request_response::cbor::Behaviour::new(
                    [(
                        StreamProtocol::new("/msnnext/handshake/1"),
                        ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                ),
                mdns,
                identify: identify::Behaviour::new(identify::Config::new(
                    "/msnnext/id/1".into(),
                    key.public(),
                )),
                kad,
                autonat: autonat::Behaviour::new(local_peer_id, autonat::Config::default()),
                relay_client,
                relay_server: Toggle::from(
                    relay_server_enabled
                        .then(|| relay::Behaviour::new(local_peer_id, relay::Config::default())),
                ),
                dcutr: dcutr::Behaviour::new(local_peer_id),
                ping: ping::Behaviour::new(ping::Config::new()),
            }
        })?
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(60)))
        .build())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse()?;
    let identity = load_or_create_identity(&args.identity)?;
    let public_key = identity.public();
    let history_key = blake3::derive_key(
        "msnnext local history v1",
        &identity.to_protobuf_encoding()?,
    );
    let history = History::open(&args.history, history_key)?;
    let local_peer_id = PeerId::from(identity.public());
    let mut swarm = build_swarm(identity, args.relay_server)?;

    swarm.listen_on(args.listen.clone())?;
    swarm.listen_on(args.listen_tcp.clone())?;
    if let Some(address) = args.connect.clone() {
        swarm.dial(address)?;
    }
    let bootstrap_peers = args
        .bootstrap
        .iter()
        .filter_map(|address| split_peer_address(address).ok().map(|(peer, _)| peer))
        .collect::<HashSet<_>>();
    for address in &args.bootstrap {
        let (peer, base) = split_peer_address(address)?;
        swarm.behaviour_mut().kad.add_address(&peer, base);
        if let Err(error) = swarm.dial(address.clone()) {
            eprintln!("bootstrap non raggiungibile {peer}: {error}");
        }
    }
    if !args.bootstrap.is_empty() {
        swarm.behaviour_mut().kad.bootstrap()?;
    }
    let relay_addresses = args
        .relays
        .iter()
        .map(|address| split_peer_address(address).map(|(peer, _)| (peer, address.clone())))
        .collect::<Result<HashMap<_, _>, _>>()?;
    for address in &args.relays {
        let (peer, _) = split_peer_address(address)?;
        if !bootstrap_peers.contains(&peer) {
            if let Err(error) = swarm.dial(address.clone()) {
                eprintln!("relay non raggiungibile {address}: {error}");
            }
        }
    }

    println!("peer: {local_peer_id}");
    println!("comandi: text <messaggio> | emote <trigger> <file> | file <percorso> | contact qr | contact export/import <file> | nudge | history | quit");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut peers = HashSet::new();
    let mut dialing = HashSet::new();
    let mut bootstrap_fallbacks = HashMap::<PeerId, VecDeque<Multiaddr>>::new();
    let mut known_contacts = HashSet::<PeerId>::new();
    let infrastructure_peers = args
        .bootstrap
        .iter()
        .chain(&args.relays)
        .filter_map(|address| split_peer_address(address).ok().map(|(peer, _)| peer))
        .collect::<HashSet<_>>();
    let manual_peer = args
        .connect
        .as_ref()
        .and_then(|address| split_peer_address(address).ok().map(|(peer, _)| peer));
    if let Some(peer) = manual_peer {
        known_contacts.insert(peer);
    }
    let mut accept_unclassified_manual = args.connect.is_some() && manual_peer.is_none();
    let mut fallback_planner = FallbackPlanner::new(args.relays.clone());
    let mut pending_dht = HashMap::<kad::QueryId, PeerId>::new();
    let mut requested_relay_reservations = HashSet::<PeerId>::new();
    let mut triggers = load_triggers(&args.emotes)?;
    let mut nudge_limits = HashMap::<PeerId, NudgeRateLimit>::new();
    let mut incoming_nudge_limits = HashMap::<PeerId, NudgeRateLimit>::new();
    let mut sent_numbers = HashMap::<PeerId, u64>::new();
    let mut pending_offers = HashMap::<request_response::OutboundRequestId, PendingOffer>::new();
    let mut pending_transfers =
        HashMap::<request_response::OutboundRequestId, PendingTransfer>::new();
    let mut pending_handshakes =
        HashMap::<request_response::OutboundRequestId, (PeerId, HybridInitiator)>::new();
    let mut pending_inbound_handshakes =
        HashMap::<request_response::InboundRequestId, (PeerId, SessionKey)>::new();
    let mut sessions = HashMap::<PeerId, RatchetSession>::new();
    let mut attachment_receiver = Receiver::new(args.downloads.clone());
    let mut peer_names = HashMap::<PeerId, String>::new();
    let mut nudge_counter = 0_u64;
    let mut stdin_open = true;

    loop {
        tokio::select! {
            line = lines.next_line(), if stdin_open => match line? {
                Some(line) if line == "quit" => break,
                Some(line) if line == "history" => match history.latest(20) {
                    Ok(entries) => for entry in entries.iter().rev() {
                        println!("{} {} {} {}: {}", entry.timestamp_ms, entry.direction, entry.peer, entry.kind, entry.body);
                    },
                    Err(error) => eprintln!("cronologia non disponibile: {error}"),
                },
                Some(line) if line.starts_with("contact export ") => {
                    let path = Path::new(line[15..].trim());
                    match contacts::export(
                        path,
                        &args.name,
                        local_peer_id,
                        &public_key,
                        contact_addresses(&swarm).into_iter(),
                    ) {
                        Ok(()) => println!("scheda contatto salvata in {}", path.display()),
                        Err(error) => eprintln!("esportazione contatto fallita: {error}"),
                    }
                },
                Some(line) if line == "contact qr" => {
                    let result = contacts::link(
                        &args.name,
                        local_peer_id,
                        &public_key,
                        contact_addresses(&swarm).into_iter(),
                    )
                    .and_then(|link| {
                        let qr = contacts::render_qr(&link)?;
                        Ok((link, qr))
                    });
                    match result {
                        Ok((link, qr)) => println!("{qr}\n{link}"),
                        Err(error) => eprintln!("generazione QR fallita: {error}"),
                    }
                },
                Some(line) if line.starts_with("contact import-link ") => match contacts::import_link(line[20..].trim()) {
                    Ok((name, peer_id, addresses)) => {
                        peer_names.insert(peer_id, name.clone());
                        known_contacts.insert(peer_id);
                        println!("identità contatto verificata: {name} ({peer_id})");
                        connect_contact(
                            &mut swarm,
                            peer_id,
                            addresses,
                            &mut dialing,
                            &mut bootstrap_fallbacks,
                            &mut fallback_planner,
                            &mut pending_dht,
                        );
                    }
                    Err(error) => eprintln!("link contatto rifiutato: {error}"),
                },
                Some(line) if line.starts_with("contact import ") => match contacts::import(Path::new(line[15..].trim())) {
                    Ok((name, peer_id, addresses)) => {
                        peer_names.insert(peer_id, name.clone());
                        known_contacts.insert(peer_id);
                        println!("contatto importato: {name} ({peer_id})");
                        connect_contact(
                            &mut swarm,
                            peer_id,
                            addresses,
                            &mut dialing,
                            &mut bootstrap_fallbacks,
                            &mut fallback_planner,
                            &mut pending_dht,
                        );
                    }
                    Err(error) => eprintln!("importazione contatto fallita: {error}"),
                },
                Some(line) if line == "nudge" => {
                    nudge_counter += 1;
                    let now = now_ms();
                    let mut id = [0; 16];
                    id[..8].copy_from_slice(&now.to_be_bytes());
                    id[8..].copy_from_slice(&nudge_counter.to_be_bytes());
                    if peers.is_empty() { eprintln!("nessun peer collegato"); }
                    for peer in &peers {
                        match nudge_limits.entry(*peer).or_default().try_acquire(now) {
                            Ok(()) => {
                                send_event(&mut swarm, &mut sessions, *peer, local_peer_id, &mut sent_numbers, ChatEvent::Nudge(Nudge { id, intensity: 1, timestamp_ms: now }));
                                record(&history, peer, "out", "nudge", "trillo");
                            }
                            Err(wait_ms) => eprintln!("trillo limitato per {peer}: riprova tra {}s", wait_ms.div_ceil(1_000)),
                        }
                    }
                }
                Some(line) if line.starts_with("text ") => {
                    let text = line[5..].to_owned();
                    let emoticons = resolve_emoticons(&text, &triggers)
                        .map_err(|error| format!("trigger emoticon non valido: {error:?}"))?;
                    for peer in &peers { record(&history, peer, "out", "text", &text); }
                    let event = ChatEvent::Text(TextMessage { text, emoticons });
                    broadcast(&mut swarm, &mut sessions, &peers, local_peer_id, &mut sent_numbers, event);
                }
                Some(line) if line.starts_with("emote ") => match parse_emote_command(&line, &args.emotes, &mut triggers) {
                    Ok(offer) => {
                        println!("emoticon salvata: {}", offer.metadata.name);
                        for peer in &peers { record(&history, peer, "out", "emote", &offer.metadata.name); }
                        broadcast(&mut swarm, &mut sessions, &peers, local_peer_id, &mut sent_numbers, ChatEvent::EmoticonOffer(offer));
                    }
                    Err(error) => eprintln!("emoticon rifiutata: {error}"),
                },
                Some(line) if line.starts_with("file ") => match build_manifest(Path::new(line[5..].trim())) {
                    Ok(_manifest) if peers.is_empty() => eprintln!("nessun peer collegato"),
                    Ok(manifest) => {
                        let path = PathBuf::from(line[5..].trim());
                        println!("offerta file: {} ({} chunk)", manifest.filename, manifest.chunks.len());
                        for peer in &peers {
                            record(&history, peer, "out", "file", &manifest.filename);
                            if let Some(request_id) = send_event(&mut swarm, &mut sessions, *peer, local_peer_id, &mut sent_numbers, ChatEvent::AttachmentOffer(manifest.clone())) {
                                pending_offers.insert(request_id, PendingOffer { peer: *peer, path: path.clone(), manifest: manifest.clone(), retries: 0 });
                            }
                        }
                    }
                    Err(error) => eprintln!("file rifiutato: {error}"),
                },
                Some(_) => eprintln!("comando non valido"),
                None => stdin_open = false,
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    let address = if split_peer_address(&address).is_ok() {
                        address
                    } else {
                        address.with(Protocol::P2p(local_peer_id))
                    };
                    println!("ascolto: {address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    dialing.remove(&peer_id);
                    bootstrap_fallbacks.remove(&peer_id);
                    fallback_planner.connected(peer_id);
                    if let Some(address) = relay_addresses.get(&peer_id) {
                        if requested_relay_reservations.insert(peer_id) {
                            if let Err(error) =
                                swarm.listen_on(address.clone().with(Protocol::P2pCircuit))
                            {
                                eprintln!("prenotazione relay fallita: {error}");
                            }
                        }
                    }
                    let is_manual = accept_unclassified_manual && !infrastructure_peers.contains(&peer_id);
                    if is_manual {
                        accept_unclassified_manual = false;
                        known_contacts.insert(peer_id);
                    }
                    if (known_contacts.contains(&peer_id) || is_manual) && peers.insert(peer_id) {
                        println!("connesso: {peer_id}");
                        send_event(&mut swarm, &mut sessions, peer_id, local_peer_id, &mut sent_numbers, ChatEvent::Presence(PresenceUpdate { display_name: args.name.clone(), online: true }));
                        maybe_start_hybrid_handshake(
                            &mut swarm,
                            &mut pending_handshakes,
                            &sessions,
                            local_peer_id,
                            peer_id,
                        );
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                    if num_established == 0 && peers.remove(&peer_id) {
                        sessions.remove(&peer_id);
                        pending_inbound_handshakes
                            .retain(|_, (pending_peer, _)| *pending_peer != peer_id);
                        nudge_limits.remove(&peer_id);
                        incoming_nudge_limits.remove(&peer_id);
                        println!("offline: {}", peer_names.get(&peer_id).map_or_else(|| peer_id.to_string(), Clone::clone));
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Chat(request_response::Event::Message { peer, message, .. })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let validation = if matches!(&request.event, ChatEvent::Presence(_)) {
                            validate_envelope(peer, local_peer_id, &request)
                        } else {
                            Err("usa il protocollo chat cifrato")
                        };
                        let (response, valid) = match validation {
                            Ok(()) => (
                                receive_event(peer, &request.event, &mut Incoming {
                                    emotes: &args.emotes,
                                    triggers: &mut triggers,
                                    nudge_limits: &mut incoming_nudge_limits,
                                    attachments: &mut attachment_receiver,
                                    history: &history,
                                    notifications: args.notifications,
                                    peer_names: &mut peer_names,
                                }),
                                true,
                            ),
                            Err(error) => (ProtocolResponse::Rejected(error.into()), false),
                        };
                        swarm.behaviour_mut().chat.send_response(channel, response).ok();
                        if valid {
                            if should_classify_application_peer(
                                true,
                                infrastructure_peers.contains(&peer),
                                peers.contains(&peer),
                            ) {
                                known_contacts.insert(peer);
                                peers.insert(peer);
                                println!("connesso: {peer}");
                                send_event(
                                    &mut swarm,
                                    &mut sessions,
                                    peer,
                                    local_peer_id,
                                    &mut sent_numbers,
                                    ChatEvent::Presence(PresenceUpdate {
                                        display_name: args.name.clone(),
                                        online: true,
                                    }),
                                );
                            }
                            maybe_start_hybrid_handshake(
                                &mut swarm,
                                &mut pending_handshakes,
                                &sessions,
                                local_peer_id,
                                peer,
                            );
                        }
                    }
                    request_response::Message::Response { .. } => {}
                },
                SwarmEvent::Behaviour(BehaviourEvent::SecureChat(request_response::Event::Message { peer, message, .. })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let envelope = sessions
                            .get_mut(&peer)
                            .ok_or("sessione sicura non disponibile")
                            .and_then(|session| {
                                decrypt_envelope(session, peer, local_peer_id, &request)
                                    .map_err(|_| "messaggio cifrato non valido")
                            });
                        let response = match envelope {
                            Ok(envelope) => match validate_envelope(peer, local_peer_id, &envelope) {
                                Ok(()) => receive_event(peer, &envelope.event, &mut Incoming {
                                    emotes: &args.emotes,
                                    triggers: &mut triggers,
                                    nudge_limits: &mut incoming_nudge_limits,
                                    attachments: &mut attachment_receiver,
                                    history: &history,
                                    notifications: args.notifications,
                                    peer_names: &mut peer_names,
                                }),
                                Err(error) => ProtocolResponse::Rejected(error.into()),
                            },
                            Err(error) => ProtocolResponse::Rejected(error.into()),
                        };
                        swarm.behaviour_mut().secure_chat.send_response(channel, response).ok();
                    }
                    request_response::Message::Response { request_id, response } => {
                        if let Some(pending) = pending_offers.remove(&request_id) {
                            match response {
                                ProtocolResponse::MissingChunks(indices) => {
                                    println!("invio {}: {} chunk richiesti", pending.manifest.filename, indices.len());
                                    send_next_transfer_chunk(
                                        &mut swarm,
                                        &mut sessions,
                                        &mut sent_numbers,
                                        &mut pending_transfers,
                                        local_peer_id,
                                        PendingTransfer {
                                            peer: pending.peer,
                                            path: pending.path,
                                            manifest: pending.manifest,
                                            remaining: indices.into(),
                                            current: 0,
                                            retries: 0,
                                        },
                                    );
                                }
                                ProtocolResponse::Rejected(error) => eprintln!("file rifiutato da {peer}: {error}"),
                                ProtocolResponse::Ack => println!("file già completo sul destinatario"),
                            }
                        } else if let Some(transfer) = pending_transfers.remove(&request_id) {
                            match response {
                                ProtocolResponse::Ack => send_next_transfer_chunk(
                                    &mut swarm,
                                    &mut sessions,
                                    &mut sent_numbers,
                                    &mut pending_transfers,
                                    local_peer_id,
                                    transfer,
                                ),
                                ProtocolResponse::Rejected(error) => {
                                    eprintln!("chunk rifiutato da {peer}: {error}")
                                }
                                ProtocolResponse::MissingChunks(_) => {
                                    eprintln!("risposta chunk non valida da {peer}")
                                }
                            }
                        }
                    }
                },
                SwarmEvent::Behaviour(BehaviourEvent::SecureChat(request_response::Event::OutboundFailure { peer, request_id, error, .. })) => {
                    if let Some(mut pending) = pending_offers.remove(&request_id) {
                        if let Some(retries) = next_request_retry(pending.retries) {
                            pending.retries = retries;
                            if let Some(next_id) = send_event(
                                &mut swarm,
                                &mut sessions,
                                pending.peer,
                                local_peer_id,
                                &mut sent_numbers,
                                ChatEvent::AttachmentOffer(pending.manifest.clone()),
                            ) {
                                pending_offers.insert(next_id, pending);
                            }
                        } else {
                            eprintln!("offerta file fallita definitivamente per {peer}: {error}");
                        }
                    } else if let Some(mut transfer) = pending_transfers.remove(&request_id) {
                        if let Some(retries) = next_request_retry(transfer.retries) {
                            transfer.retries = retries;
                            send_current_transfer_chunk(
                                &mut swarm,
                                &mut sessions,
                                &mut sent_numbers,
                                &mut pending_transfers,
                                local_peer_id,
                                transfer,
                            );
                        } else {
                            eprintln!("trasferimento file fallito definitivamente per {peer}: {error}");
                        }
                    } else {
                        eprintln!("invio cifrato fallito per {peer}: {error}");
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::SecureChat(request_response::Event::InboundFailure { peer, error, .. })) => {
                    eprintln!("messaggio cifrato non ricevibile da {peer}: {error}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::SecureChat(request_response::Event::ResponseSent { .. })) => {}
                SwarmEvent::Behaviour(BehaviourEvent::Handshake(request_response::Event::Message { peer, message, .. })) => match message {
                    request_response::Message::Request { request_id, request, channel } => {
                        let authorized = handshake_peer_authorized(
                            known_contacts.contains(&peer),
                            peers.contains(&peer),
                        );
                        let (response, pending_key) = if !accepts_inbound(local_peer_id, peer)
                            || !authorized
                        {
                            (
                                HybridResponse::Rejected("handshake non autorizzato".into()),
                                None,
                            )
                        } else {
                            match respond_hybrid(&request, peer, local_peer_id) {
                                Ok((hello, session_key)) => (
                                    HybridResponse::Accepted(hello),
                                    Some(session_key),
                                ),
                                Err(error) => (HybridResponse::Rejected(error.to_string()), None),
                            }
                        };
                        if swarm.behaviour_mut().handshake.send_response(channel, response).is_ok() {
                            if let Some(session_key) = pending_key {
                                pending_inbound_handshakes.insert(request_id, (peer, session_key));
                            }
                        } else {
                            eprintln!("risposta handshake non consegnata a {peer}");
                        }
                    }
                    request_response::Message::Response { request_id, response } => {
                        let Some((expected_peer, initiator)) = pending_handshakes.remove(&request_id) else {
                            continue;
                        };
                        if expected_peer != peer {
                            eprintln!("risposta handshake ricevuta dal peer errato");
                            continue;
                        }
                        match response {
                            HybridResponse::Accepted(hello) => match initiator.finish(&hello) {
                                Ok(session_key) => {
                                    sessions.insert(
                                        peer,
                                        RatchetSession::new(session_key, local_peer_id, peer),
                                    );
                                    println!("handshake ibrido completato: {peer}");
                                }
                                Err(error) => eprintln!("handshake ibrido fallito con {peer}: {error}"),
                            },
                            HybridResponse::Rejected(error) => {
                                eprintln!("handshake ibrido rifiutato da {peer}: {error:?}");
                            }
                        }
                    }
                },
                SwarmEvent::Behaviour(BehaviourEvent::Handshake(request_response::Event::OutboundFailure { peer, request_id, error, .. })) => {
                    pending_handshakes.remove(&request_id);
                    eprintln!("handshake ibrido fallito con {peer}: {error}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Handshake(request_response::Event::InboundFailure { peer, request_id, error, .. })) => {
                    pending_inbound_handshakes.remove(&request_id);
                    eprintln!("richiesta handshake non valida da {peer}: {error}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Handshake(request_response::Event::ResponseSent { peer, request_id, .. })) => {
                    if let Some((expected_peer, session_key)) = pending_inbound_handshakes.remove(&request_id) {
                        if expected_peer == peer {
                            sessions.insert(
                                peer,
                                RatchetSession::new(session_key, local_peer_id, peer),
                            );
                            println!("handshake ibrido completato: {peer}");
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                    for address in info.listen_addrs {
                        let address = match split_peer_address(&address) {
                            Ok((address_peer, base)) if address_peer == peer_id => base,
                            Ok(_) => continue,
                            Err(_) => address,
                        };
                        swarm.behaviour_mut().kad.add_address(&peer_id, address);
                    }
                    swarm.add_external_address(info.observed_addr);
                }
                SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed { id, step, .. })) if step.last => {
                    if let Some(peer) = pending_dht.remove(&id) {
                        let recovery = fallback_planner.after_dht(peer);
                        execute_recovery(&mut swarm, &mut pending_dht, recovery);
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Autonat(autonat::Event::StatusChanged { new, .. })) => {
                    println!("raggiungibilità: {new:?}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::RelayClient(event)) => {
                    println!("relay client: {event:?}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::RelayServer(event)) => {
                    println!("relay server: {event:?}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Dcutr(event)) => {
                    println!("hole punching: {event:?}");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Ping(_)) => {}
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(discovered))) => {
                    for (peer_id, address) in discovered {
                        swarm.behaviour_mut().kad.add_address(&peer_id, address.clone());
                        if !args.relay_server
                            && peer_id != local_peer_id
                            && !infrastructure_peers.contains(&peer_id)
                            && !peers.contains(&peer_id)
                            && dialing.insert(peer_id)
                        {
                            known_contacts.insert(peer_id);
                            println!("peer LAN trovato: {peer_id}");
                            if let Err(error) = swarm.dial(address.with(Protocol::P2p(peer_id))) {
                                eprintln!("dial mDNS fallito: {error}");
                            }
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(_))) => {}
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    if let Some(peer_id) = peer_id {
                        if peers.contains(&peer_id) {
                            eprintln!("dial duplicato ignorato per peer già connesso: {peer_id}");
                        } else if let Some(address) = bootstrap_fallbacks.get_mut(&peer_id).and_then(VecDeque::pop_front) {
                            eprintln!("bootstrap fallito, provo il prossimo indirizzo: {error}");
                            if let Err(next_error) = swarm.dial(address) { eprintln!("indirizzo non raggiungibile: {next_error}"); }
                        } else {
                            dialing.remove(&peer_id);
                            eprintln!("connessione fallita: {error}");
                            if known_contacts.contains(&peer_id) {
                                let recovery = fallback_planner.after_failure(peer_id);
                                execute_recovery(&mut swarm, &mut pending_dht, recovery);
                            }
                        }
                    } else { eprintln!("connessione fallita: {error}"); }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn should_classify_application_peer(
    valid_message: bool,
    infrastructure: bool,
    already_connected: bool,
) -> bool {
    valid_message && !infrastructure && !already_connected
}

fn handshake_peer_authorized(known_contact: bool, active_peer: bool) -> bool {
    known_contact || active_peer
}

fn next_request_retry(attempts: u8) -> Option<u8> {
    (attempts < MAX_REQUEST_RETRIES).then_some(attempts + 1)
}

fn maybe_start_hybrid_handshake(
    swarm: &mut Swarm<Behaviour>,
    pending: &mut HashMap<request_response::OutboundRequestId, (PeerId, HybridInitiator)>,
    established: &HashMap<PeerId, RatchetSession>,
    local_peer: PeerId,
    remote_peer: PeerId,
) {
    let already_pending = pending
        .values()
        .any(|(pending_peer, _)| *pending_peer == remote_peer);
    if !needs_outbound_handshake(
        local_peer,
        remote_peer,
        established.contains_key(&remote_peer),
        already_pending,
    ) {
        return;
    }

    match HybridInitiator::start(local_peer, remote_peer) {
        Ok((initiator, hello)) => {
            let request_id = swarm
                .behaviour_mut()
                .handshake
                .send_request(&remote_peer, hello);
            pending.insert(request_id, (remote_peer, initiator));
        }
        Err(error) => eprintln!("avvio handshake ibrido fallito: {error}"),
    }
}

fn contact_addresses(swarm: &Swarm<Behaviour>) -> Vec<Multiaddr> {
    swarm
        .listeners()
        .cloned()
        .map(|address| {
            split_peer_address(&address)
                .map(|(_, base)| base)
                .unwrap_or(address)
        })
        .collect()
}

fn connect_contact(
    swarm: &mut Swarm<Behaviour>,
    peer: PeerId,
    addresses: Vec<Multiaddr>,
    dialing: &mut HashSet<PeerId>,
    direct_fallbacks: &mut HashMap<PeerId, VecDeque<Multiaddr>>,
    planner: &mut FallbackPlanner,
    pending_dht: &mut HashMap<kad::QueryId, PeerId>,
) {
    let mut valid = VecDeque::new();
    for address in addresses {
        match split_peer_address(&address) {
            Ok((address_peer, base)) if address_peer == peer => {
                swarm.behaviour_mut().kad.add_address(&peer, base);
                valid.push_back(address);
            }
            Ok(_) => eprintln!("indirizzo ignorato: Peer ID diverso dal contatto"),
            Err(error) => eprintln!("indirizzo contatto ignorato: {error}"),
        }
    }

    if let Some(address) = valid.pop_front() {
        dialing.insert(peer);
        direct_fallbacks.insert(peer, valid);
        if let Err(error) = swarm.dial(address) {
            eprintln!("indirizzo non raggiungibile: {error}");
            let recovery = planner.after_failure(peer);
            execute_recovery(swarm, pending_dht, recovery);
        }
    } else {
        let recovery = planner.after_failure(peer);
        execute_recovery(swarm, pending_dht, recovery);
    }
}

fn execute_recovery(
    swarm: &mut Swarm<Behaviour>,
    pending_dht: &mut HashMap<kad::QueryId, PeerId>,
    recovery: Recovery,
) {
    match recovery {
        Recovery::SearchDht(peer) => {
            println!("cerco {peer} nella DHT");
            let query = swarm.behaviour_mut().kad.get_closest_peers(peer);
            pending_dht.insert(query, peer);
        }
        Recovery::DialPeer(peer) => {
            println!("provo indirizzi DHT per {peer}");
            if let Err(error) = swarm.dial(DialOpts::peer_id(peer).build()) {
                eprintln!("dial DHT fallito: {error}");
            }
        }
        Recovery::ViaRelay(address) => {
            println!("provo il relay: {address}");
            if let Err(error) = swarm.dial(address) {
                eprintln!("dial relay fallito: {error}");
            }
        }
        Recovery::Exhausted => eprintln!("nessun altro percorso disponibile"),
    }
}

fn encrypt_envelope(
    session: &mut RatchetSession,
    sender: PeerId,
    recipient: PeerId,
    envelope: &Envelope,
) -> Result<RatchetMessage, Box<dyn Error>> {
    let encoded = cbor4ii::serde::to_vec(Vec::new(), envelope)?;
    Ok(session.encrypt(&encoded, &chat_associated_data(sender, recipient))?)
}

fn decrypt_envelope(
    session: &mut RatchetSession,
    sender: PeerId,
    recipient: PeerId,
    message: &RatchetMessage,
) -> Result<Envelope, Box<dyn Error>> {
    let encoded = session.decrypt(message, &chat_associated_data(sender, recipient))?;
    Ok(cbor4ii::serde::from_slice(&encoded)?)
}

fn chat_associated_data(sender: PeerId, recipient: PeerId) -> Vec<u8> {
    let mut associated_data = b"/msnnext/chat/2".to_vec();
    associated_data.extend_from_slice(&sender.to_bytes());
    associated_data.extend_from_slice(&recipient.to_bytes());
    associated_data
}

fn broadcast(
    swarm: &mut libp2p::Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    peers: &HashSet<PeerId>,
    local_peer_id: PeerId,
    sent_numbers: &mut HashMap<PeerId, u64>,
    event: ChatEvent,
) {
    if peers.is_empty() {
        eprintln!("nessun peer collegato");
    }
    for peer in peers {
        send_event(
            swarm,
            sessions,
            *peer,
            local_peer_id,
            sent_numbers,
            event.clone(),
        );
    }
}

fn send_event(
    swarm: &mut libp2p::Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    peer: PeerId,
    local_peer_id: PeerId,
    sent_numbers: &mut HashMap<PeerId, u64>,
    event: ChatEvent,
) -> Option<request_response::OutboundRequestId> {
    let secure = sessions.contains_key(&peer);
    if !secure && !matches!(event, ChatEvent::Presence(_)) {
        eprintln!("sessione sicura non pronta per {peer}");
        return None;
    }
    let previous = sent_numbers.get(&peer).copied().unwrap_or_default();
    let number = match previous.checked_add(1) {
        Some(number) => number,
        None => {
            eprintln!("contatore messaggi esaurito per {peer}");
            return None;
        }
    };
    let envelope = Envelope {
        protocol_version: PROTOCOL_VERSION,
        conversation_id: conversation_id(local_peer_id, peer),
        sender_device_id: device_id(local_peer_id),
        message_number: number,
        previous_message_number: previous,
        event,
    };
    let request_id = if let Some(session) = sessions.get_mut(&peer) {
        let encrypted = match encrypt_envelope(session, local_peer_id, peer, &envelope) {
            Ok(encrypted) => encrypted,
            Err(error) => {
                eprintln!("cifratura messaggio fallita per {peer}: {error}");
                return None;
            }
        };
        swarm
            .behaviour_mut()
            .secure_chat
            .send_request(&peer, encrypted)
    } else {
        swarm.behaviour_mut().chat.send_request(&peer, envelope)
    };
    sent_numbers.insert(peer, number);
    Some(request_id)
}

fn send_next_transfer_chunk(
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_transfers: &mut HashMap<request_response::OutboundRequestId, PendingTransfer>,
    local_peer_id: PeerId,
    mut transfer: PendingTransfer,
) {
    let Some(index) = transfer.remaining.pop_front() else {
        println!("invio completato: {}", transfer.manifest.filename);
        return;
    };
    transfer.current = index;
    transfer.retries = 0;
    send_current_transfer_chunk(
        swarm,
        sessions,
        sent_numbers,
        pending_transfers,
        local_peer_id,
        transfer,
    );
}

fn send_current_transfer_chunk(
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_transfers: &mut HashMap<request_response::OutboundRequestId, PendingTransfer>,
    local_peer_id: PeerId,
    transfer: PendingTransfer,
) {
    let chunk = match read_chunk(&transfer.path, &transfer.manifest, transfer.current) {
        Ok(chunk) => chunk,
        Err(error) => {
            eprintln!("invio interrotto: {error}");
            return;
        }
    };
    if let Some(request_id) = send_event(
        swarm,
        sessions,
        transfer.peer,
        local_peer_id,
        sent_numbers,
        ChatEvent::AttachmentChunk(chunk),
    ) {
        pending_transfers.insert(request_id, transfer);
    }
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
        ChatEvent::Presence(presence)
            if presence.display_name.trim().is_empty() || presence.display_name.len() > 64 =>
        {
            Err("presenza non valida")
        }
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

fn receive_event(peer: PeerId, event: &ChatEvent, context: &mut Incoming<'_>) -> ProtocolResponse {
    match event {
        ChatEvent::Text(message) => {
            println!(
                "{peer}: {} [{} emoticon]",
                message.text,
                message.emoticons.len()
            );
            record(context.history, &peer, "in", "text", &message.text);
            notify(context.notifications, "Nuovo messaggio", &message.text);
            ProtocolResponse::Ack
        }
        ChatEvent::Nudge(_) => match context
            .nudge_limits
            .entry(peer)
            .or_default()
            .try_acquire(now_ms())
        {
            Ok(()) => {
                println!("{peer}: *** TRILLO ***");
                record(context.history, &peer, "in", "nudge", "trillo");
                notify(context.notifications, "Trillo", "Hai ricevuto un trillo");
                ProtocolResponse::Ack
            }
            Err(_) => {
                eprintln!("{peer}: trillo ricevuto ignorato per rate limit");
                ProtocolResponse::Ack
            }
        },
        ChatEvent::EmoticonOffer(offer) => {
            match save_offer(context.emotes, offer, context.triggers) {
                Ok(path) => {
                    println!("{peer}: emoticon salvata in {}", path.display());
                    record(context.history, &peer, "in", "emote", &offer.metadata.name);
                    ProtocolResponse::Ack
                }
                Err(error) => ProtocolResponse::Rejected(error.to_string()),
            }
        }
        ChatEvent::AttachmentOffer(manifest) => {
            match context.attachments.accept_offer(manifest.clone()) {
                Ok((missing, completed)) => {
                    record(context.history, &peer, "in", "file", &manifest.filename);
                    if let Some(path) = completed {
                        println!("{peer}: file già completo in {}", path.display());
                        notify(context.notifications, "File ricevuto", &manifest.filename);
                    }
                    ProtocolResponse::MissingChunks(missing)
                }
                Err(error) => ProtocolResponse::Rejected(error.to_string()),
            }
        }
        ChatEvent::AttachmentChunk(chunk) => match context.attachments.accept_chunk(chunk) {
            Ok(Some(path)) => {
                println!("{peer}: file ricevuto in {}", path.display());
                notify(
                    context.notifications,
                    "File ricevuto",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("file"),
                );
                ProtocolResponse::Ack
            }
            Ok(None) => ProtocolResponse::Ack,
            Err(error) => ProtocolResponse::Rejected(error.to_string()),
        },
        ChatEvent::Presence(presence) => {
            context
                .peer_names
                .insert(peer, presence.display_name.clone());
            println!(
                "{} è {}",
                presence.display_name,
                if presence.online { "online" } else { "offline" }
            );
            ProtocolResponse::Ack
        }
    }
}

fn record(history: &History, peer: &PeerId, direction: &str, kind: &str, body: &str) {
    if let Err(error) = history.record(&peer.to_string(), direction, kind, body, now_ms()) {
        eprintln!("cronologia non aggiornata: {error}");
    }
}

fn notify(enabled: bool, summary: &str, body: &str) {
    if enabled {
        notify_rust::Notification::new()
            .summary(summary)
            .body(body)
            .appname("msnnext")
            .show()
            .ok();
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
    listen_tcp: Multiaddr,
    connect: Option<Multiaddr>,
    bootstrap: Vec<Multiaddr>,
    relays: Vec<Multiaddr>,
    relay_server: bool,
    identity: PathBuf,
    emotes: PathBuf,
    downloads: PathBuf,
    history: PathBuf,
    notifications: bool,
    name: String,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from<I, S>(args: I) -> Result<Self, Box<dyn Error>>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut listen = "/ip4/0.0.0.0/udp/4040/quic-v1".parse()?;
        let mut listen_tcp = "/ip4/0.0.0.0/tcp/0".parse()?;
        let mut connect = None;
        let mut bootstrap = Vec::new();
        let mut relays = Vec::new();
        let mut relay_server = false;
        let mut identity = PathBuf::from(".msnnext/identity.key");
        let mut emotes = PathBuf::from(".msnnext/emoticons");
        let mut downloads = PathBuf::from(".msnnext/downloads");
        let mut history = PathBuf::from(".msnnext/history.db");
        let mut notifications = false;
        let mut name = std::env::var("USERNAME").unwrap_or_else(|_| "Amico".into());
        let mut args = args.into_iter().map(Into::into);
        while let Some(flag) = args.next() {
            if flag == "--relay-server" {
                relay_server = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("manca il valore per {flag}"))?;
            match flag.as_str() {
                "--listen" => listen = value.parse()?,
                "--listen-tcp" => listen_tcp = value.parse()?,
                "--connect" => connect = Some(value.parse()?),
                "--bootstrap" => bootstrap.push(value.parse()?),
                "--relay" => relays.push(value.parse()?),
                "--identity" => identity = value.into(),
                "--emotes" => emotes = value.into(),
                "--downloads" => downloads = value.into(),
                "--history" => history = value.into(),
                "--notify" => notifications = value.parse()?,
                "--name" => name = value,
                _ => return Err(format!("opzione sconosciuta: {flag}").into()),
            }
        }
        if name.trim().is_empty() || name.len() > 64 {
            return Err("nome non valido".into());
        }
        Ok(Self {
            listen,
            listen_tcp,
            connect,
            bootstrap,
            relays,
            relay_server,
            identity,
            emotes,
            downloads,
            history,
            notifications,
            name,
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

    #[test]
    fn envelope_round_trips_through_the_session_ratchet() {
        let alice = PeerId::from(Keypair::generate_ed25519().public());
        let bob = PeerId::from(Keypair::generate_ed25519().public());
        let (initiator, hello) = HybridInitiator::start(alice, bob).unwrap();
        let (response, bob_key) = respond_hybrid(&hello, alice, bob).unwrap();
        let alice_key = initiator.finish(&response).unwrap();
        let mut alice_session = RatchetSession::new(alice_key, alice, bob);
        let mut bob_session = RatchetSession::new(bob_key, bob, alice);
        let envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            conversation_id: conversation_id(alice, bob),
            sender_device_id: device_id(alice),
            message_number: 1,
            previous_message_number: 0,
            event: ChatEvent::Text(TextMessage {
                text: "segreto".into(),
                emoticons: vec![],
            }),
        };

        let encrypted = encrypt_envelope(&mut alice_session, alice, bob, &envelope).unwrap();
        let decrypted = decrypt_envelope(&mut bob_session, alice, bob, &encrypted).unwrap();

        assert_eq!(decrypted, envelope);
    }

    #[test]
    fn valid_presence_classifies_an_unknown_application_peer() {
        assert!(should_classify_application_peer(true, false, false));
        assert!(!should_classify_application_peer(true, true, false));
        assert!(!should_classify_application_peer(false, false, false));
        assert!(!should_classify_application_peer(true, false, true));
    }

    #[test]
    fn handshake_requires_an_authorized_application_peer() {
        assert!(!handshake_peer_authorized(false, false));
        assert!(handshake_peer_authorized(true, false));
        assert!(handshake_peer_authorized(false, true));
    }

    #[test]
    fn failed_secure_requests_have_bounded_retries() {
        assert_eq!(next_request_retry(0), Some(1));
        assert_eq!(next_request_retry(1), Some(2));
        assert_eq!(next_request_retry(2), None);
    }

    #[test]
    fn parses_connectivity_options() {
        let bootstrap_peer = PeerId::from(Keypair::generate_ed25519().public());
        let relay_peer = PeerId::from(Keypair::generate_ed25519().public());
        let bootstrap = format!("/ip4/127.0.0.1/tcp/4001/p2p/{bootstrap_peer}");
        let relay = format!("/ip4/127.0.0.1/tcp/4002/p2p/{relay_peer}");

        let args = Args::parse_from(vec![
            "--listen-tcp".to_owned(),
            "/ip4/127.0.0.1/tcp/4000".to_owned(),
            "--bootstrap".to_owned(),
            bootstrap,
            "--relay".to_owned(),
            relay,
            "--relay-server".to_owned(),
        ])
        .unwrap();

        assert_eq!(args.bootstrap.len(), 1);
        assert_eq!(args.relays.len(), 1);
        assert!(args.relay_server);
        assert_eq!(args.listen_tcp, "/ip4/127.0.0.1/tcp/4000".parse().unwrap());
    }

    #[tokio::test]
    async fn builds_connectivity_swarm() {
        let identity = Keypair::generate_ed25519();
        let expected_peer = PeerId::from(identity.public());

        let swarm = build_swarm(identity, false).unwrap();

        assert_eq!(*swarm.local_peer_id(), expected_peer);
    }
}
