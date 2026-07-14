mod attachments;
mod connectivity;
mod contacts;
mod crypto;
mod history;

use attachments::{build_manifest, read_chunk, CompletedAttachment, Receiver};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chacha20poly1305::aead::rand_core::{OsRng, RngCore};
use connectivity::{split_peer_address, FallbackPlanner, Recovery};
use crypto::{
    accepts_inbound, needs_outbound_handshake, respond as respond_hybrid, HybridInitiator,
    HybridResponse, RatchetMessage, RatchetSession, SessionKey,
};
use futures::StreamExt;
use history::{GroupChatEntry, History};
use libp2p::{
    autonat, dcutr, identify,
    identity::Keypair,
    kad, mdns,
    multiaddr::Protocol,
    noise, ping, relay,
    request_response::{self, ProtocolSupport},
    swarm::{
        behaviour::toggle::Toggle,
        dial_opts::{DialOpts, PeerCondition},
        NetworkBehaviour, SwarmEvent,
    },
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm,
};
use msnnext_protocol::{
    resolve_emoticons, validate_text_message, validate_triggers, AttachmentManifest, ChatEvent,
    Emoticon, EmoticonOffer, Envelope, GroupAttachmentOffer, GroupDefinition, GroupTextMessage,
    Mime, Nudge, NudgeRateLimit, PresenceUpdate, ProtocolResponse, TextMessage, Trigger,
    PROTOCOL_VERSION,
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fs,
    future::pending,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
    time::{sleep_until, Instant},
};

// Emoticons stay tiny and atomic; regular attachments use the resumable chunk protocol.
const MAX_EMOTICON_BYTES: usize = 350_000;
const MAX_EMOTICON_SIDE: usize = 512;
const MAX_REQUEST_RETRIES: u8 = 2;
const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
pub enum ClientCommand {
    SendText { peer: PeerId, text: String },
    SendNudge { peer: PeerId },
    SendFile { peer: PeerId, path: PathBuf },
    CreateEmoticon { path: PathBuf, trigger: String },
    SaveEmoticon { asset_id: String, trigger: String },
    UpdateEmoticon { asset_id: String, trigger: String },
    DeleteEmoticon { asset_id: String },
    ImportContactLink { link: String },
    RenameContact { peer: PeerId, name: String },
    DeleteContact { peer: PeerId },
    ClearConversation { peer: PeerId },
    CreateChatGroup { name: String, members: Vec<PeerId> },
    SendGroupText { group_id: String, text: String },
    SendGroupFile { group_id: String, path: PathBuf },
    ClearGroupConversation { group_id: String },
    DeleteChatGroup { group_id: String },
    ReadAttachment { id: String, mime: String },
    ExportAttachment { id: String, path: PathBuf },
    RequestContactLink,
    UpdateDisplayName { name: String },
    Shutdown,
}

impl ClientCommand {
    pub fn peer(&self) -> Option<PeerId> {
        match self {
            Self::SendText { peer, .. }
            | Self::SendNudge { peer }
            | Self::SendFile { peer, .. }
            | Self::RenameContact { peer, .. }
            | Self::DeleteContact { peer }
            | Self::ClearConversation { peer } => Some(*peer),
            Self::CreateEmoticon { .. }
            | Self::SaveEmoticon { .. }
            | Self::UpdateEmoticon { .. }
            | Self::DeleteEmoticon { .. }
            | Self::CreateChatGroup { .. }
            | Self::SendGroupText { .. }
            | Self::SendGroupFile { .. }
            | Self::ClearGroupConversation { .. }
            | Self::DeleteChatGroup { .. }
            | Self::ReadAttachment { .. }
            | Self::ExportAttachment { .. }
            | Self::ImportContactLink { .. }
            | Self::RequestContactLink
            | Self::UpdateDisplayName { .. }
            | Self::Shutdown => None,
        }
    }
}

pub fn parse_peer_id(value: &str) -> Result<PeerId, String> {
    value
        .parse()
        .map_err(|error| format!("peer id non valido: {error}"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientContact {
    pub peer_id: String,
    pub name: String,
    pub online: bool,
    pub secure: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientMessage {
    pub peer_id: String,
    pub direction: String,
    pub kind: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub emoticons: Vec<ClientEmoticonSpan>,
    pub attachment_id: Option<String>,
    pub attachment_mime: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientEmoticonSpan {
    pub start: u32,
    pub end: u32,
    pub asset_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientEmoticon {
    pub asset_id: String,
    pub name: String,
    pub trigger: String,
    pub mime: String,
    pub data_url: String,
    pub animated: bool,
    pub saved: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientGroupChat {
    pub id: String,
    pub name: String,
    pub owner_peer_id: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientGroupMessage {
    pub group_id: String,
    pub sender_peer_id: String,
    pub direction: String,
    pub kind: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub emoticons: Vec<ClientEmoticonSpan>,
    pub attachment_id: Option<String>,
    pub attachment_mime: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientEvent {
    Started {
        peer_id: String,
        display_name: String,
    },
    ContactUpdated {
        contact: ClientContact,
    },
    ConversationLoaded {
        peer_id: String,
        messages: Vec<ClientMessage>,
    },
    Message {
        message: ClientMessage,
    },
    EmoticonCatalog {
        emoticons: Vec<ClientEmoticon>,
    },
    EmoticonOffered {
        peer_id: String,
        emoticon: ClientEmoticon,
    },
    EmoticonRemoved {
        asset_id: String,
    },
    ContactLink {
        link: String,
    },
    AttachmentReceived {
        peer_id: String,
        id: String,
        filename: String,
        mime: String,
    },
    GroupAttachmentReceived {
        group_id: String,
        id: String,
        filename: String,
        mime: String,
    },
    AttachmentSent {
        peer_id: String,
        filename: String,
    },
    ContactRemoved {
        peer_id: String,
    },
    ConversationCleared {
        peer_id: String,
    },
    GroupChatsUpdated {
        groups: Vec<ClientGroupChat>,
    },
    GroupConversationLoaded {
        group_id: String,
        messages: Vec<ClientGroupMessage>,
    },
    GroupMessage {
        message: ClientGroupMessage,
    },
    GroupConversationCleared {
        group_id: String,
    },
    AttachmentOpened {
        id: String,
        data_url: String,
    },
    AttachmentExported {
        path: String,
    },
    Error {
        message: String,
    },
    Ready,
    Stopped,
}

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
    group_id: Option<[u8; 16]>,
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
    pending_emoticons: &'a mut HashMap<[u8; 32], EmoticonOffer>,
    events: &'a mpsc::UnboundedSender<ClientEvent>,
    nudge_limits: &'a mut HashMap<PeerId, NudgeRateLimit>,
    attachments: &'a mut Receiver,
    history: &'a History,
    notifications: bool,
    peer_names: &'a mut HashMap<PeerId, String>,
    local_peer_id: PeerId,
    incoming_attachments: &'a mut HashMap<(PeerId, [u8; 32]), HashSet<Option<String>>>,
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
        .with_swarm_config(|config| config.with_idle_connection_timeout(CHAT_IDLE_TIMEOUT))
        .build())
}

#[tokio::main]
pub async fn run_cli() -> Result<(), Box<dyn Error>> {
    let args = ClientConfig::parse()?;
    let (_command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    run(args, command_rx, event_tx).await
}

pub async fn run(
    args: ClientConfig,
    mut commands: mpsc::UnboundedReceiver<ClientCommand>,
    events: mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), Box<dyn Error>> {
    let identity = load_or_create_identity(&args.identity)?;
    let public_key = identity.public();
    let identity_bytes = identity.to_protobuf_encoding()?;
    let history_key = blake3::derive_key("msnnext local history v1", &identity_bytes);
    let attachment_key = blake3::derive_key("msnnext attachment vault v1", &identity_bytes);
    let history = History::open(&args.history, history_key)?;
    let persisted_contacts = history.contacts()?;
    let mut ignored_contacts = history
        .ignored_contacts()?
        .into_iter()
        .filter_map(|peer| peer.parse::<PeerId>().ok())
        .collect::<HashSet<_>>();
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
    let mut display_name = args.name.clone();
    let _ = events.send(ClientEvent::Started {
        peer_id: local_peer_id.to_string(),
        display_name: display_name.clone(),
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut peers = HashSet::new();
    let mut dialing = HashSet::new();
    let mut mdns_dialing = HashSet::new();
    let mut reconnect_at = HashMap::<PeerId, Instant>::new();
    let mut reconnect_attempts = HashMap::<PeerId, u8>::new();
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
    let mut shared_emoticons = HashSet::<(PeerId, [u8; 32])>::new();
    let mut pending_emoticons = HashMap::<[u8; 32], EmoticonOffer>::new();
    let mut attachment_receiver = Receiver::new(args.downloads.clone(), attachment_key);
    let mut incoming_attachments = HashMap::<(PeerId, [u8; 32]), HashSet<Option<String>>>::new();
    let mut peer_names = HashMap::<PeerId, String>::new();
    let mut nudge_counter = 0_u64;
    let mut stdin_open = true;
    let mut commands_open = true;
    let mut contact_link_requested = false;

    for contact in persisted_contacts {
        let restored = if contact.link.is_empty() {
            contact
                .peer
                .parse::<PeerId>()
                .map(|peer_id| (contact.name.clone(), peer_id, Vec::new()))
                .map_err(|error| error.to_string())
        } else {
            contacts::import_link(&contact.link).map_err(|error| error.to_string())
        };
        let (name, peer_id, addresses) = match restored {
            Ok(restored) => restored,
            Err(error) => {
                let _ = events.send(ClientEvent::Error {
                    message: format!("contatto salvato non valido: {} ({error})", contact.name),
                });
                continue;
            }
        };
        peer_names.insert(peer_id, name.clone());
        known_contacts.insert(peer_id);
        let _ = events.send(ClientEvent::ContactUpdated {
            contact: ClientContact {
                peer_id: peer_id.to_string(),
                name,
                online: false,
                secure: false,
            },
        });
        let messages = history
            .conversation(&peer_id.to_string(), 100)?
            .into_iter()
            .rev()
            .map(|entry| {
                let attachment = (entry.kind == "file")
                    .then(|| decode_attachment(&entry.body))
                    .flatten();
                ClientMessage {
                    peer_id: entry.peer,
                    direction: entry.direction,
                    kind: entry.kind,
                    body: attachment
                        .as_ref()
                        .map_or(entry.body, |item| item.2.clone()),
                    timestamp_ms: entry.timestamp_ms,
                    emoticons: Vec::new(),
                    attachment_id: attachment.as_ref().map(|item| item.0.clone()),
                    attachment_mime: attachment.map(|item| item.1),
                }
            })
            .collect();
        let _ = events.send(ClientEvent::ConversationLoaded {
            peer_id: peer_id.to_string(),
            messages,
        });
        if !addresses.is_empty() {
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
    }
    let group_chats = history.group_chats()?;
    send_group_chats(&group_chats, &events);
    for group in &group_chats {
        let messages = history
            .conversation(&group_history_key(&group.id), 100)?
            .into_iter()
            .rev()
            .filter_map(|entry| {
                let (kind, sender_peer_id) = entry
                    .kind
                    .strip_prefix("group-text:")
                    .map(|sender| ("text", sender.to_owned()))
                    .or_else(|| {
                        entry
                            .kind
                            .strip_prefix("group-file:")
                            .map(|sender| ("file", sender.to_owned()))
                    })?;
                let attachment = (kind == "file")
                    .then(|| decode_attachment(&entry.body))
                    .flatten();
                Some(ClientGroupMessage {
                    group_id: group.id.clone(),
                    sender_peer_id,
                    direction: entry.direction,
                    kind: kind.into(),
                    body: attachment
                        .as_ref()
                        .map_or(entry.body, |item| item.2.clone()),
                    timestamp_ms: entry.timestamp_ms,
                    emoticons: Vec::new(),
                    attachment_id: attachment.as_ref().map(|item| item.0.clone()),
                    attachment_mime: attachment.map(|item| item.1),
                })
            })
            .collect();
        let _ = events.send(ClientEvent::GroupConversationLoaded {
            group_id: group.id.clone(),
            messages,
        });
    }
    let emoticons = triggers
        .iter()
        .filter_map(|trigger| {
            load_emoticon_offer(&args.emotes, trigger)
                .ok()
                .map(|offer| client_emoticon(&offer, &trigger.text, true))
        })
        .collect();
    let _ = events.send(ClientEvent::EmoticonCatalog { emoticons });
    let _ = events.send(ClientEvent::Ready);

    loop {
        let next_reconnect = reconnect_at
            .iter()
            .min_by_key(|(_, deadline)| **deadline)
            .map(|(peer, deadline)| (*peer, *deadline));
        tokio::select! {
            _ = wait_for_reconnect(next_reconnect.map(|(_, deadline)| deadline)) => {
                let Some((peer, _)) = next_reconnect else {
                    continue;
                };
                reconnect_at.remove(&peer);
                if known_contacts.contains(&peer)
                    && !peers.contains(&peer)
                    && dialing.insert(peer)
                {
                    if let Err(error) = swarm.dial(DialOpts::peer_id(peer).build()) {
                        dialing.remove(&peer);
                        eprintln!("riconnessione fallita per {peer}: {error}");
                        schedule_reconnect(
                            peer,
                            &mut reconnect_at,
                            &mut reconnect_attempts,
                        );
                    }
                }
            }
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
                        &display_name,
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
                        &display_name,
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
                                pending_offers.insert(request_id, PendingOffer { peer: *peer, path: path.clone(), manifest: manifest.clone(), retries: 0, group_id: None });
                            }
                        }
                    }
                    Err(error) => eprintln!("file rifiutato: {error}"),
                },
                Some(_) => eprintln!("comando non valido"),
                None => stdin_open = false,
            },
            command = commands.recv(), if commands_open => match command {
                Some(ClientCommand::Shutdown) => break,
                Some(ClientCommand::RequestContactLink) => {
                    contact_link_requested = true;
                    let addresses = contact_addresses(&swarm);
                    if !addresses.is_empty() {
                        match contacts::link(
                            &display_name,
                            local_peer_id,
                            &public_key,
                            addresses.into_iter(),
                        ) {
                            Ok(link) => {
                                contact_link_requested = false;
                                let _ = events.send(ClientEvent::ContactLink { link });
                            }
                            Err(error) => {
                                let _ = events.send(ClientEvent::Error { message: error.to_string() });
                            }
                        }
                    }
                }
                Some(ClientCommand::UpdateDisplayName { name }) => {
                    let name = name.trim();
                    if name.is_empty() || name.len() > 64 {
                        let _ = events.send(ClientEvent::Error { message: "nome non valido".into() });
                    } else {
                        display_name = name.to_owned();
                        broadcast(
                            &mut swarm,
                            &mut sessions,
                            &peers,
                            local_peer_id,
                            &mut sent_numbers,
                            ChatEvent::Presence(PresenceUpdate { display_name: display_name.clone(), online: true }),
                        );
                    }
                }
                Some(ClientCommand::ImportContactLink { link }) => {
                    match contacts::import_link(&link) {
                        Ok((name, peer_id, addresses)) => {
                            history.allow_contact(&peer_id.to_string())?;
                            ignored_contacts.remove(&peer_id);
                            history.save_contact(
                                &peer_id.to_string(),
                                &name,
                                &link,
                                now_ms(),
                            )?;
                            peer_names.insert(peer_id, name.clone());
                            known_contacts.insert(peer_id);
                            let _ = events.send(ClientEvent::ContactUpdated {
                                contact: ClientContact {
                                    peer_id: peer_id.to_string(),
                                    name,
                                    online: false,
                                    secure: false,
                                },
                            });
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
                        Err(error) => {
                            let _ = events.send(ClientEvent::Error { message: error.to_string() });
                        }
                    }
                }
                Some(ClientCommand::SendText { peer, text }) => {
                    if !peers.contains(&peer) {
                        let _ = events.send(ClientEvent::Error {
                            message: "contatto non collegato".into(),
                        });
                        continue;
                    }
                    let emoticons = resolve_emoticons(&text, &triggers)
                        .map_err(|error| format!("trigger emoticon non valido: {error:?}"))?;
                    for asset_id in emoticons
                        .iter()
                        .map(|span| span.asset_id)
                        .collect::<HashSet<_>>()
                    {
                        if shared_emoticons.insert((peer, asset_id)) {
                            if let Some(trigger) =
                                triggers.iter().find(|trigger| trigger.asset_id == asset_id)
                            {
                                match load_emoticon_offer(&args.emotes, trigger) {
                                    Ok(offer) => {
                                        send_event(
                                            &mut swarm,
                                            &mut sessions,
                                            peer,
                                            local_peer_id,
                                            &mut sent_numbers,
                                            ChatEvent::EmoticonOffer(offer),
                                        );
                                    }
                                    Err(error) => {
                                        shared_emoticons.remove(&(peer, asset_id));
                                        let _ = events.send(ClientEvent::Error {
                                            message: format!(
                                                "emoticon {} non disponibile: {error}",
                                                trigger.text
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                    }
                    let client_emoticons = emoticons
                        .iter()
                        .map(|span| ClientEmoticonSpan {
                            start: span.start,
                            end: span.end,
                            asset_id: hex_asset_id(&span.asset_id),
                        })
                        .collect();
                    let event = ChatEvent::Text(TextMessage {
                        text: text.clone(),
                        emoticons,
                    });
                    if send_event(
                        &mut swarm,
                        &mut sessions,
                        peer,
                        local_peer_id,
                        &mut sent_numbers,
                        event,
                    ).is_some() {
                        let timestamp_ms = now_ms();
                        record(&history, &peer, "out", "text", &text);
                        let _ = events.send(ClientEvent::Message {
                            message: ClientMessage {
                                peer_id: peer.to_string(),
                                direction: "out".into(),
                                kind: "text".into(),
                                body: text,
                                timestamp_ms,
                                emoticons: client_emoticons,
                                attachment_id: None,
                                attachment_mime: None,
                            },
                        });
                    }
                }
                Some(ClientCommand::SendNudge { peer }) => {
                    nudge_counter += 1;
                    let timestamp_ms = now_ms();
                    let mut id = [0; 16];
                    id[..8].copy_from_slice(&timestamp_ms.to_be_bytes());
                    id[8..].copy_from_slice(&nudge_counter.to_be_bytes());
                    match nudge_limits.entry(peer).or_default().try_acquire(timestamp_ms) {
                        Ok(()) if peers.contains(&peer) => {
                            if send_event(
                                &mut swarm,
                                &mut sessions,
                                peer,
                                local_peer_id,
                                &mut sent_numbers,
                                ChatEvent::Nudge(Nudge { id, intensity: 1, timestamp_ms }),
                            ).is_some() {
                                record(&history, &peer, "out", "nudge", "trillo");
                                let _ = events.send(ClientEvent::Message {
                                    message: ClientMessage {
                                        peer_id: peer.to_string(),
                                        direction: "out".into(),
                                        kind: "nudge".into(),
                                        body: "trillo".into(),
                                        timestamp_ms,
                                        emoticons: Vec::new(),
                                        attachment_id: None,
                                        attachment_mime: None,
                                    },
                                });
                            }
                        }
                        Ok(()) => {
                            let _ = events.send(ClientEvent::Error {
                                message: "contatto non collegato".into(),
                            });
                        }
                        Err(wait_ms) => {
                            let _ = events.send(ClientEvent::Error {
                                message: format!(
                                    "trillo limitato: riprova tra {}s",
                                    wait_ms.div_ceil(1_000)
                                ),
                            });
                        }
                    }
                }
                Some(ClientCommand::SendFile { peer, path }) => {
                    if !peers.contains(&peer) {
                        let _ = events.send(ClientEvent::Error {
                            message: "contatto non collegato".into(),
                        });
                        continue;
                    }
                    match build_manifest(&path) {
                        Ok(manifest) => {
                            record(&history, &peer, "out", "file", &manifest.filename);
                            if let Some(request_id) = send_event(
                                &mut swarm,
                                &mut sessions,
                                peer,
                                local_peer_id,
                                &mut sent_numbers,
                                ChatEvent::AttachmentOffer(manifest.clone()),
                            ) {
                                let _ = events.send(ClientEvent::Message {
                                    message: ClientMessage {
                                        peer_id: peer.to_string(),
                                        direction: "out".into(),
                                        kind: "file".into(),
                                        body: manifest.filename.clone(),
                                        timestamp_ms: now_ms(),
                                        emoticons: Vec::new(),
                                        attachment_id: None,
                                        attachment_mime: Some(manifest.mime.clone()),
                                    },
                                });
                                pending_offers.insert(request_id, PendingOffer {
                                    peer,
                                    path,
                                    manifest,
                                    retries: 0,
                                    group_id: None,
                                });
                            }
                        }
                        Err(error) => {
                            let _ = events.send(ClientEvent::Error { message: error.to_string() });
                        }
                    }
                }
                Some(ClientCommand::CreateEmoticon { path, trigger }) => {
                    match create_emoticon(&path, &trigger, &args.emotes, &mut triggers) {
                        Ok(offer) => {
                            let trigger = offer
                                .metadata
                                .suggested_triggers
                                .first()
                                .cloned()
                                .unwrap_or_default();
                            let _ = events.send(ClientEvent::EmoticonCatalog {
                                emoticons: vec![client_emoticon(&offer, &trigger, true)],
                            });
                        }
                        Err(error) => {
                            let _ = events.send(ClientEvent::Error {
                                message: format!("emoticon non creata: {error}"),
                            });
                        }
                    }
                }
                Some(ClientCommand::SaveEmoticon { asset_id, trigger }) => {
                    let parsed = blake3::Hash::from_hex(&asset_id)
                        .map(|hash| *hash.as_bytes())
                        .map_err(|error| format!("id emoticon non valido: {error}"));
                    match parsed.and_then(|asset_id| {
                        pending_emoticons
                            .get(&asset_id)
                            .cloned()
                            .ok_or_else(|| "emoticon ricevuta non disponibile".to_owned())
                            .map(|offer| (asset_id, offer))
                    }) {
                        Ok((asset_id, mut offer)) => {
                            offer.metadata.suggested_triggers = vec![trigger.trim().to_owned()];
                            match save_offer(&args.emotes, &offer, &mut triggers) {
                                Ok(_) => {
                                    pending_emoticons.remove(&asset_id);
                                    let _ = events.send(ClientEvent::EmoticonCatalog {
                                        emoticons: vec![client_emoticon(
                                            &offer,
                                            trigger.trim(),
                                            true,
                                        )],
                                    });
                                }
                                Err(error) => {
                                    let _ = events.send(ClientEvent::Error {
                                        message: format!("emoticon non salvata: {error}"),
                                    });
                                }
                            }
                        }
                        Err(message) => {
                            let _ = events.send(ClientEvent::Error { message });
                        }
                    }
                }
                Some(ClientCommand::UpdateEmoticon { asset_id, trigger }) => {
                    match parse_asset_id(&asset_id).and_then(|asset_id| {
                        let current = triggers.iter().find(|item| item.asset_id == asset_id).cloned()
                            .ok_or_else(|| "emoticon non trovata".to_owned())?;
                        let mut offer = load_emoticon_offer(&args.emotes, &current)
                            .map_err(|error| error.to_string())?;
                        offer.metadata.suggested_triggers = vec![trigger.trim().to_owned()];
                        save_offer(&args.emotes, &offer, &mut triggers)
                            .map_err(|error| error.to_string())?;
                        Ok((offer, trigger.trim().to_owned()))
                    }) {
                        Ok((offer, trigger)) => {
                            let _ = events.send(ClientEvent::EmoticonCatalog {
                                emoticons: vec![client_emoticon(&offer, &trigger, true)],
                            });
                        }
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); }
                    }
                }
                Some(ClientCommand::DeleteEmoticon { asset_id }) => {
                    match parse_asset_id(&asset_id).and_then(|asset_id| {
                        delete_emoticon(&args.emotes, asset_id, &mut triggers)
                            .map_err(|error| error.to_string())
                    }) {
                        Ok(()) => { let _ = events.send(ClientEvent::EmoticonRemoved { asset_id }); }
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); }
                    }
                }
                Some(ClientCommand::RenameContact { peer, name }) => {
                    match history.rename_contact(&peer.to_string(), &name) {
                        Ok(()) => {
                            let name = name.trim().to_owned();
                            peer_names.insert(peer, name.clone());
                            let _ = events.send(ClientEvent::ContactUpdated { contact: ClientContact {
                                peer_id: peer.to_string(), name, online: peers.contains(&peer), secure: sessions.contains_key(&peer),
                            }});
                        }
                        Err(error) => { let _ = events.send(ClientEvent::Error { message: error.to_string() }); }
                    }
                }
                Some(ClientCommand::ClearConversation { peer }) => {
                    match history.clear_conversation(&peer.to_string()) {
                        Ok(()) => { let _ = events.send(ClientEvent::ConversationCleared { peer_id: peer.to_string() }); }
                        Err(error) => { let _ = events.send(ClientEvent::Error { message: error.to_string() }); }
                    }
                }
                Some(ClientCommand::DeleteContact { peer }) => {
                    match history.delete_contact(&peer.to_string()) {
                        Ok(()) => {
                            ignored_contacts.insert(peer);
                            known_contacts.remove(&peer);
                            peer_names.remove(&peer);
                            peers.remove(&peer);
                            sessions.remove(&peer);
                            reconnect_at.remove(&peer);
                            reconnect_attempts.remove(&peer);
                            let _ = swarm.disconnect_peer_id(peer);
                            let _ = events.send(ClientEvent::ContactRemoved { peer_id: peer.to_string() });
                        }
                        Err(error) => { let _ = events.send(ClientEvent::Error { message: error.to_string() }); }
                    }
                }
                Some(ClientCommand::CreateChatGroup { name, members }) => {
                    let name = name.trim();
                    let mut members = members.into_iter().filter(|peer| *peer != local_peer_id).collect::<HashSet<_>>();
                    if name.is_empty() || name.len() > 64 {
                        let _ = events.send(ClientEvent::Error { message: "nome della chat non valido".into() });
                        continue;
                    }
                    if members.len() < 2 || members.len() > 31 || members.iter().any(|peer| !known_contacts.contains(peer)) {
                        let _ = events.send(ClientEvent::Error { message: "scegli da 2 a 31 contatti validi".into() });
                        continue;
                    }
                    let mut group_id = [0_u8; 16];
                    OsRng.fill_bytes(&mut group_id);
                    let mut member_ids = members.drain().map(|peer| peer.to_string()).collect::<Vec<_>>();
                    member_ids.push(local_peer_id.to_string());
                    member_ids.sort();
                    let group = GroupChatEntry {
                        id: hex_group_id(&group_id),
                        name: name.to_owned(),
                        owner_peer: local_peer_id.to_string(),
                        members: member_ids,
                        revision: 1,
                    };
                    history.save_group_chat(&group)?;
                    let definition = group_definition(&group)?;
                    for member in group.members.iter().filter_map(|value| value.parse::<PeerId>().ok()).filter(|peer| *peer != local_peer_id) {
                        send_event(&mut swarm, &mut sessions, member, local_peer_id, &mut sent_numbers, ChatEvent::GroupDefinition(definition.clone()));
                    }
                    send_group_chats(&history.group_chats()?, &events);
                    let _ = events.send(ClientEvent::GroupConversationLoaded { group_id: group.id, messages: Vec::new() });
                }
                Some(ClientCommand::SendGroupText { group_id, text }) => {
                    let parsed_id = match parse_group_id(&group_id) {
                        Ok(id) => id,
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); continue; }
                    };
                    let Some(group) = history.group_chat(&group_id)? else {
                        let _ = events.send(ClientEvent::Error { message: "chat di gruppo non trovata".into() });
                        continue;
                    };
                    if !group.members.contains(&local_peer_id.to_string()) {
                        let _ = events.send(ClientEvent::Error { message: "non fai parte di questa chat".into() });
                        continue;
                    }
                    let emoticons = match resolve_emoticons(&text, &triggers) {
                        Ok(emoticons) => emoticons,
                        Err(error) => { let _ = events.send(ClientEvent::Error { message: format!("trigger emoticon non valido: {error:?}") }); continue; }
                    };
                    let timestamp_ms = now_ms();
                    let group_event = ChatEvent::GroupText(GroupTextMessage {
                        group_id: parsed_id,
                        message: TextMessage { text: text.clone(), emoticons: emoticons.clone() },
                        timestamp_ms,
                    });
                    let recipients = group.members.iter()
                        .filter_map(|value| value.parse::<PeerId>().ok())
                        .filter(|peer| *peer != local_peer_id && sessions.contains_key(peer))
                        .collect::<Vec<_>>();
                    if recipients.is_empty() {
                        let _ = events.send(ClientEvent::Error { message: "nessun partecipante è online con una sessione protetta".into() });
                        continue;
                    }
                    for recipient in &recipients {
                        for asset_id in emoticons.iter().map(|span| span.asset_id).collect::<HashSet<_>>() {
                            if shared_emoticons.insert((*recipient, asset_id)) {
                                if let Some(trigger) = triggers.iter().find(|trigger| trigger.asset_id == asset_id) {
                                    if let Ok(offer) = load_emoticon_offer(&args.emotes, trigger) {
                                        send_event(&mut swarm, &mut sessions, *recipient, local_peer_id, &mut sent_numbers, ChatEvent::EmoticonOffer(offer));
                                    }
                                }
                            }
                        }
                        send_event(&mut swarm, &mut sessions, *recipient, local_peer_id, &mut sent_numbers, group_event.clone());
                    }
                    history.record(&group_history_key(&group_id), "out", &format!("group-text:{local_peer_id}"), &text, timestamp_ms)?;
                    let _ = events.send(ClientEvent::GroupMessage { message: ClientGroupMessage {
                        group_id,
                        sender_peer_id: local_peer_id.to_string(),
                        direction: "out".into(),
                        kind: "text".into(),
                        body: text,
                        timestamp_ms,
                        emoticons: emoticons.iter().map(|span| ClientEmoticonSpan { start: span.start, end: span.end, asset_id: hex_asset_id(&span.asset_id) }).collect(),
                        attachment_id: None,
                        attachment_mime: None,
                    }});
                }
                Some(ClientCommand::SendGroupFile { group_id, path }) => {
                    let parsed_id = match parse_group_id(&group_id) {
                        Ok(id) => id,
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); continue; }
                    };
                    let Some(group) = history.group_chat(&group_id)? else {
                        let _ = events.send(ClientEvent::Error { message: "chat di gruppo non trovata".into() });
                        continue;
                    };
                    if !group.members.contains(&local_peer_id.to_string()) {
                        let _ = events.send(ClientEvent::Error { message: "non fai parte di questa chat".into() });
                        continue;
                    }
                    let recipients = group.members.iter()
                        .filter_map(|value| value.parse::<PeerId>().ok())
                        .filter(|peer| *peer != local_peer_id && sessions.contains_key(peer))
                        .collect::<Vec<_>>();
                    if recipients.is_empty() {
                        let _ = events.send(ClientEvent::Error { message: "nessun partecipante è online con una sessione protetta".into() });
                        continue;
                    }
                    match build_manifest(&path) {
                        Ok(manifest) => {
                            let timestamp_ms = now_ms();
                            history.record(
                                &group_history_key(&group_id),
                                "out",
                                &format!("group-file:{local_peer_id}"),
                                &encode_attachment(&manifest),
                                timestamp_ms,
                            )?;
                            for recipient in recipients {
                                if let Some(request_id) = send_event(
                                    &mut swarm,
                                    &mut sessions,
                                    recipient,
                                    local_peer_id,
                                    &mut sent_numbers,
                                    ChatEvent::GroupAttachmentOffer(GroupAttachmentOffer {
                                        group_id: parsed_id,
                                        manifest: manifest.clone(),
                                    }),
                                ) {
                                    pending_offers.insert(request_id, PendingOffer {
                                        peer: recipient,
                                        path: path.clone(),
                                        manifest: manifest.clone(),
                                        retries: 0,
                                        group_id: Some(parsed_id),
                                    });
                                }
                            }
                            let _ = events.send(ClientEvent::GroupMessage { message: ClientGroupMessage {
                                group_id,
                                sender_peer_id: local_peer_id.to_string(),
                                direction: "out".into(),
                                kind: "file".into(),
                                body: manifest.filename.clone(),
                                timestamp_ms,
                                emoticons: Vec::new(),
                                attachment_id: None,
                                attachment_mime: Some(manifest.mime),
                            }});
                        }
                        Err(error) => { let _ = events.send(ClientEvent::Error { message: error.to_string() }); }
                    }
                }
                Some(ClientCommand::ClearGroupConversation { group_id }) => {
                    match parse_group_id(&group_id).and_then(|_| history.clear_conversation(&group_history_key(&group_id)).map_err(|error| error.to_string())) {
                        Ok(()) => { let _ = events.send(ClientEvent::GroupConversationCleared { group_id }); }
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); }
                    }
                }
                Some(ClientCommand::DeleteChatGroup { group_id }) => {
                    match parse_group_id(&group_id).and_then(|_| history.delete_group_chat(&group_id).map_err(|error| error.to_string())) {
                        Ok(()) => send_group_chats(&history.group_chats()?, &events),
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); }
                    }
                }
                Some(ClientCommand::ReadAttachment { id, mime }) => {
                    match parse_asset_id(&id).and_then(|id_bytes| {
                        attachment_receiver.read(&id_bytes)
                            .map(|bytes| format!("data:{mime};base64,{}", BASE64.encode(bytes)))
                            .map_err(|error| error.to_string())
                    }) {
                        Ok(data_url) => { let _ = events.send(ClientEvent::AttachmentOpened { id, data_url }); }
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); }
                    }
                }
                Some(ClientCommand::ExportAttachment { id, path }) => {
                    match parse_asset_id(&id).and_then(|id_bytes| {
                        attachment_receiver
                            .export(&id_bytes, &path)
                            .map_err(|error| error.to_string())
                    }) {
                        Ok(()) => { let _ = events.send(ClientEvent::AttachmentExported { path: path.to_string_lossy().into_owned() }); }
                        Err(message) => { let _ = events.send(ClientEvent::Error { message }); }
                    }
                }
                None => commands_open = false,
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    let address = if split_peer_address(&address).is_ok() {
                        address
                    } else {
                        address.with(Protocol::P2p(local_peer_id))
                    };
                    println!("ascolto: {address}");
                    if contact_link_requested {
                        let addresses = contact_addresses(&swarm);
                        if !addresses.is_empty() {
                            match contacts::link(
                                &display_name,
                                local_peer_id,
                                &public_key,
                                addresses.into_iter(),
                            ) {
                                Ok(link) => {
                                    contact_link_requested = false;
                                    let _ = events.send(ClientEvent::ContactLink { link });
                                }
                                Err(error) => {
                                    let _ = events.send(ClientEvent::Error {
                                        message: error.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    dialing.remove(&peer_id);
                    mdns_dialing.remove(&peer_id);
                    bootstrap_fallbacks.remove(&peer_id);
                    reconnect_at.remove(&peer_id);
                    reconnect_attempts.remove(&peer_id);
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
                        let _ = events.send(ClientEvent::ContactUpdated {
                            contact: ClientContact {
                                peer_id: peer_id.to_string(),
                                name: peer_names
                                    .get(&peer_id)
                                    .cloned()
                                    .unwrap_or_else(|| "Nuovo contatto".into()),
                                online: true,
                                secure: false,
                            },
                        });
                        send_event(&mut swarm, &mut sessions, peer_id, local_peer_id, &mut sent_numbers, ChatEvent::Presence(PresenceUpdate { display_name: display_name.clone(), online: true }));
                    }
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established, cause, .. } => {
                    if num_established == 0 {
                        let was_application_peer = peers.remove(&peer_id);
                        sessions.remove(&peer_id);
                        shared_emoticons.retain(|(peer, _)| *peer != peer_id);
                        pending_handshakes
                            .retain(|_, (pending_peer, _)| *pending_peer != peer_id);
                        pending_inbound_handshakes
                            .retain(|_, (pending_peer, _)| *pending_peer != peer_id);
                        nudge_limits.remove(&peer_id);
                        incoming_nudge_limits.remove(&peer_id);
                        if was_application_peer {
                            println!("offline: {} ({cause:?})", peer_names.get(&peer_id).map_or_else(|| peer_id.to_string(), Clone::clone));
                            let _ = events.send(ClientEvent::ContactUpdated {
                                contact: ClientContact {
                                    peer_id: peer_id.to_string(),
                                    name: peer_names
                                        .get(&peer_id)
                                        .cloned()
                                        .unwrap_or_else(|| "Contatto".into()),
                                    online: false,
                                    secure: false,
                                },
                            });
                        }
                        if should_reconnect_closed_peer(
                            known_contacts.contains(&peer_id),
                            num_established,
                        ) {
                            dialing.remove(&peer_id);
                            schedule_reconnect(
                                peer_id,
                                &mut reconnect_at,
                                &mut reconnect_attempts,
                            );
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Chat(request_response::Event::Message { peer, message, .. })) => match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let validation = if ignored_contacts.contains(&peer) {
                            Err("contatto rimosso")
                        } else if matches!(&request.event, ChatEvent::Presence(_)) {
                            validate_envelope(peer, local_peer_id, &request)
                        } else {
                            Err("usa il protocollo chat cifrato")
                        };
                        let (response, valid) = match validation {
                            Ok(()) => (
                                receive_event(peer, &request.event, &mut Incoming {
                                    pending_emoticons: &mut pending_emoticons,
                                    events: &events,
                                    nudge_limits: &mut incoming_nudge_limits,
                                    attachments: &mut attachment_receiver,
                                    history: &history,
                                    notifications: args.notifications,
                                    peer_names: &mut peer_names,
                                    local_peer_id,
                                    incoming_attachments: &mut incoming_attachments,
                                }),
                                true,
                            ),
                            Err(error) => (ProtocolResponse::Rejected(error.into()), false),
                        };
                        swarm.behaviour_mut().chat.send_response(channel, response).ok();
                        if valid {
                            if let ChatEvent::Presence(presence) = &request.event {
                                history.ensure_contact(
                                    &peer.to_string(),
                                    &presence.display_name,
                                    now_ms(),
                                )?;
                            }
                            if let Some(event) =
                                client_event_from_chat(peer, &request.event, "in", false, peer_names.get(&peer).map(String::as_str))
                            {
                                let _ = events.send(event);
                            }
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
                                        display_name: display_name.clone(),
                                        online: true,
                                    }),
                                );
                            }
                            if event_authorizes_handshake(&request.event) {
                                maybe_start_hybrid_handshake(
                                    &mut swarm,
                                    &mut pending_handshakes,
                                    &sessions,
                                    local_peer_id,
                                    peer,
                                );
                            }
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
                                Ok(()) => {
                                    let client_event = client_event_from_chat(
                                        peer, &envelope.event, "in", true,
                                        peer_names.get(&peer).map(String::as_str),
                                    );
                                    if let ChatEvent::Presence(presence) = &envelope.event {
                                        history.ensure_contact(
                                            &peer.to_string(),
                                            &presence.display_name,
                                            now_ms(),
                                        )?;
                                    }
                                    let response = receive_event(peer, &envelope.event, &mut Incoming {
                                        pending_emoticons: &mut pending_emoticons,
                                        events: &events,
                                        nudge_limits: &mut incoming_nudge_limits,
                                        attachments: &mut attachment_receiver,
                                        history: &history,
                                        notifications: args.notifications,
                                        peer_names: &mut peer_names,
                                        local_peer_id,
                                        incoming_attachments: &mut incoming_attachments,
                                    });
                                    if let Some(event) = client_event {
                                        let _ = events.send(event);
                                    }
                                    response
                                }
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
                                        &events,
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
                                ProtocolResponse::Rejected(error) => {
                                    let _ = events.send(ClientEvent::Error { message: format!("file rifiutato: {error}") });
                                }
                                ProtocolResponse::Ack => {
                                    let _ = events.send(ClientEvent::AttachmentSent {
                                        peer_id: pending.peer.to_string(),
                                        filename: pending.manifest.filename,
                                    });
                                }
                            }
                        } else if let Some(transfer) = pending_transfers.remove(&request_id) {
                            match response {
                                ProtocolResponse::Ack => send_next_transfer_chunk(
                                    &mut swarm,
                                    &mut sessions,
                                    &mut sent_numbers,
                                    &mut pending_transfers,
                                    local_peer_id,
                                    &events,
                                    transfer,
                                ),
                                ProtocolResponse::Rejected(error) => {
                                    let _ = events.send(ClientEvent::Error { message: format!("trasferimento rifiutato: {error}") });
                                }
                                ProtocolResponse::MissingChunks(_) => {
                                    let _ = events.send(ClientEvent::Error { message: "risposta non valida durante il trasferimento".into() });
                                }
                            }
                        }
                    }
                },
                SwarmEvent::Behaviour(BehaviourEvent::SecureChat(request_response::Event::OutboundFailure { peer, request_id, error, .. })) => {
                    if let Some(mut pending) = pending_offers.remove(&request_id) {
                        if let Some(retries) = next_request_retry(pending.retries) {
                            pending.retries = retries;
                            let event = pending.group_id.map_or_else(
                                || ChatEvent::AttachmentOffer(pending.manifest.clone()),
                                |group_id| ChatEvent::GroupAttachmentOffer(GroupAttachmentOffer {
                                    group_id,
                                    manifest: pending.manifest.clone(),
                                }),
                            );
                            if let Some(next_id) = send_event(
                                &mut swarm,
                                &mut sessions,
                                pending.peer,
                                local_peer_id,
                                &mut sent_numbers,
                                event,
                            ) {
                                pending_offers.insert(next_id, pending);
                            }
                        } else {
                            eprintln!("offerta file fallita definitivamente per {peer}: {error}");
                            let _ = events.send(ClientEvent::Error { message: format!("invio file fallito: {error}") });
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
                                &events,
                                transfer,
                            );
                        } else {
                            eprintln!("trasferimento file fallito definitivamente per {peer}: {error}");
                            let _ = events.send(ClientEvent::Error { message: format!("trasferimento file fallito: {error}") });
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
                            swarm.is_connected(&peer)
                                && !infrastructure_peers.contains(&peer)
                                && !ignored_contacts.contains(&peer),
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
                                    let _ = events.send(ClientEvent::ContactUpdated {
                                        contact: ClientContact {
                                            peer_id: peer.to_string(),
                                            name: peer_names
                                                .get(&peer)
                                                .cloned()
                                                .unwrap_or_else(|| "Contatto".into()),
                                            online: true,
                                            secure: true,
                                        },
                                    });
                                    send_group_definitions_for_peer(
                                        &history,
                                        &mut swarm,
                                        &mut sessions,
                                        &mut sent_numbers,
                                        local_peer_id,
                                        peer,
                                    );
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
                            let _ = events.send(ClientEvent::ContactUpdated {
                                contact: ClientContact {
                                    peer_id: peer.to_string(),
                                    name: peer_names
                                        .get(&peer)
                                        .cloned()
                                        .unwrap_or_else(|| "Contatto".into()),
                                    online: true,
                                    secure: true,
                                },
                            });
                            send_group_definitions_for_peer(
                                &history,
                                &mut swarm,
                                &mut sessions,
                                &mut sent_numbers,
                                local_peer_id,
                                peer,
                            );
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
                        let base = split_peer_address(&address)
                            .map(|(_, base)| base)
                            .unwrap_or(address);
                        swarm.behaviour_mut().kad.add_address(&peer_id, base.clone());
                        if !args.relay_server
                            && peer_id != local_peer_id
                            && !infrastructure_peers.contains(&peer_id)
                            && !ignored_contacts.contains(&peer_id)
                            && !peers.contains(&peer_id)
                            && mdns_dialing.insert(peer_id)
                        {
                            known_contacts.insert(peer_id);
                            dialing.insert(peer_id);
                            println!("peer LAN trovato: {peer_id}");
                            let address = base.with(Protocol::P2p(peer_id));
                            let options = DialOpts::peer_id(peer_id)
                                .condition(PeerCondition::Always)
                                .addresses(vec![address])
                                .build();
                            if let Err(error) = swarm.dial(options) {
                                eprintln!("dial mDNS fallito: {error}");
                            }
                        }
                    }
                }
                SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(_))) => {}
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    if let Some(peer_id) = peer_id {
                        mdns_dialing.remove(&peer_id);
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
                                schedule_reconnect(
                                    peer_id,
                                    &mut reconnect_at,
                                    &mut reconnect_attempts,
                                );
                            }
                        }
                    } else { eprintln!("connessione fallita: {error}"); }
                }
                _ => {}
            }
        }
    }
    let _ = events.send(ClientEvent::Stopped);
    Ok(())
}

fn client_event_from_chat(
    peer: PeerId,
    event: &ChatEvent,
    direction: &str,
    secure: bool,
    local_name: Option<&str>,
) -> Option<ClientEvent> {
    let peer_id = peer.to_string();
    match event {
        ChatEvent::Presence(presence) => Some(ClientEvent::ContactUpdated {
            contact: ClientContact {
                peer_id,
                name: local_name.unwrap_or(&presence.display_name).to_owned(),
                online: presence.online,
                secure,
            },
        }),
        ChatEvent::Text(message) => Some(ClientEvent::Message {
            message: ClientMessage {
                peer_id,
                direction: direction.into(),
                kind: "text".into(),
                body: message.text.clone(),
                timestamp_ms: now_ms(),
                emoticons: message
                    .emoticons
                    .iter()
                    .map(|span| ClientEmoticonSpan {
                        start: span.start,
                        end: span.end,
                        asset_id: hex_asset_id(&span.asset_id),
                    })
                    .collect(),
                attachment_id: None,
                attachment_mime: None,
            },
        }),
        ChatEvent::Nudge(nudge) => Some(ClientEvent::Message {
            message: ClientMessage {
                peer_id,
                direction: direction.into(),
                kind: "nudge".into(),
                body: "trillo".into(),
                timestamp_ms: nudge.timestamp_ms,
                emoticons: Vec::new(),
                attachment_id: None,
                attachment_mime: None,
            },
        }),
        ChatEvent::AttachmentOffer(manifest) => Some(ClientEvent::Message {
            message: ClientMessage {
                peer_id,
                direction: direction.into(),
                kind: "file".into(),
                body: manifest.filename.clone(),
                timestamp_ms: now_ms(),
                emoticons: Vec::new(),
                attachment_id: Some(hex_asset_id(&manifest.attachment_id)),
                attachment_mime: Some(manifest.mime.clone()),
            },
        }),
        ChatEvent::EmoticonOffer(_) => None,
        ChatEvent::AttachmentChunk(_) => None,
        ChatEvent::GroupDefinition(_)
        | ChatEvent::GroupText(_)
        | ChatEvent::GroupAttachmentOffer(_) => None,
    }
}

fn should_classify_application_peer(
    valid_message: bool,
    infrastructure: bool,
    already_connected: bool,
) -> bool {
    valid_message && !infrastructure && !already_connected
}

fn should_reconnect_closed_peer(known_contact: bool, num_established: u32) -> bool {
    known_contact && num_established == 0
}

fn reconnect_delay(attempt: u8) -> Duration {
    Duration::from_secs((1_u64 << attempt.min(5)).min(30))
}

fn schedule_reconnect(
    peer: PeerId,
    reconnect_at: &mut HashMap<PeerId, Instant>,
    reconnect_attempts: &mut HashMap<PeerId, u8>,
) {
    let attempt = reconnect_attempts.entry(peer).or_default();
    reconnect_at.insert(peer, Instant::now() + reconnect_delay(*attempt));
    *attempt = attempt.saturating_add(1);
}

async fn wait_for_reconnect(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

fn event_authorizes_handshake(event: &ChatEvent) -> bool {
    matches!(
        event,
        ChatEvent::Presence(PresenceUpdate { online: true, .. })
    )
}

fn handshake_peer_authorized(
    known_contact: bool,
    active_peer: bool,
    trusted_connection: bool,
) -> bool {
    known_contact || active_peer || trusted_connection
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
    events: &mpsc::UnboundedSender<ClientEvent>,
    mut transfer: PendingTransfer,
) {
    let Some(index) = transfer.remaining.pop_front() else {
        println!("invio completato: {}", transfer.manifest.filename);
        let _ = events.send(ClientEvent::AttachmentSent {
            peer_id: transfer.peer.to_string(),
            filename: transfer.manifest.filename,
        });
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
        events,
        transfer,
    );
}

fn send_current_transfer_chunk(
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_transfers: &mut HashMap<request_response::OutboundRequestId, PendingTransfer>,
    local_peer_id: PeerId,
    events: &mpsc::UnboundedSender<ClientEvent>,
    transfer: PendingTransfer,
) {
    let chunk = match read_chunk(&transfer.path, &transfer.manifest, transfer.current) {
        Ok(chunk) => chunk,
        Err(error) => {
            eprintln!("invio interrotto: {error}");
            let _ = events.send(ClientEvent::Error {
                message: format!("invio file interrotto: {error}"),
            });
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
        ChatEvent::GroupDefinition(group) => validate_group_definition(group),
        ChatEvent::GroupText(message) => {
            validate_text_message(&message.message).map_err(|_| "messaggio di gruppo non valido")
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
        ChatEvent::GroupDefinition(definition) => {
            if definition.owner_peer != peer.to_string()
                || !definition
                    .members
                    .contains(&context.local_peer_id.to_string())
            {
                return ProtocolResponse::Rejected("invito al gruppo non autorizzato".into());
            }
            let id = hex_group_id(&definition.group_id);
            let existing = match context.history.group_chat(&id) {
                Ok(existing) => existing,
                Err(error) => return ProtocolResponse::Rejected(error.to_string()),
            };
            if existing.as_ref().is_some_and(|group| {
                group.owner_peer != definition.owner_peer || group.revision > definition.revision
            }) {
                return ProtocolResponse::Rejected("definizione del gruppo non valida".into());
            }
            if existing
                .as_ref()
                .is_some_and(|group| group.revision == definition.revision)
            {
                return ProtocolResponse::Ack;
            }
            let group = GroupChatEntry {
                id: id.clone(),
                name: definition.name.trim().to_owned(),
                owner_peer: definition.owner_peer.clone(),
                members: definition.members.clone(),
                revision: definition.revision,
            };
            if let Err(error) = context.history.save_group_chat(&group) {
                return ProtocolResponse::Rejected(error.to_string());
            }
            if let Ok(groups) = context.history.group_chats() {
                send_group_chats(&groups, context.events);
            }
            if existing.is_none() {
                let _ = context.events.send(ClientEvent::GroupConversationLoaded {
                    group_id: id,
                    messages: Vec::new(),
                });
            }
            ProtocolResponse::Ack
        }
        ChatEvent::GroupText(group_message) => {
            let id = hex_group_id(&group_message.group_id);
            let group = match context.history.group_chat(&id) {
                Ok(Some(group)) => group,
                Ok(None) => return ProtocolResponse::Rejected("chat di gruppo sconosciuta".into()),
                Err(error) => return ProtocolResponse::Rejected(error.to_string()),
            };
            if !group.members.contains(&peer.to_string())
                || !group.members.contains(&context.local_peer_id.to_string())
            {
                return ProtocolResponse::Rejected("mittente non appartenente al gruppo".into());
            }
            if let Err(error) = context.history.record(
                &group_history_key(&id),
                "in",
                &format!("group-text:{peer}"),
                &group_message.message.text,
                group_message.timestamp_ms,
            ) {
                return ProtocolResponse::Rejected(error.to_string());
            }
            let _ = context.events.send(ClientEvent::GroupMessage {
                message: ClientGroupMessage {
                    group_id: id,
                    sender_peer_id: peer.to_string(),
                    direction: "in".into(),
                    kind: "text".into(),
                    body: group_message.message.text.clone(),
                    timestamp_ms: group_message.timestamp_ms,
                    emoticons: group_message
                        .message
                        .emoticons
                        .iter()
                        .map(|span| ClientEmoticonSpan {
                            start: span.start,
                            end: span.end,
                            asset_id: hex_asset_id(&span.asset_id),
                        })
                        .collect(),
                    attachment_id: None,
                    attachment_mime: None,
                },
            });
            notify(
                context.notifications,
                &group.name,
                &group_message.message.text,
            );
            ProtocolResponse::Ack
        }
        ChatEvent::GroupAttachmentOffer(offer) => {
            let id = hex_group_id(&offer.group_id);
            let group = match context.history.group_chat(&id) {
                Ok(Some(group)) => group,
                Ok(None) => return ProtocolResponse::Rejected("chat di gruppo sconosciuta".into()),
                Err(error) => return ProtocolResponse::Rejected(error.to_string()),
            };
            if !group.members.contains(&peer.to_string())
                || !group.members.contains(&context.local_peer_id.to_string())
            {
                return ProtocolResponse::Rejected("mittente non appartenente al gruppo".into());
            }
            match context.attachments.accept_offer(offer.manifest.clone()) {
                Ok((missing, completed)) => {
                    if let Err(error) = context.history.record(
                        &group_history_key(&id),
                        "in",
                        &format!("group-file:{peer}"),
                        &encode_attachment(&offer.manifest),
                        now_ms(),
                    ) {
                        return ProtocolResponse::Rejected(error.to_string());
                    }
                    let _ = context.events.send(ClientEvent::GroupMessage {
                        message: ClientGroupMessage {
                            group_id: id.clone(),
                            sender_peer_id: peer.to_string(),
                            direction: "in".into(),
                            kind: "file".into(),
                            body: offer.manifest.filename.clone(),
                            timestamp_ms: now_ms(),
                            emoticons: Vec::new(),
                            attachment_id: Some(hex_asset_id(&offer.manifest.attachment_id)),
                            attachment_mime: Some(offer.manifest.mime.clone()),
                        },
                    });
                    if let Some(completed) = completed {
                        emit_attachment_received(context.events, peer, Some(id), &completed);
                    } else {
                        context
                            .incoming_attachments
                            .entry((peer, offer.manifest.attachment_id))
                            .or_default()
                            .insert(Some(id));
                    }
                    ProtocolResponse::MissingChunks(missing)
                }
                Err(error) => ProtocolResponse::Rejected(error.to_string()),
            }
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
        ChatEvent::EmoticonOffer(offer) => match validate_offer(offer) {
            Ok(()) => {
                let trigger = offer
                    .metadata
                    .suggested_triggers
                    .first()
                    .cloned()
                    .unwrap_or_else(|| {
                        format!(":e{}:", &hex_asset_id(&offer.metadata.asset_id)[..6])
                    });
                context
                    .pending_emoticons
                    .insert(offer.metadata.asset_id, offer.clone());
                let _ = context.events.send(ClientEvent::EmoticonOffered {
                    peer_id: peer.to_string(),
                    emoticon: client_emoticon(offer, &trigger, false),
                });
                println!("{peer}: emoticon ricevuta, in attesa di salvataggio");
                record(context.history, &peer, "in", "emote", &offer.metadata.name);
                ProtocolResponse::Ack
            }
            Err(error) => ProtocolResponse::Rejected(error.to_string()),
        },
        ChatEvent::AttachmentOffer(manifest) => {
            match context.attachments.accept_offer(manifest.clone()) {
                Ok((missing, completed)) => {
                    record(
                        context.history,
                        &peer,
                        "in",
                        "file",
                        &encode_attachment(manifest),
                    );
                    if let Some(completed) = completed {
                        println!("{peer}: file già presente nell'archivio cifrato");
                        notify(context.notifications, "File ricevuto", &manifest.filename);
                        emit_attachment_received(context.events, peer, None, &completed);
                    } else {
                        context
                            .incoming_attachments
                            .entry((peer, manifest.attachment_id))
                            .or_default()
                            .insert(None);
                    }
                    ProtocolResponse::MissingChunks(missing)
                }
                Err(error) => ProtocolResponse::Rejected(error.to_string()),
            }
        }
        ChatEvent::AttachmentChunk(chunk) => match context.attachments.accept_chunk(chunk) {
            Ok(Some(completed)) => {
                println!("{peer}: file ricevuto nell'archivio cifrato");
                notify(context.notifications, "File ricevuto", &completed.filename);
                let destinations = context
                    .incoming_attachments
                    .remove(&(peer, completed.id))
                    .unwrap_or_else(|| HashSet::from([None]));
                for group_id in destinations {
                    emit_attachment_received(context.events, peer, group_id, &completed);
                }
                ProtocolResponse::Ack
            }
            Ok(None) => ProtocolResponse::Ack,
            Err(error) => ProtocolResponse::Rejected(error.to_string()),
        },
        ChatEvent::Presence(presence) => {
            context
                .peer_names
                .entry(peer)
                .or_insert_with(|| presence.display_name.clone());
            println!(
                "{} è {}",
                presence.display_name,
                if presence.online { "online" } else { "offline" }
            );
            ProtocolResponse::Ack
        }
    }
}

fn emit_attachment_received(
    events: &mpsc::UnboundedSender<ClientEvent>,
    peer: PeerId,
    group_id: Option<String>,
    completed: &CompletedAttachment,
) {
    let id = hex_asset_id(&completed.id);
    let event = match group_id {
        Some(group_id) => ClientEvent::GroupAttachmentReceived {
            group_id,
            id,
            filename: completed.filename.clone(),
            mime: completed.mime.clone(),
        },
        None => ClientEvent::AttachmentReceived {
            peer_id: peer.to_string(),
            id,
            filename: completed.filename.clone(),
            mime: completed.mime.clone(),
        },
    };
    let _ = events.send(event);
}

fn record(history: &History, peer: &PeerId, direction: &str, kind: &str, body: &str) {
    if let Err(error) = history.record(&peer.to_string(), direction, kind, body, now_ms()) {
        eprintln!("cronologia non aggiornata: {error}");
    }
}

fn encode_attachment(manifest: &AttachmentManifest) -> String {
    format!(
        "{}\t{}\t{}",
        hex_asset_id(&manifest.attachment_id),
        manifest.mime,
        manifest.filename
    )
}

fn decode_attachment(value: &str) -> Option<(String, String, String)> {
    let mut parts = value.splitn(3, '\t');
    let id = parts.next()?.to_owned();
    let mime = parts.next()?.to_owned();
    let filename = parts.next()?.to_owned();
    parse_asset_id(&id).ok()?;
    Some((id, mime, filename))
}

fn validate_group_definition(group: &GroupDefinition) -> Result<(), &'static str> {
    if group.group_id == [0; 16]
        || group.name.trim().is_empty()
        || group.name.len() > 64
        || group.revision == 0
        || group.members.len() < 3
        || group.members.len() > 32
        || !group.members.contains(&group.owner_peer)
    {
        return Err("definizione del gruppo non valida");
    }
    let unique = group.members.iter().collect::<HashSet<_>>();
    if unique.len() != group.members.len()
        || group
            .members
            .iter()
            .any(|member| member.parse::<PeerId>().is_err())
    {
        return Err("partecipanti del gruppo non validi");
    }
    Ok(())
}

fn hex_group_id(id: &[u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_group_id(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("id chat di gruppo non valido".into());
    }
    Ok(u128::from_str_radix(value, 16)
        .map_err(|_| "id chat di gruppo non valido")?
        .to_be_bytes())
}

fn group_history_key(id: &str) -> String {
    format!("group:{id}")
}

fn group_definition(group: &GroupChatEntry) -> Result<GroupDefinition, String> {
    let definition = GroupDefinition {
        group_id: parse_group_id(&group.id)?,
        name: group.name.clone(),
        owner_peer: group.owner_peer.clone(),
        members: group.members.clone(),
        revision: group.revision,
    };
    validate_group_definition(&definition).map_err(str::to_owned)?;
    Ok(definition)
}

fn send_group_chats(groups: &[GroupChatEntry], events: &mpsc::UnboundedSender<ClientEvent>) {
    let groups = groups
        .iter()
        .map(|group| ClientGroupChat {
            id: group.id.clone(),
            name: group.name.clone(),
            owner_peer_id: group.owner_peer.clone(),
            members: group.members.clone(),
        })
        .collect();
    let _ = events.send(ClientEvent::GroupChatsUpdated { groups });
}

fn send_group_definitions_for_peer(
    history: &History,
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    sent_numbers: &mut HashMap<PeerId, u64>,
    local_peer_id: PeerId,
    peer: PeerId,
) {
    let Ok(groups) = history.group_chats() else {
        return;
    };
    for group in groups.iter().filter(|group| {
        group.owner_peer == local_peer_id.to_string() && group.members.contains(&peer.to_string())
    }) {
        if let Ok(definition) = group_definition(group) {
            send_event(
                swarm,
                sessions,
                peer,
                local_peer_id,
                sent_numbers,
                ChatEvent::GroupDefinition(definition),
            );
        }
    }
}

fn notify(enabled: bool, summary: &str, body: &str) {
    if enabled {
        let summary = summary.to_owned();
        let body = body.to_owned();
        thread::spawn(move || {
            notify_rust::Notification::new()
                .summary(&summary)
                .body(&body)
                .appname("msnnext")
                .show()
                .ok();
        });
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
    create_emoticon(Path::new(path), trigger, store, triggers)
}

fn create_emoticon(
    path: &Path,
    trigger: &str,
    store: &Path,
    triggers: &mut Vec<Trigger>,
) -> Result<EmoticonOffer, Box<dyn Error>> {
    let trigger = trigger.trim();
    if trigger.is_empty() {
        return Err("manca il trigger".into());
    }
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
            name: path
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
    let hash = blake3::Hash::from_bytes(offer.metadata.asset_id)
        .to_hex()
        .to_string();
    let trigger = offer
        .metadata
        .suggested_triggers
        .first()
        .map(|text| Trigger {
            text: text.trim().to_owned(),
            asset_id: offer.metadata.asset_id,
            case_sensitive: true,
        });
    if let Some(trigger) = &trigger {
        let mut candidates = triggers
            .iter()
            .filter(|item| item.asset_id != trigger.asset_id)
            .cloned()
            .collect::<Vec<_>>();
        candidates.push(trigger.clone());
        validate_triggers(&candidates).map_err(|error| format!("trigger non valido: {error:?}"))?;
    }

    fs::create_dir_all(store)?;
    let asset_path = store.join(format!("{hash}.{}", extension(offer.metadata.mime)));
    if !asset_path.exists() {
        fs::write(&asset_path, &offer.bytes)?;
    }

    fs::write(store.join(format!("{hash}.name")), &offer.metadata.name)?;
    if let Some(trigger) = trigger {
        fs::write(store.join(format!("{hash}.trigger")), &trigger.text)?;
        triggers.retain(|item| item.asset_id != trigger.asset_id);
        triggers.push(trigger);
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

fn mime_name(mime: Mime) -> &'static str {
    match mime {
        Mime::Png => "image/png",
        Mime::Jpeg => "image/jpeg",
        Mime::Gif => "image/gif",
        Mime::Webp => "image/webp",
    }
}

fn hex_asset_id(asset_id: &[u8; 32]) -> String {
    blake3::Hash::from_bytes(*asset_id).to_hex().to_string()
}

fn parse_asset_id(value: &str) -> Result<[u8; 32], String> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|error| format!("id emoticon non valido: {error}"))
}

fn delete_emoticon(
    store: &Path,
    asset_id: [u8; 32],
    triggers: &mut Vec<Trigger>,
) -> Result<(), Box<dyn Error>> {
    if !triggers.iter().any(|item| item.asset_id == asset_id) {
        return Err("emoticon non trovata".into());
    }
    let hash = hex_asset_id(&asset_id);
    for extension in ["png", "jpg", "gif", "webp", "trigger", "name"] {
        let path = store.join(format!("{hash}.{extension}"));
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    triggers.retain(|item| item.asset_id != asset_id);
    Ok(())
}

fn client_emoticon(offer: &EmoticonOffer, trigger: &str, saved: bool) -> ClientEmoticon {
    ClientEmoticon {
        asset_id: hex_asset_id(&offer.metadata.asset_id),
        name: offer.metadata.name.clone(),
        trigger: trigger.to_owned(),
        mime: mime_name(offer.metadata.mime).into(),
        data_url: format!(
            "data:{};base64,{}",
            mime_name(offer.metadata.mime),
            BASE64.encode(&offer.bytes)
        ),
        animated: offer.metadata.animated,
        saved,
    }
}

fn load_emoticon_offer(store: &Path, trigger: &Trigger) -> Result<EmoticonOffer, Box<dyn Error>> {
    let hash = hex_asset_id(&trigger.asset_id);
    let path = ["png", "jpg", "gif", "webp"]
        .into_iter()
        .map(|extension| store.join(format!("{hash}.{extension}")))
        .find(|path| path.is_file())
        .ok_or("asset emoticon non trovato")?;
    let bytes = fs::read(&path)?;
    let mime = detect_mime(&bytes).ok_or("formato emoticon non supportato")?;
    let size = imagesize::blob_size(&bytes)?;
    Ok(EmoticonOffer {
        metadata: Emoticon {
            asset_id: trigger.asset_id,
            mime,
            width: size.width as u16,
            height: size.height as u16,
            animated: mime == Mime::Gif
                || mime == Mime::Webp && bytes.windows(4).any(|part| part == b"ANIM"),
            suggested_triggers: vec![trigger.text.clone()],
            name: fs::read_to_string(store.join(format!("{hash}.name")))
                .unwrap_or_else(|_| "Emoticon".into()),
        },
        bytes,
    })
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

pub struct ClientConfig {
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

impl ClientConfig {
    pub fn desktop(
        name: String,
        data_dir: PathBuf,
        connect: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
        if name.trim().is_empty() || name.len() > 64 {
            return Err("nome non valido".into());
        }
        Ok(Self {
            listen: "/ip4/0.0.0.0/udp/0/quic-v1".parse()?,
            listen_tcp: "/ip4/0.0.0.0/tcp/0".parse()?,
            connect: connect
                .filter(|address| !address.trim().is_empty())
                .map(|address| address.parse())
                .transpose()?,
            bootstrap: Vec::new(),
            relays: Vec::new(),
            relay_server: false,
            identity: data_dir.join("identity.key"),
            emotes: data_dir.join("emoticons"),
            downloads: data_dir.join("downloads"),
            history: data_dir.join("history.db"),
            notifications: true,
            name,
        })
    }

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

    async fn receive_until(
        events: &mut mpsc::UnboundedReceiver<ClientEvent>,
        predicate: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let event = events.recv().await.expect("canale eventi chiuso");
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("evento client non ricevuto in tempo")
    }

    fn available_udp_port() -> u16 {
        std::net::UdpSocket::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

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
    fn desktop_message_preserves_emoticon_spans() {
        let peer = PeerId::from(Keypair::generate_ed25519().public());
        let asset_id = [7; 32];
        let event = ChatEvent::Text(TextMessage {
            text: "ciao :x:".into(),
            emoticons: vec![msnnext_protocol::EmoticonSpan {
                start: 5,
                end: 8,
                asset_id,
            }],
        });

        let ClientEvent::Message { message } =
            client_event_from_chat(peer, &event, "in", true, None).unwrap()
        else {
            panic!("evento messaggio atteso");
        };

        assert_eq!(message.emoticons.len(), 1);
        assert_eq!(message.emoticons[0].asset_id, hex_asset_id(&asset_id));
    }

    #[test]
    fn saved_emoticon_can_be_reloaded_for_sharing() {
        let store = std::env::temp_dir().join(format!("msnnext-emoticon-{}", std::process::id()));
        fs::remove_dir_all(&store).ok();
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
        let offer = EmoticonOffer {
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
        let mut triggers = Vec::new();
        save_offer(&store, &offer, &mut triggers).unwrap();

        let reloaded = load_emoticon_offer(&store, &triggers[0]).unwrap();

        assert_eq!(reloaded.metadata.asset_id, offer.metadata.asset_id);
        assert_eq!(reloaded.bytes, offer.bytes);
        fs::remove_dir_all(store).ok();
    }

    #[test]
    fn emoticon_trigger_conflicts_are_rejected() {
        let store =
            std::env::temp_dir().join(format!("msnnext-emoticon-conflict-{}", std::process::id()));
        fs::remove_dir_all(&store).ok();
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01".to_vec();
        let offer = EmoticonOffer {
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
        let mut triggers = vec![Trigger {
            text: ":x:".into(),
            asset_id: [9; 32],
            case_sensitive: true,
        }];

        assert!(save_offer(&store, &offer, &mut triggers).is_err());
        fs::remove_dir_all(store).ok();
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
    fn group_definition_requires_an_owner_and_three_unique_members() {
        let owner = PeerId::from(Keypair::generate_ed25519().public()).to_string();
        let alice = PeerId::from(Keypair::generate_ed25519().public()).to_string();
        let bob = PeerId::from(Keypair::generate_ed25519().public()).to_string();
        let mut group = GroupDefinition {
            group_id: [1; 16],
            name: "Amici".into(),
            owner_peer: owner.clone(),
            members: vec![owner, alice, bob],
            revision: 1,
        };
        assert_eq!(validate_group_definition(&group), Ok(()));
        group.members[2] = group.members[1].clone();
        assert_eq!(
            validate_group_definition(&group),
            Err("partecipanti del gruppo non validi")
        );
    }

    #[test]
    fn group_file_is_not_shown_before_membership_validation() {
        let peer = PeerId::from(Keypair::generate_ed25519().public());
        let event = ChatEvent::GroupAttachmentOffer(GroupAttachmentOffer {
            group_id: [1; 16],
            manifest: AttachmentManifest {
                attachment_id: *blake3::hash(&[]).as_bytes(),
                filename: "foto.png".into(),
                mime: "image/png".into(),
                size: 0,
                chunk_size: attachments::CHUNK_SIZE as u32,
                chunks: Vec::new(),
            },
        });
        assert!(client_event_from_chat(peer, &event, "in", true, None).is_none());
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
        assert!(!handshake_peer_authorized(false, false, false));
        assert!(handshake_peer_authorized(true, false, false));
        assert!(handshake_peer_authorized(false, true, false));
        assert!(handshake_peer_authorized(false, false, true));
    }

    #[test]
    fn failed_secure_requests_have_bounded_retries() {
        assert_eq!(next_request_retry(0), Some(1));
        assert_eq!(next_request_retry(1), Some(2));
        assert_eq!(next_request_retry(2), None);
    }

    #[test]
    fn desktop_commands_target_exactly_one_conversation() {
        let peer = PeerId::from(Keypair::generate_ed25519().public());
        let command = ClientCommand::SendText {
            peer,
            text: "ciao".into(),
        };

        assert_eq!(command.peer(), Some(peer));
    }

    #[test]
    fn parses_connectivity_options() {
        let bootstrap_peer = PeerId::from(Keypair::generate_ed25519().public());
        let relay_peer = PeerId::from(Keypair::generate_ed25519().public());
        let bootstrap = format!("/ip4/127.0.0.1/tcp/4001/p2p/{bootstrap_peer}");
        let relay = format!("/ip4/127.0.0.1/tcp/4002/p2p/{relay_peer}");

        let args = ClientConfig::parse_from(vec![
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

    #[tokio::test(flavor = "current_thread")]
    #[ignore = "usa socket reali e il teardown libp2p può bloccare l'harness Windows"]
    async fn two_clients_exchange_messages_and_reconnect_after_restart() {
        let root =
            std::env::temp_dir().join(format!("msnnext-pair-{}-{}", std::process::id(), now_ms()));
        let alice_dir = root.join("alice");
        let bob_dir = root.join("bob");
        fs::remove_dir_all(&root).ok();
        let alice_port = available_udp_port();
        let alice_listen: Multiaddr = format!("/ip4/127.0.0.1/udp/{alice_port}/quic-v1")
            .parse()
            .unwrap();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice_config =
                    ClientConfig::desktop("Alice".into(), alice_dir.clone(), None).unwrap();
                alice_config.listen = alice_listen.clone();
                alice_config.notifications = false;
                let (alice_commands, alice_command_rx) = mpsc::unbounded_channel();
                let (alice_event_tx, mut alice_events) = mpsc::unbounded_channel();
                let alice_task =
                    tokio::task::spawn_local(run(alice_config, alice_command_rx, alice_event_tx));
                receive_until(&mut alice_events, |event| {
                    matches!(event, ClientEvent::Started { .. })
                })
                .await;
                alice_commands
                    .send(ClientCommand::RequestContactLink)
                    .unwrap();
                let ClientEvent::ContactLink { link } = receive_until(&mut alice_events, |event| {
                    matches!(event, ClientEvent::ContactLink { .. })
                })
                .await
                else {
                    unreachable!()
                };

                let mut bob_config =
                    ClientConfig::desktop("Bob".into(), bob_dir.clone(), None).unwrap();
                bob_config.notifications = false;
                let (bob_commands, bob_command_rx) = mpsc::unbounded_channel();
                let (bob_event_tx, mut bob_events) = mpsc::unbounded_channel();
                let mut bob_task =
                    tokio::task::spawn_local(run(bob_config, bob_command_rx, bob_event_tx));
                let ClientEvent::Started {
                    peer_id: bob_peer, ..
                } = receive_until(&mut bob_events, |event| {
                    matches!(event, ClientEvent::Started { .. })
                })
                .await
                else {
                    unreachable!()
                };
                bob_commands
                    .send(ClientCommand::ImportContactLink { link })
                    .unwrap();

                let ClientEvent::ContactUpdated { contact } =
                    receive_until(&mut bob_events, |event| {
                        matches!(
                            event,
                            ClientEvent::ContactUpdated {
                                contact: ClientContact { secure: true, .. }
                            }
                        )
                    })
                    .await
                else {
                    unreachable!()
                };
                let alice_peer: PeerId = contact.peer_id.parse().unwrap();
                receive_until(&mut alice_events, |event| {
                    matches!(
                        event,
                        ClientEvent::ContactUpdated {
                            contact: ClientContact { secure: true, .. }
                        }
                    )
                })
                .await;

                bob_commands
                    .send(ClientCommand::SendText {
                        peer: alice_peer,
                        text: "prima".into(),
                    })
                    .unwrap();
                receive_until(&mut alice_events, |event| {
                    matches!(
                        event,
                        ClientEvent::Message {
                            message: ClientMessage { body, .. }
                        } if body == "prima"
                    )
                })
                .await;

                alice_commands.send(ClientCommand::Shutdown).unwrap();
                alice_task.await.unwrap().unwrap();
                tokio::select! {
                    _ = receive_until(&mut bob_events, |event| {
                        matches!(
                            event,
                            ClientEvent::ContactUpdated {
                                contact: ClientContact { online: false, .. }
                            }
                        )
                    }) => {}
                    result = &mut bob_task => {
                        panic!("Bob terminato durante la disconnessione: {result:?}");
                    }
                }

                let mut restarted_config =
                    ClientConfig::desktop("Alice".into(), alice_dir.clone(), None).unwrap();
                restarted_config.listen = alice_listen;
                restarted_config.notifications = false;
                let (restarted_commands, restarted_command_rx) = mpsc::unbounded_channel();
                let (restarted_event_tx, mut restarted_events) = mpsc::unbounded_channel();
                let restarted_task = tokio::task::spawn_local(run(
                    restarted_config,
                    restarted_command_rx,
                    restarted_event_tx,
                ));
                receive_until(&mut restarted_events, |event| {
                    matches!(event, ClientEvent::Started { .. })
                })
                .await;
                receive_until(&mut bob_events, |event| {
                    matches!(
                        event,
                        ClientEvent::ContactUpdated {
                            contact: ClientContact { secure: true, .. }
                        }
                    )
                })
                .await;
                receive_until(&mut restarted_events, |event| {
                    matches!(
                        event,
                        ClientEvent::ContactUpdated {
                            contact: ClientContact { secure: true, .. }
                        }
                    )
                })
                .await;

                restarted_commands
                    .send(ClientCommand::SendText {
                        peer: bob_peer.parse().unwrap(),
                        text: "dopo il riavvio".into(),
                    })
                    .unwrap();
                receive_until(&mut bob_events, |event| {
                    matches!(
                        event,
                        ClientEvent::Message {
                            message: ClientMessage { body, .. }
                        } if body == "dopo il riavvio"
                    )
                })
                .await;

                restarted_commands.send(ClientCommand::Shutdown).unwrap();
                tokio::time::timeout(Duration::from_secs(5), restarted_task)
                    .await
                    .expect("Alice non si è arrestata")
                    .unwrap()
                    .unwrap();
                bob_commands.send(ClientCommand::Shutdown).unwrap();
                tokio::time::timeout(Duration::from_secs(5), bob_task)
                    .await
                    .expect("Bob non si è arrestato")
                    .unwrap()
                    .unwrap();
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn chat_connections_are_not_treated_as_idle_after_one_minute() {
        assert!(CHAT_IDLE_TIMEOUT >= Duration::from_secs(24 * 60 * 60));
    }

    #[test]
    fn only_known_peers_reconnect_after_the_last_connection_closes() {
        assert!(should_reconnect_closed_peer(true, 0));
        assert!(!should_reconnect_closed_peer(false, 0));
        assert!(!should_reconnect_closed_peer(true, 1));
    }

    #[test]
    fn reconnect_backoff_is_fast_then_bounded() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(5), Duration::from_secs(30));
        assert_eq!(reconnect_delay(20), Duration::from_secs(30));
    }

    #[test]
    fn secure_handshake_starts_only_after_presence() {
        let presence = ChatEvent::Presence(PresenceUpdate {
            display_name: "Alice".into(),
            online: true,
        });
        let text = ChatEvent::Text(TextMessage {
            text: "ciao".into(),
            emoticons: vec![],
        });

        assert!(event_authorizes_handshake(&presence));
        assert!(!event_authorizes_handshake(&text));
    }

    #[test]
    fn integrated_client_starts_and_stops_through_typed_channels() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let data_dir =
            std::env::temp_dir().join(format!("msnnext-integrated-{}", std::process::id()));
        fs::remove_dir_all(&data_dir).ok();
        runtime.block_on(async {
            let config = ClientConfig::desktop("Alice".into(), data_dir.clone(), None).unwrap();
            let (command_tx, command_rx) = mpsc::unbounded_channel();
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let client = run(config, command_rx, event_tx);
            tokio::pin!(client);

            let started = loop {
                tokio::select! {
                    result = &mut client => panic!("client terminato prima dell'avvio: {result:?}"),
                    event = event_rx.recv() => {
                        if let Some(ClientEvent::Started { peer_id, .. }) = event {
                            break peer_id;
                        }
                    }
                }
            };
            assert!(!started.is_empty());

            command_tx.send(ClientCommand::Shutdown).unwrap();
            client.await.unwrap();
        });
        runtime.shutdown_background();
        fs::remove_dir_all(data_dir).ok();
    }
}
