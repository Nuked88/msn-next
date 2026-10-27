mod attachments;
mod connectivity;
mod contacts;
mod crypto;
mod devices;
mod history;
mod invite;

use attachments::{
    build_manifest, read_chunk, validate_manifest, CompletedAttachment, Completion, FinalizeJob,
    Receiver,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chacha20poly1305::aead::rand_core::{OsRng, RngCore};
use connectivity::{relay_route, split_peer_address, FallbackPlanner, Recovery};
use crypto::{
    accepts_inbound, needs_outbound_handshake, respond as respond_hybrid, should_initiate,
    verify_account_proof, AccountIdentity, HybridInitiator, HybridResponse, RatchetMessage,
    RatchetSession, SessionKey,
};
use devices::{
    AccountRotation, DeviceDescriptor, DeviceRequest, DeviceResponse, PairingLink, RevokedDevice,
    SealedDeviceMessage,
};
use futures::StreamExt;
use history::{ContactEntry, DeviceEntry, GroupChatEntry, History};
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
pub use msnnext_protocol::MAX_TEXT_BYTES;
use msnnext_protocol::{
    resolve_emoticons, validate_text_message, validate_triggers, AccountProof, AttachmentCancel,
    AttachmentManifest, ChatEvent, DeleteRequest, Emoticon, EmoticonOffer, EmoticonSpan, Envelope,
    GroupAttachmentOffer, GroupBan, GroupDefinition, GroupTextMessage, Mime, Nudge, NudgeRateLimit,
    PresenceUpdate, ProtocolResponse, TextMessage, Trigger, PROTOCOL_VERSION,
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fs,
    future::pending,
    path::{Path, PathBuf},
    sync::Arc,
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
const TEXT_HISTORY_PREFIX: &str = "msnnext-text-v1:";
/// Richieste di contatto mostrate e in attesa di risposta.
const MAX_PENDING_CONTACT_REQUESTS: usize = 20;
const MAX_PENDING_INBOUND_OFFERS: usize = 8;
/// Emoticon ricevute in attesa di salvataggio (fino a ~350 KB l'una).
const MAX_PENDING_EMOTICONS: usize = 64;
const ATTACHMENT_DECISION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Messaggi caricati per volta in una conversazione; i precedenti arrivano
/// a pagine su richiesta della UI.
const HISTORY_PAGE: usize = 100;
/// Entro quanto un messaggio deve essere confermato prima del reinvio. Il
/// timeout di libp2p è unico per il protocollo cifrato e vale 5 minuti per
/// via delle offerte di file: per i messaggi lo accorciamo noi.
const MESSAGE_ACK_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 20 });
const DELIVERY_CHECK_INTERVAL: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 5 });

/// Evento cifrato in attesa di conferma: destinatario, evento, tentativi e
/// istante di invio (ms).
type PendingMessage = (PeerId, ChatEvent, u8, u64);
const RELAY_MAX_CIRCUIT_BYTES: u64 = attachments::MAX_FILE_BYTES + 64 * 1024 * 1024;
/// Basta per 5 GB anche a ~250 KB/s; oltre, il trasferimento riprende su un
/// nuovo circuito grazie al resume a chunk.
const RELAY_MAX_CIRCUIT_DURATION: Duration = Duration::from_secs(6 * 60 * 60);
/// Circuiti aperti per peer sorgente: limita quanto un singolo estraneo può
/// occupare il relay pubblico.
const RELAY_MAX_CIRCUITS_PER_PEER: usize = 2;
const DEFAULT_RELAY_ADDRESS: &str =
    "/ip4/173.212.233.151/tcp/4001/p2p/12D3KooWCQBCLXYhjkhay3hXTrhrNF8ZjRLd8qHL2nD2iufais4k";
type AccountKeyStore = Arc<dyn Fn([u8; 32]) -> Result<(), String> + Send + Sync>;

#[derive(Debug)]
pub enum ClientCommand {
    SendText {
        peer: PeerId,
        text: String,
    },
    SendNudge {
        peer: PeerId,
    },
    SendFile {
        peer: PeerId,
        path: PathBuf,
    },
    CreateEmoticon {
        path: PathBuf,
        trigger: String,
    },
    SaveEmoticon {
        asset_id: String,
        trigger: String,
    },
    UpdateEmoticon {
        asset_id: String,
        trigger: String,
    },
    DeleteEmoticon {
        asset_id: String,
    },
    ImportContactLink {
        link: String,
    },
    RenameContact {
        peer: PeerId,
        name: String,
    },
    DeleteContact {
        peer: PeerId,
    },
    ClearConversation {
        peer: PeerId,
    },
    CreateChatGroup {
        name: String,
        members: Vec<PeerId>,
    },
    ModerateGroup {
        group_id: String,
        peer: PeerId,
        action: GroupModeration,
    },
    SendGroupText {
        group_id: String,
        text: String,
    },
    SendGroupFile {
        group_id: String,
        path: PathBuf,
    },
    ClearGroupConversation {
        group_id: String,
    },
    /// Pagina di messaggi precedenti a `before_event_id` in una chat.
    LoadOlderMessages {
        peer: PeerId,
        before_event_id: String,
    },
    LoadOlderGroupMessages {
        group_id: String,
        before_event_id: String,
    },
    DeleteChatGroup {
        group_id: String,
    },
    ReadAttachment {
        id: String,
        mime: String,
        /// Solo anteprima ridotta (immagini), per il messaggio in chat.
        thumbnail: bool,
    },
    ExportAttachment {
        id: String,
        path: PathBuf,
    },
    /// Annulla gli invii di file di una conversazione (id del contatto o
    /// `group:<id>`); senza conversazione, tutti.
    CancelFileTransfers {
        conversation: Option<String>,
    },
    AcceptAttachment {
        offer_id: u64,
    },
    RejectAttachment {
        offer_id: u64,
    },
    AcceptContactRequest {
        peer: PeerId,
    },
    RejectContactRequest {
        peer: PeerId,
    },
    RequestPendingContactRequests,
    DeleteMessageForMe {
        event_id: String,
    },
    DeleteMessageForEveryone {
        peer: PeerId,
        event_id: String,
    },
    SetAutoAcceptExtensions {
        extensions: Vec<String>,
    },
    SetPresenceStatus {
        status: String,
    },
    SetNotificationMute {
        conversation: String,
        muted: bool,
        until_ms: Option<u64>,
    },
    RequestContactLink,
    /// Crea un codice d'invito corto (sostituisce quello precedente).
    CreateInvite,
    CancelInvite,
    /// Usa il codice d'invito mostrato da un'altra persona.
    JoinInvite {
        code: String,
    },
    RequestDeviceLink,
    ImportDeviceLink {
        link: String,
    },
    UpdateDisplayName {
        name: String,
    },
    /// Esclude un dispositivo collegato (perso, venduto) e cambia la chiave di
    /// account, così quel dispositivo non può più sincronizzarsi né firmare
    /// a nome dell'account.
    RevokeDevice {
        peer: PeerId,
    },
    /// Risposta dell'utente a una rotazione arrivata da un altro dispositivo:
    /// se rifiuta, il dispositivo che l'ha chiesta viene revocato.
    AnswerAccountRotation {
        request_id: u64,
        accept: bool,
    },
    /// Solo test: dimentica la sessione cifrata con un peer, come dopo un
    /// riavvio dell'altro lato o un handshake concorrente.
    #[cfg(test)]
    ForgetSession {
        peer: PeerId,
    },
    /// Solo test: sostituisce la sessione con una dalle chiavi sbagliate, come
    /// dopo una perdita di sincronizzazione del ratchet.
    #[cfg(test)]
    ScrambleSession {
        peer: PeerId,
    },
    /// Solo test: gestisce i messaggi in arrivo ma non risponde, come se la
    /// conferma si perdesse per strada.
    #[cfg(test)]
    WithholdAcks {
        enabled: bool,
    },
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
            | Self::AcceptContactRequest { peer }
            | Self::RejectContactRequest { peer }
            | Self::DeleteMessageForEveryone { peer, .. }
            | Self::LoadOlderMessages { peer, .. }
            | Self::ClearConversation { peer } => Some(*peer),
            #[cfg(test)]
            Self::ForgetSession { peer } | Self::ScrambleSession { peer } => Some(*peer),
            #[cfg(test)]
            Self::WithholdAcks { .. } => None,
            Self::CreateEmoticon { .. }
            | Self::DeleteMessageForMe { .. }
            | Self::SetAutoAcceptExtensions { .. }
            | Self::SetPresenceStatus { .. }
            | Self::SaveEmoticon { .. }
            | Self::UpdateEmoticon { .. }
            | Self::DeleteEmoticon { .. }
            | Self::CreateChatGroup { .. }
            | Self::ModerateGroup { .. }
            | Self::SendGroupText { .. }
            | Self::SendGroupFile { .. }
            | Self::ClearGroupConversation { .. }
            | Self::LoadOlderGroupMessages { .. }
            | Self::DeleteChatGroup { .. }
            | Self::ReadAttachment { .. }
            | Self::ExportAttachment { .. }
            | Self::CancelFileTransfers { .. }
            | Self::AcceptAttachment { .. }
            | Self::RejectAttachment { .. }
            | Self::RequestPendingContactRequests
            | Self::SetNotificationMute { .. }
            | Self::ImportContactLink { .. }
            | Self::RequestContactLink
            | Self::CreateInvite
            | Self::CancelInvite
            | Self::JoinInvite { .. }
            | Self::RequestDeviceLink
            | Self::ImportDeviceLink { .. }
            | Self::UpdateDisplayName { .. }
            | Self::RevokeDevice { .. }
            | Self::AnswerAccountRotation { .. }
            | Self::Shutdown => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum GroupModeration {
    SetAdmin(bool),
    SetSilenced(bool),
    Ban(Option<u64>),
    Unban,
}

const PAIRING_WITH_CONTACT: &str =
    "questo codice appartiene a un tuo contatto: è un altro account e non si può collegare";
const PAIRING_WITH_LOCAL_DATA: &str =
    "questo dispositivo ha già chat o contatti di un altro account: esci dall'account prima di collegarlo";

pub fn parse_peer_id(value: &str) -> Result<PeerId, String> {
    value
        .parse()
        .map_err(|error| format!("peer id non valido: {error}"))
}

/// Cosa mostra un codice dispositivo prima di usarlo: l'interfaccia lo fa
/// vedere all'utente per chiedere conferma.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLinkPreview {
    pub name: String,
    pub peer_id: String,
    pub expires_at_ms: u64,
}

pub fn preview_device_link(link: &str) -> Result<DeviceLinkPreview, String> {
    let pairing = devices::parse_pairing_link(link, now_ms()).map_err(|error| error.to_string())?;
    Ok(DeviceLinkPreview {
        name: pairing.inviter.name,
        peer_id: pairing.inviter.peer_id,
        expires_at_ms: pairing.expires_at_ms,
    })
}

pub fn generate_secret() -> [u8; 32] {
    let mut secret = [0; 32];
    OsRng.fill_bytes(&mut secret);
    secret
}

pub fn rekey_history_database(
    path: &Path,
    old_identity: &[u8],
    new_identity: &[u8],
) -> Result<(), Box<dyn Error>> {
    let old_key = blake3::derive_key("msnnext local history v1", old_identity);
    let new_key = blake3::derive_key("msnnext local history v1", new_identity);
    history::rekey_database(path, old_key, new_key)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientContact {
    pub peer_id: String,
    pub name: String,
    pub online: bool,
    pub secure: bool,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientDevice {
    pub peer_id: String,
    pub name: String,
    pub online: bool,
    pub last_seen_ms: u64,
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
    /// Id condiviso del messaggio (per la cancellazione). `None` per eventi
    /// legacy senza id.
    #[serde(default)]
    pub event_id: Option<String>,
    /// Vero se inviato tramite un dispositivo collegato perché il canale
    /// cifrato diretto con il contatto non era disponibile.
    #[serde(default)]
    pub relayed: bool,
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
    pub admins: Vec<String>,
    pub silenced: Vec<String>,
    pub bans: Vec<ClientGroupBan>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientGroupBan {
    pub peer_id: String,
    pub expires_at_ms: Option<u64>,
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
    /// Id in cronologia: serve alla UI per chiedere i messaggi precedenti.
    pub event_id: Option<String>,
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
        fingerprint: String,
    },
    ContactUpdated {
        contact: ClientContact,
    },
    ContactRequest {
        peer_id: String,
        name: String,
        /// Impronta da confrontare con l'altra persona prima di accettare.
        fingerprint: String,
        /// Ha usato un nostro codice d'invito: la scheda è già verificata,
        /// ma l'ultima parola resta all'utente (il codice può essere rubato).
        via_invite: bool,
    },
    MessageDeleted {
        peer_id: String,
        event_id: String,
    },
    /// Un messaggio inviato non è stato consegnato (rifiutato o tentativi
    /// esauriti).
    MessageFailed {
        peer_id: String,
        event_id: String,
        reason: String,
    },
    ContactStatus {
        peer_id: String,
        status: String,
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
        qr_link: String,
    },
    DeviceLink {
        link: String,
        qr_link: String,
        expires_at_ms: u64,
    },
    /// Codice d'invito da mostrare o dettare all'altra persona.
    Invite {
        code: String,
        expires_at_ms: u64,
    },
    /// Invito concluso: il contatto è in rubrica da entrambe le parti.
    InviteCompleted {
        peer_id: String,
        name: String,
        /// `true` dal lato di chi ha creato il codice: ora deve accettare la
        /// richiesta; `false` da chi l'ha inserito: attende quella conferma.
        inviter: bool,
    },
    /// L'invito mostrato (`inviter`) o quello inserito è fallito.
    InviteFailed {
        inviter: bool,
        message: String,
    },
    DevicesUpdated {
        devices: Vec<ClientDevice>,
    },
    DeviceSynchronized {
        peer_id: String,
        applied: usize,
        paired: bool,
    },
    /// Un altro dispositivo dell'account chiede di revocarne alcuni e di
    /// adottare una nuova chiave: serve la conferma dell'utente.
    AccountRotationRequested {
        request_id: u64,
        from_peer_id: String,
        from_name: String,
        revoked: Vec<String>,
    },
    DeviceRevoked {
        peer_id: String,
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
    AttachmentProgress {
        filename: String,
        completed_chunks: usize,
        total_chunks: usize,
    },
    AttachmentTransfersCancelled,
    /// Il mittente ha annullato un file in arrivo; `offer_id` se l'offerta
    /// era ancora in attesa di risposta.
    IncomingAttachmentCancelled {
        peer_id: String,
        attachment_id: String,
        offer_id: Option<u64>,
    },
    /// Un invio di file si è fermato senza arrivare: la UI chiude la barra di
    /// avanzamento di quel file e mostra il motivo.
    AttachmentFailed {
        filename: String,
        reason: String,
    },
    IncomingAttachmentOffered {
        offer_id: u64,
        peer_id: String,
        filename: String,
        mime: String,
        size: u64,
        group_id: Option<String>,
    },
    ContactRemoved {
        peer_id: String,
        /// "deleted", "rejected", "merged" (era un duplicato della stessa
        /// persona) o "withdrawn" (richiesta ritirata): la UI avvisa solo
        /// quando serve, e con il messaggio giusto.
        reason: String,
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
    /// Messaggi più vecchi di quelli già mostrati; `has_more` dice se ce ne
    /// sono altri ancora.
    OlderMessagesLoaded {
        peer_id: String,
        messages: Vec<ClientMessage>,
        has_more: bool,
    },
    OlderGroupMessagesLoaded {
        group_id: String,
        messages: Vec<ClientGroupMessage>,
        has_more: bool,
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
        thumbnail: bool,
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
    devices: request_response::cbor::Behaviour<SealedDeviceMessage, SealedDeviceMessage>,
    invite: request_response::cbor::Behaviour<invite::InviteRequest, invite::InviteResponse>,
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    autonat: autonat::Behaviour,
    relay_client: relay::client::Behaviour,
    relay_server: Toggle<relay::Behaviour>,
    dcutr: dcutr::Behaviour,
    ping: ping::Behaviour,
}

/// Ricerca e scambio per un codice d'invito inserito dall'utente.
struct InviteJoin {
    code: invite::InviteCode,
    query: Option<kad::QueryId>,
    /// Provider trovati nella DHT e non ancora provati.
    candidates: VecDeque<PeerId>,
    tried: HashSet<PeerId>,
    current: Option<PeerId>,
    session: Option<invite::JoinSession>,
    /// Scheda di chi invita già verificata, in attesa del suo `Finished`.
    card: Option<contacts::InviteCard>,
    /// Ultimo errore di un candidato, mostrato se non ne restano altri.
    last_error: Option<String>,
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
    group_id: Option<[u8; 16]>,
    remaining: VecDeque<u32>,
    current: u32,
    retries: u8,
    completed: usize,
    total: usize,
}

struct PendingInboundOffer {
    peer: PeerId,
    request_id: request_response::InboundRequestId,
    event: ChatEvent,
    channel: request_response::ResponseChannel<ProtocolResponse>,
}

enum PendingDeviceRequest {
    Pair {
        peer: PeerId,
        key: [u8; 32],
    },
    Sync {
        peer: PeerId,
    },
    Relay {
        peer: PeerId,
        target: PeerId,
        message: TextMessage,
    },
    /// Avviso di rotazione a un altro dispositivo: sigillato con la chiave
    /// ormai ritirata, la risposta non porta informazioni.
    Rotate,
}

struct PendingPairing {
    secret: [u8; 32],
    expires_at_ms: u64,
}

struct Incoming<'a> {
    pending_emoticons: &'a mut HashMap<[u8; 32], EmoticonOffer>,
    events: &'a mpsc::UnboundedSender<ClientEvent>,
    nudge_limits: &'a mut HashMap<PeerId, NudgeRateLimit>,
    attachments: &'a mut Receiver,
    history: &'a History,
    notifications: bool,
    notification_mutes: &'a HashMap<String, Option<u64>>,
    peer_names: &'a mut HashMap<PeerId, String>,
    local_peer_id: PeerId,
    incoming_attachments: &'a mut HashMap<(PeerId, [u8; 32]), HashSet<Option<String>>>,
    vault_jobs: &'a mpsc::UnboundedSender<VaultJobDone>,
}

struct SyncConnections<'a> {
    swarm: &'a mut Swarm<Behaviour>,
    online_contacts: &'a HashSet<PeerId>,
    online_devices: &'a HashSet<PeerId>,
    dialing: &'a mut HashSet<PeerId>,
    bootstrap_fallbacks: &'a mut HashMap<PeerId, VecDeque<Multiaddr>>,
    fallback_planner: &'a mut FallbackPlanner,
    pending_dht: &'a mut HashMap<kad::QueryId, PeerId>,
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
            let mut kad = kad::Behaviour::with_config(
                local_peer_id,
                kad::store::MemoryStore::new(local_peer_id),
                kad::Config::new(StreamProtocol::new("/msnnext/kad/1")),
            );
            if relay_server_enabled {
                kad.set_mode(Some(kad::Mode::Server));
            }
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
                    request_response::Config::default()
                        .with_request_timeout(ATTACHMENT_DECISION_TIMEOUT),
                ),
                handshake: request_response::cbor::Behaviour::new(
                    [(
                        StreamProtocol::new("/msnnext/handshake/1"),
                        ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                ),
                devices: request_response::cbor::Behaviour::new(
                    [(
                        StreamProtocol::new("/msnnext/devices/1"),
                        ProtocolSupport::Full,
                    )],
                    request_response::Config::default(),
                ),
                // Il mininodo trova gli inviti nella DHT ma non ne accetta.
                invite: request_response::cbor::Behaviour::new(
                    [(
                        StreamProtocol::new("/msnnext/invite/1"),
                        if relay_server_enabled {
                            ProtocolSupport::Outbound
                        } else {
                            ProtocolSupport::Full
                        },
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
                relay_server: Toggle::from(relay_server_enabled.then(|| {
                    relay::Behaviour::new(
                        local_peer_id,
                        relay::Config {
                            max_circuit_bytes: RELAY_MAX_CIRCUIT_BYTES,
                            max_circuit_duration: RELAY_MAX_CIRCUIT_DURATION,
                            max_circuits_per_peer: RELAY_MAX_CIRCUITS_PER_PEER,
                            ..relay::Config::default()
                        },
                    )
                })),
                dcutr: dcutr::Behaviour::new(local_peer_id),
                ping: ping::Behaviour::new(ping::Config::new()),
            }
        })?
        .with_swarm_config(|config| config.with_idle_connection_timeout(CHAT_IDLE_TIMEOUT))
        .build())
}

/// Logger della CLI: i messaggi del nodo su stdout, avvisi ed errori su
/// stderr. Le librerie (libp2p) restano silenziose come prima.
struct CliLogger;

impl log::Log for CliLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info && metadata.target().starts_with("msnnext")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if record.level() <= log::Level::Warn {
            eprintln!("{}", record.args());
        } else {
            println!("{}", record.args());
        }
    }

    fn flush(&self) {}
}

#[tokio::main]
pub async fn run_cli() -> Result<(), Box<dyn Error>> {
    // La GUI raccoglie i log con tauri-plugin-log; da terminale li stampiamo.
    if log::set_logger(&CliLogger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let args = ClientConfig::parse()?;
    let (_command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    run(args, command_rx, event_tx).await
}

/// Avvia il nodo e lo fa girare finché non riceve `Shutdown`. Qualunque sia
/// l'esito, l'ultimo evento emesso è `Stopped`: la UI non deve mai restare
/// "online" con un nodo che non gira più.
pub async fn run(
    args: ClientConfig,
    commands: mpsc::UnboundedReceiver<ClientCommand>,
    events: mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), Box<dyn Error>> {
    let result = run_node(args, commands, events.clone()).await;
    if let Err(error) = &result {
        let _ = events.send(ClientEvent::Error {
            message: format!("il nodo di rete si è arrestato: {error}"),
        });
    }
    let _ = events.send(ClientEvent::Stopped);
    result
}

async fn run_node(
    args: ClientConfig,
    commands: mpsc::UnboundedReceiver<ClientCommand>,
    events: mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), Box<dyn Error>> {
    let identity = match args.identity_bytes {
        Some(ref bytes) => Keypair::from_protobuf_encoding(bytes)?,
        None => load_or_create_identity(&args.identity)?,
    };
    let ml_dsa_seed = match args.ml_dsa_seed {
        Some(seed) => seed,
        None => load_or_create_secret(&args.identity.with_extension("mldsa.key"))?,
    };
    let ml_dsa_identity = crypto::MlDsaIdentity::from_seed(&ml_dsa_seed)?;
    let contact_identity = identity.clone();
    let identity_bytes = identity.to_protobuf_encoding()?;
    let account_key = args
        .account_key
        .unwrap_or_else(|| blake3::derive_key("msnnext account root v1", &identity_bytes));
    let history_key = blake3::derive_key("msnnext local history v1", &identity_bytes);
    let attachment_key = blake3::derive_key("msnnext attachment vault v1", &identity_bytes);
    let history = History::open(&args.history, history_key)?;
    let persisted_contacts = history.contacts()?;
    let ignored_contacts = history
        .ignored_contacts()?
        .into_iter()
        .filter_map(|peer| peer.parse::<PeerId>().ok())
        .collect::<HashSet<_>>();
    let local_peer_id = PeerId::from(identity.public());
    // Identità di account: derivata dalla chiave di account condivisa fra i
    // dispositivi collegati (la chiave stessa non lascia mai il dispositivo).
    // Firma la prova con cui i contatti riconoscono i nostri dispositivi senza
    // doverli approvare di nuovo.
    let account_identity = AccountIdentity::from_account_key(&account_key)?;
    let account_proof = account_identity.proof(local_peer_id).ok();
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
            log::warn!("bootstrap non raggiungibile {peer}: {error}");
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
                log::warn!("relay non raggiungibile {address}: {error}");
            }
        }
    }

    log::info!("peer: {local_peer_id}");
    log::info!("comandi: text <messaggio> | emote <trigger> <file> | file <percorso> | contact qr | contact export/import <file> | nudge | history | quit");
    let display_name = args.name.clone();
    let _ = events.send(ClientEvent::Started {
        peer_id: local_peer_id.to_string(),
        display_name: display_name.clone(),
        fingerprint: peer_fingerprint(local_peer_id),
    });

    let lines = BufReader::new(tokio::io::stdin()).lines();
    let peers = HashSet::new();
    let mut dialing = HashSet::new();
    let mdns_dialing = HashSet::new();
    // Peer che ci hanno contattato ma non sono ancora contatti: in attesa che
    // l'utente accetti/rifiuti (niente auto-aggiunta senza consenso).
    let pending_contact_requests = HashMap::<PeerId, String>::new();
    // Account dimostrato da chi è ancora in attesa di approvazione: alla
    // conferma lega subito la persona, così i suoi altri dispositivi non
    // chiedono una seconda approvazione.
    let pending_accounts = HashMap::<PeerId, String>::new();
    let reconnect_at = HashMap::<PeerId, Instant>::new();
    let reconnect_attempts = HashMap::<PeerId, u8>::new();
    let mut bootstrap_fallbacks = HashMap::<PeerId, VecDeque<Multiaddr>>::new();
    let mut known_contacts = HashSet::<PeerId>::new();
    // Dispositivi collegati dei contatti: un contatto è una persona, non un
    // singolo peer.
    let mut contact_devices = ContactDevices::default();
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
    let accept_unclassified_manual = args.connect.is_some() && manual_peer.is_none();
    let mut fallback_planner = FallbackPlanner::new(args.relays.clone());
    let mut pending_dht = HashMap::<kad::QueryId, PeerId>::new();
    let requested_relay_reservations = HashSet::<PeerId>::new();
    let triggers = load_triggers(&args.emotes)?;
    let nudge_limits = HashMap::<PeerId, NudgeRateLimit>::new();
    let incoming_nudge_limits = HashMap::<PeerId, NudgeRateLimit>::new();
    let notification_mutes = HashMap::<String, Option<u64>>::new();
    let sent_numbers = HashMap::<PeerId, u64>::new();
    let pending_offers = HashMap::<request_response::OutboundRequestId, PendingOffer>::new();
    let pending_transfers = HashMap::<request_response::OutboundRequestId, PendingTransfer>::new();
    let pending_handshakes = Handshakes::new(
        crypto::MlDsaIdentity::from_seed(&ml_dsa_seed)?,
        args.session_rekey_interval,
    );
    // Tentativi di handshake per peer: consente il retry se il primo invio
    // fallisce (frequente su mobile finché la connessione non è stabile).
    let handshake_retries = HashMap::<PeerId, u8>::new();
    // Affidabilità messaggi: coda per-peer quando la sessione non è pronta, e
    // tracciamento (peer, evento, tentativi, invio) per retry e timeout.
    let outbox = HashMap::<PeerId, VecDeque<(ChatEvent, u8)>>::new();
    let pending_messages = HashMap::<request_response::OutboundRequestId, PendingMessage>::new();
    let delivery_tick = tokio::time::interval(DELIVERY_CHECK_INTERVAL);
    #[cfg(test)]
    let withhold_acks = false;
    #[cfg(test)]
    let held_responses = Vec::new();
    // Estensioni (minuscole, senza punto) accettate automaticamente. Vuoto = off.
    let auto_accept_extensions = HashSet::<String>::new();
    // Stato di presenza comunicato ai contatti: "online" | "busy" | "away".
    let presence_status = String::from("online");
    // (peer, chiave, rinnovo di una sessione attiva)
    let pending_inbound_handshakes =
        HashMap::<request_response::InboundRequestId, (PeerId, SessionKey, bool)>::new();
    let sessions = HashMap::<PeerId, RatchetSession>::new();
    // Errori di decifratura consecutivi per peer: oltre una soglia la sessione
    // è considerata rotta e si rifà l'handshake invece di rifiutare per sempre.
    let decrypt_failures = HashMap::<PeerId, u8>::new();
    let shared_emoticons = HashSet::<(PeerId, [u8; 32])>::new();
    // Offerte di file pronte per un contatto connesso ma ancora senza
    // sessione cifrata: partono quando l'handshake si completa.
    let waiting_offers = HashMap::<PeerId, Vec<PendingOffer>>::new();
    let pending_emoticons = HashMap::<[u8; 32], EmoticonOffer>::new();
    let attachment_receiver = Receiver::new(args.downloads.clone(), attachment_key);
    // Lavoro pesante sui file (hash in invio, verifica finale in ricezione)
    // fuori dal loop: i risultati tornano su questi canali.
    let (prepared_files_tx, prepared_files) = mpsc::unbounded_channel::<PreparedFile>();
    let (vault_jobs_tx, vault_jobs) = mpsc::unbounded_channel::<VaultJobDone>();
    let incoming_attachments = HashMap::<(PeerId, [u8; 32]), HashSet<Option<String>>>::new();
    let pending_inbound_offers = HashMap::<u64, PendingInboundOffer>::new();
    let inbound_offer_counter = 0_u64;
    let mut peer_names = HashMap::<PeerId, String>::new();
    let nudge_counter = 0_u64;
    let stdin_open = args.read_stdin;
    let commands_open = true;
    let contact_link_requested = false;
    let device_link_requested = false;
    let pending_pairings = HashMap::<[u8; 16], PendingPairing>::new();
    let pending_pair_targets = HashMap::<PeerId, PairingLink>::new();
    let active_invite = None::<invite::ActiveInvite>;
    let invite_join = None::<InviteJoin>;
    let pending_invite_cards = HashMap::<PeerId, contacts::InviteCard>::new();
    let pending_device_requests =
        HashMap::<request_response::OutboundRequestId, PendingDeviceRequest>::new();
    let online_devices = HashSet::<PeerId>::new();
    let known_devices = history
        .devices()?
        .into_iter()
        .filter_map(|device| {
            device
                .peer
                .parse::<PeerId>()
                .ok()
                .map(|peer| (peer, device))
        })
        .collect::<HashMap<_, _>>();
    let mut device_sync_tick = tokio::time::interval(Duration::from_secs(2));
    device_sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Riconciliatore handshake: periodicamente ripara le sessioni mancanti verso
    // contatti connessi (l'iniziatore ritenta, il responder ri-annuncia la
    // presence per sbloccare anche un iniziatore con build vecchia).
    let mut handshake_tick = tokio::time::interval(Duration::from_secs(3));
    handshake_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
        if known_devices.contains_key(&peer_id) {
            history.delete_contact(&peer_id.to_string())?;
            continue;
        }
        if !contact.account_id.is_empty() {
            contact_devices.bind(peer_id, contact.account_id.clone());
        }
        peer_names.insert(peer_id, name.clone());
        known_contacts.insert(peer_id);
        let _ = events.send(ClientEvent::ContactUpdated {
            contact: ClientContact {
                peer_id: peer_id.to_string(),
                name,
                online: false,
                secure: false,
                fingerprint: peer_fingerprint(peer_id),
            },
        });
        let messages = conversation_messages(&history, &peer_id.to_string())?;
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
    send_devices(&known_devices, &online_devices, &events);
    for (peer, device) in &known_devices {
        let addresses = device
            .addresses
            .iter()
            .filter_map(|address| address.parse::<Multiaddr>().ok())
            .collect();
        connect_contact(
            &mut swarm,
            *peer,
            addresses,
            &mut dialing,
            &mut bootstrap_fallbacks,
            &mut fallback_planner,
            &mut pending_dht,
        );
    }
    let group_chats = history.group_chats()?;
    send_group_chats(&group_chats, &events);
    for group in &group_chats {
        let messages = group_conversation_messages(&history, &group.id, None)?;
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

    let revoked_devices = history
        .revoked_devices()?
        .into_iter()
        .filter_map(|(peer, _)| peer.parse::<PeerId>().ok())
        .collect::<HashSet<_>>();
    let mut node = Node {
        revoked_devices,
        pending_rotations: HashMap::new(),
        rotation_counter: 0,
        ml_dsa_identity,
        contact_identity,
        account_key,
        history,
        ignored_contacts,
        local_peer_id,
        account_identity,
        account_proof,
        swarm,
        relay_addresses,
        display_name,
        lines,
        peers,
        dialing,
        mdns_dialing,
        pending_contact_requests,
        pending_accounts,
        reconnect_at,
        reconnect_attempts,
        bootstrap_fallbacks,
        known_contacts,
        contact_devices,
        infrastructure_peers,
        accept_unclassified_manual,
        fallback_planner,
        pending_dht,
        requested_relay_reservations,
        triggers,
        nudge_limits,
        incoming_nudge_limits,
        notification_mutes,
        sent_numbers,
        pending_offers,
        pending_transfers,
        pending_handshakes,
        handshake_retries,
        outbox,
        pending_messages,
        delivery_tick,
        #[cfg(test)]
        withhold_acks,
        #[cfg(test)]
        held_responses,
        auto_accept_extensions,
        presence_status,
        pending_inbound_handshakes,
        sessions,
        decrypt_failures,
        shared_emoticons,
        waiting_offers,
        pending_emoticons,
        attachment_receiver,
        prepared_files_tx,
        prepared_files,
        vault_jobs_tx,
        vault_jobs,
        incoming_attachments,
        pending_inbound_offers,
        inbound_offer_counter,
        peer_names,
        nudge_counter,
        stdin_open,
        commands_open,
        contact_link_requested,
        device_link_requested,
        pending_pairings,
        pending_pair_targets,
        active_invite,
        invite_join,
        pending_invite_cards,
        pending_device_requests,
        online_devices,
        known_devices,
        device_sync_tick,
        handshake_tick,
        args,
        commands,
        events,
    };
    loop {
        let next_reconnect = node
            .reconnect_at
            .iter()
            .min_by_key(|(_, deadline)| **deadline)
            .map(|(peer, deadline)| (*peer, *deadline));
        // Ogni evento è gestito in un metodo a sé: un errore (SQLite, link
        // malformato, dati di sync non validi...) diventa un ClientEvent::Error
        // e il nodo continua a girare invece di spegnersi.
        let step: Result<LoopFlow, Box<dyn Error>> = async {
            tokio::select! {
            Some(prepared) = node.prepared_files.recv() => node.on_prepared_file(prepared),
            Some(done) = node.vault_jobs.recv() => node.on_vault_job(done),
            _ = node.device_sync_tick.tick(), if !node.online_devices.is_empty() => node.on_device_sync_tick(),
            _ = node.delivery_tick.tick(), if !node.pending_messages.is_empty() => node.on_delivery_tick(),
            _ = node.handshake_tick.tick(), if !node.peers.is_empty() => node.on_handshake_tick(),
            _ = wait_for_reconnect(next_reconnect.map(|(_, deadline)| deadline)) => node.on_reconnect_due(next_reconnect),
            line = node.lines.next_line(), if node.stdin_open => node.on_stdin_line(line?),
            command = node.commands.recv(), if node.commands_open => node.on_command(command),
            event = node.swarm.select_next_some() => node.on_swarm_event(event),
            }
        }
        .await;
        match step {
            Ok(LoopFlow::Continue) => {}
            Ok(LoopFlow::Stop) => break,
            Err(error) => report_loop_error(&node.events, error.as_ref()),
        }
    }
    Ok(())
}

/// Stato del nodo tra un evento e l'altro del loop di `run_node`. Ogni
/// sorgente di eventi ha il suo gestore qui sotto.
struct Node {
    ml_dsa_identity: crypto::MlDsaIdentity,
    contact_identity: Keypair,
    account_key: [u8; 32],
    history: History,
    ignored_contacts: HashSet<PeerId>,
    local_peer_id: PeerId,
    account_identity: AccountIdentity,
    account_proof: Option<AccountProof>,
    swarm: Swarm<Behaviour>,
    relay_addresses: HashMap<PeerId, Multiaddr>,
    display_name: String,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>,
    peers: HashSet<PeerId>,
    dialing: HashSet<PeerId>,
    mdns_dialing: HashSet<PeerId>,
    pending_contact_requests: HashMap<PeerId, String>,
    pending_accounts: HashMap<PeerId, String>,
    reconnect_at: HashMap<PeerId, Instant>,
    reconnect_attempts: HashMap<PeerId, u8>,
    bootstrap_fallbacks: HashMap<PeerId, VecDeque<Multiaddr>>,
    known_contacts: HashSet<PeerId>,
    contact_devices: ContactDevices,
    infrastructure_peers: HashSet<PeerId>,
    accept_unclassified_manual: bool,
    fallback_planner: FallbackPlanner,
    pending_dht: HashMap<kad::QueryId, PeerId>,
    requested_relay_reservations: HashSet<PeerId>,
    triggers: Vec<Trigger>,
    nudge_limits: HashMap<PeerId, NudgeRateLimit>,
    incoming_nudge_limits: HashMap<PeerId, NudgeRateLimit>,
    notification_mutes: HashMap<String, Option<u64>>,
    sent_numbers: HashMap<PeerId, u64>,
    pending_offers: HashMap<request_response::OutboundRequestId, PendingOffer>,
    pending_transfers: HashMap<request_response::OutboundRequestId, PendingTransfer>,
    pending_handshakes: Handshakes,
    handshake_retries: HashMap<PeerId, u8>,
    outbox: HashMap<PeerId, VecDeque<(ChatEvent, u8)>>,
    pending_messages: HashMap<request_response::OutboundRequestId, PendingMessage>,
    delivery_tick: tokio::time::Interval,
    #[cfg(test)]
    withhold_acks: bool,
    #[cfg(test)]
    held_responses: Vec<request_response::ResponseChannel<ProtocolResponse>>,
    auto_accept_extensions: HashSet<String>,
    presence_status: String,
    pending_inbound_handshakes:
        HashMap<request_response::InboundRequestId, (PeerId, SessionKey, bool)>,
    sessions: HashMap<PeerId, RatchetSession>,
    decrypt_failures: HashMap<PeerId, u8>,
    shared_emoticons: HashSet<(PeerId, [u8; 32])>,
    waiting_offers: HashMap<PeerId, Vec<PendingOffer>>,
    pending_emoticons: HashMap<[u8; 32], EmoticonOffer>,
    attachment_receiver: attachments::Receiver,
    prepared_files_tx: mpsc::UnboundedSender<PreparedFile>,
    prepared_files: mpsc::UnboundedReceiver<PreparedFile>,
    vault_jobs_tx: mpsc::UnboundedSender<VaultJobDone>,
    vault_jobs: mpsc::UnboundedReceiver<VaultJobDone>,
    incoming_attachments: HashMap<(PeerId, [u8; 32]), HashSet<Option<String>>>,
    pending_inbound_offers: HashMap<u64, PendingInboundOffer>,
    inbound_offer_counter: u64,
    peer_names: HashMap<PeerId, String>,
    nudge_counter: u64,
    stdin_open: bool,
    commands_open: bool,
    contact_link_requested: bool,
    device_link_requested: bool,
    pending_pairings: HashMap<[u8; 16], PendingPairing>,
    pending_pair_targets: HashMap<PeerId, PairingLink>,
    /// Invito a codice mostrato da questo dispositivo.
    active_invite: Option<invite::ActiveInvite>,
    /// Codice d'invito inserito qui e ancora in corso.
    invite_join: Option<InviteJoin>,
    /// Schede verificate di chi ha usato un nostro codice, in attesa che
    /// l'utente accetti la richiesta.
    pending_invite_cards: HashMap<PeerId, contacts::InviteCard>,
    pending_device_requests: HashMap<request_response::OutboundRequestId, PendingDeviceRequest>,
    online_devices: HashSet<PeerId>,
    known_devices: HashMap<PeerId, DeviceEntry>,
    device_sync_tick: tokio::time::Interval,
    revoked_devices: HashSet<PeerId>,
    pending_rotations: HashMap<u64, (PeerId, AccountRotation)>,
    rotation_counter: u64,
    handshake_tick: tokio::time::Interval,
    args: ClientConfig,
    commands: mpsc::UnboundedReceiver<ClientCommand>,
    events: mpsc::UnboundedSender<ClientEvent>,
}

impl Node {
    fn on_prepared_file(&mut self, prepared: PreparedFile) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            local_peer_id,
            swarm,
            peers,
            contact_devices,
            sent_numbers,
            pending_offers,
            pending_handshakes,
            sessions,
            waiting_offers,
            attachment_receiver,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        let manifest = match prepared.manifest {
            Ok(manifest) => manifest,
            Err(message) => {
                let filename = prepared
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                report_file_failed(events, &filename, &message);
                return Ok(LoopFlow::Continue);
            }
        };
        let path = prepared.path;
        let attachment_hex = hex_asset_id(&manifest.attachment_id);
        // Niente copia nel vault: solo un riferimento al file originale,
        // che resta riapribile finché esiste.
        if let Err(error) = attachment_receiver.store_reference(&manifest, &path) {
            log::warn!("riferimento al file inviato non salvato: {error}");
        }
        match prepared.target {
            FileTarget::Peer(peer) => {
                if !contact_devices.online(peer, peers) {
                    report_file_failed(events, &manifest.filename, "contatto non collegato");
                    return Ok(LoopFlow::Continue);
                }
                let offer = PendingOffer {
                    peer,
                    path,
                    manifest,
                    retries: 0,
                    group_id: None,
                };
                if contact_devices.secure(peer, sessions) {
                    offer_file(
                        swarm,
                        sessions,
                        contact_devices,
                        sent_numbers,
                        pending_offers,
                        history,
                        events,
                        local_peer_id,
                        offer,
                    );
                } else {
                    waiting_offers.entry(peer).or_default().push(offer);
                    maybe_start_hybrid_handshake(
                        swarm,
                        pending_handshakes,
                        sessions,
                        local_peer_id,
                        peer,
                    );
                }
            }
            FileTarget::Group {
                group_id,
                parsed_id,
            } => {
                let Some(group) = history.group_chat(&group_id)? else {
                    report_file_failed(events, &manifest.filename, "chat di gruppo non trovata");
                    return Ok(LoopFlow::Continue);
                };
                let timestamp_ms = now_ms();
                let recipients =
                    match group_file_recipients(&group, local_peer_id, sessions, timestamp_ms) {
                        Ok(recipients) => recipients,
                        Err(message) => {
                            report_file_failed(events, &manifest.filename, message);
                            return Ok(LoopFlow::Continue);
                        }
                    };
                let mut offered = 0;
                for recipient in recipients {
                    if let Some(request_id) = send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        recipient,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::GroupAttachmentOffer(GroupAttachmentOffer {
                            group_id: parsed_id,
                            manifest: manifest.clone(),
                        }),
                    ) {
                        offered += 1;
                        pending_offers.insert(
                            request_id,
                            PendingOffer {
                                peer: recipient,
                                path: path.clone(),
                                manifest: manifest.clone(),
                                retries: 0,
                                group_id: Some(parsed_id),
                            },
                        );
                    } else {
                        report_file_failed(events, &manifest.filename, "canale cifrato non pronto");
                    }
                }
                // In cronologia solo se almeno un'offerta è partita.
                if offered == 0 {
                    return Ok(LoopFlow::Continue);
                }
                history.record(
                    &group_history_key(&group_id),
                    "out",
                    &format!("group-file:{local_peer_id}"),
                    &encode_attachment(&manifest),
                    timestamp_ms,
                )?;
                let _ = events.send(ClientEvent::GroupMessage {
                    message: ClientGroupMessage {
                        group_id,
                        sender_peer_id: local_peer_id.to_string(),
                        direction: "out".into(),
                        kind: "file".into(),
                        body: manifest.filename.clone(),
                        timestamp_ms,
                        emoticons: Vec::new(),
                        attachment_id: Some(attachment_hex),
                        attachment_mime: Some(manifest.mime),
                        event_id: None,
                    },
                });
            }
            FileTarget::AllPeers => {
                log::info!(
                    "offerta file: {} ({} chunk)",
                    manifest.filename,
                    manifest.chunks.len()
                );
                for peer in peers.iter() {
                    record(history, peer, "out", "file", &manifest.filename);
                    if let Some(request_id) = send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        *peer,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::AttachmentOffer(manifest.clone()),
                    ) {
                        pending_offers.insert(
                            request_id,
                            PendingOffer {
                                peer: *peer,
                                path: path.clone(),
                                manifest: manifest.clone(),
                                retries: 0,
                                group_id: None,
                            },
                        );
                    }
                }
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_vault_job(&mut self, done: VaultJobDone) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            notification_mutes,
            incoming_attachments,
            args,
            events,
            ..
        } = self;
        let destinations = incoming_attachments
            .remove(&(done.peer, done.id))
            .unwrap_or_else(|| HashSet::from([None]));
        match done.result {
            Ok(completed) => {
                log::info!("{}: file ricevuto nell'archivio cifrato", done.peer);
                let should_notify = args.notifications
                    && destinations.iter().any(|group_id| {
                        let conversation = group_id.as_ref().map_or_else(
                            || format!("peer:{}", done.peer),
                            |id| format!("group:{id}"),
                        );
                        notification_mute_allows(notification_mutes, &conversation, now_ms())
                    });
                notify(should_notify, "File ricevuto", &completed.filename);
                for group_id in destinations {
                    emit_attachment_received(events, done.peer, group_id, &completed);
                }
            }
            Err(message) => {
                let _ = events.send(ClientEvent::Error {
                    message: format!("file ricevuto non valido: {message}"),
                });
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_device_sync_tick(&mut self) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            account_key,
            history,
            local_peer_id,
            swarm,
            display_name,
            pending_device_requests,
            online_devices,
            args,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        let account_key = *account_key;
        let latest_seq = history.latest_sync_seq()?;
        let available = online_devices
            .iter()
            .copied()
            .filter(|peer| {
                history
                    .sync_cursors(&peer.to_string())
                    .map(|(_, sent_seq)| latest_seq > sent_seq)
                    .unwrap_or(true)
            })
            .filter(|peer| {
                !pending_device_requests
                    .values()
                    .any(|pending| match pending {
                        PendingDeviceRequest::Pair {
                            peer: pending_peer, ..
                        }
                        | PendingDeviceRequest::Sync { peer: pending_peer }
                        | PendingDeviceRequest::Relay {
                            peer: pending_peer, ..
                        } => pending_peer == peer,
                        PendingDeviceRequest::Rotate => false,
                    })
            })
            .collect::<Vec<_>>();
        for peer in available {
            let descriptor = local_device_descriptor(
                local_peer_id,
                display_name,
                contact_addresses(swarm, &args.relays, local_peer_id),
            );
            send_device_sync(
                swarm,
                history,
                &account_key,
                pending_device_requests,
                peer,
                descriptor,
            )?;
        }
        Ok(LoopFlow::Continue)
    }

    fn on_delivery_tick(&mut self) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            local_peer_id,
            swarm,
            contact_devices,
            sent_numbers,
            outbox,
            pending_messages,
            sessions,
            shared_emoticons,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        // La richiesta cifrata ha un timeout di 5 minuti, pensato per le
        // offerte di file che aspettano l'utente. Un messaggio senza
        // conferma entro pochi secondi viene invece reinviato subito; il
        // destinatario scarta i doppioni per id.
        let now = now_ms();
        let expired = pending_messages
            .iter()
            .filter(|(_, (_, _, _, sent_at))| {
                now.saturating_sub(*sent_at) >= MESSAGE_ACK_TIMEOUT.as_millis() as u64
            })
            .map(|(request_id, _)| *request_id)
            .collect::<Vec<_>>();
        for request_id in expired {
            let Some((msg_peer, event, attempts, _)) = pending_messages.remove(&request_id) else {
                continue;
            };
            match next_delivery_attempt(attempts) {
                Some(next) => {
                    log::info!("nessuna conferma da {msg_peer}: reinvio");
                    resend_pending(
                        swarm,
                        sessions,
                        contact_devices,
                        sent_numbers,
                        pending_messages,
                        outbox,
                        local_peer_id,
                        msg_peer,
                        event,
                        next,
                    );
                }
                None => report_undelivered(
                    events,
                    shared_emoticons,
                    msg_peer,
                    &event,
                    "nessuna conferma dal destinatario",
                ),
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_handshake_tick(&mut self) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            local_peer_id,
            account_proof,
            swarm,
            display_name,
            peers,
            contact_devices,
            sent_numbers,
            pending_handshakes,
            presence_status,
            sessions,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        // Per ogni contatto connesso senza canale cifrato: se siamo
        // l'iniziatore ritentiamo l'handshake, altrimenti ri-annunciamo
        // la presence per invitare l'altro lato ad avviarlo.
        let stalled = peers
            .iter()
            .copied()
            .filter(|peer| !sessions.contains_key(peer))
            .collect::<Vec<_>>();
        for peer in stalled {
            if should_initiate(local_peer_id, peer) {
                maybe_start_hybrid_handshake(
                    swarm,
                    pending_handshakes,
                    sessions,
                    local_peer_id,
                    peer,
                );
            } else {
                send_event(
                    swarm,
                    sessions,
                    contact_devices,
                    peer,
                    local_peer_id,
                    sent_numbers,
                    ChatEvent::Presence(presence_update(
                        display_name,
                        presence_status,
                        true,
                        account_proof,
                    )),
                );
            }
        }
        pending_handshakes.rekey_due_sessions(swarm, sessions, local_peer_id);
        Ok(LoopFlow::Continue)
    }

    fn on_reconnect_due(
        &mut self,
        next_reconnect: Option<(PeerId, Instant)>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            swarm,
            peers,
            dialing,
            reconnect_at,
            reconnect_attempts,
            known_contacts,
            online_devices,
            known_devices,
            ..
        } = self;
        let Some((peer, _)) = next_reconnect else {
            return Ok(LoopFlow::Continue);
        };
        reconnect_at.remove(&peer);
        if (known_contacts.contains(&peer) || known_devices.contains_key(&peer))
            && !peers.contains(&peer)
            && !online_devices.contains(&peer)
            && dialing.insert(peer)
        {
            if let Err(error) = swarm.dial(DialOpts::peer_id(peer).build()) {
                dialing.remove(&peer);
                log::warn!("riconnessione fallita per {peer}: {error}");
                schedule_reconnect(peer, reconnect_at, reconnect_attempts);
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_stdin_line(&mut self, line: Option<String>) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            ml_dsa_identity,
            contact_identity,
            history,
            local_peer_id,
            swarm,
            display_name,
            peers,
            dialing,
            bootstrap_fallbacks,
            known_contacts,
            contact_devices,
            fallback_planner,
            pending_dht,
            triggers,
            nudge_limits,
            sent_numbers,
            sessions,
            prepared_files_tx,
            peer_names,
            nudge_counter,
            stdin_open,
            args,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match line {
            Some(line) if line == "quit" => return Ok(LoopFlow::Stop),
            Some(line) if line == "history" => match history.latest(20) {
                Ok(entries) => {
                    for entry in entries.iter().rev() {
                        log::info!(
                            "{} {} {} {}: {}",
                            entry.timestamp_ms,
                            entry.direction,
                            entry.peer,
                            entry.kind,
                            entry.body
                        );
                    }
                }
                Err(error) => log::warn!("cronologia non disponibile: {error}"),
            },
            Some(line) if line.starts_with("contact export ") => {
                let path = Path::new(line[15..].trim());
                match contacts::export(
                    path,
                    display_name,
                    local_peer_id,
                    contact_identity,
                    ml_dsa_identity,
                    contact_addresses(swarm, &args.relays, local_peer_id).into_iter(),
                ) {
                    Ok(()) => log::info!("scheda contatto salvata in {}", path.display()),
                    Err(error) => log::warn!("esportazione contatto fallita: {error}"),
                }
            }
            Some(line) if line == "contact qr" => {
                let result = contacts::share_links(
                    display_name,
                    local_peer_id,
                    contact_identity,
                    ml_dsa_identity,
                    contact_addresses(swarm, &args.relays, local_peer_id),
                )
                .and_then(|(link, qr_link)| {
                    let qr = contacts::render_qr(&qr_link)?;
                    Ok((link, qr))
                });
                match result {
                    Ok((link, qr)) => log::info!("{qr}\n{link}"),
                    Err(error) => log::warn!("generazione QR fallita: {error}"),
                }
            }
            Some(line) if line.starts_with("contact import-link ") => {
                match contacts::import_link(line[20..].trim()) {
                    Ok((name, peer_id, addresses)) => {
                        peer_names.insert(peer_id, name.clone());
                        known_contacts.insert(peer_id);
                        log::info!("identità contatto verificata: {name} ({peer_id})");
                        connect_contact(
                            swarm,
                            peer_id,
                            addresses,
                            dialing,
                            bootstrap_fallbacks,
                            fallback_planner,
                            pending_dht,
                        );
                    }
                    Err(error) => log::warn!("link contatto rifiutato: {error}"),
                }
            }
            Some(line) if line.starts_with("contact import ") => {
                match contacts::import(Path::new(line[15..].trim())) {
                    Ok((name, peer_id, addresses)) => {
                        peer_names.insert(peer_id, name.clone());
                        known_contacts.insert(peer_id);
                        log::info!("contatto importato: {name} ({peer_id})");
                        connect_contact(
                            swarm,
                            peer_id,
                            addresses,
                            dialing,
                            bootstrap_fallbacks,
                            fallback_planner,
                            pending_dht,
                        );
                    }
                    Err(error) => log::warn!("importazione contatto fallita: {error}"),
                }
            }
            Some(line) if line == "nudge" => {
                *nudge_counter += 1;
                let now = now_ms();
                let mut id = [0; 16];
                id[..8].copy_from_slice(&now.to_be_bytes());
                id[8..].copy_from_slice(&nudge_counter.to_be_bytes());
                if peers.is_empty() {
                    log::warn!("nessun peer collegato");
                }
                for peer in peers.iter() {
                    match nudge_limits.entry(*peer).or_default().try_acquire(now) {
                        Ok(()) => {
                            send_event(
                                swarm,
                                sessions,
                                contact_devices,
                                *peer,
                                local_peer_id,
                                sent_numbers,
                                ChatEvent::Nudge(Nudge {
                                    id,
                                    intensity: 1,
                                    timestamp_ms: now,
                                }),
                            );
                            record(history, peer, "out", "nudge", "trillo");
                        }
                        Err(wait_ms) => log::warn!(
                            "trillo limitato per {peer}: riprova tra {}s",
                            wait_ms.div_ceil(1_000)
                        ),
                    }
                }
            }
            Some(line) if line.starts_with("text ") => {
                let text = line[5..].to_owned();
                let emoticons = resolve_emoticons(&text, triggers)
                    .map_err(|error| format!("trigger emoticon non valido: {error:?}"))?;
                let mut message_id = [0u8; 16];
                OsRng.fill_bytes(&mut message_id);
                let message = TextMessage {
                    text,
                    emoticons,
                    id: message_id,
                };
                for peer in peers.iter() {
                    record_text(history, peer, "out", &message);
                }
                let event = ChatEvent::Text(message);
                broadcast(
                    swarm,
                    sessions,
                    contact_devices,
                    peers,
                    local_peer_id,
                    sent_numbers,
                    event,
                );
            }
            Some(line) if line.starts_with("emote ") => {
                match parse_emote_command(&line, &args.emotes, triggers) {
                    Ok(offer) => {
                        log::info!("emoticon salvata: {}", offer.metadata.name);
                        for peer in peers.iter() {
                            record(history, peer, "out", "emote", &offer.metadata.name);
                        }
                        broadcast(
                            swarm,
                            sessions,
                            contact_devices,
                            peers,
                            local_peer_id,
                            sent_numbers,
                            ChatEvent::EmoticonOffer(offer),
                        );
                    }
                    Err(error) => log::warn!("emoticon rifiutata: {error}"),
                }
            }
            Some(line) if line.starts_with("file ") => {
                if peers.is_empty() {
                    log::warn!("nessun peer collegato");
                } else {
                    prepare_file(
                        prepared_files_tx,
                        FileTarget::AllPeers,
                        PathBuf::from(line[5..].trim()),
                    );
                }
            }
            Some(_) => log::warn!("comando non valido"),
            None => *stdin_open = false,
        }
        Ok(LoopFlow::Continue)
    }

    fn on_command(&mut self, command: Option<ClientCommand>) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            local_peer_id,
            account_proof,
            swarm,
            display_name,
            peers,
            contact_devices,
            notification_mutes,
            sent_numbers,
            #[cfg(test)]
            withhold_acks,
            auto_accept_extensions,
            presence_status,
            sessions,
            commands_open,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match command {
            Some(
                command @ (ClientCommand::CreateInvite
                | ClientCommand::CancelInvite
                | ClientCommand::JoinInvite { .. }),
            ) => return self.on_invite_command(command),
            Some(
                command @ (ClientCommand::RequestContactLink
                | ClientCommand::RequestDeviceLink
                | ClientCommand::ImportDeviceLink { .. }
                | ClientCommand::ImportContactLink { .. }),
            ) => return self.on_link_command(command),
            Some(
                command @ (ClientCommand::SendText { .. }
                | ClientCommand::SendNudge { .. }
                | ClientCommand::SendFile { .. }
                | ClientCommand::DeleteMessageForMe { .. }
                | ClientCommand::DeleteMessageForEveryone { .. }
                | ClientCommand::LoadOlderMessages { .. }),
            ) => return self.on_message_command(command),
            Some(
                command @ (ClientCommand::CreateEmoticon { .. }
                | ClientCommand::SaveEmoticon { .. }
                | ClientCommand::UpdateEmoticon { .. }
                | ClientCommand::DeleteEmoticon { .. }),
            ) => return self.on_emoticon_command(command),
            Some(
                command @ (ClientCommand::RenameContact { .. }
                | ClientCommand::ClearConversation { .. }
                | ClientCommand::DeleteContact { .. }
                | ClientCommand::AcceptContactRequest { .. }
                | ClientCommand::RejectContactRequest { .. }
                | ClientCommand::RequestPendingContactRequests),
            ) => return self.on_contact_command(command),
            Some(
                command @ (ClientCommand::CreateChatGroup { .. }
                | ClientCommand::ModerateGroup { .. }
                | ClientCommand::SendGroupText { .. }
                | ClientCommand::SendGroupFile { .. }
                | ClientCommand::LoadOlderGroupMessages { .. }
                | ClientCommand::ClearGroupConversation { .. }
                | ClientCommand::DeleteChatGroup { .. }),
            ) => return self.on_group_command(command),
            Some(
                command @ (ClientCommand::ReadAttachment { .. }
                | ClientCommand::ExportAttachment { .. }
                | ClientCommand::CancelFileTransfers { .. }
                | ClientCommand::AcceptAttachment { .. }
                | ClientCommand::RejectAttachment { .. }),
            ) => return self.on_attachment_command(command),
            Some(ClientCommand::Shutdown) => return Ok(LoopFlow::Stop),
            Some(ClientCommand::RevokeDevice { peer }) => return self.revoke_device(peer),
            Some(ClientCommand::AnswerAccountRotation { request_id, accept }) => {
                return self.answer_account_rotation(request_id, accept);
            }
            #[cfg(test)]
            Some(ClientCommand::ForgetSession { peer }) => {
                sessions.remove(&peer);
                let _ = events.send(ClientEvent::ContactStatus {
                    peer_id: peer.to_string(),
                    status: "session-forgotten".into(),
                });
            }
            #[cfg(test)]
            Some(ClientCommand::ScrambleSession { peer }) => {
                sessions.insert(
                    peer,
                    RatchetSession::new(SessionKey::random_for_test(), local_peer_id, peer),
                );
                let _ = events.send(ClientEvent::ContactStatus {
                    peer_id: peer.to_string(),
                    status: "session-scrambled".into(),
                });
            }
            #[cfg(test)]
            Some(ClientCommand::WithholdAcks { enabled }) => {
                // Le risposte trattenute restano aperte: chiuderle farebbe
                // fallire subito la richiesta invece di farla scadere.
                *withhold_acks = enabled;
            }
            Some(ClientCommand::UpdateDisplayName { name }) => {
                let name = name.trim();
                if name.is_empty() || name.len() > 64 {
                    let _ = events.send(ClientEvent::Error {
                        message: "nome non valido".into(),
                    });
                } else {
                    *display_name = name.to_owned();
                    broadcast(
                        swarm,
                        sessions,
                        contact_devices,
                        peers,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::Presence(presence_update(
                            display_name,
                            presence_status,
                            true,
                            account_proof,
                        )),
                    );
                }
            }
            Some(ClientCommand::SetNotificationMute {
                conversation,
                muted,
                until_ms,
            }) => {
                if muted {
                    notification_mutes.insert(conversation, until_ms);
                } else {
                    notification_mutes.remove(&conversation);
                }
            }
            Some(ClientCommand::SetAutoAcceptExtensions { extensions }) => {
                *auto_accept_extensions = extensions
                    .into_iter()
                    .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|ext| !ext.is_empty())
                    .collect();
            }
            Some(ClientCommand::SetPresenceStatus { status }) => {
                *presence_status = match status.as_str() {
                    "busy" | "away" => status,
                    _ => "online".to_string(),
                };
                let update = presence_update(display_name, presence_status, true, account_proof);
                for &peer in peers.iter() {
                    send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        peer,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::Presence(update.clone()),
                    );
                }
            }
            None => *commands_open = false,
        }
        Ok(LoopFlow::Continue)
    }

    fn on_swarm_event(
        &mut self,
        event: SwarmEvent<BehaviourEvent>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            ml_dsa_identity,
            contact_identity,
            history,
            local_peer_id,
            swarm,
            display_name,
            peers,
            dialing,
            mdns_dialing,
            reconnect_at,
            reconnect_attempts,
            bootstrap_fallbacks,
            known_contacts,
            contact_devices,
            infrastructure_peers,
            fallback_planner,
            pending_dht,
            triggers,
            nudge_limits,
            incoming_nudge_limits,
            sent_numbers,
            pending_offers,
            pending_transfers,
            pending_handshakes,
            handshake_retries,
            outbox,
            pending_messages,
            pending_inbound_handshakes,
            sessions,
            decrypt_failures,
            shared_emoticons,
            waiting_offers,
            pending_inbound_offers,
            peer_names,
            contact_link_requested,
            device_link_requested,
            pending_pairings,
            pending_pair_targets,
            pending_device_requests,
            online_devices,
            known_devices,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                let address = if split_peer_address(&address).is_ok() {
                    address
                } else {
                    address.with(Protocol::P2p(local_peer_id))
                };
                log::info!("ascolto: {address}");
                if *contact_link_requested {
                    let addresses = contact_addresses(swarm, &args.relays, local_peer_id);
                    if !addresses.is_empty() {
                        let links = contacts::share_links(
                            display_name,
                            local_peer_id,
                            contact_identity,
                            ml_dsa_identity,
                            addresses,
                        );
                        match links {
                            Ok((link, qr_link)) => {
                                *contact_link_requested = false;
                                let _ = events.send(ClientEvent::ContactLink { link, qr_link });
                            }
                            Err(error) => {
                                let _ = events.send(ClientEvent::Error {
                                    message: error.to_string(),
                                });
                            }
                        }
                    }
                }
                if *device_link_requested {
                    let addresses = contact_addresses(swarm, &args.relays, local_peer_id);
                    if !addresses.is_empty() {
                        let descriptor =
                            local_device_descriptor(local_peer_id, display_name, addresses);
                        match devices::create_pairing_link(descriptor, now_ms()) {
                            Ok((link, pairing)) => {
                                *device_link_requested = false;
                                pending_pairings.insert(
                                    devices::key_id(&pairing.secret),
                                    PendingPairing {
                                        secret: pairing.secret,
                                        expires_at_ms: pairing.expires_at_ms,
                                    },
                                );
                                let _ = events.send(ClientEvent::DeviceLink {
                                    qr_link: link.clone(),
                                    link,
                                    expires_at_ms: pairing.expires_at_ms,
                                });
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
                return self.on_connection_established(peer_id)
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                cause,
                ..
            } => {
                if num_established == 0 {
                    if online_devices.remove(&peer_id) {
                        send_devices(known_devices, online_devices, events);
                    }
                    let was_application_peer = peers.remove(&peer_id);
                    sessions.remove(&peer_id);
                    shared_emoticons.retain(|(peer, _)| *peer != peer_id);
                    pending_handshakes.forget(peer_id);
                    pending_inbound_handshakes
                        .retain(|_, (pending_peer, _, _)| *pending_peer != peer_id);
                    handshake_retries.remove(&peer_id);
                    decrypt_failures.remove(&peer_id);
                    if !contact_devices.online(contact_devices.canonical(peer_id), peers) {
                        for offer in take_waiting_offers(waiting_offers, contact_devices, peer_id) {
                            report_file_failed(
                                events,
                                &offer.manifest.filename,
                                "contatto disconnesso",
                            );
                        }
                    }
                    nudge_limits.remove(&peer_id);
                    incoming_nudge_limits.remove(&peer_id);
                    if was_application_peer {
                        log::info!(
                            "offline: {} ({cause:?})",
                            peer_names
                                .get(&peer_id)
                                .map_or_else(|| peer_id.to_string(), Clone::clone)
                        );
                        log::info!("offline: {peer_id} ({cause:?})");
                        // Un contatto va offline solo quando cade l'ultimo dei
                        // suoi dispositivi.
                        let contact = contact_devices.canonical(peer_id);
                        if !contact_devices.online(contact, peers) {
                            let _ = events.send(ClientEvent::ContactUpdated {
                                contact: ClientContact {
                                    peer_id: contact.to_string(),
                                    name: peer_names
                                        .get(&contact)
                                        .cloned()
                                        .unwrap_or_else(|| "Contatto".into()),
                                    online: false,
                                    secure: false,
                                    fingerprint: peer_fingerprint(contact),
                                },
                            });
                        }
                    }
                    if should_reconnect_closed_peer(
                        known_contacts.contains(&peer_id) || known_devices.contains_key(&peer_id),
                        num_established,
                    ) {
                        dialing.remove(&peer_id);
                        schedule_reconnect(peer_id, reconnect_at, reconnect_attempts);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Chat(request_response::Event::Message {
                peer,
                message,
                ..
            })) => return self.on_chat_message(peer, message),
            SwarmEvent::Behaviour(BehaviourEvent::SecureChat(
                request_response::Event::Message { peer, message, .. },
            )) => return self.on_secure_chat_message(peer, message),
            SwarmEvent::Behaviour(BehaviourEvent::SecureChat(
                request_response::Event::OutboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                },
            )) => {
                if let Some(mut pending) = pending_offers.remove(&request_id) {
                    if let Some(retries) = next_request_retry(pending.retries) {
                        pending.retries = retries;
                        let event = offer_event(&pending);
                        if let Some(next_id) = send_event(
                            swarm,
                            sessions,
                            contact_devices,
                            pending.peer,
                            local_peer_id,
                            sent_numbers,
                            event,
                        ) {
                            pending_offers.insert(next_id, pending);
                        } else {
                            report_file_failed(
                                events,
                                &pending.manifest.filename,
                                "sessione cifrata persa",
                            );
                        }
                    } else {
                        log::warn!("offerta file fallita definitivamente per {peer}: {error}");
                        report_file_failed(events, &pending.manifest.filename, &error.to_string());
                    }
                } else if let Some(mut transfer) = pending_transfers.remove(&request_id) {
                    if let Some(retries) = next_request_retry(transfer.retries) {
                        transfer.retries = retries;
                        send_current_transfer_chunk(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_transfers,
                            local_peer_id,
                            events,
                            transfer,
                        );
                    } else {
                        log::warn!(
                            "trasferimento file fallito definitivamente per {peer}: {error}"
                        );
                        report_file_failed(events, &transfer.manifest.filename, &error.to_string());
                    }
                } else if let Some((msg_peer, event, retries, _)) =
                    pending_messages.remove(&request_id)
                {
                    // Retry di un messaggio di testo fallito in transito.
                    if let Some(next) = next_delivery_attempt(retries) {
                        resend_pending(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_messages,
                            outbox,
                            local_peer_id,
                            msg_peer,
                            event,
                            next,
                        );
                    } else {
                        log::warn!("messaggio non consegnato definitivamente a {peer}: {error}");
                        report_undelivered(
                            events,
                            shared_emoticons,
                            msg_peer,
                            &event,
                            &error.to_string(),
                        );
                    }
                } else {
                    log::warn!("invio cifrato fallito per {peer}: {error}");
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::SecureChat(
                request_response::Event::InboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                },
            )) => {
                pending_inbound_offers.retain(|_, pending| pending.request_id != request_id);
                log::warn!("messaggio cifrato non ricevibile da {peer}: {error}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::SecureChat(
                request_response::Event::ResponseSent { .. },
            )) => {}
            SwarmEvent::Behaviour(BehaviourEvent::Handshake(
                request_response::Event::Message { peer, message, .. },
            )) => return self.on_handshake_message(peer, message),
            SwarmEvent::Behaviour(BehaviourEvent::Handshake(
                request_response::Event::OutboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                },
            )) => {
                pending_handshakes.pending.remove(&request_id);
                log::warn!("handshake ibrido fallito con {peer}: {error}");
                // Riprova finché il peer è connesso e non è già cifrato: evita
                // di restare bloccati su "preparazione" per un invio transitorio.
                let attempts = handshake_retries.entry(peer).or_insert(0);
                if *attempts < MAX_REQUEST_RETRIES
                    && peers.contains(&peer)
                    && !sessions.contains_key(&peer)
                {
                    *attempts += 1;
                    maybe_start_hybrid_handshake(
                        swarm,
                        pending_handshakes,
                        sessions,
                        local_peer_id,
                        peer,
                    );
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Handshake(
                request_response::Event::InboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                },
            )) => {
                pending_inbound_handshakes.remove(&request_id);
                log::warn!("richiesta handshake non valida da {peer}: {error}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Handshake(
                request_response::Event::ResponseSent {
                    peer, request_id, ..
                },
            )) => {
                if let Some((expected_peer, session_key, rekey)) =
                    pending_inbound_handshakes.remove(&request_id)
                {
                    if expected_peer == peer {
                        let session = RatchetSession::new(session_key, local_peer_id, peer);
                        if rekey && sessions.contains_key(&peer) {
                            // Si continua a cifrare con la vecchia finché
                            // l'iniziatore non usa la nuova: i messaggi già
                            // in volo restano decifrabili da entrambi.
                            pending_handshakes.stage(peer, session);
                            log::info!("sessione rinnovata (responder): {peer}");
                            return Ok(LoopFlow::Continue);
                        }
                        pending_handshakes.install(sessions, peer, session, false);
                        log::info!("handshake ibrido completato (responder): {peer}");
                        let contact = contact_devices.canonical(peer);
                        let _ = events.send(ClientEvent::ContactUpdated {
                            contact: ClientContact {
                                peer_id: contact.to_string(),
                                name: peer_names
                                    .get(&contact)
                                    .cloned()
                                    .unwrap_or_else(|| "Contatto".into()),
                                online: true,
                                secure: true,
                                fingerprint: peer_fingerprint(contact),
                            },
                        });
                        send_group_definitions_for_peer(
                            history,
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            local_peer_id,
                            peer,
                        );
                        flush_outbox(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_messages,
                            outbox,
                            local_peer_id,
                            peer,
                        );
                        for offer in take_waiting_offers(waiting_offers, contact_devices, peer) {
                            offer_file(
                                swarm,
                                sessions,
                                contact_devices,
                                sent_numbers,
                                pending_offers,
                                history,
                                events,
                                local_peer_id,
                                offer,
                            );
                        }
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Invite(request_response::Event::Message {
                peer,
                message,
                ..
            })) => return self.on_invite_message(peer, message),
            SwarmEvent::Behaviour(BehaviourEvent::Invite(
                request_response::Event::OutboundFailure { peer, error, .. },
            )) => {
                log::warn!("invito verso {peer} fallito: {error}");
                return self
                    .invite_candidate_failed(peer, "chi ha creato il codice non è raggiungibile");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                id,
                result: kad::QueryResult::GetProviders(result),
                step,
                ..
            })) if self
                .invite_join
                .as_ref()
                .is_some_and(|join| join.query == Some(id)) =>
            {
                return self.on_invite_providers(result, step.last)
            }
            SwarmEvent::Behaviour(BehaviourEvent::Devices(request_response::Event::Message {
                peer,
                message,
                ..
            })) => return self.on_device_message(peer, message),
            SwarmEvent::Behaviour(BehaviourEvent::Devices(
                request_response::Event::OutboundFailure {
                    peer,
                    request_id,
                    error,
                    ..
                },
            )) => {
                if let Some(PendingDeviceRequest::Relay {
                    target, message, ..
                }) = pending_device_requests.remove(&request_id)
                {
                    // Relay non riuscito: il messaggio torna nella coda diretta.
                    log::warn!("relay tramite {peer} fallito: {error}");
                    for offer in new_emoticon_offers(
                        &message.emoticons,
                        target,
                        shared_emoticons,
                        triggers,
                        &args.emotes,
                        events,
                    ) {
                        deliver_event(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_messages,
                            outbox,
                            pending_handshakes,
                            local_peer_id,
                            target,
                            ChatEvent::EmoticonOffer(offer),
                        );
                    }
                    deliver_event(
                        swarm,
                        sessions,
                        contact_devices,
                        sent_numbers,
                        pending_messages,
                        outbox,
                        pending_handshakes,
                        local_peer_id,
                        target,
                        ChatEvent::Text(message),
                    );
                    return Ok(LoopFlow::Continue);
                }
                let _ = events.send(ClientEvent::Error {
                    message: format!("sincronizzazione con {peer} fallita: {error}"),
                });
            }
            SwarmEvent::Behaviour(BehaviourEvent::Devices(
                request_response::Event::InboundFailure { peer, error, .. },
            )) => {
                log::warn!("sincronizzazione non ricevibile da {peer}: {error}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Devices(
                request_response::Event::ResponseSent { .. },
            )) => {}
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                ..
            })) => {
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
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                id,
                step,
                ..
            })) if step.last => {
                if let Some(peer) = pending_dht.remove(&id) {
                    let recovery = fallback_planner.after_dht(peer);
                    execute_recovery(swarm, pending_dht, fallback_planner, peer, recovery);
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Autonat(autonat::Event::StatusChanged {
                new,
                ..
            })) => {
                log::info!("raggiungibilità: {new:?}");
                log::info!("raggiungibilità (autonat): {new:?}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::RelayClient(event)) => {
                log::info!("relay client: {event:?}");
                log::info!("relay client: {event:?}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::RelayServer(event)) => {
                log::info!("relay server: {event:?}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(event)) => {
                log::info!("hole punching: {event:?}");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Ping(_)) => {}
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(discovered))) => {
                for (peer_id, address) in discovered {
                    let base = split_peer_address(&address)
                        .map(|(_, base)| base)
                        .unwrap_or(address);
                    swarm
                        .behaviour_mut()
                        .kad
                        .add_address(&peer_id, base.clone());
                    let is_device = known_devices.contains_key(&peer_id)
                        || pending_pair_targets.contains_key(&peer_id);
                    // Privacy: sulla LAN ci si connette solo ai device in
                    // pairing e ai contatti GIÀ noti. Mai auto-aggiungere o
                    // dialare uno sconosciuto trovato via mDNS: la presence
                    // scambiata lo trasformerebbe in contatto senza consenso.
                    if !args.relay_server
                        && peer_id != local_peer_id
                        && !infrastructure_peers.contains(&peer_id)
                        && (is_device || known_contacts.contains(&peer_id))
                        && if is_device {
                            !online_devices.contains(&peer_id)
                        } else {
                            !peers.contains(&peer_id)
                        }
                        && mdns_dialing.insert(peer_id)
                    {
                        dialing.insert(peer_id);
                        log::info!("peer LAN trovato: {peer_id}");
                        let address = base.with(Protocol::P2p(peer_id));
                        let options = DialOpts::peer_id(peer_id)
                            .condition(PeerCondition::Always)
                            .addresses(vec![address])
                            .build();
                        if let Err(error) = swarm.dial(options) {
                            log::warn!("dial mDNS fallito: {error}");
                        }
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Expired(_))) => {}
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    mdns_dialing.remove(&peer_id);
                    if peers.contains(&peer_id) || online_devices.contains(&peer_id) {
                        log::warn!("dial duplicato ignorato per peer già connesso: {peer_id}");
                    } else if let Some(address) = bootstrap_fallbacks
                        .get_mut(&peer_id)
                        .and_then(VecDeque::pop_front)
                    {
                        log::warn!("bootstrap fallito, provo il prossimo indirizzo: {error}");
                        if let Err(next_error) = swarm.dial(address) {
                            log::warn!("indirizzo non raggiungibile: {next_error}");
                        }
                    } else {
                        dialing.remove(&peer_id);
                        log::warn!("connessione fallita: {error}");
                        log::warn!("connessione fallita: {error}");
                        if pending_pair_targets.contains_key(&peer_id) {
                            let _ = events.send(ClientEvent::Error {
                                    message: "l'altro dispositivo non è raggiungibile; verifica che sia online e che il codice non sia scaduto".into(),
                                });
                        }
                        if known_contacts.contains(&peer_id) || known_devices.contains_key(&peer_id)
                        {
                            let recovery = fallback_planner.after_failure(peer_id);
                            execute_recovery(
                                swarm,
                                pending_dht,
                                fallback_planner,
                                peer_id,
                                recovery,
                            );
                            schedule_reconnect(peer_id, reconnect_at, reconnect_attempts);
                        }
                    }
                } else {
                    log::warn!("connessione fallita: {error}");
                }
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_connection_established(&mut self, peer_id: PeerId) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            account_key,
            history,
            local_peer_id,
            account_proof,
            swarm,
            relay_addresses,
            display_name,
            peers,
            dialing,
            mdns_dialing,
            reconnect_at,
            reconnect_attempts,
            bootstrap_fallbacks,
            known_contacts,
            contact_devices,
            infrastructure_peers,
            accept_unclassified_manual,
            fallback_planner,
            requested_relay_reservations,
            sent_numbers,
            pending_handshakes,
            handshake_retries,
            presence_status,
            sessions,
            peer_names,
            pending_pair_targets,
            pending_device_requests,
            online_devices,
            known_devices,
            active_invite,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        let account_key = *account_key;
        dialing.remove(&peer_id);
        mdns_dialing.remove(&peer_id);
        bootstrap_fallbacks.remove(&peer_id);
        reconnect_at.remove(&peer_id);
        reconnect_attempts.remove(&peer_id);
        fallback_planner.connected(peer_id);
        // Un invito creato prima di raggiungere il mininodo non era arrivato
        // nella DHT: lo ripubblichiamo appena c'è.
        if infrastructure_peers.contains(&peer_id) {
            if let Some(invite) = active_invite.as_ref() {
                let key = kad::RecordKey::new(&invite.code.rendezvous_key());
                if let Err(error) = swarm.behaviour_mut().kad.start_providing(key) {
                    log::warn!("invito non pubblicato: {error}");
                }
            }
        }
        if let Some(address) = relay_addresses.get(&peer_id) {
            if requested_relay_reservations.insert(peer_id) {
                if let Err(error) = swarm.listen_on(address.clone().with(Protocol::P2pCircuit)) {
                    log::warn!("prenotazione relay fallita: {error}");
                    log::warn!("prenotazione relay fallita: {error}");
                }
            }
        }
        let device_request_pending =
            pending_device_requests
                .values()
                .any(|pending| match pending {
                    PendingDeviceRequest::Pair { peer, .. }
                    | PendingDeviceRequest::Sync { peer }
                    | PendingDeviceRequest::Relay { peer, .. } => *peer == peer_id,
                    PendingDeviceRequest::Rotate => false,
                });
        if let Some(pairing) = pending_pair_targets
            .get(&peer_id)
            .filter(|_| !device_request_pending)
        {
            let descriptor = local_device_descriptor(
                local_peer_id,
                display_name,
                contact_addresses(swarm, &args.relays, local_peer_id),
            );
            // Chi si collega non porta dati suoi: riceve quelli dell'account.
            let request = DeviceRequest::Pair {
                device: descriptor,
                records: Vec::new(),
            };
            let sealed = devices::seal(&pairing.secret, &request)?;
            let request_id = swarm.behaviour_mut().devices.send_request(&peer_id, sealed);
            pending_device_requests.insert(
                request_id,
                PendingDeviceRequest::Pair {
                    peer: peer_id,
                    key: pairing.secret,
                },
            );
        } else if known_devices.contains_key(&peer_id) && !device_request_pending {
            online_devices.insert(peer_id);
            if let Some(device) = known_devices.get_mut(&peer_id) {
                device.last_seen_ms = now_ms();
                history.save_device(device)?;
            }
            send_devices(known_devices, online_devices, events);
            let descriptor = local_device_descriptor(
                local_peer_id,
                display_name,
                contact_addresses(swarm, &args.relays, local_peer_id),
            );
            send_device_sync(
                swarm,
                history,
                &account_key,
                pending_device_requests,
                peer_id,
                descriptor,
            )?;
        }
        let is_manual = *accept_unclassified_manual && !infrastructure_peers.contains(&peer_id);
        if is_manual {
            *accept_unclassified_manual = false;
            known_contacts.insert(peer_id);
        }
        if (known_contacts.contains(&peer_id) || is_manual) && peers.insert(peer_id) {
            log::info!("connesso: {peer_id}");
            log::info!("connesso: {peer_id}");
            // La UI conosce il contatto, non il suo singolo dispositivo.
            let contact = contact_devices.canonical(peer_id);
            let _ = events.send(ClientEvent::ContactUpdated {
                contact: ClientContact {
                    peer_id: contact.to_string(),
                    name: peer_names
                        .get(&contact)
                        .cloned()
                        .unwrap_or_else(|| "Nuovo contatto".into()),
                    online: true,
                    secure: contact_devices.secure(contact, sessions),
                    fingerprint: peer_fingerprint(contact),
                },
            });
            send_event(
                swarm,
                sessions,
                contact_devices,
                peer_id,
                local_peer_id,
                sent_numbers,
                ChatEvent::Presence(presence_update(
                    display_name,
                    presence_status,
                    true,
                    account_proof,
                )),
            );
            // Avvia subito l'handshake appena la connessione è su, senza
            // attendere il round-trip della presence (che può perdersi).
            handshake_retries.remove(&peer_id);
            maybe_start_hybrid_handshake(
                swarm,
                pending_handshakes,
                sessions,
                local_peer_id,
                peer_id,
            );
        }
        Ok(LoopFlow::Continue)
    }

    fn on_chat_message(
        &mut self,
        peer: PeerId,
        message: request_response::Message<Envelope, ProtocolResponse>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            ignored_contacts,
            local_peer_id,
            account_proof,
            swarm,
            display_name,
            peers,
            pending_contact_requests,
            pending_accounts,
            known_contacts,
            contact_devices,
            infrastructure_peers,
            incoming_nudge_limits,
            notification_mutes,
            sent_numbers,
            pending_handshakes,
            presence_status,
            sessions,
            pending_emoticons,
            attachment_receiver,
            vault_jobs_tx,
            incoming_attachments,
            peer_names,
            known_devices,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                // Riconosci il mittente PRIMA di decidere se è uno
                // sconosciuto: un dispositivo che dimostra di appartenere
                // all'account di un contatto già approvato non lo è.
                let mut source_account = None;
                if let ChatEvent::Presence(presence) = &request.event {
                    let source = recognize_presence(
                        contact_devices,
                        known_contacts,
                        &history.contacts()?,
                        peer,
                        presence,
                    );
                    source_account = source.account.clone();
                    apply_presence_source(
                        &source,
                        peer,
                        &mut PresenceBinding {
                            history,
                            events,
                            known_contacts,
                            ignored_contacts,
                            peer_names,
                            pending_contact_requests,
                        },
                    )?;
                }
                // Cronologia e UI seguono sempre il contatto, non il
                // singolo dispositivo che ha aperto la connessione.
                let contact = contact_devices.canonical(peer);
                let validation =
                    if ignored_contacts.contains(&peer) || ignored_contacts.contains(&contact) {
                        Err("contatto rimosso")
                    } else if matches!(&request.event, ChatEvent::Presence(_)) {
                        validate_envelope(peer, local_peer_id, &request)
                    } else {
                        Err("usa il protocollo chat cifrato")
                    };
                let (response, valid) = match validation {
                    Ok(()) => (
                        receive_event(
                            contact,
                            &request.event,
                            &mut Incoming {
                                pending_emoticons,
                                events,
                                nudge_limits: incoming_nudge_limits,
                                attachments: attachment_receiver,
                                history,
                                notifications: args.notifications,
                                notification_mutes,
                                peer_names,
                                local_peer_id,
                                incoming_attachments,
                                vault_jobs: vault_jobs_tx,
                            },
                        ),
                        true,
                    ),
                    Err(error) => (ProtocolResponse::Rejected(error.into()), false),
                };
                swarm
                    .behaviour_mut()
                    .chat
                    .send_response(channel, response)
                    .ok();
                if valid {
                    let is_known_peer = known_contacts.contains(&peer)
                        || known_devices.contains_key(&peer)
                        || infrastructure_peers.contains(&peer);
                    if !is_known_peer {
                        // Presence non sollecitata da uno sconosciuto: niente
                        // auto-aggiunta. Chiedi conferma all'utente una volta.
                        if let ChatEvent::Presence(presence) = &request.event {
                            // Tetto alle richieste in attesa: uno spam di
                            // peer nuovi non riempie memoria e UI.
                            let accepting = !ignored_contacts.contains(&peer)
                                && (pending_contact_requests.contains_key(&peer)
                                    || pending_contact_requests.len()
                                        < MAX_PENDING_CONTACT_REQUESTS);
                            if !accepting && !ignored_contacts.contains(&peer) {
                                log::warn!(
                                    "richiesta di contatto da {peer} ignorata: troppe in attesa"
                                );
                            }
                            if accepting {
                                if let Some(account) = &source_account {
                                    pending_accounts.insert(peer, account.clone());
                                }
                            }
                            if accepting
                                && pending_contact_requests
                                    .insert(peer, presence.display_name.clone())
                                    .is_none()
                            {
                                peer_names.insert(peer, presence.display_name.clone());
                                let _ = events.send(ClientEvent::ContactRequest {
                                    peer_id: peer.to_string(),
                                    name: presence.display_name.clone(),
                                    fingerprint: peer_fingerprint(peer),
                                    via_invite: false,
                                });
                            }
                        }
                    } else {
                        if let ChatEvent::Presence(presence) = &request.event {
                            // Un nostro dispositivo collegato non è un
                            // contatto: non deve finire in rubrica.
                            if !known_devices.contains_key(&peer) {
                                history.ensure_contact(
                                    &contact.to_string(),
                                    &presence.display_name,
                                    now_ms(),
                                )?;
                            }
                        }
                        if let Some(event) = client_event_from_chat(
                            contact,
                            &request.event,
                            "in",
                            contact_devices.secure(contact, sessions),
                            peer_names.get(&contact).map(String::as_str),
                        ) {
                            let _ = events.send(event);
                        }
                        emit_contact_status(events, contact, &request.event);
                        if should_classify_application_peer(
                            true,
                            infrastructure_peers.contains(&peer),
                            peers.contains(&peer),
                        ) {
                            known_contacts.insert(peer);
                            peers.insert(peer);
                            log::info!("connesso: {peer}");
                            send_event(
                                swarm,
                                sessions,
                                contact_devices,
                                peer,
                                local_peer_id,
                                sent_numbers,
                                ChatEvent::Presence(presence_update(
                                    display_name,
                                    presence_status,
                                    true,
                                    account_proof,
                                )),
                            );
                        }
                        if event_authorizes_handshake(&request.event) {
                            // Una presence IN CHIARO da un contatto con cui
                            // crediamo di avere una sessione significa che
                            // l'altro lato l'ha persa (un restart, altrimenti
                            // userebbe il canale cifrato). La nostra è stale:
                            // la scartiamo così l'handshake riparte pulito
                            // (l'iniziatore reinvia l'hello; il responder
                            // accetta e sostituisce la sessione). Auto-heal
                            // del caso "connesso ma bloccato su preparazione".
                            if known_contacts.contains(&peer) && sessions.remove(&peer).is_some() {
                                log::info!(
                                    "sessione stale scartata per {peer}: presence in chiaro"
                                );
                            }
                            maybe_start_hybrid_handshake(
                                swarm,
                                pending_handshakes,
                                sessions,
                                local_peer_id,
                                peer,
                            );
                        }
                    }
                }
            }
            request_response::Message::Response { .. } => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_secure_chat_message(
        &mut self,
        peer: PeerId,
        message: request_response::Message<RatchetMessage, ProtocolResponse>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            ignored_contacts,
            local_peer_id,
            account_proof,
            swarm,
            display_name,
            pending_contact_requests,
            known_contacts,
            contact_devices,
            incoming_nudge_limits,
            notification_mutes,
            sent_numbers,
            pending_offers,
            pending_transfers,
            pending_handshakes,
            outbox,
            pending_messages,
            #[cfg(test)]
            withhold_acks,
            #[cfg(test)]
            held_responses,
            auto_accept_extensions,
            presence_status,
            sessions,
            decrypt_failures,
            shared_emoticons,
            waiting_offers,
            pending_emoticons,
            attachment_receiver,
            vault_jobs_tx,
            incoming_attachments,
            pending_inbound_offers,
            inbound_offer_counter,
            peer_names,
            known_devices,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match message {
            request_response::Message::Request {
                request_id,
                request,
                channel,
            } => {
                // Il canale è per dispositivo, la conversazione è per
                // contatto: cronologia e UI seguono il contatto.
                let contact = contact_devices.canonical(peer);
                let envelope =
                    if ignored_contacts.contains(&peer) || ignored_contacts.contains(&contact) {
                        Err("contatto rimosso")
                    } else {
                        pending_handshakes.decrypt(sessions, peer, local_peer_id, &request)
                    };
                match &envelope {
                    Ok(_) => {
                        decrypt_failures.remove(&peer);
                    }
                    Err(reason) if *reason == REJECT_UNDECRYPTABLE => {
                        let failures = decrypt_failures.entry(peer).or_insert(0);
                        *failures += 1;
                        if *failures >= MAX_DECRYPT_FAILURES {
                            decrypt_failures.remove(&peer);
                            log::warn!("sessione con {peer} non decifrabile: nuovo handshake");
                            sessions.remove(&peer);
                            if should_initiate(local_peer_id, peer) {
                                maybe_start_hybrid_handshake(
                                    swarm,
                                    pending_handshakes,
                                    sessions,
                                    local_peer_id,
                                    peer,
                                );
                            } else {
                                // Senza sessione la presence parte in chiaro:
                                // per l'iniziatore è il segnale di rifare
                                // l'handshake.
                                send_event(
                                    swarm,
                                    sessions,
                                    contact_devices,
                                    peer,
                                    local_peer_id,
                                    sent_numbers,
                                    ChatEvent::Presence(presence_update(
                                        display_name,
                                        presence_status,
                                        true,
                                        account_proof,
                                    )),
                                );
                            }
                        }
                    }
                    Err(_) => {}
                }
                let response = match envelope {
                    Ok(envelope) => match validate_envelope(peer, local_peer_id, &envelope) {
                        Ok(()) => {
                            match incoming_offer_details(
                                contact,
                                local_peer_id,
                                &envelope.event,
                                history,
                            ) {
                                Ok(Some((manifest, group_id))) => {
                                    if offer_auto_accepted(&manifest, auto_accept_extensions) {
                                        // Estensione consentita e file piccolo: accetta senza chiedere.
                                        let client_event = client_event_from_chat(
                                            contact,
                                            &envelope.event,
                                            "in",
                                            true,
                                            peer_names.get(&contact).map(String::as_str),
                                        );
                                        let resp = receive_event(
                                            contact,
                                            &envelope.event,
                                            &mut Incoming {
                                                pending_emoticons,
                                                events,
                                                nudge_limits: incoming_nudge_limits,
                                                attachments: attachment_receiver,
                                                history,
                                                notifications: args.notifications,
                                                notification_mutes,
                                                peer_names,
                                                local_peer_id,
                                                incoming_attachments,
                                                vault_jobs: vault_jobs_tx,
                                            },
                                        );
                                        if !matches!(&resp, ProtocolResponse::Rejected(_)) {
                                            if let Some(event) = client_event {
                                                let _ = events.send(event);
                                            }
                                        }
                                        swarm
                                            .behaviour_mut()
                                            .secure_chat
                                            .send_response(channel, resp)
                                            .ok();
                                        return Ok(LoopFlow::Continue);
                                    }
                                    if pending_inbound_offers.len() >= MAX_PENDING_INBOUND_OFFERS {
                                        swarm
                                            .behaviour_mut()
                                            .secure_chat
                                            .send_response(
                                                channel,
                                                ProtocolResponse::Rejected(
                                                    "troppe offerte file in attesa".into(),
                                                ),
                                            )
                                            .ok();
                                        return Ok(LoopFlow::Continue);
                                    }
                                    *inbound_offer_counter =
                                        inbound_offer_counter.checked_add(1).unwrap_or(1);
                                    let offer_id = *inbound_offer_counter;
                                    let _ = events.send(ClientEvent::IncomingAttachmentOffered {
                                        offer_id,
                                        peer_id: contact.to_string(),
                                        filename: manifest.filename.clone(),
                                        mime: manifest.mime.clone(),
                                        size: manifest.size,
                                        group_id,
                                    });
                                    pending_inbound_offers.insert(
                                        offer_id,
                                        PendingInboundOffer {
                                            // Il trasferimento appartiene alla
                                            // conversazione del contatto, non al
                                            // dispositivo che l'ha aperto.
                                            peer: contact,
                                            request_id,
                                            event: envelope.event,
                                            channel,
                                        },
                                    );
                                    return Ok(LoopFlow::Continue);
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    swarm
                                        .behaviour_mut()
                                        .secure_chat
                                        .send_response(channel, ProtocolResponse::Rejected(error))
                                        .ok();
                                    return Ok(LoopFlow::Continue);
                                }
                            }
                            // Annullato un file ancora in attesa di risposta:
                            // chiudi l'offerta invece di lasciarla aperta.
                            if let ChatEvent::AttachmentCancelled(cancel) = &envelope.event {
                                let offers = pending_inbound_offers
                                    .iter()
                                    .filter(|(_, pending)| {
                                        pending.peer == contact
                                            && offered_attachment_id(&pending.event)
                                                == Some(cancel.attachment_id)
                                    })
                                    .map(|(offer_id, _)| *offer_id)
                                    .collect::<Vec<_>>();
                                for offer_id in offers {
                                    if let Some(pending) = pending_inbound_offers.remove(&offer_id)
                                    {
                                        swarm
                                            .behaviour_mut()
                                            .secure_chat
                                            .send_response(
                                                pending.channel,
                                                ProtocolResponse::Rejected(
                                                    "invio annullato dal mittente".into(),
                                                ),
                                            )
                                            .ok();
                                        let _ =
                                            events.send(ClientEvent::IncomingAttachmentCancelled {
                                                peer_id: contact.to_string(),
                                                attachment_id: hex_asset_id(&cancel.attachment_id),
                                                offer_id: Some(offer_id),
                                            });
                                    }
                                }
                            }
                            let client_event = client_event_from_chat(
                                contact,
                                &envelope.event,
                                "in",
                                true,
                                peer_names.get(&contact).map(String::as_str),
                            );
                            if let ChatEvent::Presence(presence) = &envelope.event {
                                // Aggiorna anche qui il legame con l'account:
                                // la prova viaggia su entrambi i canali.
                                let source = recognize_presence(
                                    contact_devices,
                                    known_contacts,
                                    &history.contacts()?,
                                    peer,
                                    presence,
                                );
                                apply_presence_source(
                                    &source,
                                    peer,
                                    &mut PresenceBinding {
                                        history,
                                        events,
                                        known_contacts,
                                        ignored_contacts,
                                        peer_names,
                                        pending_contact_requests,
                                    },
                                )?;
                                // Difesa: aggiorna il contatto solo se già
                                // noto; mai auto-aggiungere via canale cifrato.
                                // Un nostro dispositivo collegato non è un
                                // contatto e non va in rubrica.
                                if known_contacts.contains(&peer)
                                    && !known_devices.contains_key(&peer)
                                {
                                    history.ensure_contact(
                                        &contact.to_string(),
                                        &presence.display_name,
                                        now_ms(),
                                    )?;
                                }
                            }
                            // Un reinvio dopo un timeout può far arrivare due volte
                            // lo stesso messaggio: confermalo senza mostrarlo di nuovo.
                            if already_received(history, &envelope.event) {
                                #[cfg(test)]
                                let _ = events.send(ClientEvent::ContactStatus {
                                    peer_id: contact.to_string(),
                                    status: "duplicate-ignored".into(),
                                });
                                swarm
                                    .behaviour_mut()
                                    .secure_chat
                                    .send_response(channel, ProtocolResponse::Ack)
                                    .ok();
                                return Ok(LoopFlow::Continue);
                            }
                            let response = receive_event(
                                contact,
                                &envelope.event,
                                &mut Incoming {
                                    pending_emoticons,
                                    events,
                                    nudge_limits: incoming_nudge_limits,
                                    attachments: attachment_receiver,
                                    history,
                                    notifications: args.notifications,
                                    notification_mutes,
                                    peer_names,
                                    local_peer_id,
                                    incoming_attachments,
                                    vault_jobs: vault_jobs_tx,
                                },
                            );
                            if let Some(event) = client_event {
                                let _ = events.send(event);
                            }
                            emit_contact_status(events, contact, &envelope.event);
                            response
                        }
                        Err(error) => ProtocolResponse::Rejected(error.into()),
                    },
                    Err(error) => ProtocolResponse::Rejected(error.into()),
                };
                #[cfg(test)]
                if *withhold_acks {
                    held_responses.push(channel);
                    return Ok(LoopFlow::Continue);
                }
                swarm
                    .behaviour_mut()
                    .secure_chat
                    .send_response(channel, response)
                    .ok();
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                if let Some(pending) = pending_offers.remove(&request_id) {
                    match response {
                        ProtocolResponse::MissingChunks(indices) => {
                            match validate_missing_chunks(indices, pending.manifest.chunks.len()) {
                                Ok(remaining) => {
                                    log::info!(
                                        "invio {}: {} chunk richiesti",
                                        pending.manifest.filename,
                                        remaining.len()
                                    );
                                    let total = remaining.len();
                                    let _ = events.send(ClientEvent::AttachmentProgress {
                                        filename: pending.manifest.filename.clone(),
                                        completed_chunks: 0,
                                        total_chunks: total,
                                    });
                                    send_next_transfer_chunk(
                                        swarm,
                                        sessions,
                                        contact_devices,
                                        sent_numbers,
                                        pending_transfers,
                                        local_peer_id,
                                        events,
                                        PendingTransfer {
                                            peer: pending.peer,
                                            path: pending.path,
                                            manifest: pending.manifest,
                                            group_id: pending.group_id,
                                            remaining,
                                            current: 0,
                                            retries: 0,
                                            completed: 0,
                                            total,
                                        },
                                    );
                                }
                                Err(error) => {
                                    report_file_failed(
                                        events,
                                        &pending.manifest.filename,
                                        &format!("richiesta chunk non valida: {error}"),
                                    );
                                }
                            }
                        }
                        // Come per i messaggi: un'offerta cifrata con una
                        // sessione che il destinatario non ha più va
                        // riproposta dopo un nuovo handshake, non persa.
                        ProtocolResponse::Rejected(reason)
                            if is_session_rejection(&reason)
                                && pending.retries < MAX_DELIVERY_ATTEMPTS =>
                        {
                            log::info!("offerta file rifiutata da {peer} ({reason}): nuova sessione e reinvio");
                            sessions.remove(&peer);
                            let mut offer = pending;
                            offer.retries += 1;
                            waiting_offers.entry(offer.peer).or_default().push(offer);
                            maybe_start_hybrid_handshake(
                                swarm,
                                pending_handshakes,
                                sessions,
                                local_peer_id,
                                peer,
                            );
                        }
                        ProtocolResponse::Rejected(error) => {
                            report_file_failed(
                                events,
                                &pending.manifest.filename,
                                &format!("file rifiutato: {error}"),
                            );
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
                        ProtocolResponse::Ack => {
                            let mut transfer = transfer;
                            transfer.completed += 1;
                            let _ = events.send(ClientEvent::AttachmentProgress {
                                filename: transfer.manifest.filename.clone(),
                                completed_chunks: transfer.completed,
                                total_chunks: transfer.total,
                            });
                            send_next_transfer_chunk(
                                swarm,
                                sessions,
                                contact_devices,
                                sent_numbers,
                                pending_transfers,
                                local_peer_id,
                                events,
                                transfer,
                            )
                        }
                        ProtocolResponse::Rejected(error) => {
                            report_file_failed(
                                events,
                                &transfer.manifest.filename,
                                &format!("trasferimento rifiutato: {error}"),
                            );
                        }
                        ProtocolResponse::MissingChunks(_) => {
                            report_file_failed(
                                events,
                                &transfer.manifest.filename,
                                "risposta non valida durante il trasferimento",
                            );
                        }
                    }
                } else if let Some((msg_peer, event, attempts, _)) =
                    pending_messages.remove(&request_id)
                {
                    match response {
                        // Consegnato: smetti di tracciarlo.
                        ProtocolResponse::Ack => {}
                        // Il destinatario non riesce a decifrare (sessione
                        // persa o sostituita da un nuovo handshake): la
                        // nostra è stale. La scartiamo, rimettiamo il
                        // messaggio in testa alla coda e rifacciamo
                        // l'handshake; parte appena il canale è pronto.
                        ProtocolResponse::Rejected(reason)
                            if is_session_rejection(&reason)
                                && attempts < MAX_DELIVERY_ATTEMPTS =>
                        {
                            log::info!("messaggio rifiutato da {peer} ({reason}): nuova sessione e reinvio");
                            sessions.remove(&peer);
                            requeue_in_order(
                                outbox.entry(msg_peer).or_default(),
                                event,
                                attempts + 1,
                            );
                            maybe_start_hybrid_handshake(
                                swarm,
                                pending_handshakes,
                                sessions,
                                local_peer_id,
                                peer,
                            );
                        }
                        ProtocolResponse::Rejected(reason) => {
                            report_undelivered(events, shared_emoticons, msg_peer, &event, &reason);
                        }
                        ProtocolResponse::MissingChunks(_) => {
                            report_undelivered(
                                events,
                                shared_emoticons,
                                msg_peer,
                                &event,
                                "risposta non valida",
                            );
                        }
                    }
                }
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_handshake_message(
        &mut self,
        peer: PeerId,
        message: request_response::Message<crypto::HybridClientHello, HybridResponse>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            display_name,
            presence_status,
            account_proof,
            ignored_contacts,
            local_peer_id,
            swarm,
            peers,
            known_contacts,
            contact_devices,
            infrastructure_peers,
            sent_numbers,
            pending_offers,
            pending_handshakes,
            handshake_retries,
            outbox,
            pending_messages,
            pending_inbound_handshakes,
            sessions,
            waiting_offers,
            peer_names,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match message {
            request_response::Message::Request {
                request_id,
                request,
                channel,
            } => {
                let authorized = handshake_peer_authorized(
                    known_contacts.contains(&peer),
                    peers.contains(&peer),
                    swarm.is_connected(&peer)
                        && !infrastructure_peers.contains(&peer)
                        && !ignored_contacts.contains(&peer),
                );
                let (response, pending_key) =
                    if !accepts_inbound(local_peer_id, peer) || !authorized {
                        (
                            HybridResponse::Rejected("handshake non autorizzato".into()),
                            None,
                        )
                    } else {
                        match respond_hybrid(
                            &request,
                            peer,
                            local_peer_id,
                            &pending_handshakes.identity,
                        )
                        .map_err(|error| error.to_string())
                        .and_then(|(hello, handshaked)| {
                            pin_pq_identity(history, peer, handshaked.peer_ml_dsa_key.as_deref())?;
                            Ok((hello, handshaked.key))
                        }) {
                            Ok((hello, session_key)) => {
                                (HybridResponse::Accepted(hello), Some(session_key))
                            }
                            Err(error) => {
                                log::warn!("handshake da {peer} rifiutato: {error}");
                                (HybridResponse::Rejected(error), None)
                            }
                        }
                    };
                if swarm
                    .behaviour_mut()
                    .handshake
                    .send_response(channel, response)
                    .is_ok()
                {
                    if let Some(session_key) = pending_key {
                        pending_inbound_handshakes
                            .insert(request_id, (peer, session_key, request.rekey));
                    }
                } else {
                    log::warn!("risposta handshake non consegnata a {peer}");
                }
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                let Some((expected_peer, initiator)) =
                    pending_handshakes.pending.remove(&request_id)
                else {
                    return Ok(LoopFlow::Continue);
                };
                if expected_peer != peer {
                    log::warn!("risposta handshake ricevuta dal peer errato");
                    return Ok(LoopFlow::Continue);
                }
                match response {
                    HybridResponse::Accepted(hello) => {
                        // Il responder tiene la sessione nuova in attesa
                        // finché non la vede usata: se l'hello era un
                        // rinnovo, gli si manda subito un messaggio.
                        let rekey = initiator.rekey();
                        match initiator
                            .finish(&hello)
                            .map_err(|error| error.to_string())
                            .and_then(|handshaked| {
                                pin_pq_identity(
                                    history,
                                    peer,
                                    handshaked.peer_ml_dsa_key.as_deref(),
                                )?;
                                Ok(handshaked.key)
                            }) {
                            Ok(session_key) => {
                                pending_handshakes.install(
                                    sessions,
                                    peer,
                                    RatchetSession::new(session_key, local_peer_id, peer),
                                    rekey,
                                );
                                handshake_retries.remove(&peer);
                                log::info!("handshake ibrido completato: {peer}");
                                if rekey {
                                    send_event(
                                        swarm,
                                        sessions,
                                        contact_devices,
                                        peer,
                                        local_peer_id,
                                        sent_numbers,
                                        ChatEvent::Presence(presence_update(
                                            display_name,
                                            presence_status,
                                            true,
                                            account_proof,
                                        )),
                                    );
                                }
                                let contact = contact_devices.canonical(peer);
                                let _ = events.send(ClientEvent::ContactUpdated {
                                    contact: ClientContact {
                                        peer_id: contact.to_string(),
                                        name: peer_names
                                            .get(&contact)
                                            .cloned()
                                            .unwrap_or_else(|| "Contatto".into()),
                                        online: true,
                                        secure: true,
                                        fingerprint: peer_fingerprint(contact),
                                    },
                                });
                                send_group_definitions_for_peer(
                                    history,
                                    swarm,
                                    sessions,
                                    contact_devices,
                                    sent_numbers,
                                    local_peer_id,
                                    peer,
                                );
                                flush_outbox(
                                    swarm,
                                    sessions,
                                    contact_devices,
                                    sent_numbers,
                                    pending_messages,
                                    outbox,
                                    local_peer_id,
                                    peer,
                                );
                                for offer in
                                    take_waiting_offers(waiting_offers, contact_devices, peer)
                                {
                                    offer_file(
                                        swarm,
                                        sessions,
                                        contact_devices,
                                        sent_numbers,
                                        pending_offers,
                                        history,
                                        events,
                                        local_peer_id,
                                        offer,
                                    );
                                }
                            }
                            Err(error) => {
                                log::warn!("handshake ibrido fallito con {peer}: {error}")
                            }
                        }
                    }
                    HybridResponse::Rejected(error) => {
                        log::warn!("handshake ibrido rifiutato da {peer}: {error:?}");
                    }
                }
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_device_message(
        &mut self,
        peer: PeerId,
        message: request_response::Message<SealedDeviceMessage, SealedDeviceMessage>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            revoked_devices,
            account_key,
            history,
            ignored_contacts,
            local_peer_id,
            account_identity,
            account_proof,
            swarm,
            display_name,
            peers,
            dialing,
            bootstrap_fallbacks,
            known_contacts,
            contact_devices,
            fallback_planner,
            pending_dht,
            triggers,
            sent_numbers,
            pending_handshakes,
            outbox,
            pending_messages,
            presence_status,
            sessions,
            shared_emoticons,
            peer_names,
            pending_pairings,
            pending_pair_targets,
            pending_device_requests,
            online_devices,
            known_devices,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                if revoked_devices.contains(&peer)
                    && !pending_pairings.contains_key(&request.key_id)
                {
                    let response = devices::seal(
                        &[0u8; 32],
                        &DeviceResponse::Rejected("dispositivo revocato".into()),
                    )?;
                    swarm
                        .behaviour_mut()
                        .devices
                        .send_response(channel, response)
                        .ok();
                    return Ok(LoopFlow::Continue);
                }
                pending_pairings.retain(|_, pairing| pairing.expires_at_ms >= now_ms());
                // Sigillata con una chiave di account ritirata: il mittente è
                // un nostro dispositivo rimasto indietro e riceve la nuova.
                if request.key_id != devices::key_id(account_key)
                    && !pending_pairings.contains_key(&request.key_id)
                {
                    if let Some(retired) = history.retired_account_key(&request.key_id)? {
                        let reply = match devices::open::<DeviceRequest>(&retired, &request) {
                            Ok(DeviceRequest::Sync { device, .. })
                                if device.peer_id == peer.to_string()
                                    && known_devices.contains_key(&peer) =>
                            {
                                DeviceResponse::RotationRequired {
                                    rotation: AccountRotation {
                                        account_key: *account_key,
                                        revoked: revoked_list(history)?,
                                        from_name: display_name.clone(),
                                    },
                                }
                            }
                            _ => {
                                DeviceResponse::Rejected("chiave di account non più valida".into())
                            }
                        };
                        let response = devices::seal(&retired, &reply)?;
                        swarm
                            .behaviour_mut()
                            .devices
                            .send_response(channel, response)
                            .ok();
                        return Ok(LoopFlow::Continue);
                    }
                }
                let pairing_key = pending_pairings
                    .get(&request.key_id)
                    .map(|pairing| pairing.secret);
                let selected_key = if request.key_id == devices::key_id(account_key) {
                    Some(*account_key)
                } else {
                    pairing_key
                };
                let response = match selected_key {
                    Some(key) => match devices::open::<DeviceRequest>(&key, &request) {
                        Ok(DeviceRequest::Pair { device, records })
                            if pairing_key.is_some()
                                && device.peer_id == peer.to_string()
                                && (!records.is_empty()
                                    || history
                                        .contacts()?
                                        .iter()
                                        .any(|contact| contact.peer == peer.to_string())) =>
                        {
                            // Un contatto, o un dispositivo che porta chat di
                            // un altro account, non entra: i due account si
                            // mescolerebbero. Il codice resta valido.
                            let reason = if records.is_empty() {
                                PAIRING_WITH_CONTACT
                            } else {
                                PAIRING_WITH_LOCAL_DATA
                            };
                            let _ = events.send(ClientEvent::Error {
                                message: format!(
                                    "collegamento di {} rifiutato: {reason}",
                                    device.name
                                ),
                            });
                            devices::seal(&key, &DeviceResponse::Rejected(reason.into()))?
                        }
                        Ok(DeviceRequest::Pair { device, .. })
                            if pairing_key.is_some() && device.peer_id == peer.to_string() =>
                        {
                            let entry = device_entry(&device, now_ms());
                            history.readmit_device(&peer.to_string())?;
                            revoked_devices.remove(&peer);
                            history.save_device(&entry)?;
                            known_devices.insert(peer, entry);
                            peers.remove(&peer);
                            sessions.remove(&peer);
                            known_contacts.remove(&peer);
                            peer_names.remove(&peer);
                            if history
                                .contacts()?
                                .iter()
                                .any(|contact| contact.peer == peer.to_string())
                            {
                                history.delete_contact(&peer.to_string())?;
                            }
                            online_devices.insert(peer);
                            pending_pairings.remove(&request.key_id);
                            let mut linked = known_devices
                                .values()
                                .map(device_descriptor)
                                .collect::<Vec<_>>();
                            linked.push(local_device_descriptor(
                                local_peer_id,
                                display_name,
                                contact_addresses(swarm, &args.relays, local_peer_id),
                            ));
                            let outgoing =
                                history.sync_records_since(0, devices::MAX_SYNC_RECORDS)?;
                            let payload = DeviceResponse::Paired {
                                account_key: *account_key,
                                devices: linked,
                                records: outgoing,
                                latest_seq: history.latest_sync_seq()?,
                                accepted_through: 0,
                            };
                            refresh_synced_state(
                                history,
                                known_contacts,
                                ignored_contacts,
                                peer_names,
                                contact_devices,
                                peers,
                                sessions,
                                events,
                            )?;
                            send_group_chats(&history.group_chats()?, events);
                            send_devices(known_devices, online_devices, events);
                            connect_synced_peers(
                                history,
                                known_devices,
                                local_peer_id,
                                &mut SyncConnections {
                                    swarm,
                                    online_contacts: peers,
                                    online_devices,
                                    dialing,
                                    bootstrap_fallbacks,
                                    fallback_planner,
                                    pending_dht,
                                },
                            )?;
                            let _ = events.send(ClientEvent::DeviceSynchronized {
                                peer_id: peer.to_string(),
                                applied: 0,
                                paired: true,
                            });
                            // Ri-annuncia la presence firmata ai contatti
                            // raggiungibili: è così che imparano il legame
                            // fra la persona e il suo account.
                            for contact_peer in peers.iter().copied().collect::<Vec<_>>() {
                                send_event(
                                    swarm,
                                    sessions,
                                    contact_devices,
                                    contact_peer,
                                    local_peer_id,
                                    sent_numbers,
                                    ChatEvent::Presence(presence_update(
                                        display_name,
                                        presence_status,
                                        true,
                                        account_proof,
                                    )),
                                );
                            }
                            devices::seal(&key, &payload)?
                        }
                        Ok(DeviceRequest::Sync {
                            device,
                            after_seq,
                            records,
                        }) if pairing_key.is_none() && device.peer_id == peer.to_string() => {
                            let applied = history.apply_sync_records(&records)?;
                            let entry = device_entry(&device, now_ms());
                            history.save_device(&entry)?;
                            known_devices.insert(peer, entry);
                            online_devices.insert(peer);
                            let outgoing =
                                history.sync_records_since(after_seq, devices::MAX_SYNC_RECORDS)?;
                            let payload = DeviceResponse::Synced {
                                records: outgoing,
                                latest_seq: history.latest_sync_seq()?,
                                accepted_through: records.last().map_or(0, |record| record.seq),
                            };
                            refresh_synced_state(
                                history,
                                known_contacts,
                                ignored_contacts,
                                peer_names,
                                contact_devices,
                                peers,
                                sessions,
                                events,
                            )?;
                            send_group_chats(&history.group_chats()?, events);
                            send_devices(known_devices, online_devices, events);
                            connect_synced_peers(
                                history,
                                known_devices,
                                local_peer_id,
                                &mut SyncConnections {
                                    swarm,
                                    online_contacts: peers,
                                    online_devices,
                                    dialing,
                                    bootstrap_fallbacks,
                                    fallback_planner,
                                    pending_dht,
                                },
                            )?;
                            let _ = events.send(ClientEvent::DeviceSynchronized {
                                peer_id: peer.to_string(),
                                applied: applied.applied,
                                paired: false,
                            });
                            devices::seal(&key, &payload)?
                        }
                        Ok(DeviceRequest::RelayText {
                            target,
                            id,
                            text,
                            emoticons,
                            offers,
                        }) if pairing_key.is_none() && known_devices.contains_key(&peer) => {
                            // Un dispositivo collegato ci chiede di consegnare un
                            // messaggio a un suo contatto che non riesce a raggiungere.
                            // Consegniamo solo verso contatti noti (mai sconosciuti).
                            let parsed = target.parse::<PeerId>().ok();
                            log::info!(
                                        "relay: RelayText da {peer} per {target}; known_contact={}, session={}",
                                        parsed.map(|p| known_contacts.contains(&p)).unwrap_or(false),
                                        parsed.map(|p| sessions.contains_key(&p)).unwrap_or(false)
                                    );
                            // Le emoticon arrivano dal dispositivo mittente insieme
                            // al testo: le sue emoticon personalizzate qui potrebbero
                            // non esistere. Un mittente vecchio non le manda, e si
                            // usano i trigger locali come prima.
                            let emoticons = if emoticons.is_empty() {
                                resolve_emoticons(&text, triggers).unwrap_or_default()
                            } else {
                                emoticons
                            };
                            let message = TextMessage {
                                text: text.clone(),
                                emoticons,
                                id,
                            };
                            let delivered = match parsed {
                                Some(target_peer)
                                    if known_contacts.contains(&target_peer)
                                        && validate_text_message(&message).is_ok() =>
                                {
                                    for offer in offers {
                                        let asset_id = offer.metadata.asset_id;
                                        if message
                                            .emoticons
                                            .iter()
                                            .any(|span| span.asset_id == asset_id)
                                            && shared_emoticons.insert((target_peer, asset_id))
                                        {
                                            deliver_event(
                                                swarm,
                                                sessions,
                                                contact_devices,
                                                sent_numbers,
                                                pending_messages,
                                                outbox,
                                                pending_handshakes,
                                                local_peer_id,
                                                target_peer,
                                                ChatEvent::EmoticonOffer(offer),
                                            );
                                        }
                                    }
                                    for offer in new_emoticon_offers(
                                        &message.emoticons,
                                        target_peer,
                                        shared_emoticons,
                                        triggers,
                                        &args.emotes,
                                        events,
                                    ) {
                                        deliver_event(
                                            swarm,
                                            sessions,
                                            contact_devices,
                                            sent_numbers,
                                            pending_messages,
                                            outbox,
                                            pending_handshakes,
                                            local_peer_id,
                                            target_peer,
                                            ChatEvent::EmoticonOffer(offer),
                                        );
                                    }
                                    deliver_event(
                                        swarm,
                                        sessions,
                                        contact_devices,
                                        sent_numbers,
                                        pending_messages,
                                        outbox,
                                        pending_handshakes,
                                        local_peer_id,
                                        target_peer,
                                        ChatEvent::Text(message.clone()),
                                    );
                                    record_text(history, &target_peer, "out", &message);
                                    let _ = events.send(ClientEvent::Message {
                                        message: ClientMessage {
                                            peer_id: target_peer.to_string(),
                                            direction: "out".into(),
                                            kind: "text".into(),
                                            body: text,
                                            timestamp_ms: now_ms(),
                                            emoticons: client_emoticon_spans(&message),
                                            attachment_id: None,
                                            attachment_mime: None,
                                            event_id: Some(hex_group_id(&id)),
                                            relayed: false,
                                        },
                                    });
                                    true
                                }
                                _ => false,
                            };
                            log::info!("relay: consegna a {target} delivered={delivered}");
                            devices::seal(&key, &DeviceResponse::RelayResult { id, delivered })?
                        }
                        Ok(DeviceRequest::RotateAccount { rotation })
                            if pairing_key.is_none() && known_devices.contains_key(&peer) =>
                        {
                            let response = devices::seal(&key, &DeviceResponse::RotationPending)?;
                            swarm
                                .behaviour_mut()
                                .devices
                                .send_response(channel, response)
                                .ok();
                            self.queue_account_rotation(peer, rotation);
                            return Ok(LoopFlow::Continue);
                        }
                        _ => devices::seal(
                            &key,
                            &DeviceResponse::Rejected("richiesta dispositivo non valida".into()),
                        )?,
                    },
                    None => devices::seal(
                        &[0u8; 32],
                        &DeviceResponse::Rejected("codice non riconosciuto".into()),
                    )?,
                };
                swarm
                    .behaviour_mut()
                    .devices
                    .send_response(channel, response)
                    .ok();
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                let Some(pending) = pending_device_requests.remove(&request_id) else {
                    return Ok(LoopFlow::Continue);
                };
                // Il messaggio è già mostrato e verrà sincronizzato dal
                // dispositivo che l'ha inoltrato. Se invece il dispositivo non
                // ha potuto consegnarlo torna nella coda diretta: parte appena
                // abbiamo una sessione col contatto.
                if let PendingDeviceRequest::Relay {
                    peer: relay_peer,
                    target,
                    message,
                } = pending
                {
                    let delivered = relay_peer == peer
                        && match devices::open::<DeviceResponse>(account_key, &response) {
                            Ok(DeviceResponse::RelayResult { delivered, .. }) => delivered,
                            other => {
                                log::warn!("relay: risposta inattesa da {peer}: {other:?}");
                                false
                            }
                        };
                    log::info!("relay: risposta da {peer} delivered={delivered}");
                    if !delivered {
                        for offer in new_emoticon_offers(
                            &message.emoticons,
                            target,
                            shared_emoticons,
                            triggers,
                            &args.emotes,
                            events,
                        ) {
                            deliver_event(
                                swarm,
                                sessions,
                                contact_devices,
                                sent_numbers,
                                pending_messages,
                                outbox,
                                pending_handshakes,
                                local_peer_id,
                                target,
                                ChatEvent::EmoticonOffer(offer),
                            );
                        }
                        deliver_event(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_messages,
                            outbox,
                            pending_handshakes,
                            local_peer_id,
                            target,
                            ChatEvent::Text(message),
                        );
                    }
                    return Ok(LoopFlow::Continue);
                }
                let (expected_peer, key, pairing) = match pending {
                    PendingDeviceRequest::Pair { peer, key } => (peer, key, true),
                    PendingDeviceRequest::Sync { peer } => (peer, *account_key, false),
                    PendingDeviceRequest::Relay { .. } | PendingDeviceRequest::Rotate => {
                        return Ok(LoopFlow::Continue)
                    }
                };
                if expected_peer != peer {
                    return Ok(LoopFlow::Continue);
                }
                match devices::open::<DeviceResponse>(&key, &response) {
                    Ok(DeviceResponse::Paired {
                        account_key: linked_key,
                        devices: linked,
                        records,
                        latest_seq,
                        accepted_through,
                    }) if pairing => {
                        if let Some(store) = &args.account_key_store {
                            store(linked_key).map_err(|error| {
                                format!("account collegato ma non salvabile: {error}")
                            })?;
                        }
                        *account_key = linked_key;
                        history.readmit_device(&peer.to_string())?;
                        revoked_devices.remove(&peer);
                        // Adottato l'account del dispositivo che ci ha
                        // collegati: da ora firmiamo le presence con la
                        // sua identità, così i suoi contatti ci
                        // riconoscono senza approvarci di nuovo.
                        *account_identity = AccountIdentity::from_account_key(account_key)?;
                        *account_proof = account_identity.proof(local_peer_id).ok();
                        pending_pair_targets.remove(&peer);
                        if history
                            .contacts()?
                            .iter()
                            .any(|contact| contact.peer == local_peer_id.to_string())
                        {
                            history.delete_contact(&local_peer_id.to_string())?;
                        }
                        for device in linked {
                            if device.peer_id == local_peer_id.to_string() {
                                continue;
                            }
                            if let Ok(device_peer) = device.peer_id.parse::<PeerId>() {
                                let entry = device_entry(&device, now_ms());
                                history.save_device(&entry)?;
                                known_devices.insert(device_peer, entry);
                                peers.remove(&device_peer);
                                sessions.remove(&device_peer);
                                known_contacts.remove(&device_peer);
                                peer_names.remove(&device_peer);
                                if history
                                    .contacts()?
                                    .iter()
                                    .any(|contact| contact.peer == device_peer.to_string())
                                {
                                    history.delete_contact(&device_peer.to_string())?;
                                }
                            }
                        }
                        online_devices.insert(peer);
                        let applied = history.apply_sync_records(&records)?;
                        let received_through = records.last().map_or(0, |record| record.seq);
                        history.update_sync_cursors(
                            &peer.to_string(),
                            received_through,
                            accepted_through,
                        )?;
                        refresh_synced_state(
                            history,
                            known_contacts,
                            ignored_contacts,
                            peer_names,
                            contact_devices,
                            peers,
                            sessions,
                            events,
                        )?;
                        send_group_chats(&history.group_chats()?, events);
                        send_devices(known_devices, online_devices, events);
                        connect_synced_peers(
                            history,
                            known_devices,
                            local_peer_id,
                            &mut SyncConnections {
                                swarm,
                                online_contacts: peers,
                                online_devices,
                                dialing,
                                bootstrap_fallbacks,
                                fallback_planner,
                                pending_dht,
                            },
                        )?;
                        let _ = events.send(ClientEvent::DeviceSynchronized {
                            peer_id: peer.to_string(),
                            applied: applied.applied,
                            paired: true,
                        });
                        // Ri-annuncia la presence firmata ai contatti
                        // raggiungibili: è così che imparano il legame
                        // fra la persona e il suo account.
                        for contact_peer in peers.iter().copied().collect::<Vec<_>>() {
                            send_event(
                                swarm,
                                sessions,
                                contact_devices,
                                contact_peer,
                                local_peer_id,
                                sent_numbers,
                                ChatEvent::Presence(presence_update(
                                    display_name,
                                    presence_status,
                                    true,
                                    account_proof,
                                )),
                            );
                        }
                        if received_through < latest_seq
                            || accepted_through < history.latest_sync_seq()?
                        {
                            let descriptor = local_device_descriptor(
                                local_peer_id,
                                display_name,
                                contact_addresses(swarm, &args.relays, local_peer_id),
                            );
                            send_device_sync(
                                swarm,
                                history,
                                account_key,
                                pending_device_requests,
                                peer,
                                descriptor,
                            )?;
                        }
                    }
                    Ok(DeviceResponse::Synced {
                        records,
                        latest_seq,
                        accepted_through,
                    }) if !pairing => {
                        let applied = history.apply_sync_records(&records)?;
                        let received_through = records.last().map_or_else(
                            || {
                                history
                                    .sync_cursors(&peer.to_string())
                                    .map(|cursor| cursor.0)
                                    .unwrap_or_default()
                            },
                            |record| record.seq,
                        );
                        history.update_sync_cursors(
                            &peer.to_string(),
                            received_through,
                            accepted_through,
                        )?;
                        refresh_synced_state(
                            history,
                            known_contacts,
                            ignored_contacts,
                            peer_names,
                            contact_devices,
                            peers,
                            sessions,
                            events,
                        )?;
                        send_group_chats(&history.group_chats()?, events);
                        connect_synced_peers(
                            history,
                            known_devices,
                            local_peer_id,
                            &mut SyncConnections {
                                swarm,
                                online_contacts: peers,
                                online_devices,
                                dialing,
                                bootstrap_fallbacks,
                                fallback_planner,
                                pending_dht,
                            },
                        )?;
                        let _ = events.send(ClientEvent::DeviceSynchronized {
                            peer_id: peer.to_string(),
                            applied: applied.applied,
                            paired: false,
                        });
                        if received_through < latest_seq
                            || accepted_through < history.latest_sync_seq()?
                        {
                            let descriptor = local_device_descriptor(
                                local_peer_id,
                                display_name,
                                contact_addresses(swarm, &args.relays, local_peer_id),
                            );
                            send_device_sync(
                                swarm,
                                history,
                                account_key,
                                pending_device_requests,
                                peer,
                                descriptor,
                            )?;
                        }
                    }
                    Ok(DeviceResponse::RotationRequired { rotation }) if !pairing => {
                        self.queue_account_rotation(peer, rotation);
                    }
                    Ok(DeviceResponse::Rejected(message)) => {
                        let _ = events.send(ClientEvent::Error { message });
                    }
                    // Un dispositivo che non ha ancora adottato la nostra
                    // nuova chiave non sa rispondere al sync periodico: non è
                    // un errore da mostrare ogni pochi secondi.
                    Err(_) if !pairing => {
                        log::warn!("risposta di sync da {peer} non leggibile");
                    }
                    _ => {
                        let _ = events.send(ClientEvent::Error {
                            message: "risposta dispositivo non valida".into(),
                        });
                    }
                }
            }
        }
        Ok(LoopFlow::Continue)
    }

    fn on_link_command(&mut self, command: ClientCommand) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            ml_dsa_identity,
            contact_identity,
            history,
            ignored_contacts,
            local_peer_id,
            swarm,
            display_name,
            dialing,
            bootstrap_fallbacks,
            known_contacts,
            fallback_planner,
            pending_dht,
            peer_names,
            contact_link_requested,
            device_link_requested,
            pending_pairings,
            pending_pair_targets,
            pending_device_requests,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match command {
            ClientCommand::RequestContactLink => {
                *contact_link_requested = true;
                let addresses = contact_addresses(swarm, &args.relays, local_peer_id);
                if !addresses.is_empty() {
                    let links = contacts::share_links(
                        display_name,
                        local_peer_id,
                        contact_identity,
                        ml_dsa_identity,
                        addresses,
                    );
                    match links {
                        Ok((link, qr_link)) => {
                            *contact_link_requested = false;
                            let _ = events.send(ClientEvent::ContactLink { link, qr_link });
                        }
                        Err(error) => {
                            let _ = events.send(ClientEvent::Error {
                                message: error.to_string(),
                            });
                        }
                    }
                }
            }
            ClientCommand::RequestDeviceLink => {
                *device_link_requested = true;
                let addresses = contact_addresses(swarm, &args.relays, local_peer_id);
                if !addresses.is_empty() {
                    let descriptor =
                        local_device_descriptor(local_peer_id, display_name, addresses);
                    match devices::create_pairing_link(descriptor, now_ms()) {
                        Ok((link, pairing)) => {
                            *device_link_requested = false;
                            pending_pairings.insert(
                                devices::key_id(&pairing.secret),
                                PendingPairing {
                                    secret: pairing.secret,
                                    expires_at_ms: pairing.expires_at_ms,
                                },
                            );
                            let _ = events.send(ClientEvent::DeviceLink {
                                qr_link: link.clone(),
                                link,
                                expires_at_ms: pairing.expires_at_ms,
                            });
                        }
                        Err(error) => {
                            let _ = events.send(ClientEvent::Error {
                                message: error.to_string(),
                            });
                        }
                    }
                }
            }
            ClientCommand::ImportDeviceLink { link } => {
                match devices::parse_pairing_link(&link, now_ms()) {
                    Ok(pairing) => {
                        let peer = pairing.inviter.peer_id.parse::<PeerId>()?;
                        if peer == local_peer_id {
                            let _ = events.send(ClientEvent::Error {
                                message: "non puoi collegare il dispositivo a se stesso".into(),
                            });
                            return Ok(LoopFlow::Continue);
                        }
                        // Un codice mostrato da un contatto è di un'altra
                        // persona: collegarsi unirebbe i due account.
                        if history
                            .contacts()?
                            .iter()
                            .any(|contact| contact.peer == peer.to_string())
                        {
                            let _ = events.send(ClientEvent::Error {
                                message: PAIRING_WITH_CONTACT.into(),
                            });
                            return Ok(LoopFlow::Continue);
                        }
                        // Un dispositivo con chat o contatti propri appartiene
                        // già a un account: va prima svuotato (l'app archivia i
                        // dati), altrimenti i due account si mescolano.
                        if history.latest_sync_seq()? > 0 {
                            let _ = events.send(ClientEvent::Error {
                                message: PAIRING_WITH_LOCAL_DATA.into(),
                            });
                            return Ok(LoopFlow::Continue);
                        }
                        let addresses = pairing
                            .inviter
                            .addresses
                            .iter()
                            .map(|address| address.parse::<Multiaddr>())
                            .collect::<Result<Vec<_>, _>>()?;
                        let secret = pairing.secret;
                        pending_pair_targets.insert(peer, pairing);
                        if swarm.is_connected(&peer) {
                            let descriptor = local_device_descriptor(
                                local_peer_id,
                                display_name,
                                contact_addresses(swarm, &args.relays, local_peer_id),
                            );
                            let request = DeviceRequest::Pair {
                                device: descriptor,
                                records: Vec::new(),
                            };
                            let sealed = devices::seal(&secret, &request)?;
                            let request_id =
                                swarm.behaviour_mut().devices.send_request(&peer, sealed);
                            pending_device_requests.insert(
                                request_id,
                                PendingDeviceRequest::Pair { peer, key: secret },
                            );
                        } else {
                            connect_contact(
                                swarm,
                                peer,
                                addresses,
                                dialing,
                                bootstrap_fallbacks,
                                fallback_planner,
                                pending_dht,
                            );
                        }
                    }
                    Err(error) => {
                        let _ = events.send(ClientEvent::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientCommand::ImportContactLink { link } => match contacts::import_link(&link) {
                Ok((name, peer_id, addresses)) => {
                    history.allow_contact(&peer_id.to_string())?;
                    ignored_contacts.remove(&peer_id);
                    history.save_contact(&peer_id.to_string(), &name, &link, now_ms())?;
                    peer_names.insert(peer_id, name.clone());
                    known_contacts.insert(peer_id);
                    let _ = events.send(ClientEvent::ContactUpdated {
                        contact: ClientContact {
                            peer_id: peer_id.to_string(),
                            name,
                            online: false,
                            secure: false,
                            fingerprint: peer_fingerprint(peer_id),
                        },
                    });
                    connect_contact(
                        swarm,
                        peer_id,
                        addresses,
                        dialing,
                        bootstrap_fallbacks,
                        fallback_planner,
                        pending_dht,
                    );
                }
                Err(error) => {
                    let _ = events.send(ClientEvent::Error {
                        message: error.to_string(),
                    });
                }
            },
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_invite_command(&mut self, command: ClientCommand) -> Result<LoopFlow, Box<dyn Error>> {
        match command {
            ClientCommand::CreateInvite => {
                self.stop_invite();
                let invite = invite::ActiveInvite::new(now_ms());
                let key = kad::RecordKey::new(&invite.code.rendezvous_key());
                if let Err(error) = self.swarm.behaviour_mut().kad.start_providing(key) {
                    self.invite_failed(true, error.to_string());
                    return Ok(LoopFlow::Continue);
                }
                let _ = self.events.send(ClientEvent::Invite {
                    code: invite.code.to_string(),
                    expires_at_ms: invite.expires_at_ms,
                });
                self.active_invite = Some(invite);
            }
            ClientCommand::CancelInvite => self.stop_invite(),
            ClientCommand::JoinInvite { code } => {
                let code = match invite::InviteCode::parse(&code) {
                    Ok(code) => code,
                    Err(message) => {
                        self.invite_failed(false, message.into());
                        return Ok(LoopFlow::Continue);
                    }
                };
                if self
                    .active_invite
                    .as_ref()
                    .is_some_and(|active| active.code == code)
                {
                    self.invite_failed(
                        false,
                        "questo è il tuo codice: va inserito dall'altra persona".into(),
                    );
                    return Ok(LoopFlow::Continue);
                }
                let key = kad::RecordKey::new(&code.rendezvous_key());
                let query = self.swarm.behaviour_mut().kad.get_providers(key);
                self.invite_join = Some(InviteJoin {
                    code,
                    query: Some(query),
                    candidates: VecDeque::new(),
                    tried: HashSet::new(),
                    current: None,
                    session: None,
                    card: None,
                    last_error: None,
                });
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn invite_failed(&mut self, inviter: bool, message: String) {
        let _ = self
            .events
            .send(ClientEvent::InviteFailed { inviter, message });
    }

    /// Ritira l'invito mostrato: la DHT smette di indicare questo peer e i
    /// tentativi successivi vengono rifiutati.
    fn stop_invite(&mut self) {
        if let Some(invite) = self.active_invite.take() {
            self.swarm
                .behaviour_mut()
                .kad
                .stop_providing(&kad::RecordKey::new(&invite.code.rendezvous_key()));
        }
    }

    fn own_invite_card(&self) -> Result<Vec<u8>, Box<dyn Error>> {
        contacts::invite_card(
            &self.display_name,
            self.local_peer_id,
            &self.contact_identity,
            &self.ml_dsa_identity,
            contact_addresses(&self.swarm, &self.args.relays, self.local_peer_id).into_iter(),
        )
    }

    fn on_invite_providers(
        &mut self,
        result: kad::GetProvidersResult,
        last: bool,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let local_peer_id = self.local_peer_id;
        let Some(join) = self.invite_join.as_mut() else {
            return Ok(LoopFlow::Continue);
        };
        if let Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) = result {
            for provider in providers {
                if provider != local_peer_id
                    && !join.tried.contains(&provider)
                    && !join.candidates.contains(&provider)
                    && join.tried.len() + join.candidates.len() < invite::MAX_INVITE_CANDIDATES
                {
                    join.candidates.push_back(provider);
                }
            }
        }
        // Ogni server DHT restituisce tutti i provider che conosce: appena
        // ne arriva uno non serve interrogare anche i peer più lontani, che
        // in modalità client non rispondono e farebbero solo attendere.
        if !join.candidates.is_empty() {
            if let Some(mut query) = join
                .query
                .take()
                .and_then(|id| self.swarm.behaviour_mut().kad.query_mut(&id))
            {
                query.finish();
            }
        } else if last {
            join.query = None;
        }
        self.invite_next_candidate();
        Ok(LoopFlow::Continue)
    }

    /// Prova il prossimo provider del nameplate, oppure chiude la ricerca.
    fn invite_next_candidate(&mut self) {
        let Some(join) = self.invite_join.as_mut() else {
            return;
        };
        if join.current.is_some() {
            return;
        }
        let Some(peer) = join.candidates.pop_front() else {
            if join.query.is_none() {
                let message = join.last_error.take().unwrap_or_else(|| {
                    "codice non trovato: controlla che sia giusto e che chi l'ha creato sia online"
                        .into()
                });
                self.invite_join = None;
                self.invite_failed(false, message);
            }
            return;
        };
        join.tried.insert(peer);
        join.current = Some(peer);
        let (session, request) = invite::JoinSession::start(join.code, peer, self.local_peer_id);
        join.session = Some(session);
        // La richiesta compone da sola il numero: indirizzi dalla ricerca DHT
        // in corso, più i circuiti sui relay configurati.
        for relay in &self.args.relays {
            if let Ok(route) = relay_route(relay, peer) {
                self.swarm.add_peer_address(peer, route);
            }
        }
        self.swarm
            .behaviour_mut()
            .invite
            .send_request(&peer, request);
    }

    fn invite_candidate_failed(
        &mut self,
        peer: PeerId,
        reason: &str,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        if let Some(join) = self
            .invite_join
            .as_mut()
            .filter(|join| join.current == Some(peer))
        {
            join.current = None;
            join.session = None;
            join.card = None;
            join.last_error = Some(reason.to_owned());
            self.invite_next_candidate();
        }
        Ok(LoopFlow::Continue)
    }

    fn on_invite_message(
        &mut self,
        peer: PeerId,
        message: request_response::Message<invite::InviteRequest, invite::InviteResponse>,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                let (response, card) = self.answer_invite(peer, request);
                let finished = matches!(response, invite::InviteResponse::Finished);
                self.swarm
                    .behaviour_mut()
                    .invite
                    .send_response(channel, response)
                    .ok();
                if let (true, Some(card)) = (finished, card) {
                    // Il codice è usato: nessun altro può più provarci. Chi
                    // l'ha usato però non entra in rubrica da solo: il codice
                    // potrebbe essere stato letto da qualcun altro, quindi
                    // diventa una richiesta da accettare con nome e impronta.
                    self.stop_invite();
                    let name = card.name.clone();
                    self.ignored_contacts.remove(&peer);
                    self.peer_names.insert(peer, name.clone());
                    self.pending_contact_requests.insert(peer, name.clone());
                    self.pending_invite_cards.insert(peer, card);
                    let _ = self.events.send(ClientEvent::InviteCompleted {
                        peer_id: peer.to_string(),
                        name: name.clone(),
                        inviter: true,
                    });
                    let _ = self.events.send(ClientEvent::ContactRequest {
                        peer_id: peer.to_string(),
                        name,
                        fingerprint: peer_fingerprint(peer),
                        via_invite: true,
                    });
                }
            }
            request_response::Message::Response { response, .. } => {
                if !self
                    .invite_join
                    .as_ref()
                    .is_some_and(|join| join.current == Some(peer))
                {
                    return Ok(LoopFlow::Continue);
                }
                match response {
                    invite::InviteResponse::Started { message, card } => {
                        let Some(session) =
                            self.invite_join.as_mut().and_then(|j| j.session.take())
                        else {
                            return Ok(LoopFlow::Continue);
                        };
                        let own_card = self.own_invite_card()?;
                        let verified = session
                            .complete(&message, &card, &own_card)
                            .map_err(|error| error.to_string())
                            .and_then(|(bytes, finish)| {
                                let card = contacts::decode_invite_card(&bytes)
                                    .map_err(|error| error.to_string())?;
                                if card.peer_id != peer {
                                    return Err("la scheda non è di chi ha risposto".into());
                                }
                                Ok((card, finish))
                            });
                        match verified {
                            Ok((card, finish)) => {
                                // Chi invita è verificato: lo aggiungiamo subito,
                                // così la sua presence non diventa una richiesta.
                                if let Err(error) = self.add_invited_contact(card.clone()) {
                                    return self.invite_candidate_failed(peer, &error.to_string());
                                }
                                if let Some(join) = self.invite_join.as_mut() {
                                    join.card = Some(card);
                                }
                                self.swarm
                                    .behaviour_mut()
                                    .invite
                                    .send_request(&peer, finish);
                            }
                            Err(error) => return self.invite_candidate_failed(peer, &error),
                        }
                    }
                    invite::InviteResponse::Finished => {
                        let Some(card) = self.invite_join.take().and_then(|join| join.card) else {
                            return Ok(LoopFlow::Continue);
                        };
                        let _ = self.events.send(ClientEvent::InviteCompleted {
                            peer_id: peer.to_string(),
                            name: card.name,
                            inviter: false,
                        });
                        return self.on_connection_established(peer);
                    }
                    invite::InviteResponse::Rejected(reason) => {
                        return self.invite_candidate_failed(peer, &reason)
                    }
                }
            }
        }
        Ok(LoopFlow::Continue)
    }

    /// Lato di chi invita: risponde a `Start` e `Finish`. Con `Finished`
    /// restituisce anche la scheda verificata di chi ha usato il codice.
    fn answer_invite(
        &mut self,
        peer: PeerId,
        request: invite::InviteRequest,
    ) -> (invite::InviteResponse, Option<contacts::InviteCard>) {
        let now = now_ms();
        let rejected = |reason: &str| (invite::InviteResponse::Rejected(reason.into()), None);
        if self.known_devices.contains_key(&peer) {
            return rejected("un tuo dispositivo non può essere un contatto");
        }
        if self
            .active_invite
            .as_ref()
            .is_none_or(|invite| invite.expired(now))
        {
            self.stop_invite();
            return rejected(invite::INVITE_EXPIRED);
        }
        match request {
            invite::InviteRequest::Start { nameplate, message } => {
                if self
                    .active_invite
                    .as_ref()
                    .is_some_and(|invite| invite.exhausted(now))
                {
                    self.stop_invite();
                    self.invite_failed(true, invite::INVITE_BURNED.into());
                    return rejected(invite::INVITE_EXPIRED);
                }
                let card = match self.own_invite_card() {
                    Ok(card) => card,
                    Err(error) => return rejected(&error.to_string()),
                };
                let local_peer_id = self.local_peer_id;
                let Some(active) = self.active_invite.as_mut() else {
                    return rejected(invite::INVITE_EXPIRED);
                };
                match active.start(local_peer_id, peer, nameplate, &message, &card, now) {
                    Ok((message, card)) => {
                        (invite::InviteResponse::Started { message, card }, None)
                    }
                    Err(error) => rejected(error),
                }
            }
            invite::InviteRequest::Finish { nameplate, card } => {
                let Some(active) = self.active_invite.as_mut() else {
                    return rejected(invite::INVITE_EXPIRED);
                };
                let card = active
                    .finish(peer, nameplate, &card, now)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| {
                        contacts::decode_invite_card(&bytes).map_err(|error| error.to_string())
                    });
                match card {
                    Ok(card) if card.peer_id == peer => {
                        (invite::InviteResponse::Finished, Some(card))
                    }
                    Ok(_) => rejected("la scheda non è di chi ha usato il codice"),
                    Err(error) => rejected(&error),
                }
            }
        }
    }

    /// Mette in rubrica un contatto arrivato con un invito e fissa subito la
    /// sua chiave ML-DSA, già verificata dalla firma della scheda.
    fn add_invited_contact(&mut self, card: contacts::InviteCard) -> Result<(), Box<dyn Error>> {
        let peer = card.peer_id;
        let key = peer.to_string();
        store_invite_card(&self.history, &mut self.swarm, &card)?;
        self.history.allow_contact(&key)?;
        self.ignored_contacts.remove(&peer);
        self.pending_contact_requests.remove(&peer);
        self.peer_names.insert(peer, card.name.clone());
        self.known_contacts.insert(peer);
        let _ = self.events.send(ClientEvent::ContactUpdated {
            contact: ClientContact {
                peer_id: key,
                name: card.name,
                online: false,
                secure: false,
                fingerprint: peer_fingerprint(peer),
            },
        });
        Ok(())
    }

    fn on_message_command(&mut self, command: ClientCommand) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            account_key,
            history,
            local_peer_id,
            swarm,
            peers,
            contact_devices,
            triggers,
            nudge_limits,
            sent_numbers,
            pending_handshakes,
            outbox,
            pending_messages,
            sessions,
            shared_emoticons,
            prepared_files_tx,
            nudge_counter,
            pending_device_requests,
            online_devices,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        let account_key = *account_key;
        match command {
            ClientCommand::SendText { peer, text } => {
                if let Err(message) = check_outgoing_text(&text) {
                    let _ = events.send(ClientEvent::Error { message });
                    return Ok(LoopFlow::Continue);
                }
                let emoticons = resolve_emoticons(&text, triggers)
                    .map_err(|error| format!("trigger emoticon non valido: {error:?}"))?;
                let mut message_id = [0u8; 16];
                OsRng.fill_bytes(&mut message_id);
                let message = TextMessage {
                    text: text.clone(),
                    emoticons,
                    id: message_id,
                };
                let client_emoticons = client_emoticon_spans(&message);
                // Contatto non connesso ma un mio dispositivo collegato è online:
                // gli chiedo di consegnare il messaggio (relay). Uno solo, così il
                // contatto non riceve doppioni; se il relay fallisce il messaggio
                // torna nella coda diretta. Con il contatto connesso e l'handshake
                // ancora in corso si aspetta la sessione diretta, che arriva a breve.
                let relay_device = if contact_devices.online(peer, peers) {
                    None
                } else {
                    online_devices.iter().min().copied()
                };
                let relayed = relay_device.is_some_and(|device| {
                    log::info!("relay: chiedo a {device} di consegnare a {peer}");
                    let offers = relay_emoticon_offers(&message.emoticons, triggers, &args.emotes);
                    send_relay_text(
                        swarm,
                        &account_key,
                        pending_device_requests,
                        device,
                        peer,
                        message.clone(),
                        offers,
                    )
                    .inspect_err(|error| log::warn!("relay verso dispositivo fallito: {error}"))
                    .is_ok()
                });
                if !relayed {
                    // Le emoticon partono prima del testo e con la stessa
                    // consegna affidabile: il destinatario le ha quando lo legge.
                    for offer in new_emoticon_offers(
                        &message.emoticons,
                        peer,
                        shared_emoticons,
                        triggers,
                        &args.emotes,
                        events,
                    ) {
                        deliver_event(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_messages,
                            outbox,
                            pending_handshakes,
                            local_peer_id,
                            peer,
                            ChatEvent::EmoticonOffer(offer),
                        );
                    }
                    deliver_event(
                        swarm,
                        sessions,
                        contact_devices,
                        sent_numbers,
                        pending_messages,
                        outbox,
                        pending_handshakes,
                        local_peer_id,
                        peer,
                        ChatEvent::Text(message.clone()),
                    );
                }
                record_text(history, &peer, "out", &message);
                let _ = events.send(ClientEvent::Message {
                    message: ClientMessage {
                        peer_id: peer.to_string(),
                        direction: "out".into(),
                        kind: "text".into(),
                        body: text,
                        timestamp_ms: now_ms(),
                        emoticons: client_emoticons,
                        attachment_id: None,
                        attachment_mime: None,
                        event_id: Some(hex_group_id(&message_id)),
                        relayed,
                    },
                });
            }
            ClientCommand::SendNudge { peer } => {
                // Prima la connessione, poi il limite: un trillo che non può
                // partire non deve consumare il rate limit.
                if !contact_devices.online(peer, peers) {
                    let _ = events.send(ClientEvent::Error {
                        message: "contatto non collegato".into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                let timestamp_ms = now_ms();
                if let Err(wait_ms) = nudge_limits
                    .entry(peer)
                    .or_default()
                    .try_acquire(timestamp_ms)
                {
                    let _ = events.send(ClientEvent::Error {
                        message: format!(
                            "trillo limitato: riprova tra {}s",
                            wait_ms.div_ceil(1_000)
                        ),
                    });
                    return Ok(LoopFlow::Continue);
                }
                *nudge_counter += 1;
                let mut id = [0; 16];
                id[..8].copy_from_slice(&timestamp_ms.to_be_bytes());
                id[8..].copy_from_slice(&nudge_counter.to_be_bytes());
                deliver_event(
                    swarm,
                    sessions,
                    contact_devices,
                    sent_numbers,
                    pending_messages,
                    outbox,
                    pending_handshakes,
                    local_peer_id,
                    peer,
                    ChatEvent::Nudge(Nudge {
                        id,
                        intensity: 1,
                        timestamp_ms,
                    }),
                );
                record(history, &peer, "out", "nudge", "trillo");
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
                        event_id: None,
                        relayed: false,
                    },
                });
            }
            ClientCommand::SendFile { peer, path } => {
                if !contact_devices.online(peer, peers) {
                    let _ = events.send(ClientEvent::Error {
                        message: "contatto non collegato".into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                // L'hash di un file fino a 5 GB richiede minuti: lo calcola un
                // thread a parte e l'offerta parte quando è pronto. Intanto il
                // loop continua a gestire messaggi, ping e handshake.
                prepare_file(prepared_files_tx, FileTarget::Peer(peer), path);
            }
            ClientCommand::DeleteMessageForMe { event_id } => {
                if let Err(error) = history.delete_event(&event_id, now_ms()) {
                    let _ = events.send(ClientEvent::Error {
                        message: error.to_string(),
                    });
                }
            }
            ClientCommand::DeleteMessageForEveryone { peer, event_id } => {
                const DELETE_WINDOW_MS: u64 = 15 * 60 * 1000;
                // Solo un mio messaggio (direction "out") verso questo peer e
                // dentro la finestra può essere cancellato per tutti.
                match history.event_meta(&event_id) {
                    Ok(Some((stored_peer, direction, timestamp_ms))) => {
                        if stored_peer != peer.to_string() || direction != "out" {
                            let _ = events.send(ClientEvent::Error {
                                message: "puoi cancellare per tutti solo i tuoi messaggi".into(),
                            });
                        } else if now_ms().saturating_sub(timestamp_ms) > DELETE_WINDOW_MS {
                            let _ = events.send(ClientEvent::Error {
                                message: "tempo scaduto per la cancellazione per tutti".into(),
                            });
                        } else if let Some(id) = parse_message_id(&event_id) {
                            // Come i messaggi: in coda se il contatto è offline,
                            // con retry e avviso se non arriva.
                            deliver_event(
                                swarm,
                                sessions,
                                contact_devices,
                                sent_numbers,
                                pending_messages,
                                outbox,
                                pending_handshakes,
                                local_peer_id,
                                peer,
                                ChatEvent::DeleteMessage(DeleteRequest { id, group_id: None }),
                            );
                            if let Err(error) = history.tombstone_event(&event_id, now_ms()) {
                                let _ = events.send(ClientEvent::Error {
                                    message: error.to_string(),
                                });
                            } else {
                                let _ = events.send(ClientEvent::MessageDeleted {
                                    peer_id: peer.to_string(),
                                    event_id,
                                });
                            }
                        } else {
                            let _ = events.send(ClientEvent::Error {
                                message: "id messaggio non valido".into(),
                            });
                        }
                    }
                    Ok(None) => {
                        let _ = events.send(ClientEvent::Error {
                            message: "messaggio non trovato".into(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ClientEvent::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientCommand::LoadOlderMessages {
                peer,
                before_event_id,
            } => {
                let messages =
                    conversation_page(history, &peer.to_string(), Some(&before_event_id))?;
                let _ = events.send(ClientEvent::OlderMessagesLoaded {
                    peer_id: peer.to_string(),
                    has_more: messages.len() == HISTORY_PAGE,
                    messages,
                });
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_emoticon_command(&mut self, command: ClientCommand) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            triggers,
            pending_emoticons,
            args,
            events,
            ..
        } = self;
        match command {
            ClientCommand::CreateEmoticon { path, trigger } => {
                match create_emoticon(&path, &trigger, &args.emotes, triggers) {
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
            ClientCommand::SaveEmoticon { asset_id, trigger } => {
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
                        match save_offer(&args.emotes, &offer, triggers) {
                            Ok(_) => {
                                pending_emoticons.remove(&asset_id);
                                let _ = events.send(ClientEvent::EmoticonCatalog {
                                    emoticons: vec![client_emoticon(&offer, trigger.trim(), true)],
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
            ClientCommand::UpdateEmoticon { asset_id, trigger } => {
                match parse_asset_id(&asset_id).and_then(|asset_id| {
                    let current = triggers
                        .iter()
                        .find(|item| item.asset_id == asset_id)
                        .cloned()
                        .ok_or_else(|| "emoticon non trovata".to_owned())?;
                    let mut offer = load_emoticon_offer(&args.emotes, &current)
                        .map_err(|error| error.to_string())?;
                    offer.metadata.suggested_triggers = vec![trigger.trim().to_owned()];
                    save_offer(&args.emotes, &offer, triggers)
                        .map_err(|error| error.to_string())?;
                    Ok((offer, trigger.trim().to_owned()))
                }) {
                    Ok((offer, trigger)) => {
                        let _ = events.send(ClientEvent::EmoticonCatalog {
                            emoticons: vec![client_emoticon(&offer, &trigger, true)],
                        });
                    }
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                    }
                }
            }
            ClientCommand::DeleteEmoticon { asset_id } => {
                match parse_asset_id(&asset_id).and_then(|asset_id| {
                    delete_emoticon(&args.emotes, asset_id, triggers)
                        .map_err(|error| error.to_string())
                }) {
                    Ok(()) => {
                        let _ = events.send(ClientEvent::EmoticonRemoved { asset_id });
                    }
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                    }
                }
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_contact_command(&mut self, command: ClientCommand) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            ignored_contacts,
            local_peer_id,
            account_proof,
            swarm,
            display_name,
            peers,
            pending_contact_requests,
            pending_invite_cards,
            pending_accounts,
            reconnect_at,
            reconnect_attempts,
            known_contacts,
            contact_devices,
            sent_numbers,
            pending_handshakes,
            presence_status,
            sessions,
            peer_names,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match command {
            ClientCommand::RenameContact { peer, name } => {
                match history.rename_contact(&peer.to_string(), &name) {
                    Ok(()) => {
                        let name = name.trim().to_owned();
                        peer_names.insert(peer, name.clone());
                        let _ = events.send(ClientEvent::ContactUpdated {
                            contact: ClientContact {
                                peer_id: peer.to_string(),
                                name,
                                online: contact_devices.online(peer, peers),
                                secure: contact_devices.secure(peer, sessions),
                                fingerprint: peer_fingerprint(peer),
                            },
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ClientEvent::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientCommand::ClearConversation { peer } => {
                match history.clear_conversation(&peer.to_string()) {
                    Ok(()) => {
                        let _ = events.send(ClientEvent::ConversationCleared {
                            peer_id: peer.to_string(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ClientEvent::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientCommand::DeleteContact { peer } => {
                match history.delete_contact(&peer.to_string()) {
                    Ok(()) => {
                        // Bloccare una persona blocca tutti i suoi dispositivi.
                        for device in contact_devices.forget(peer) {
                            history.delete_contact(&device.to_string())?;
                            ignored_contacts.insert(device);
                            known_contacts.remove(&device);
                            peer_names.remove(&device);
                            peers.remove(&device);
                            sessions.remove(&device);
                            reconnect_at.remove(&device);
                            reconnect_attempts.remove(&device);
                            let _ = swarm.disconnect_peer_id(device);
                        }
                        ignored_contacts.insert(peer);
                        known_contacts.remove(&peer);
                        peer_names.remove(&peer);
                        peers.remove(&peer);
                        sessions.remove(&peer);
                        reconnect_at.remove(&peer);
                        reconnect_attempts.remove(&peer);
                        let _ = swarm.disconnect_peer_id(peer);
                        let _ = events.send(ClientEvent::ContactRemoved {
                            peer_id: peer.to_string(),
                            reason: "deleted".into(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ClientEvent::Error {
                            message: error.to_string(),
                        });
                    }
                }
            }
            ClientCommand::AcceptContactRequest { peer } => {
                let invite_card = pending_invite_cards.remove(&peer);
                if let Some(name) = pending_contact_requests.remove(&peer) {
                    ignored_contacts.remove(&peer);
                    history.allow_contact(&peer.to_string())?;
                    history.ensure_contact(&peer.to_string(), &name, now_ms())?;
                    // Arrivato con il nostro codice: scheda già verificata,
                    // si fissa subito la sua chiave ML-DSA.
                    if let Some(card) = &invite_card {
                        store_invite_card(history, swarm, card)?;
                    }
                    // Approvata la persona, i suoi altri dispositivi non
                    // dovranno più chiedere.
                    if let Some(account) = pending_accounts.remove(&peer) {
                        if contact_devices.bind(peer, account.clone()) {
                            history.bind_contact_account(&peer.to_string(), &account, now_ms())?;
                        }
                    }
                    known_contacts.insert(peer);
                    peer_names.entry(peer).or_insert_with(|| name.clone());
                    peers.insert(peer);
                    let _ = events.send(ClientEvent::ContactUpdated {
                        contact: ClientContact {
                            peer_id: peer.to_string(),
                            name: peer_names.get(&peer).cloned().unwrap_or(name),
                            online: true,
                            secure: contact_devices.secure(peer, sessions),
                            fingerprint: peer_fingerprint(peer),
                        },
                    });
                    // Ricambia la presence e avvia il canale cifrato.
                    send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        peer,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::Presence(presence_update(
                            display_name,
                            presence_status,
                            true,
                            account_proof,
                        )),
                    );
                    maybe_start_hybrid_handshake(
                        swarm,
                        pending_handshakes,
                        sessions,
                        local_peer_id,
                        peer,
                    );
                }
            }
            ClientCommand::RejectContactRequest { peer } => {
                pending_contact_requests.remove(&peer);
                pending_invite_cards.remove(&peer);
                pending_accounts.remove(&peer);
                // Il rifiuto resta dopo un riavvio e arriva agli altri
                // dispositivi: lo stesso sconosciuto non può richiedere di nuovo.
                history.delete_contact(&peer.to_string())?;
                ignored_contacts.insert(peer);
                known_contacts.remove(&peer);
                peers.remove(&peer);
                sessions.remove(&peer);
                let _ = swarm.disconnect_peer_id(peer);
                let _ = events.send(ClientEvent::ContactRemoved {
                    peer_id: peer.to_string(),
                    reason: "rejected".into(),
                });
            }
            ClientCommand::RequestPendingContactRequests => {
                // Gli eventi UI non vengono conservati da Tauri. Su mobile la
                // WebView può essere sospesa o ricreata mentre il nodo continua
                // a girare: ripubblica quindi le richieste ancora in attesa.
                for (peer, name) in pending_contact_requests.iter() {
                    let _ = events.send(ClientEvent::ContactRequest {
                        peer_id: peer.to_string(),
                        name: name.clone(),
                        fingerprint: peer_fingerprint(*peer),
                        via_invite: pending_invite_cards.contains_key(peer),
                    });
                }
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_group_command(&mut self, command: ClientCommand) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            local_peer_id,
            swarm,
            known_contacts,
            contact_devices,
            triggers,
            sent_numbers,
            pending_handshakes,
            outbox,
            pending_messages,
            sessions,
            shared_emoticons,
            prepared_files_tx,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match command {
            ClientCommand::CreateChatGroup { name, members } => {
                let name = name.trim();
                let mut members = members
                    .into_iter()
                    .filter(|peer| *peer != local_peer_id)
                    .collect::<HashSet<_>>();
                if name.is_empty() || name.len() > 64 {
                    let _ = events.send(ClientEvent::Error {
                        message: "nome della chat non valido".into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                if members.len() < 2
                    || members.len() > 31
                    || members.iter().any(|peer| !known_contacts.contains(peer))
                {
                    let _ = events.send(ClientEvent::Error {
                        message: "scegli da 2 a 31 contatti validi".into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                let mut group_id = [0_u8; 16];
                OsRng.fill_bytes(&mut group_id);
                let mut member_ids = members
                    .drain()
                    .map(|peer| peer.to_string())
                    .collect::<Vec<_>>();
                member_ids.push(local_peer_id.to_string());
                member_ids.sort();
                let group = GroupChatEntry {
                    id: hex_group_id(&group_id),
                    name: name.to_owned(),
                    owner_peer: local_peer_id.to_string(),
                    members: member_ids,
                    admins: Vec::new(),
                    silenced: Vec::new(),
                    bans: Vec::new(),
                    revision: 1,
                };
                history.save_group_chat(&group)?;
                let definition = group_definition(&group)?;
                for member in group
                    .members
                    .iter()
                    .filter_map(|value| value.parse::<PeerId>().ok())
                    .filter(|peer| *peer != local_peer_id)
                {
                    send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        member,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::GroupDefinition(definition.clone()),
                    );
                }
                send_group_chats(&history.group_chats()?, events);
                let _ = events.send(ClientEvent::GroupConversationLoaded {
                    group_id: group.id,
                    messages: Vec::new(),
                });
            }
            ClientCommand::ModerateGroup {
                group_id,
                peer,
                action,
            } => {
                if let Err(message) = parse_group_id(&group_id) {
                    let _ = events.send(ClientEvent::Error { message });
                    return Ok(LoopFlow::Continue);
                }
                let Some(mut group) = history.group_chat(&group_id)? else {
                    let _ = events.send(ClientEvent::Error {
                        message: "chat di gruppo non trovata".into(),
                    });
                    return Ok(LoopFlow::Continue);
                };
                if let Err(message) =
                    apply_group_moderation(&mut group, local_peer_id, peer, action, now_ms())
                {
                    let _ = events.send(ClientEvent::Error {
                        message: message.into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                history.save_group_chat(&group)?;
                let definition = group_definition(&group)?;
                let recipients = group
                    .members
                    .iter()
                    .filter_map(|value| value.parse::<PeerId>().ok())
                    .filter(|member| *member != local_peer_id && sessions.contains_key(member))
                    .collect::<Vec<_>>();
                for member in recipients {
                    send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        member,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::GroupDefinition(definition.clone()),
                    );
                }
                send_group_chats(&history.group_chats()?, events);
            }
            ClientCommand::SendGroupText { group_id, text } => {
                if let Err(message) = check_outgoing_text(&text) {
                    let _ = events.send(ClientEvent::Error { message });
                    return Ok(LoopFlow::Continue);
                }
                let parsed_id = match parse_group_id(&group_id) {
                    Ok(id) => id,
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                        return Ok(LoopFlow::Continue);
                    }
                };
                let Some(group) = history.group_chat(&group_id)? else {
                    let _ = events.send(ClientEvent::Error {
                        message: "chat di gruppo non trovata".into(),
                    });
                    return Ok(LoopFlow::Continue);
                };
                let timestamp_ms = now_ms();
                if !group_member_can_send(&group, &local_peer_id.to_string(), timestamp_ms) {
                    let _ = events.send(ClientEvent::Error {
                        message: "non puoi scrivere: sei silenziato o bannato".into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                let emoticons = match resolve_emoticons(&text, triggers) {
                    Ok(emoticons) => emoticons,
                    Err(error) => {
                        let _ = events.send(ClientEvent::Error {
                            message: format!("trigger emoticon non valido: {error:?}"),
                        });
                        return Ok(LoopFlow::Continue);
                    }
                };
                let mut group_message_id = [0u8; 16];
                OsRng.fill_bytes(&mut group_message_id);
                let message = TextMessage {
                    text: text.clone(),
                    emoticons: emoticons.clone(),
                    id: group_message_id,
                };
                let group_event = ChatEvent::GroupText(GroupTextMessage {
                    group_id: parsed_id,
                    message: message.clone(),
                    timestamp_ms,
                });
                let recipients = group
                    .members
                    .iter()
                    .filter_map(|value| value.parse::<PeerId>().ok())
                    .filter(|peer| {
                        *peer != local_peer_id
                            && sessions.contains_key(peer)
                            && !active_group_ban(&group, &peer.to_string(), timestamp_ms)
                    })
                    .collect::<Vec<_>>();
                if recipients.is_empty() {
                    let _ = events.send(ClientEvent::Error {
                        message: "nessun partecipante è online con una sessione protetta".into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                for recipient in recipients {
                    for offer in new_emoticon_offers(
                        &emoticons,
                        recipient,
                        shared_emoticons,
                        triggers,
                        &args.emotes,
                        events,
                    ) {
                        deliver_event(
                            swarm,
                            sessions,
                            contact_devices,
                            sent_numbers,
                            pending_messages,
                            outbox,
                            pending_handshakes,
                            local_peer_id,
                            recipient,
                            ChatEvent::EmoticonOffer(offer),
                        );
                    }
                    deliver_event(
                        swarm,
                        sessions,
                        contact_devices,
                        sent_numbers,
                        pending_messages,
                        outbox,
                        pending_handshakes,
                        local_peer_id,
                        recipient,
                        group_event.clone(),
                    );
                }
                history.record(
                    &group_history_key(&group_id),
                    "out",
                    &format!("group-text:{local_peer_id}"),
                    &encode_text_history(&message)?,
                    timestamp_ms,
                )?;
                let _ = events.send(ClientEvent::GroupMessage {
                    message: ClientGroupMessage {
                        group_id,
                        sender_peer_id: local_peer_id.to_string(),
                        direction: "out".into(),
                        kind: "text".into(),
                        body: text,
                        timestamp_ms,
                        emoticons: emoticons
                            .iter()
                            .map(|span| ClientEmoticonSpan {
                                start: span.start,
                                end: span.end,
                                asset_id: hex_asset_id(&span.asset_id),
                            })
                            .collect(),
                        attachment_id: None,
                        attachment_mime: None,
                        event_id: None,
                    },
                });
            }
            ClientCommand::SendGroupFile { group_id, path } => {
                let parsed_id = match parse_group_id(&group_id) {
                    Ok(id) => id,
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                        return Ok(LoopFlow::Continue);
                    }
                };
                let Some(group) = history.group_chat(&group_id)? else {
                    let _ = events.send(ClientEvent::Error {
                        message: "chat di gruppo non trovata".into(),
                    });
                    return Ok(LoopFlow::Continue);
                };
                if let Err(message) =
                    group_file_recipients(&group, local_peer_id, sessions, now_ms())
                {
                    let _ = events.send(ClientEvent::Error {
                        message: message.into(),
                    });
                    return Ok(LoopFlow::Continue);
                }
                prepare_file(
                    prepared_files_tx,
                    FileTarget::Group {
                        group_id,
                        parsed_id,
                    },
                    path,
                );
            }
            ClientCommand::LoadOlderGroupMessages {
                group_id,
                before_event_id,
            } => {
                let messages =
                    group_conversation_messages(history, &group_id, Some(&before_event_id))?;
                let _ = events.send(ClientEvent::OlderGroupMessagesLoaded {
                    group_id,
                    has_more: messages.len() == HISTORY_PAGE,
                    messages,
                });
            }
            ClientCommand::ClearGroupConversation { group_id } => {
                match parse_group_id(&group_id).and_then(|_| {
                    history
                        .clear_conversation(&group_history_key(&group_id))
                        .map_err(|error| error.to_string())
                }) {
                    Ok(()) => {
                        let _ = events.send(ClientEvent::GroupConversationCleared { group_id });
                    }
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                    }
                }
            }
            ClientCommand::DeleteChatGroup { group_id } => {
                match parse_group_id(&group_id).and_then(|_| {
                    history
                        .delete_group_chat(&group_id)
                        .map_err(|error| error.to_string())
                }) {
                    Ok(()) => send_group_chats(&history.group_chats()?, events),
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                    }
                }
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    fn on_attachment_command(
        &mut self,
        command: ClientCommand,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Node {
            history,
            local_peer_id,
            swarm,
            contact_devices,
            incoming_nudge_limits,
            notification_mutes,
            sent_numbers,
            pending_offers,
            pending_transfers,
            sessions,
            waiting_offers,
            pending_emoticons,
            attachment_receiver,
            vault_jobs_tx,
            incoming_attachments,
            pending_inbound_offers,
            peer_names,
            args,
            events,
            ..
        } = self;
        let local_peer_id = *local_peer_id;
        match command {
            ClientCommand::ReadAttachment {
                id,
                mime,
                thumbnail,
            } => match parse_asset_id(&id) {
                Ok(id_bytes) => open_attachment(
                    attachment_receiver.vault(),
                    events,
                    id,
                    id_bytes,
                    mime,
                    thumbnail,
                ),
                Err(message) => {
                    let _ = events.send(ClientEvent::Error { message });
                }
            },
            ClientCommand::ExportAttachment { id, path } => {
                match parse_asset_id(&id).and_then(|id_bytes| {
                    attachment_receiver
                        .export(&id_bytes, &path)
                        .map_err(|error| error.to_string())
                }) {
                    Ok(()) => {
                        let _ = events.send(ClientEvent::AttachmentExported {
                            path: path.to_string_lossy().into_owned(),
                        });
                    }
                    Err(message) => {
                        let _ = events.send(ClientEvent::Error { message });
                    }
                }
            }
            ClientCommand::CancelFileTransfers { conversation } => {
                // Solo gli invii della conversazione aperta, e il destinatario
                // lo viene a sapere invece di restare con un file a metà.
                let in_scope = |peer: PeerId, group_id: Option<[u8; 16]>| match conversation
                    .as_deref()
                {
                    None => true,
                    Some(key) => match key.strip_prefix("group:") {
                        Some(group) => group_id.is_some_and(|id| hex_group_id(&id) == group),
                        None => {
                            group_id.is_none() && contact_devices.canonical(peer).to_string() == key
                        }
                    },
                };
                let mut cancelled = Vec::new();
                pending_offers.retain(|_, offer| {
                    let keep = !in_scope(offer.peer, offer.group_id);
                    if !keep {
                        cancelled.push((offer.peer, offer.manifest.attachment_id));
                    }
                    keep
                });
                pending_transfers.retain(|_, transfer| {
                    let keep = !in_scope(transfer.peer, transfer.group_id);
                    if !keep {
                        cancelled.push((transfer.peer, transfer.manifest.attachment_id));
                    }
                    keep
                });
                waiting_offers.retain(|_, offers| {
                    offers.retain(|offer| !in_scope(offer.peer, offer.group_id));
                    !offers.is_empty()
                });
                for (peer, attachment_id) in cancelled {
                    // Avviso senza retry: se non arriva, il destinatario scarta
                    // comunque le parti quando l'offerta scade.
                    send_event(
                        swarm,
                        sessions,
                        contact_devices,
                        peer,
                        local_peer_id,
                        sent_numbers,
                        ChatEvent::AttachmentCancelled(AttachmentCancel { attachment_id }),
                    );
                }
                let _ = events.send(ClientEvent::AttachmentTransfersCancelled);
            }
            ClientCommand::AcceptAttachment { offer_id } => {
                let Some(pending) = pending_inbound_offers.remove(&offer_id) else {
                    let _ = events.send(ClientEvent::Error {
                        message: "offerta file scaduta".into(),
                    });
                    return Ok(LoopFlow::Continue);
                };
                let client_event = client_event_from_chat(
                    pending.peer,
                    &pending.event,
                    "in",
                    true,
                    peer_names.get(&pending.peer).map(String::as_str),
                );
                let response = receive_event(
                    pending.peer,
                    &pending.event,
                    &mut Incoming {
                        pending_emoticons,
                        events,
                        nudge_limits: incoming_nudge_limits,
                        attachments: attachment_receiver,
                        history,
                        notifications: args.notifications,
                        notification_mutes,
                        peer_names,
                        local_peer_id,
                        incoming_attachments,
                        vault_jobs: vault_jobs_tx,
                    },
                );
                if !matches!(&response, ProtocolResponse::Rejected(_)) {
                    if let Some(event) = client_event {
                        let _ = events.send(event);
                    }
                }
                swarm
                    .behaviour_mut()
                    .secure_chat
                    .send_response(pending.channel, response)
                    .ok();
            }
            ClientCommand::RejectAttachment { offer_id } => {
                let Some(pending) = pending_inbound_offers.remove(&offer_id) else {
                    return Ok(LoopFlow::Continue);
                };
                swarm
                    .behaviour_mut()
                    .secure_chat
                    .send_response(
                        pending.channel,
                        ProtocolResponse::Rejected("file rifiutato dall'utente".into()),
                    )
                    .ok();
            }
            _ => {}
        }
        Ok(LoopFlow::Continue)
    }

    /// Revoca chiesta dall'utente su questo dispositivo.
    fn revoke_device(&mut self, peer: PeerId) -> Result<LoopFlow, Box<dyn Error>> {
        if !self.known_devices.contains_key(&peer) {
            return Err("dispositivo non collegato".into());
        }
        self.rotate_account(&[peer])?;
        Ok(LoopFlow::Continue)
    }

    /// Esclude `revoked`, adotta una chiave di account nuova e la passa agli
    /// altri dispositivi collegati raggiungibili. Quelli offline la ricevono
    /// al primo sync, che arriva sigillato con la chiave ritirata.
    fn rotate_account(&mut self, revoked: &[PeerId]) -> Result<(), Box<dyn Error>> {
        let previous = self.account_key;
        let revoked = revoked
            .iter()
            .map(|peer| {
                let name = self
                    .known_devices
                    .get(peer)
                    .map_or_else(|| peer.to_string(), |device| device.name.clone());
                (*peer, name)
            })
            .collect::<Vec<_>>();
        self.exclude_devices(&revoked)?;
        self.adopt_account_key(generate_secret())?;
        let rotation = AccountRotation {
            account_key: self.account_key,
            revoked: revoked_list(&self.history)?,
            from_name: self.display_name.clone(),
        };
        let online = self
            .known_devices
            .keys()
            .filter(|device| self.online_devices.contains(device))
            .copied()
            .collect::<Vec<_>>();
        for device in online {
            let sealed = devices::seal(
                &previous,
                &DeviceRequest::RotateAccount {
                    rotation: rotation.clone(),
                },
            )?;
            let request_id = self
                .swarm
                .behaviour_mut()
                .devices
                .send_request(&device, sealed);
            self.pending_device_requests
                .insert(request_id, PendingDeviceRequest::Rotate);
        }
        Ok(())
    }

    fn answer_account_rotation(
        &mut self,
        request_id: u64,
        accept: bool,
    ) -> Result<LoopFlow, Box<dyn Error>> {
        let Some((from, rotation)) = self.pending_rotations.remove(&request_id) else {
            return Ok(LoopFlow::Continue);
        };
        if !accept {
            // L'utente non riconosce la revoca: il dispositivo che l'ha chiesta
            // va trattato come compromesso.
            if self.known_devices.contains_key(&from) {
                self.rotate_account(&[from])?;
            }
            return Ok(LoopFlow::Continue);
        }
        if !self.known_devices.contains_key(&from) || self.revoked_devices.contains(&from) {
            return Err("il dispositivo che ha chiesto la rotazione non è più collegato".into());
        }
        let revoked = rotation
            .revoked
            .iter()
            .filter_map(|device| {
                Some((device.peer_id.parse::<PeerId>().ok()?, device.name.clone()))
            })
            .filter(|(peer, _)| *peer != self.local_peer_id)
            .collect::<Vec<_>>();
        self.exclude_devices(&revoked)?;
        self.adopt_account_key(rotation.account_key)?;
        // Altre richieste della stessa rotazione (arrivate a ogni sync) non
        // servono più.
        self.pending_rotations
            .retain(|_, (_, pending)| pending.account_key != rotation.account_key);
        let descriptor = local_device_descriptor(
            self.local_peer_id,
            &self.display_name,
            contact_addresses(&self.swarm, &self.args.relays, self.local_peer_id),
        );
        if self.known_devices.contains_key(&from) {
            send_device_sync(
                &mut self.swarm,
                &self.history,
                &self.account_key,
                &mut self.pending_device_requests,
                from,
                descriptor,
            )?;
        }
        Ok(LoopFlow::Continue)
    }

    /// Mette in attesa una rotazione arrivata da `from` e chiede all'utente.
    fn queue_account_rotation(&mut self, from: PeerId, rotation: AccountRotation) {
        if rotation.account_key == self.account_key
            || self
                .pending_rotations
                .values()
                .any(|(_, pending)| pending.account_key == rotation.account_key)
        {
            return;
        }
        if rotation
            .revoked
            .iter()
            .any(|device| device.peer_id == self.local_peer_id.to_string())
        {
            log::warn!("{from} ha revocato questo dispositivo");
            return;
        }
        self.rotation_counter += 1;
        let request_id = self.rotation_counter;
        let revoked = rotation
            .revoked
            .iter()
            .map(|device| device.name.clone())
            .collect();
        let _ = self.events.send(ClientEvent::AccountRotationRequested {
            request_id,
            from_peer_id: from.to_string(),
            from_name: rotation.from_name.clone(),
            revoked,
        });
        self.pending_rotations.insert(request_id, (from, rotation));
    }

    fn exclude_devices(&mut self, revoked: &[(PeerId, String)]) -> Result<(), Box<dyn Error>> {
        for (peer, name) in revoked {
            self.history.revoke_device(&peer.to_string(), name)?;
            self.known_devices.remove(peer);
            self.online_devices.remove(peer);
            self.revoked_devices.insert(*peer);
            let _ = self.swarm.disconnect_peer_id(*peer);
            let _ = self.events.send(ClientEvent::DeviceRevoked {
                peer_id: peer.to_string(),
            });
        }
        send_devices(&self.known_devices, &self.online_devices, &self.events);
        Ok(())
    }

    /// Passa a una nuova chiave di account: la vecchia resta solo per
    /// riconoscere i dispositivi che non l'hanno ancora cambiata.
    fn adopt_account_key(&mut self, account_key: [u8; 32]) -> Result<(), Box<dyn Error>> {
        let store = self
            .args
            .account_key_store
            .as_ref()
            .ok_or("la chiave di account non si può salvare su questo client")?;
        store(account_key)
            .map_err(|error| format!("nuova chiave di account non salvata: {error}"))?;
        self.history
            .retire_account_key(&devices::key_id(&self.account_key), &self.account_key)?;
        self.account_key = account_key;
        self.account_identity = AccountIdentity::from_account_key(&self.account_key)?;
        self.account_proof = self.account_identity.proof(self.local_peer_id).ok();
        // I contatti legano i nostri dispositivi al nuovo account solo
        // vedendo la nuova prova.
        for contact in self.peers.iter().copied().collect::<Vec<_>>() {
            send_event(
                &mut self.swarm,
                &mut self.sessions,
                &self.contact_devices,
                contact,
                self.local_peer_id,
                &mut self.sent_numbers,
                ChatEvent::Presence(presence_update(
                    &self.display_name,
                    &self.presence_status,
                    true,
                    &self.account_proof,
                )),
            );
        }
        Ok(())
    }
}

/// Un testo oltre il limite del protocollo verrebbe rifiutato dal destinatario
/// (e prima di questo controllo spegneva il nodo): lo fermiamo subito con un
/// errore leggibile.
fn check_outgoing_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("il messaggio è vuoto".into());
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "messaggio troppo lungo: massimo {} KB",
            MAX_TEXT_BYTES / 1024
        ));
    }
    Ok(())
}

/// Motivi di rifiuto con cui il destinatario segnala che la sessione cifrata
/// del mittente non è più valida. Fanno parte del protocollo: un mittente li
/// riconosce per rifare l'handshake e reinviare invece di perdere il messaggio.
const REJECT_NO_SESSION: &str = "sessione sicura non disponibile";
const REJECT_UNDECRYPTABLE: &str = "messaggio cifrato non valido";
/// Errori di decifratura consecutivi dopo cui una sessione si considera rotta.
const MAX_DECRYPT_FAILURES: u8 = 3;
/// Tentativi di consegna di un messaggio di testo, handshake compresi.
const MAX_DELIVERY_ATTEMPTS: u8 = 4;

fn is_session_rejection(reason: &str) -> bool {
    reason == REJECT_NO_SESSION || reason == REJECT_UNDECRYPTABLE
}

/// Rimette in coda un messaggio da reinviare prima di quelli mai partiti ma
/// dopo gli altri già in reinvio: le risposte arrivano nell'ordine di invio,
/// quindi l'ordine originale si conserva.
fn requeue_in_order(queue: &mut VecDeque<(ChatEvent, u8)>, event: ChatEvent, attempts: u8) {
    let position = queue
        .iter()
        .position(|(_, queued_attempts)| *queued_attempts == 0)
        .unwrap_or(queue.len());
    queue.insert(position, (event, attempts));
}

/// Un messaggio non è arrivato: l'utente deve saperlo, e per i testi la UI
/// segna proprio quel messaggio come non consegnato.
fn report_undelivered(
    events: &mpsc::UnboundedSender<ClientEvent>,
    shared_emoticons: &mut HashSet<(PeerId, [u8; 32])>,
    peer: PeerId,
    event: &ChatEvent,
    reason: &str,
) {
    log::warn!("messaggio non consegnato a {peer}: {reason}");
    match event {
        ChatEvent::Text(message) => {
            let _ = events.send(ClientEvent::MessageFailed {
                peer_id: peer.to_string(),
                event_id: hex_group_id(&message.id),
                reason: reason.to_owned(),
            });
        }
        ChatEvent::EmoticonOffer(offer) => {
            // Non più "già condivisa": il prossimo messaggio la riprova.
            shared_emoticons.remove(&(peer, offer.metadata.asset_id));
        }
        other => {
            let what = match other {
                ChatEvent::Nudge(_) => "il trillo",
                ChatEvent::DeleteMessage(_) => "la cancellazione per tutti",
                ChatEvent::GroupText(_) => "un messaggio di gruppo",
                _ => "un messaggio",
            };
            let _ = events.send(ClientEvent::Error {
                message: format!("{what} non è stato consegnato: {reason}"),
            });
        }
    }
}

/// Un invio di file finito male: la UI deve chiudere la barra di quel file,
/// altrimenti resta ferma per sempre.
/// L'allegato proposto da un'offerta di file (diretta o di gruppo).
fn offered_attachment_id(event: &ChatEvent) -> Option<[u8; 32]> {
    match event {
        ChatEvent::AttachmentOffer(manifest) => Some(manifest.attachment_id),
        ChatEvent::GroupAttachmentOffer(offer) => Some(offer.manifest.attachment_id),
        _ => None,
    }
}

fn report_file_failed(events: &mpsc::UnboundedSender<ClientEvent>, filename: &str, reason: &str) {
    log::warn!("invio file {filename} non riuscito: {reason}");
    let _ = events.send(ClientEvent::AttachmentFailed {
        filename: filename.to_owned(),
        reason: reason.to_owned(),
    });
}

/// Invia l'offerta di un file a un contatto. La prima volta va in cronologia
/// e in chat solo se parte davvero; altrimenti la UI riceve `AttachmentFailed`.
/// Un reinvio (dopo un nuovo handshake) non crea un secondo messaggio.
#[allow(clippy::too_many_arguments)]
fn offer_file(
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_offers: &mut HashMap<request_response::OutboundRequestId, PendingOffer>,
    history: &History,
    events: &mpsc::UnboundedSender<ClientEvent>,
    local_peer_id: PeerId,
    offer: PendingOffer,
) {
    let Some(request_id) = send_event(
        swarm,
        sessions,
        devices,
        offer.peer,
        local_peer_id,
        sent_numbers,
        offer_event(&offer),
    ) else {
        report_file_failed(
            events,
            &offer.manifest.filename,
            "canale cifrato non pronto",
        );
        return;
    };
    if offer.retries > 0 || offer.group_id.is_some() {
        pending_offers.insert(request_id, offer);
        return;
    }
    record(
        history,
        &offer.peer,
        "out",
        "file",
        &encode_attachment(&offer.manifest),
    );
    let _ = events.send(ClientEvent::Message {
        message: ClientMessage {
            peer_id: offer.peer.to_string(),
            direction: "out".into(),
            kind: "file".into(),
            body: offer.manifest.filename.clone(),
            timestamp_ms: now_ms(),
            emoticons: Vec::new(),
            attachment_id: Some(hex_asset_id(&offer.manifest.attachment_id)),
            attachment_mime: Some(offer.manifest.mime.clone()),
            event_id: None,
            relayed: false,
        },
    });
    pending_offers.insert(request_id, offer);
}

fn offer_event(offer: &PendingOffer) -> ChatEvent {
    match offer.group_id {
        Some(group_id) => ChatEvent::GroupAttachmentOffer(GroupAttachmentOffer {
            group_id,
            manifest: offer.manifest.clone(),
        }),
        None => ChatEvent::AttachmentOffer(offer.manifest.clone()),
    }
}

/// Le offerte in attesa per un peer, cercate anche sul contatto a cui
/// appartiene (le code sono indicizzate sul contatto).
fn take_waiting_offers(
    waiting: &mut HashMap<PeerId, Vec<PendingOffer>>,
    devices: &ContactDevices,
    peer: PeerId,
) -> Vec<PendingOffer> {
    let mut offers = waiting.remove(&peer).unwrap_or_default();
    let contact = devices.canonical(peer);
    if contact != peer {
        offers.extend(waiting.remove(&contact).unwrap_or_default());
    }
    offers
}

/// Destinazione di un file in preparazione (hash in corso).
enum FileTarget {
    Peer(PeerId),
    Group {
        group_id: String,
        parsed_id: [u8; 16],
    },
    /// Comando `file` della CLI: a tutti i peer collegati.
    AllPeers,
}

struct PreparedFile {
    target: FileTarget,
    path: PathBuf,
    manifest: Result<AttachmentManifest, String>,
}

/// Esito della verifica finale di un file ricevuto.
struct VaultJobDone {
    peer: PeerId,
    id: [u8; 32],
    result: Result<CompletedAttachment, String>,
}

/// Calcola il manifest (hash BLAKE3 dell'intero file) in un thread bloccante.
/// Lato massimo delle anteprime in chat: bastano poche decine di KB invece
/// di copiare l'intero file (fino a 50 MB in base64) nella UI.
const THUMBNAIL_SIDE: u32 = 640;

/// Apre un allegato del vault in un thread a parte: decifrare e codificare
/// decine di MB non deve fermare il loop. Con `thumbnail` le immagini
/// arrivano ridotte, per l'anteprima nel messaggio.
fn open_attachment(
    vault: attachments::Vault,
    events: &mpsc::UnboundedSender<ClientEvent>,
    id: String,
    id_bytes: [u8; 32],
    mime: String,
    thumbnail: bool,
) {
    let events = events.clone();
    tokio::task::spawn_blocking(move || {
        let result = vault
            .read(&id_bytes)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                if thumbnail && mime.starts_with("image/") {
                    thumbnail_data_url(&bytes)
                        .map_err(|error| format!("anteprima non disponibile: {error}"))
                } else {
                    Ok(format!("data:{mime};base64,{}", BASE64.encode(bytes)))
                }
            });
        let _ = events.send(match result {
            Ok(data_url) => ClientEvent::AttachmentOpened {
                id,
                data_url,
                thumbnail,
            },
            Err(message) => ClientEvent::Error { message },
        });
    });
}

fn thumbnail_data_url(bytes: &[u8]) -> Result<String, Box<dyn Error>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    // Niente "bombe" di decompressione: dimensioni e memoria limitate.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?.thumbnail(THUMBNAIL_SIDE, THUMBNAIL_SIDE);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    Ok(format!(
        "data:image/png;base64,{}",
        BASE64.encode(png.into_inner())
    ))
}

fn prepare_file(tx: &mpsc::UnboundedSender<PreparedFile>, target: FileTarget, path: PathBuf) {
    let tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let manifest = build_manifest(&path).map_err(|error| error.to_string());
        let _ = tx.send(PreparedFile {
            target,
            path,
            manifest,
        });
    });
}

fn spawn_finalize(tx: &mpsc::UnboundedSender<VaultJobDone>, peer: PeerId, job: FinalizeJob) {
    let tx = tx.clone();
    let id = job.attachment_id();
    tokio::task::spawn_blocking(move || {
        let result = job.run().map_err(|error| error.to_string());
        let _ = tx.send(VaultJobDone { peer, id, result });
    });
}

/// Membri del gruppo a cui si può mandare un file adesso.
fn group_file_recipients(
    group: &GroupChatEntry,
    local_peer_id: PeerId,
    sessions: &HashMap<PeerId, RatchetSession>,
    timestamp_ms: u64,
) -> Result<Vec<PeerId>, &'static str> {
    if !group_member_can_send(group, &local_peer_id.to_string(), timestamp_ms) {
        return Err("non puoi inviare file: sei silenziato o bannato");
    }
    let recipients = group
        .members
        .iter()
        .filter_map(|value| value.parse::<PeerId>().ok())
        .filter(|peer| {
            *peer != local_peer_id
                && sessions.contains_key(peer)
                && !active_group_ban(group, &peer.to_string(), timestamp_ms)
        })
        .collect::<Vec<_>>();
    if recipients.is_empty() {
        return Err("nessun partecipante è online con una sessione protetta");
    }
    Ok(recipients)
}

/// Esito della gestione di un singolo evento del loop principale.
enum LoopFlow {
    Continue,
    Stop,
}

/// Un errore durante la gestione di un evento non deve spegnere il nodo:
/// viene mostrato all'utente e il loop prosegue con l'evento successivo.
fn report_loop_error(events: &mpsc::UnboundedSender<ClientEvent>, error: &dyn Error) {
    log::warn!("errore gestito nel loop: {error}");
    log::warn!("errore gestito nel loop: {error}");
    let _ = events.send(ClientEvent::Error {
        message: error.to_string(),
    });
}

/// Una pagina di messaggi di un gruppo, dal più vecchio, fino a
/// `HISTORY_PAGE` e prima di `before` (id evento) se indicato.
fn group_conversation_messages(
    history: &History,
    group_id: &str,
    before: Option<&str>,
) -> Result<Vec<ClientGroupMessage>, Box<dyn Error>> {
    Ok(history
        .conversation_before(&group_history_key(group_id), before, HISTORY_PAGE)?
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
            let text = (kind == "text").then(|| decode_text_history(&entry.body));
            Some(ClientGroupMessage {
                group_id: group_id.to_owned(),
                sender_peer_id,
                direction: entry.direction,
                kind: kind.into(),
                body: text.as_ref().map_or_else(
                    || {
                        attachment
                            .as_ref()
                            .map_or(entry.body, |item| item.2.clone())
                    },
                    |message| message.text.clone(),
                ),
                timestamp_ms: entry.timestamp_ms,
                emoticons: text.as_ref().map(client_emoticon_spans).unwrap_or_default(),
                attachment_id: attachment.as_ref().map(|item| item.0.clone()),
                attachment_mime: attachment.map(|item| item.1),
                event_id: Some(entry.event_id),
            })
        })
        .collect())
}

/// Messaggi più recenti di una conversazione nel formato atteso dalla UI.
fn conversation_messages(
    history: &History,
    peer: &str,
) -> Result<Vec<ClientMessage>, Box<dyn Error>> {
    conversation_page(history, peer, None)
}

/// Una pagina di messaggi di una conversazione, dal più vecchio, fino a
/// `HISTORY_PAGE` e prima di `before` (id evento) se indicato.
fn conversation_page(
    history: &History,
    peer: &str,
    before: Option<&str>,
) -> Result<Vec<ClientMessage>, Box<dyn Error>> {
    Ok(history
        .conversation_before(peer, before, HISTORY_PAGE)?
        .into_iter()
        .rev()
        .map(|entry| {
            let attachment = (entry.kind == "file")
                .then(|| decode_attachment(&entry.body))
                .flatten();
            let text = (entry.kind == "text").then(|| decode_text_history(&entry.body));
            ClientMessage {
                peer_id: entry.peer,
                direction: entry.direction,
                kind: entry.kind,
                body: text.as_ref().map_or_else(
                    || {
                        attachment
                            .as_ref()
                            .map_or(entry.body, |item| item.2.clone())
                    },
                    |message| message.text.clone(),
                ),
                timestamp_ms: entry.timestamp_ms,
                emoticons: text.as_ref().map(client_emoticon_spans).unwrap_or_default(),
                attachment_id: attachment.as_ref().map(|item| item.0.clone()),
                attachment_mime: attachment.map(|item| item.1),
                event_id: Some(entry.event_id),
                relayed: false,
            }
        })
        .collect())
}

/// Un contatto è una PERSONA (un account), non un singolo dispositivo. Questa
/// mappa lega i dispositivi collegati di un contatto al contatto stesso: le
/// prove di account ricevute nelle presence popolano `alias`, così un
/// dispositivo nuovo di un contatto già approvato non genera né una richiesta
/// di contatto né un duplicato in rubrica.
#[derive(Default)]
struct ContactDevices {
    /// contatto -> account
    accounts: HashMap<PeerId, String>,
    /// account -> contatto
    contacts: HashMap<String, PeerId>,
    /// dispositivo -> contatto
    alias: HashMap<PeerId, PeerId>,
}

impl ContactDevices {
    /// Lega un contatto al suo account. Ritorna `true` se il legame è nuovo o
    /// cambiato (va persistito e sincronizzato).
    fn bind(&mut self, contact: PeerId, account: String) -> bool {
        if self
            .accounts
            .get(&contact)
            .is_some_and(|known| *known == account)
        {
            return false;
        }
        if let Some(previous) = self.accounts.insert(contact, account.clone()) {
            self.contacts.remove(&previous);
            self.alias.retain(|_, owner| *owner != contact);
        }
        self.contacts.insert(account, contact);
        true
    }

    /// Registra `device` come dispositivo del contatto che possiede `account`.
    fn attach(&mut self, device: PeerId, account: &str) -> Option<PeerId> {
        let contact = *self.contacts.get(account)?;
        if device != contact {
            self.alias.insert(device, contact);
        }
        Some(contact)
    }

    /// Contatto a cui appartiene il peer: se stesso se non è un dispositivo.
    fn canonical(&self, peer: PeerId) -> PeerId {
        self.alias.get(&peer).copied().unwrap_or(peer)
    }

    fn is_device(&self, peer: PeerId) -> bool {
        self.alias.contains_key(&peer)
    }

    fn devices(&self, contact: PeerId) -> impl Iterator<Item = PeerId> + '_ {
        self.alias
            .iter()
            .filter(move |(_, owner)| **owner == contact)
            .map(|(device, _)| *device)
    }

    /// Dimentica contatto e dispositivi (contatto cancellato): ritorna i
    /// dispositivi da scollegare.
    fn forget(&mut self, contact: PeerId) -> Vec<PeerId> {
        if let Some(account) = self.accounts.remove(&contact) {
            self.contacts.remove(&account);
        }
        let devices = self.devices(contact).collect::<Vec<_>>();
        self.alias.retain(|_, owner| *owner != contact);
        devices
    }

    /// Dispositivo su cui inviare: il contatto stesso se ha una sessione,
    /// altrimenti un suo dispositivo collegato che ce l'ha.
    // ponytail: con più dispositivi online ne sceglie uno qualsiasi; basta,
    // perché i dispositivi di un contatto si sincronizzano fra loro.
    fn route(&self, contact: PeerId, sessions: &HashMap<PeerId, RatchetSession>) -> PeerId {
        if sessions.contains_key(&contact) {
            return contact;
        }
        self.devices(contact)
            .find(|device| sessions.contains_key(device))
            .unwrap_or(contact)
    }

    fn online(&self, contact: PeerId, peers: &HashSet<PeerId>) -> bool {
        peers.contains(&contact) || self.devices(contact).any(|device| peers.contains(&device))
    }

    fn secure(&self, contact: PeerId, sessions: &HashMap<PeerId, RatchetSession>) -> bool {
        sessions.contains_key(&self.route(contact, sessions))
    }
}

/// Esito del riconoscimento di una presence in ingresso.
#[derive(Debug, Default, PartialEq, Eq)]
struct PresenceSource {
    /// Contatto a cui attribuire la presence, se riconosciuto.
    contact: Option<PeerId>,
    /// Account che ha firmato la prova, se la firma è valida.
    account: Option<String>,
    /// Legame contatto -> account da persistere.
    bind: Option<(PeerId, String)>,
    /// Due contatti in rubrica sono la stessa persona: (da fondere, superstite).
    merge: Option<(PeerId, PeerId)>,
}

/// Riconosce il mittente di una presence PRIMA di decidere se serve
/// l'approvazione dell'utente: un dispositivo che dimostra di appartenere
/// all'account di un contatto già approvato non è uno sconosciuto.
fn recognize_presence(
    devices: &mut ContactDevices,
    known_contacts: &HashSet<PeerId>,
    contacts: &[ContactEntry],
    peer: PeerId,
    presence: &PresenceUpdate,
) -> PresenceSource {
    let account = presence
        .account
        .as_ref()
        .and_then(|proof| verify_account_proof(proof, peer));
    if known_contacts.contains(&peer) && !devices.is_device(peer) {
        let Some(account) = account else {
            return PresenceSource {
                contact: Some(peer),
                ..PresenceSource::default()
            };
        };
        let verified = Some(account.clone());
        // Stesso account di un ALTRO contatto in rubrica: sono la stessa
        // persona vista da due dispositivi, vanno fusi.
        if let Some(other) = devices
            .contacts
            .get(&account)
            .copied()
            .filter(|other| *other != peer)
        {
            let (keep, drop) = merge_target(contacts, other, peer);
            devices.forget(drop);
            let bind = devices
                .bind(keep, account.clone())
                .then_some((keep, account));
            // Il peer del duplicato resta raggiungibile come dispositivo del
            // contatto superstite.
            devices.alias.insert(drop, keep);
            return PresenceSource {
                contact: Some(keep),
                account: verified,
                bind,
                merge: Some((drop, keep)),
            };
        }
        let bind = devices
            .bind(peer, account.clone())
            .then_some((peer, account));
        return PresenceSource {
            contact: Some(peer),
            account: verified,
            bind,
            merge: None,
        };
    }
    if let Some(contact) = devices.alias.get(&peer).copied() {
        // Un dispositivo che ora firma per un altro account (per esempio
        // dopo che la persona lo ha revocato) non è più suo.
        if account
            .as_ref()
            .is_none_or(|account| devices.accounts.get(&contact) == Some(account))
        {
            return PresenceSource {
                contact: Some(contact),
                account,
                ..PresenceSource::default()
            };
        }
        devices.alias.remove(&peer);
    }
    PresenceSource {
        contact: account
            .as_ref()
            .and_then(|account| devices.attach(peer, account)),
        account,
        ..PresenceSource::default()
    }
}

/// Stato toccato dall'esito di `recognize_presence`.
struct PresenceBinding<'a> {
    history: &'a History,
    events: &'a mpsc::UnboundedSender<ClientEvent>,
    known_contacts: &'a mut HashSet<PeerId>,
    ignored_contacts: &'a mut HashSet<PeerId>,
    peer_names: &'a mut HashMap<PeerId, String>,
    pending_contact_requests: &'a mut HashMap<PeerId, String>,
}

/// Applica l'esito del riconoscimento: salva il legame con l'account, fonde i
/// contatti duplicati e promuove il peer a dispositivo di un contatto noto.
fn apply_presence_source(
    source: &PresenceSource,
    peer: PeerId,
    context: &mut PresenceBinding<'_>,
) -> Result<(), Box<dyn Error>> {
    if let Some((contact, account)) = &source.bind {
        context
            .history
            .bind_contact_account(&contact.to_string(), account, now_ms())?;
    }
    if let Some((duplicate, keep)) = source.merge {
        let moved =
            context
                .history
                .merge_contact(&duplicate.to_string(), &keep.to_string(), now_ms())?;
        log::info!("contatti uniti: {duplicate} -> {keep} ({moved} messaggi)");
        log::info!("contatti uniti: {duplicate} -> {keep} ({moved} messaggi)");
        context.peer_names.remove(&duplicate);
        let _ = context.events.send(ClientEvent::ContactRemoved {
            peer_id: duplicate.to_string(),
            reason: "merged".into(),
        });
        let _ = context.events.send(ClientEvent::ConversationLoaded {
            peer_id: keep.to_string(),
            messages: conversation_messages(context.history, &keep.to_string())?,
        });
    }
    let Some(contact) = source.contact.filter(|contact| *contact != peer) else {
        return Ok(());
    };
    // Dispositivo verificato di un contatto già approvato: nessuna richiesta di
    // contatto, nessun duplicato in rubrica. Vale esattamente i permessi del
    // contatto a cui appartiene.
    if context.known_contacts.insert(peer) {
        log::info!("dispositivo di {contact} riconosciuto: {peer}");
        log::info!("dispositivo di {contact} riconosciuto: {peer}");
    }
    if context.pending_contact_requests.remove(&peer).is_some() {
        // Richiesta già mostrata all'utente ma nel frattempo il peer si è
        // rivelato un dispositivo di un contatto noto: ritira la domanda.
        let _ = context.events.send(ClientEvent::ContactRemoved {
            peer_id: peer.to_string(),
            reason: "withdrawn".into(),
        });
    }
    if context.ignored_contacts.remove(&peer) {
        context.history.allow_contact(&peer.to_string())?;
    }
    if let Some(name) = context.peer_names.get(&contact).cloned() {
        context.peer_names.insert(peer, name);
    }
    Ok(())
}

/// Quale dei due contatti sopravvive alla fusione: quello importato da contact
/// card (link presente) e, a parità, il peer id minore. Deterministico, così
/// tutti i dispositivi fondono nella stessa direzione.
fn merge_target(contacts: &[ContactEntry], first: PeerId, second: PeerId) -> (PeerId, PeerId) {
    let linked = |peer: PeerId| {
        contacts
            .iter()
            .any(|contact| contact.peer == peer.to_string() && !contact.link.is_empty())
    };
    match (linked(first), linked(second)) {
        (true, false) => (first, second),
        (false, true) => (second, first),
        _ if first.to_string() <= second.to_string() => (first, second),
        _ => (second, first),
    }
}

fn presence_update(
    display_name: &str,
    status: &str,
    online: bool,
    account: &Option<AccountProof>,
) -> PresenceUpdate {
    PresenceUpdate {
        display_name: display_name.to_owned(),
        online,
        status: status.to_owned(),
        account: account.clone(),
    }
}

fn emit_contact_status(
    events: &mpsc::UnboundedSender<ClientEvent>,
    peer: PeerId,
    event: &ChatEvent,
) {
    if let ChatEvent::Presence(presence) = event {
        if !presence.status.is_empty() {
            let _ = events.send(ClientEvent::ContactStatus {
                peer_id: peer.to_string(),
                status: presence.status.clone(),
            });
        }
    }
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
                fingerprint: peer_fingerprint(peer),
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
                event_id: (message.id != [0u8; 16]).then(|| hex_group_id(&message.id)),
                relayed: false,
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
                event_id: None,
                relayed: false,
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
                event_id: None,
                relayed: false,
            },
        }),
        ChatEvent::EmoticonOffer(_) => None,
        ChatEvent::AttachmentChunk(_) => None,
        ChatEvent::DeleteMessage(_) | ChatEvent::AttachmentCancelled(_) => None,
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
    _trusted_connection: bool,
) -> bool {
    // La sola connessione non basta: uno sconosciuto in attesa di approvazione
    // non deve poter stabilire un canale cifrato (che lo aggiungerebbe come
    // contatto). Solo contatti già noti o peer applicativi attivi.
    known_contact || active_peer
}

/// Tentativi di un messaggio: rifiuti per sessione scaduta, errori di rete e
/// timeout di conferma consumano lo stesso budget.
fn next_delivery_attempt(attempts: u8) -> Option<u8> {
    (attempts < MAX_DELIVERY_ATTEMPTS).then_some(attempts + 1)
}

fn next_request_retry(attempts: u8) -> Option<u8> {
    (attempts < MAX_REQUEST_RETRIES).then_some(attempts + 1)
}

fn maybe_start_hybrid_handshake(
    swarm: &mut Swarm<Behaviour>,
    handshakes: &mut Handshakes,
    established: &HashMap<PeerId, RatchetSession>,
    local_peer: PeerId,
    remote_peer: PeerId,
) {
    if !needs_outbound_handshake(
        local_peer,
        remote_peer,
        established.contains_key(&remote_peer),
        handshakes.pending_for(remote_peer),
    ) {
        return;
    }
    handshakes.start(swarm, local_peer, remote_peer, false);
}

/// Ogni quanto si rinnova una sessione cifrata con un nuovo handshake ibrido
/// (ML-KEM + X25519, chiavi effimere fresche). Il ratchet è solo simmetrico:
/// chi ruba lo stato di una sessione legge tutto ciò che segue, ma solo fino
/// al rinnovo successivo (post-compromise security).
const SESSION_REKEY_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Rinnovo anticipato dopo questo numero di messaggi nella stessa sessione.
const SESSION_REKEY_MESSAGES: u64 = 1_000;
/// Per quanto la sessione sostituita decifra ancora i messaggi in volo.
const RETIRED_SESSION_GRACE: Duration = Duration::from_secs(120);

#[cfg(test)]
static SESSION_REKEYS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Handshake in corso e rinnovo periodico delle sessioni.
struct Handshakes {
    pending: HashMap<request_response::OutboundRequestId, (PeerId, HybridInitiator)>,
    /// Chiave ML-DSA del dispositivo: firma ogni handshake.
    identity: crypto::MlDsaIdentity,
    rekey_interval: Duration,
    /// Quando è nata ogni sessione attiva.
    started: HashMap<PeerId, Instant>,
    /// Sessione nuova del responder, in attesa del primo messaggio che la usa.
    staged: HashMap<PeerId, RatchetSession>,
    /// Sessione sostituita, ancora valida per i messaggi in volo.
    retired: HashMap<PeerId, (RatchetSession, Instant)>,
}

impl Handshakes {
    fn new(identity: crypto::MlDsaIdentity, rekey_interval: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            identity,
            rekey_interval,
            started: HashMap::new(),
            staged: HashMap::new(),
            retired: HashMap::new(),
        }
    }

    fn pending_for(&self, peer: PeerId) -> bool {
        self.pending.values().any(|(pending, _)| *pending == peer)
    }

    fn start(&mut self, swarm: &mut Swarm<Behaviour>, local: PeerId, remote: PeerId, rekey: bool) {
        match HybridInitiator::start(local, remote, &self.identity, rekey) {
            Ok((initiator, hello)) => {
                let request_id = swarm.behaviour_mut().handshake.send_request(&remote, hello);
                self.pending.insert(request_id, (remote, initiator));
            }
            Err(error) => log::warn!("avvio handshake ibrido fallito: {error}"),
        }
    }

    /// Installa la sessione nata da un handshake. Se è un rinnovo, la vecchia
    /// resta per poco a decifrare i messaggi partiti prima.
    fn install(
        &mut self,
        sessions: &mut HashMap<PeerId, RatchetSession>,
        peer: PeerId,
        session: RatchetSession,
        rekey: bool,
    ) {
        self.staged.remove(&peer);
        match sessions.insert(peer, session) {
            Some(old) if rekey => {
                #[cfg(test)]
                SESSION_REKEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.retired.insert(peer, (old, Instant::now()));
            }
            _ => {
                self.retired.remove(&peer);
            }
        }
        self.started.insert(peer, Instant::now());
    }

    fn stage(&mut self, peer: PeerId, session: RatchetSession) {
        self.staged.insert(peer, session);
    }

    fn forget(&mut self, peer: PeerId) {
        self.pending.retain(|_, (pending, _)| *pending != peer);
        self.started.remove(&peer);
        self.staged.remove(&peer);
        self.retired.remove(&peer);
    }

    /// L'iniziatore rinnova le sessioni troppo vecchie o troppo usate.
    fn rekey_due_sessions(
        &mut self,
        swarm: &mut Swarm<Behaviour>,
        sessions: &HashMap<PeerId, RatchetSession>,
        local: PeerId,
    ) {
        self.retired
            .retain(|_, (_, retired_at)| retired_at.elapsed() < RETIRED_SESSION_GRACE);
        self.started.retain(|peer, _| sessions.contains_key(peer));
        self.staged.retain(|peer, _| sessions.contains_key(peer));
        for (peer, session) in sessions {
            let started = *self.started.entry(*peer).or_insert_with(Instant::now);
            let due = started.elapsed() >= self.rekey_interval
                || session.messages() >= SESSION_REKEY_MESSAGES;
            if due && should_initiate(local, *peer) && !self.pending_for(*peer) {
                log::info!("rinnovo della sessione con {peer}");
                self.start(swarm, local, *peer, true);
            }
        }
    }

    /// Decifra con la sessione attiva; durante un rinnovo prova anche quella
    /// nuova (che diventa attiva al primo messaggio) e quella appena ritirata.
    fn decrypt(
        &mut self,
        sessions: &mut HashMap<PeerId, RatchetSession>,
        peer: PeerId,
        local: PeerId,
        message: &RatchetMessage,
    ) -> Result<Envelope, &'static str> {
        let Some(current) = sessions.get_mut(&peer) else {
            return Err(REJECT_NO_SESSION);
        };
        if let Ok(envelope) = decrypt_envelope(current, peer, local, message) {
            return Ok(envelope);
        }
        if let Some(mut staged) = self.staged.remove(&peer) {
            if let Ok(envelope) = decrypt_envelope(&mut staged, peer, local, message) {
                self.install(sessions, peer, staged, true);
                return Ok(envelope);
            }
            self.staged.insert(peer, staged);
        }
        if let Some((retired, _)) = self.retired.get_mut(&peer) {
            if let Ok(envelope) = decrypt_envelope(retired, peer, local, message) {
                return Ok(envelope);
            }
        }
        Err(REJECT_UNDECRYPTABLE)
    }
}

/// Fissa la chiave ML-DSA di un peer al primo handshake firmato e poi la
/// esige: un peer che cambia chiave, o che smette di firmare dopo averlo
/// fatto (downgrade), viene rifiutato. I peer che non hanno mai firmato
/// (client vecchi) restano accettati con la sola autenticazione classica.
fn pin_pq_identity(history: &History, peer: PeerId, key: Option<&[u8]>) -> Result<(), String> {
    let peer = peer.to_string();
    let pinned = history
        .pq_identity(&peer)
        .map_err(|error| error.to_string())?;
    match (pinned, key) {
        (Some(pinned), Some(key)) if pinned == key => Ok(()),
        (Some(_), Some(_)) => Err("la chiave ML-DSA del contatto è cambiata".into()),
        (Some(_), None) => Err("handshake senza firma ML-DSA da un contatto che la usava".into()),
        (None, Some(key)) => history
            .pin_pq_identity(&peer, key)
            .map_err(|error| error.to_string()),
        (None, None) => {
            log::warn!(
                "{peer} non firma l'handshake: client vecchio, solo autenticazione classica"
            );
            Ok(())
        }
    }
}

fn contact_addresses(
    swarm: &Swarm<Behaviour>,
    relays: &[Multiaddr],
    local_peer: PeerId,
) -> Vec<Multiaddr> {
    let mut addresses = relays
        .iter()
        .filter_map(|relay| relay_route(relay, local_peer).ok())
        .chain(swarm.external_addresses().cloned())
        .chain(swarm.listeners().cloned())
        .map(|address| {
            split_peer_address(&address)
                .map(|(_, base)| base)
                .unwrap_or(address)
        })
        .collect::<Vec<_>>();
    addresses.dedup();
    addresses
}

/// Fissa la chiave ML-DSA di una scheda d'invito verificata e salva il
/// contatto con il suo link, così si ricollega anche dopo un riavvio.
fn store_invite_card(
    history: &History,
    swarm: &mut Swarm<Behaviour>,
    card: &contacts::InviteCard,
) -> Result<(), Box<dyn Error>> {
    let key = card.peer_id.to_string();
    match history.pq_identity(&key)? {
        Some(pinned) if pinned != card.ml_dsa_public_key => {
            return Err("la chiave ML-DSA del contatto è cambiata".into())
        }
        Some(_) => {}
        None => history.pin_pq_identity(&key, &card.ml_dsa_public_key)?,
    }
    history.save_contact(&key, &card.name, &card.link, now_ms())?;
    for address in &card.addresses {
        swarm.add_peer_address(card.peer_id, address.clone());
    }
    Ok(())
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
    for mut address in addresses {
        // I descrittori dei dispositivi portano gli indirizzi senza il Peer
        // ID finale: senza aggiungerlo venivano scartati tutti e il pairing
        // dipendeva da DHT e mDNS.
        if !matches!(address.iter().last(), Some(Protocol::P2p(_))) {
            address.push(Protocol::P2p(peer));
        }
        // Il loopback di un peer remoto non è mai raggiungibile: scartarlo evita
        // di sprecare lo slot di dial (cicli "Rifiuto persistente" su 127.0.0.1).
        // Nei test end-to-end i due nodi girano davvero sullo stesso host.
        if !cfg!(test) && is_loopback_multiaddr(&address) {
            continue;
        }
        match split_peer_address(&address) {
            Ok((address_peer, base)) if address_peer == peer => {
                swarm.behaviour_mut().kad.add_address(&peer, base);
                valid.push_back(address);
            }
            Ok(_) => log::warn!("indirizzo ignorato: Peer ID diverso dal contatto"),
            Err(error) => log::warn!("indirizzo contatto ignorato: {error}"),
        }
    }

    if let Some(address) = valid.pop_front() {
        dialing.insert(peer);
        direct_fallbacks.insert(peer, valid);
        if let Err(error) = swarm.dial(address) {
            log::warn!("indirizzo non raggiungibile: {error}");
            let recovery = planner.after_failure(peer);
            execute_recovery(swarm, pending_dht, planner, peer, recovery);
        }
    } else {
        let recovery = planner.after_failure(peer);
        execute_recovery(swarm, pending_dht, planner, peer, recovery);
    }
}

fn is_loopback_multiaddr(address: &Multiaddr) -> bool {
    address.iter().any(|proto| match proto {
        Protocol::Ip4(ip) => ip.is_loopback(),
        Protocol::Ip6(ip) => ip.is_loopback(),
        _ => false,
    })
}

fn execute_recovery(
    swarm: &mut Swarm<Behaviour>,
    pending_dht: &mut HashMap<kad::QueryId, PeerId>,
    planner: &mut FallbackPlanner,
    peer: PeerId,
    mut recovery: Recovery,
) {
    // Se un dial fallisce subito (es. "already dialing" mentre un indirizzo
    // diretto irraggiungibile è ancora in timeout), avanza al percorso
    // successivo invece di fermarsi: così il relay viene comunque provato.
    loop {
        match recovery {
            Recovery::SearchDht(target) => {
                log::info!("cerco {target} nella DHT");
                let query = swarm.behaviour_mut().kad.get_closest_peers(target);
                pending_dht.insert(query, target);
                return;
            }
            Recovery::DialPeer(target) => {
                log::info!("provo indirizzi DHT per {target}");
                match swarm.dial(DialOpts::peer_id(target).build()) {
                    Ok(()) => return,
                    Err(error) => {
                        log::warn!("dial DHT fallito: {error}");
                        recovery = planner.after_failure(peer);
                    }
                }
            }
            Recovery::ViaRelay(address) => {
                log::info!("provo il relay: {address}");
                log::info!("provo il relay: {address}");
                match swarm.dial(address) {
                    Ok(()) => return,
                    Err(error) => {
                        log::warn!("dial relay fallito: {error}");
                        recovery = planner.after_failure(peer);
                    }
                }
            }
            Recovery::Exhausted => {
                log::warn!("nessun altro percorso disponibile");
                log::warn!("nessun altro percorso disponibile per {peer}");
                return;
            }
        }
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
    devices: &ContactDevices,
    peers: &HashSet<PeerId>,
    local_peer_id: PeerId,
    sent_numbers: &mut HashMap<PeerId, u64>,
    event: ChatEvent,
) {
    if peers.is_empty() {
        log::warn!("nessun peer collegato");
    }
    for peer in peers {
        send_event(
            swarm,
            sessions,
            devices,
            *peer,
            local_peer_id,
            sent_numbers,
            event.clone(),
        );
    }
}

// Svuota la coda dei messaggi accodati per `peer` ora che la sessione è pronta.
#[allow(clippy::too_many_arguments)]
fn flush_outbox(
    swarm: &mut libp2p::Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_messages: &mut HashMap<request_response::OutboundRequestId, PendingMessage>,
    outbox: &mut HashMap<PeerId, VecDeque<(ChatEvent, u8)>>,
    local_peer_id: PeerId,
    peer: PeerId,
) {
    // La coda è indicizzata sul contatto: se la sessione è nata su un suo
    // dispositivo collegato, svuota comunque la coda del contatto.
    let contact = devices.canonical(peer);
    let mut targets = vec![peer];
    if contact != peer {
        targets.push(contact);
    }
    for target in targets {
        let Some(queue) = outbox.remove(&target) else {
            continue;
        };
        for (event, attempts) in queue {
            match send_event(
                swarm,
                sessions,
                devices,
                target,
                local_peer_id,
                sent_numbers,
                event.clone(),
            ) {
                Some(request_id) => {
                    pending_messages.insert(request_id, (target, event, attempts, now_ms()));
                }
                None => {
                    outbox
                        .entry(target)
                        .or_default()
                        .push_back((event, attempts));
                }
            }
        }
    }
}

/// Reinvia un evento senza conferma: subito se la sessione c'è, altrimenti
/// torna in coda nella sua posizione.
#[allow(clippy::too_many_arguments)]
fn resend_pending(
    swarm: &mut libp2p::Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_messages: &mut HashMap<request_response::OutboundRequestId, PendingMessage>,
    outbox: &mut HashMap<PeerId, VecDeque<(ChatEvent, u8)>>,
    local_peer_id: PeerId,
    peer: PeerId,
    event: ChatEvent,
    attempts: u8,
) {
    match send_event(
        swarm,
        sessions,
        devices,
        peer,
        local_peer_id,
        sent_numbers,
        event.clone(),
    ) {
        Some(request_id) => {
            pending_messages.insert(request_id, (peer, event, attempts, now_ms()));
        }
        None => requeue_in_order(outbox.entry(peer).or_default(), event, attempts),
    }
}

/// Consegna affidabile di un evento cifrato a un contatto: con la sessione
/// pronta e la coda vuota parte subito ed è tracciato per retry e rifiuti;
/// altrimenti va in fondo alla coda (l'ordine resta quello di invio) e, senza
/// sessione, si avvia l'handshake. Non c'è un esito "perso in silenzio": o
/// arriva un Ack o `report_undelivered` avvisa l'utente.
#[allow(clippy::too_many_arguments)]
fn deliver_event(
    swarm: &mut libp2p::Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_messages: &mut HashMap<request_response::OutboundRequestId, PendingMessage>,
    outbox: &mut HashMap<PeerId, VecDeque<(ChatEvent, u8)>>,
    pending_handshakes: &mut Handshakes,
    local_peer_id: PeerId,
    peer: PeerId,
    event: ChatEvent,
) {
    let queue_empty = outbox.get(&peer).is_none_or(VecDeque::is_empty);
    if devices.secure(peer, sessions) && queue_empty {
        match send_event(
            swarm,
            sessions,
            devices,
            peer,
            local_peer_id,
            sent_numbers,
            event.clone(),
        ) {
            Some(request_id) => {
                pending_messages.insert(request_id, (peer, event, 0, now_ms()));
            }
            None => outbox.entry(peer).or_default().push_back((event, 0)),
        }
    } else if devices.secure(peer, sessions) {
        outbox.entry(peer).or_default().push_back((event, 0));
        flush_outbox(
            swarm,
            sessions,
            devices,
            sent_numbers,
            pending_messages,
            outbox,
            local_peer_id,
            peer,
        );
    } else {
        outbox.entry(peer).or_default().push_back((event, 0));
        maybe_start_hybrid_handshake(swarm, pending_handshakes, sessions, local_peer_id, peer);
    }
}

/// Le emoticon personalizzate di un messaggio che `peer` non ha ancora
/// ricevuto. Vengono segnate come condivise subito, così non partono due
/// volte; se la consegna fallisce `report_undelivered` toglie il segno e il
/// messaggio successivo le riprova.
fn new_emoticon_offers(
    spans: &[EmoticonSpan],
    peer: PeerId,
    shared: &mut HashSet<(PeerId, [u8; 32])>,
    triggers: &[Trigger],
    store: &Path,
    events: &mpsc::UnboundedSender<ClientEvent>,
) -> Vec<EmoticonOffer> {
    let mut offers = Vec::new();
    for span in spans {
        let asset_id = span.asset_id;
        if !shared.insert((peer, asset_id)) {
            continue;
        }
        let Some(trigger) = triggers.iter().find(|trigger| trigger.asset_id == asset_id) else {
            shared.remove(&(peer, asset_id));
            continue;
        };
        match load_emoticon_offer(store, trigger) {
            Ok(offer) => offers.push(offer),
            Err(error) => {
                shared.remove(&(peer, asset_id));
                let _ = events.send(ClientEvent::Error {
                    message: format!("emoticon {} non disponibile: {error}", trigger.text),
                });
            }
        }
    }
    offers
}

fn send_event(
    swarm: &mut libp2p::Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    peer: PeerId,
    local_peer_id: PeerId,
    sent_numbers: &mut HashMap<PeerId, u64>,
    event: ChatEvent,
) -> Option<request_response::OutboundRequestId> {
    // Un contatto è una persona, non un peer: se non ha una sessione diretta
    // consegna al suo dispositivo collegato che ce l'ha (il messaggio si
    // propaga agli altri suoi dispositivi via device-sync).
    let peer = devices.route(peer, sessions);
    let secure = sessions.contains_key(&peer);
    if !secure && !matches!(event, ChatEvent::Presence(_)) {
        log::warn!("sessione sicura non pronta per {peer}");
        return None;
    }
    let previous = sent_numbers.get(&peer).copied().unwrap_or_default();
    let number = match previous.checked_add(1) {
        Some(number) => number,
        None => {
            log::warn!("contatore messaggi esaurito per {peer}");
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
                log::warn!("cifratura messaggio fallita per {peer}: {error}");
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

#[allow(clippy::too_many_arguments)]
fn send_next_transfer_chunk(
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_transfers: &mut HashMap<request_response::OutboundRequestId, PendingTransfer>,
    local_peer_id: PeerId,
    events: &mpsc::UnboundedSender<ClientEvent>,
    mut transfer: PendingTransfer,
) {
    let Some(index) = transfer.remaining.pop_front() else {
        log::info!("invio completato: {}", transfer.manifest.filename);
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
        devices,
        sent_numbers,
        pending_transfers,
        local_peer_id,
        events,
        transfer,
    );
}

fn validate_missing_chunks(
    indices: Vec<u32>,
    chunk_count: usize,
) -> Result<VecDeque<u32>, &'static str> {
    if indices.len() > chunk_count {
        return Err("troppi indici");
    }
    let mut unique = HashSet::with_capacity(indices.len());
    if indices
        .iter()
        .any(|&index| index as usize >= chunk_count || !unique.insert(index))
    {
        return Err("indice duplicato o fuori limite");
    }
    Ok(indices.into())
}

#[allow(clippy::too_many_arguments)]
fn send_current_transfer_chunk(
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    pending_transfers: &mut HashMap<request_response::OutboundRequestId, PendingTransfer>,
    local_peer_id: PeerId,
    events: &mpsc::UnboundedSender<ClientEvent>,
    transfer: PendingTransfer,
) {
    let chunk = match read_chunk(&transfer.path, &transfer.manifest, transfer.current) {
        Ok(chunk) => chunk,
        Err(error) => {
            log::warn!("invio interrotto: {error}");
            report_file_failed(events, &transfer.manifest.filename, &error.to_string());
            return;
        }
    };
    match send_event(
        swarm,
        sessions,
        devices,
        transfer.peer,
        local_peer_id,
        sent_numbers,
        ChatEvent::AttachmentChunk(chunk),
    ) {
        Some(request_id) => {
            pending_transfers.insert(request_id, transfer);
        }
        // La sessione è caduta a metà invio: prima il chunk successivo non
        // partiva e la barra restava ferma senza nessun avviso. Le parti già
        // ricevute restano dal destinatario, quindi un nuovo invio riprende.
        None => report_file_failed(
            events,
            &transfer.manifest.filename,
            "sessione cifrata persa durante l'invio",
        ),
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

fn incoming_offer_details(
    peer: PeerId,
    local_peer_id: PeerId,
    event: &ChatEvent,
    history: &History,
) -> Result<Option<(AttachmentManifest, Option<String>)>, String> {
    let (manifest, group_id) = match event {
        ChatEvent::AttachmentOffer(manifest) => (manifest, None),
        ChatEvent::GroupAttachmentOffer(offer) => {
            let id = hex_group_id(&offer.group_id);
            let group = history
                .group_chat(&id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "chat di gruppo sconosciuta".to_owned())?;
            if !group.members.contains(&peer.to_string())
                || !group.members.contains(&local_peer_id.to_string())
            {
                return Err("mittente non appartenente al gruppo".into());
            }
            let received_at = now_ms();
            if !group_member_can_send(&group, &peer.to_string(), received_at)
                || active_group_ban(&group, &local_peer_id.to_string(), received_at)
            {
                return Err("file bloccato dalla moderazione".into());
            }
            (&offer.manifest, Some(id))
        }
        _ => return Ok(None),
    };
    validate_manifest(manifest).map_err(|error| error.to_string())?;
    Ok(Some((manifest.clone(), group_id)))
}

fn device_id(peer: PeerId) -> [u8; 16] {
    blake3::hash(&peer.to_bytes()).as_bytes()[..16]
        .try_into()
        .expect("16 bytes")
}

fn peer_fingerprint(peer: PeerId) -> String {
    let hex = blake3::hash(&peer.to_bytes())
        .to_hex()
        .to_string()
        .to_uppercase();
    hex.as_bytes()[..32]
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
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

/// Un testo con questo id è già in cronologia (arrivato con un invio
/// precedente).
fn already_received(history: &History, event: &ChatEvent) -> bool {
    match event {
        ChatEvent::Text(message) if message.id != [0u8; 16] => history
            .event_meta(&hex_group_id(&message.id))
            .ok()
            .flatten()
            .is_some(),
        _ => false,
    }
}

fn receive_event(peer: PeerId, event: &ChatEvent, context: &mut Incoming<'_>) -> ProtocolResponse {
    match event {
        ChatEvent::Text(message) => {
            log::info!(
                "{peer}: {} [{} emoticon]",
                message.text,
                message.emoticons.len()
            );
            record_text(context.history, &peer, "in", message);
            notify(
                notification_allowed(context, &format!("peer:{peer}")),
                "Nuovo messaggio",
                &message.text,
            );
            ProtocolResponse::Ack
        }
        ChatEvent::AttachmentCancelled(cancel) => {
            // Solo chi ha offerto il file può annullarlo: le parti ricevute
            // vengono eliminate e la UI avvisata.
            if context
                .incoming_attachments
                .remove(&(peer, cancel.attachment_id))
                .is_some()
            {
                context.attachments.cancel(&cancel.attachment_id);
                let _ = context
                    .events
                    .send(ClientEvent::IncomingAttachmentCancelled {
                        peer_id: peer.to_string(),
                        attachment_id: hex_asset_id(&cancel.attachment_id),
                        offer_id: None,
                    });
            }
            ProtocolResponse::Ack
        }
        ChatEvent::DeleteMessage(request) => {
            // Cancellazione "per tutti" in ingresso. Arriva solo sul canale
            // cifrato (il chat in chiaro accetta solo Presence), quindi è
            // autenticata come proveniente da `peer`.
            if request.group_id.is_some() {
                return ProtocolResponse::Rejected("cancellazione di gruppo non supportata".into());
            }
            const DELETE_WINDOW_MS: u64 = 15 * 60 * 1000;
            const DELETE_GRACE_MS: u64 = 5 * 60 * 1000;
            let event_id = hex_group_id(&request.id);
            match context.history.event_meta(&event_id) {
                Ok(Some((stored_peer, direction, timestamp_ms))) => {
                    // Solo il mittente originale può cancellare il proprio
                    // messaggio: dev'essere uno ricevuto DA questo peer.
                    if stored_peer != peer.to_string() || direction != "in" {
                        return ProtocolResponse::Rejected("cancellazione non autorizzata".into());
                    }
                    if now_ms().saturating_sub(timestamp_ms) > DELETE_WINDOW_MS + DELETE_GRACE_MS {
                        return ProtocolResponse::Rejected(
                            "finestra di cancellazione scaduta".into(),
                        );
                    }
                    if let Err(error) = context.history.tombstone_event(&event_id, now_ms()) {
                        return ProtocolResponse::Rejected(error.to_string());
                    }
                    let _ = context.events.send(ClientEvent::MessageDeleted {
                        peer_id: peer.to_string(),
                        event_id,
                    });
                    ProtocolResponse::Ack
                }
                Ok(None) => ProtocolResponse::Ack,
                Err(error) => ProtocolResponse::Rejected(error.to_string()),
            }
        }
        ChatEvent::GroupDefinition(definition) => {
            let sender = peer.to_string();
            if !definition
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
            match &existing {
                None if definition.owner_peer != sender => {
                    return ProtocolResponse::Rejected("invito al gruppo non autorizzato".into());
                }
                Some(group)
                    if group.owner_peer != definition.owner_peer
                        || group.revision > definition.revision =>
                {
                    return ProtocolResponse::Rejected("definizione del gruppo non valida".into());
                }
                Some(group) if sender != group.owner_peer && !group.admins.contains(&sender) => {
                    return ProtocolResponse::Rejected(
                        "aggiornamento gruppo non autorizzato".into(),
                    );
                }
                Some(group) if group.revision == definition.revision => {
                    return ProtocolResponse::Ack;
                }
                Some(group)
                    if sender != group.owner_peer
                        && !admin_group_update_allowed(group, definition, &sender) =>
                {
                    return ProtocolResponse::Rejected(
                        "aggiornamento gruppo non autorizzato".into(),
                    );
                }
                _ => {}
            }
            let group = GroupChatEntry {
                id: id.clone(),
                name: definition.name.trim().to_owned(),
                owner_peer: definition.owner_peer.clone(),
                members: definition.members.clone(),
                admins: definition.admins.clone(),
                silenced: definition.silenced.clone(),
                bans: definition.bans.clone(),
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
                    group_id: id.clone(),
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
            let received_at = now_ms();
            if !group_member_can_send(&group, &peer.to_string(), received_at)
                || active_group_ban(&group, &context.local_peer_id.to_string(), received_at)
            {
                return ProtocolResponse::Rejected("messaggio bloccato dalla moderazione".into());
            }
            let encoded = match encode_text_history(&group_message.message) {
                Ok(encoded) => encoded,
                Err(error) => return ProtocolResponse::Rejected(error.to_string()),
            };
            let timestamp_ms = remote_timestamp(group_message.timestamp_ms, received_at);
            if let Err(error) = context.history.record(
                &group_history_key(&id),
                "in",
                &format!("group-text:{peer}"),
                &encoded,
                timestamp_ms,
            ) {
                return ProtocolResponse::Rejected(error.to_string());
            }
            let _ = context.events.send(ClientEvent::GroupMessage {
                message: ClientGroupMessage {
                    group_id: id.clone(),
                    sender_peer_id: peer.to_string(),
                    direction: "in".into(),
                    kind: "text".into(),
                    body: group_message.message.text.clone(),
                    timestamp_ms,
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
                    event_id: None,
                },
            });
            notify(
                notification_allowed(context, &format!("group:{id}")),
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
            let received_at = now_ms();
            if !group_member_can_send(&group, &peer.to_string(), received_at)
                || active_group_ban(&group, &context.local_peer_id.to_string(), received_at)
            {
                return ProtocolResponse::Rejected("file bloccato dalla moderazione".into());
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
                            event_id: None,
                        },
                    });
                    match completed {
                        Some(Completion::Ready(completed)) => {
                            emit_attachment_received(context.events, peer, Some(id), &completed);
                        }
                        pending => {
                            context
                                .incoming_attachments
                                .entry((peer, offer.manifest.attachment_id))
                                .or_default()
                                .insert(Some(id));
                            if let Some(Completion::Finalize(job)) = pending {
                                spawn_finalize(context.vault_jobs, peer, job);
                            }
                        }
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
                log::info!("{peer}: *** TRILLO ***");
                record(context.history, &peer, "in", "nudge", "trillo");
                notify(
                    notification_allowed(context, &format!("peer:{peer}")),
                    "Trillo",
                    "Hai ricevuto un trillo",
                );
                ProtocolResponse::Ack
            }
            Err(_) => {
                log::warn!("{peer}: trillo ricevuto ignorato per rate limit");
                ProtocolResponse::Ack
            }
        },
        ChatEvent::EmoticonOffer(offer)
            if !context
                .pending_emoticons
                .contains_key(&offer.metadata.asset_id)
                && context.pending_emoticons.len() >= MAX_PENDING_EMOTICONS =>
        {
            ProtocolResponse::Rejected("troppe emoticon in attesa".into())
        }
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
                log::info!("{peer}: emoticon ricevuta, in attesa di salvataggio");
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
                    match completed {
                        Some(Completion::Ready(completed)) => {
                            log::info!("{peer}: file già presente nell'archivio cifrato");
                            notify(
                                notification_allowed(context, &format!("peer:{peer}")),
                                "File ricevuto",
                                &manifest.filename,
                            );
                            emit_attachment_received(context.events, peer, None, &completed);
                        }
                        pending => {
                            context
                                .incoming_attachments
                                .entry((peer, manifest.attachment_id))
                                .or_default()
                                .insert(None);
                            if let Some(Completion::Finalize(job)) = pending {
                                spawn_finalize(context.vault_jobs, peer, job);
                            }
                        }
                    }
                    ProtocolResponse::MissingChunks(missing)
                }
                Err(error) => ProtocolResponse::Rejected(error.to_string()),
            }
        }
        ChatEvent::AttachmentChunk(chunk) => match context.attachments.accept_chunk(chunk) {
            Ok(Some(job)) => {
                // Ultimo chunk: la verifica dell'intero file gira fuori dal
                // loop; notifica ed eventi UI partono quando ha finito.
                spawn_finalize(context.vault_jobs, peer, job);
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
            log::info!(
                "{} è {}",
                presence.display_name,
                if presence.online { "online" } else { "offline" }
            );
            ProtocolResponse::Ack
        }
    }
}

fn notification_allowed(context: &Incoming<'_>, conversation: &str) -> bool {
    context.notifications
        && notification_mute_allows(context.notification_mutes, conversation, now_ms())
}

fn notification_mute_allows(
    mutes: &HashMap<String, Option<u64>>,
    conversation: &str,
    timestamp_ms: u64,
) -> bool {
    match mutes.get(conversation) {
        None => true,
        Some(None) => false,
        Some(Some(until_ms)) => timestamp_ms >= *until_ms,
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
        log::warn!("cronologia non aggiornata: {error}");
    }
}

/// Margine concesso all'orologio di chi scrive rispetto al nostro.
const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
/// Un messaggio può arrivare in ritardo (coda, dispositivo offline), ma non
/// più di così.
const MAX_REMOTE_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// L'istante dichiarato dal mittente, ricondotto a una finestra plausibile
/// attorno alla ricezione: non può spostare un messaggio nel futuro né in
/// fondo alla cronologia.
fn remote_timestamp(claimed_ms: u64, received_at_ms: u64) -> u64 {
    if claimed_ms > received_at_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || claimed_ms < received_at_ms.saturating_sub(MAX_REMOTE_AGE_MS)
    {
        received_at_ms
    } else {
        claimed_ms
    }
}

fn record_text(history: &History, peer: &PeerId, direction: &str, message: &TextMessage) {
    match encode_text_history(message) {
        Ok(body) => {
            // Usa l'id condiviso come event_id così mittente e destinatario
            // referenziano lo stesso messaggio per la cancellazione. I messaggi
            // legacy (id azzerato) mantengono un id casuale locale.
            let result = if message.id == [0u8; 16] {
                history.record(&peer.to_string(), direction, "text", &body, now_ms())
            } else {
                history.record_with_id(
                    &hex_group_id(&message.id),
                    &peer.to_string(),
                    direction,
                    "text",
                    &body,
                    now_ms(),
                )
            };
            if let Err(error) = result {
                log::warn!("cronologia non aggiornata: {error}");
            }
        }
        Err(error) => log::warn!("cronologia non aggiornata: {error}"),
    }
}

fn encode_text_history(message: &TextMessage) -> Result<String, Box<dyn Error>> {
    let bytes = cbor4ii::serde::to_vec(Vec::new(), message)?;
    Ok(format!("{TEXT_HISTORY_PREFIX}{}", BASE64.encode(bytes)))
}

fn decode_text_history(body: &str) -> TextMessage {
    body.strip_prefix(TEXT_HISTORY_PREFIX)
        .and_then(|encoded| BASE64.decode(encoded).ok())
        .and_then(|bytes| cbor4ii::serde::from_slice::<TextMessage>(&bytes).ok())
        .filter(|message| validate_text_message(message).is_ok())
        .unwrap_or_else(|| TextMessage {
            text: body.to_owned(),
            emoticons: Vec::new(),
            id: [0; 16],
        })
}

fn client_emoticon_spans(message: &TextMessage) -> Vec<ClientEmoticonSpan> {
    message
        .emoticons
        .iter()
        .map(|span| ClientEmoticonSpan {
            start: span.start,
            end: span.end,
            asset_id: hex_asset_id(&span.asset_id),
        })
        .collect()
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
    let admins = group.admins.iter().collect::<HashSet<_>>();
    let silenced = group.silenced.iter().collect::<HashSet<_>>();
    let banned = group
        .bans
        .iter()
        .map(|ban| &ban.peer_id)
        .collect::<HashSet<_>>();
    if admins.len() != group.admins.len()
        || silenced.len() != group.silenced.len()
        || banned.len() != group.bans.len()
        || group
            .admins
            .iter()
            .any(|peer| peer == &group.owner_peer || !group.members.contains(peer))
        || group.silenced.iter().any(|peer| {
            peer == &group.owner_peer
                || group.admins.contains(peer)
                || !group.members.contains(peer)
        })
        || group.bans.iter().any(|ban| {
            ban.peer_id == group.owner_peer
                || group.admins.contains(&ban.peer_id)
                || !group.members.contains(&ban.peer_id)
                || ban.expires_at_ms == Some(0)
        })
    {
        return Err("ruoli o sanzioni del gruppo non validi");
    }
    Ok(())
}

fn active_group_ban(group: &GroupChatEntry, peer: &str, timestamp_ms: u64) -> bool {
    group.bans.iter().any(|ban| {
        ban.peer_id == peer
            && match ban.expires_at_ms {
                None => true,
                Some(expires) => expires > timestamp_ms,
            }
    })
}

fn group_member_can_send(group: &GroupChatEntry, peer: &str, timestamp_ms: u64) -> bool {
    group.members.iter().any(|member| member == peer)
        && !group.silenced.iter().any(|member| member == peer)
        && !active_group_ban(group, peer, timestamp_ms)
}

fn apply_group_moderation(
    group: &mut GroupChatEntry,
    actor: PeerId,
    target: PeerId,
    action: GroupModeration,
    timestamp_ms: u64,
) -> Result<(), &'static str> {
    let actor = actor.to_string();
    let target = target.to_string();
    let owner = actor == group.owner_peer;
    let admin = group.admins.contains(&actor);
    if !owner && !admin {
        return Err("non hai diritti di moderazione");
    }
    if !group.members.contains(&target) || target == group.owner_peer {
        return Err("partecipante non moderabile");
    }
    if !owner && group.admins.contains(&target) {
        return Err("un amministratore non può moderare un altro amministratore");
    }

    let previous = (
        group.admins.clone(),
        group.silenced.clone(),
        group.bans.clone(),
    );
    match action {
        GroupModeration::SetAdmin(enabled) => {
            if !owner {
                return Err("solo il proprietario può assegnare diritti");
            }
            if enabled {
                if !group.admins.contains(&target) {
                    group.admins.push(target.clone());
                    group.admins.sort();
                }
                group.silenced.retain(|peer| peer != &target);
                group.bans.retain(|ban| ban.peer_id != target);
            } else {
                group.admins.retain(|peer| peer != &target);
            }
        }
        GroupModeration::SetSilenced(enabled) => {
            if group.admins.contains(&target) {
                return Err("rimuovi prima i diritti di amministratore");
            }
            if enabled && !group.silenced.contains(&target) {
                group.silenced.push(target.clone());
                group.silenced.sort();
            } else if !enabled {
                group.silenced.retain(|peer| peer != &target);
            }
        }
        GroupModeration::Ban(duration_ms) => {
            if group.admins.contains(&target) {
                return Err("rimuovi prima i diritti di amministratore");
            }
            let expires_at_ms = duration_ms
                .map(|duration| {
                    if !(60_000..=30 * 24 * 60 * 60 * 1_000).contains(&duration) {
                        return Err("durata ban non valida");
                    }
                    timestamp_ms
                        .checked_add(duration)
                        .ok_or("durata ban non valida")
                })
                .transpose()?;
            group.silenced.retain(|peer| peer != &target);
            group.bans.retain(|ban| ban.peer_id != target);
            group.bans.push(GroupBan {
                peer_id: target,
                expires_at_ms,
            });
            group
                .bans
                .sort_by(|left, right| left.peer_id.cmp(&right.peer_id));
        }
        GroupModeration::Unban => group.bans.retain(|ban| ban.peer_id != target),
    }
    if previous
        == (
            group.admins.clone(),
            group.silenced.clone(),
            group.bans.clone(),
        )
    {
        return Err("nessuna modifica da applicare");
    }
    group.revision = group.revision.checked_add(1).ok_or("revisioni esaurite")?;
    Ok(())
}

fn admin_group_update_allowed(
    existing: &GroupChatEntry,
    definition: &GroupDefinition,
    actor: &str,
) -> bool {
    // ponytail: revisioni lineari; passare a un log firmato se servono moderatori concorrenti offline.
    existing.admins.iter().any(|admin| admin == actor)
        && definition.revision == existing.revision.checked_add(1).unwrap_or_default()
        && definition.name == existing.name
        && definition.owner_peer == existing.owner_peer
        && definition.members == existing.members
        && definition.admins == existing.admins
}

/// Oltre questa dimensione un file va sempre confermato, anche con
/// l'auto-accept attivo: un contatto non deve poter riempire il disco.
const AUTO_ACCEPT_MAX_BYTES: u64 = 100 * 1024 * 1024;

fn offer_auto_accepted(manifest: &AttachmentManifest, allowed: &HashSet<String>) -> bool {
    manifest.size <= AUTO_ACCEPT_MAX_BYTES && extension_auto_accepted(&manifest.filename, allowed)
}

fn extension_auto_accepted(filename: &str, allowed: &HashSet<String>) -> bool {
    if allowed.is_empty() {
        return false;
    }
    if allowed.contains("*") {
        return true;
    }
    filename
        .rsplit_once('.')
        .map(|(_, ext)| !ext.is_empty() && allowed.contains(&ext.to_ascii_lowercase()))
        .unwrap_or(false)
}

fn parse_message_id(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
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
        admins: group.admins.clone(),
        silenced: group.silenced.clone(),
        bans: group.bans.clone(),
        revision: group.revision,
    };
    validate_group_definition(&definition).map_err(str::to_owned)?;
    Ok(definition)
}

fn local_device_descriptor(
    peer_id: PeerId,
    name: &str,
    addresses: Vec<Multiaddr>,
) -> DeviceDescriptor {
    DeviceDescriptor {
        peer_id: peer_id.to_string(),
        name: name.to_owned(),
        addresses: addresses
            .into_iter()
            .map(|address| address.to_string())
            .collect(),
    }
}

fn device_entry(device: &DeviceDescriptor, last_seen_ms: u64) -> DeviceEntry {
    DeviceEntry {
        peer: device.peer_id.clone(),
        name: device.name.clone(),
        addresses: device.addresses.clone(),
        last_seen_ms,
    }
}

fn device_descriptor(device: &DeviceEntry) -> DeviceDescriptor {
    DeviceDescriptor {
        peer_id: device.peer.clone(),
        name: device.name.clone(),
        addresses: device.addresses.clone(),
    }
}

fn revoked_list(history: &History) -> Result<Vec<RevokedDevice>, Box<dyn Error>> {
    Ok(history
        .revoked_devices()?
        .into_iter()
        .map(|(peer_id, name)| RevokedDevice { peer_id, name })
        .collect())
}

fn send_devices(
    devices: &HashMap<PeerId, DeviceEntry>,
    online: &HashSet<PeerId>,
    events: &mpsc::UnboundedSender<ClientEvent>,
) {
    let mut devices = devices
        .iter()
        .map(|(peer, device)| ClientDevice {
            peer_id: peer.to_string(),
            name: device.name.clone(),
            online: online.contains(peer),
            last_seen_ms: device.last_seen_ms,
        })
        .collect::<Vec<_>>();
    devices.sort_by_key(|device| device.name.to_lowercase());
    let _ = events.send(ClientEvent::DevicesUpdated { devices });
}

fn send_relay_text(
    swarm: &mut Swarm<Behaviour>,
    account_key: &[u8; 32],
    pending: &mut HashMap<request_response::OutboundRequestId, PendingDeviceRequest>,
    device: PeerId,
    target: PeerId,
    message: TextMessage,
    offers: Vec<EmoticonOffer>,
) -> Result<(), Box<dyn Error>> {
    let request = DeviceRequest::RelayText {
        target: target.to_string(),
        id: message.id,
        text: message.text.clone(),
        emoticons: message.emoticons.clone(),
        offers,
    };
    let sealed = devices::seal(account_key, &request)?;
    let request_id = swarm.behaviour_mut().devices.send_request(&device, sealed);
    pending.insert(
        request_id,
        PendingDeviceRequest::Relay {
            peer: device,
            target,
            message,
        },
    );
    Ok(())
}

/// Le immagini delle emoticon personalizzate viaggiano col relay: il
/// dispositivo che consegna potrebbe non averle. Restano sotto il limite di
/// una richiesta libp2p (1 MiB, e i byte in CBOR occupano fino al doppio):
/// quelle oltre il budget arrivano come testo.
const RELAY_EMOTICON_BUDGET: usize = 192 * 1024;

fn relay_emoticon_offers(
    spans: &[EmoticonSpan],
    triggers: &[Trigger],
    store: &Path,
) -> Vec<EmoticonOffer> {
    let mut offers: Vec<EmoticonOffer> = Vec::new();
    let mut used = 0;
    for span in spans {
        if offers
            .iter()
            .any(|offer| offer.metadata.asset_id == span.asset_id)
        {
            continue;
        }
        let Some(trigger) = triggers
            .iter()
            .find(|trigger| trigger.asset_id == span.asset_id)
        else {
            continue;
        };
        match load_emoticon_offer(store, trigger) {
            Ok(offer) if used + offer.bytes.len() <= RELAY_EMOTICON_BUDGET => {
                used += offer.bytes.len();
                offers.push(offer);
            }
            Ok(_) => log::warn!(
                "relay: emoticon {} oltre il budget, inviata come testo",
                trigger.text
            ),
            Err(error) => log::warn!("relay: emoticon {} non disponibile: {error}", trigger.text),
        }
    }
    offers
}

fn send_device_sync(
    swarm: &mut Swarm<Behaviour>,
    history: &History,
    account_key: &[u8; 32],
    pending: &mut HashMap<request_response::OutboundRequestId, PendingDeviceRequest>,
    peer: PeerId,
    device: DeviceDescriptor,
) -> Result<(), Box<dyn Error>> {
    let (remote_seq, sent_seq) = history.sync_cursors(&peer.to_string())?;
    let records = history.sync_records_since(sent_seq, devices::MAX_SYNC_RECORDS)?;
    let request = DeviceRequest::Sync {
        device,
        after_seq: remote_seq,
        records,
    };
    let sealed = devices::seal(account_key, &request)?;
    let request_id = swarm.behaviour_mut().devices.send_request(&peer, sealed);
    pending.insert(request_id, PendingDeviceRequest::Sync { peer });
    Ok(())
}

fn connect_synced_peers(
    history: &History,
    devices: &HashMap<PeerId, DeviceEntry>,
    local_peer_id: PeerId,
    connections: &mut SyncConnections<'_>,
) -> Result<(), Box<dyn Error>> {
    for contact in history.contacts()? {
        if contact.link.is_empty() {
            continue;
        }
        let Ok((_, peer, addresses)) = contacts::import_link(&contact.link) else {
            continue;
        };
        if peer != local_peer_id && !connections.online_contacts.contains(&peer) {
            connect_contact(
                connections.swarm,
                peer,
                addresses,
                connections.dialing,
                connections.bootstrap_fallbacks,
                connections.fallback_planner,
                connections.pending_dht,
            );
        }
    }
    for (peer, device) in devices {
        if *peer == local_peer_id || connections.online_devices.contains(peer) {
            continue;
        }
        let addresses = device
            .addresses
            .iter()
            .filter_map(|address| address.parse::<Multiaddr>().ok())
            .collect();
        connect_contact(
            connections.swarm,
            *peer,
            addresses,
            connections.dialing,
            connections.bootstrap_fallbacks,
            connections.fallback_planner,
            connections.pending_dht,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn refresh_synced_state(
    history: &History,
    known_contacts: &mut HashSet<PeerId>,
    ignored_contacts: &mut HashSet<PeerId>,
    peer_names: &mut HashMap<PeerId, String>,
    devices: &mut ContactDevices,
    online_contacts: &HashSet<PeerId>,
    sessions: &HashMap<PeerId, RatchetSession>,
    events: &mpsc::UnboundedSender<ClientEvent>,
) -> Result<(), Box<dyn Error>> {
    *ignored_contacts = history
        .ignored_contacts()?
        .into_iter()
        .filter_map(|peer| peer.parse().ok())
        .collect();
    let contacts = history.contacts()?;
    let persisted = contacts
        .iter()
        .filter_map(|contact| contact.peer.parse::<PeerId>().ok())
        .collect::<HashSet<_>>();
    // I dispositivi di un contatto restano validi: non sono voci di rubrica ma
    // non vanno nemmeno rimossi dai peer noti.
    let removed = known_contacts
        .difference(&persisted)
        .copied()
        .filter(|peer| !persisted.contains(&devices.canonical(*peer)))
        .collect::<Vec<_>>();
    for peer in removed {
        known_contacts.remove(&peer);
        peer_names.remove(&peer);
        devices.forget(peer);
        let _ = events.send(ClientEvent::ContactRemoved {
            peer_id: peer.to_string(),
            reason: "deleted".into(),
        });
    }
    // Un dispositivo riconosciuto di un contatto attivo non resta bloccato: la
    // fusione dei duplicati arriva agli altri dispositivi come cancellazione.
    for peer in known_contacts.iter().copied().collect::<Vec<_>>() {
        if devices.is_device(peer) && ignored_contacts.remove(&peer) {
            history.allow_contact(&peer.to_string())?;
        }
    }
    for contact in contacts {
        let Ok(peer) = contact.peer.parse::<PeerId>() else {
            continue;
        };
        known_contacts.insert(peer);
        peer_names.insert(peer, contact.name.clone());
        if !contact.account_id.is_empty() {
            devices.bind(peer, contact.account_id.clone());
        }
        let _ = events.send(ClientEvent::ContactUpdated {
            contact: ClientContact {
                peer_id: contact.peer.clone(),
                name: contact.name,
                online: devices.online(peer, online_contacts),
                secure: devices.secure(peer, sessions),
                fingerprint: peer_fingerprint(peer),
            },
        });
        let messages = conversation_messages(history, &contact.peer)?;
        let _ = events.send(ClientEvent::ConversationLoaded {
            peer_id: contact.peer,
            messages,
        });
    }
    for group in history.group_chats()? {
        let messages = group_conversation_messages(history, &group.id, None)?;
        let _ = events.send(ClientEvent::GroupConversationLoaded {
            group_id: group.id,
            messages,
        });
    }
    Ok(())
}

fn send_group_chats(groups: &[GroupChatEntry], events: &mpsc::UnboundedSender<ClientEvent>) {
    let groups = groups
        .iter()
        .map(|group| ClientGroupChat {
            id: group.id.clone(),
            name: group.name.clone(),
            owner_peer_id: group.owner_peer.clone(),
            members: group.members.clone(),
            admins: group.admins.clone(),
            silenced: group.silenced.clone(),
            bans: group
                .bans
                .iter()
                .map(|ban| ClientGroupBan {
                    peer_id: ban.peer_id.clone(),
                    expires_at_ms: ban.expires_at_ms,
                })
                .collect(),
        })
        .collect();
    let _ = events.send(ClientEvent::GroupChatsUpdated { groups });
}

fn send_group_definitions_for_peer(
    history: &History,
    swarm: &mut Swarm<Behaviour>,
    sessions: &mut HashMap<PeerId, RatchetSession>,
    devices: &ContactDevices,
    sent_numbers: &mut HashMap<PeerId, u64>,
    local_peer_id: PeerId,
    peer: PeerId,
) {
    let Ok(groups) = history.group_chats() else {
        return;
    };
    for group in groups.iter().filter(|group| {
        (group.owner_peer == local_peer_id.to_string()
            || group.admins.contains(&local_peer_id.to_string()))
            && group.members.contains(&peer.to_string())
    }) {
        if let Ok(definition) = group_definition(group) {
            send_event(
                swarm,
                sessions,
                devices,
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

fn load_or_create_secret(path: &Path) -> Result<[u8; 32], Box<dyn Error>> {
    if path.exists() {
        return Ok(fs::read(path)?
            .try_into()
            .map_err(|_| "chiave ML-DSA locale non valida")?);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let secret = generate_secret();
    fs::write(path, secret)?;
    Ok(secret)
}

pub struct ClientConfig {
    listen: Multiaddr,
    listen_tcp: Multiaddr,
    connect: Option<Multiaddr>,
    bootstrap: Vec<Multiaddr>,
    relays: Vec<Multiaddr>,
    relay_server: bool,
    identity: PathBuf,
    identity_bytes: Option<Vec<u8>>,
    ml_dsa_seed: Option<[u8; 32]>,
    account_key: Option<[u8; 32]>,
    account_key_store: Option<AccountKeyStore>,
    emotes: PathBuf,
    downloads: PathBuf,
    history: PathBuf,
    notifications: bool,
    /// Legge comandi da stdin (solo CLI). L'app desktop e i test non lo usano:
    /// una lettura bloccata su stdin impedisce lo spegnimento del runtime.
    read_stdin: bool,
    /// Ogni quanto si rinnovano le sessioni cifrate (configurabile nei test).
    session_rekey_interval: Duration,
    name: String,
}

impl ClientConfig {
    pub fn with_identity_bytes(
        mut self,
        bytes: Vec<u8>,
        ml_dsa_seed: [u8; 32],
    ) -> Result<Self, Box<dyn Error>> {
        Keypair::from_protobuf_encoding(&bytes)?;
        self.identity_bytes = Some(bytes);
        self.ml_dsa_seed = Some(ml_dsa_seed);
        Ok(self)
    }

    #[cfg(test)]
    fn with_session_rekey_interval(mut self, interval: Duration) -> Self {
        self.session_rekey_interval = interval;
        self
    }

    pub fn with_account_key<F>(mut self, account_key: [u8; 32], store: F) -> Self
    where
        F: Fn([u8; 32]) -> Result<(), String> + Send + Sync + 'static,
    {
        self.account_key = Some(account_key);
        self.account_key_store = Some(Arc::new(store));
        self
    }

    pub fn desktop(
        name: String,
        data_dir: PathBuf,
        connect: Option<String>,
        relay: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
        if name.trim().is_empty() || name.len() > 64 {
            return Err("nome non valido".into());
        }
        // Più relay separati da virgola o spazi: se uno cade restano gli altri.
        let relay = relay
            .filter(|address| !address.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_RELAY_ADDRESS.to_owned());
        let mut relays = Vec::<Multiaddr>::new();
        for address in relay.split(|c: char| c == ',' || c.is_whitespace()) {
            if address.is_empty() {
                continue;
            }
            let address = address.parse::<Multiaddr>()?;
            split_peer_address(&address)?;
            if !relays.contains(&address) {
                relays.push(address);
            }
        }
        Ok(Self {
            listen: "/ip4/0.0.0.0/udp/0/quic-v1".parse()?,
            listen_tcp: "/ip4/0.0.0.0/tcp/0".parse()?,
            connect: connect
                .filter(|address| !address.trim().is_empty())
                .map(|address| address.parse())
                .transpose()?,
            bootstrap: relays.clone(),
            relays,
            relay_server: false,
            identity: data_dir.join("identity.key"),
            identity_bytes: None,
            ml_dsa_seed: None,
            account_key: None,
            account_key_store: None,
            emotes: data_dir.join("emoticons"),
            downloads: data_dir.join("downloads"),
            history: data_dir.join("history.db"),
            notifications: true,
            read_stdin: false,
            session_rekey_interval: SESSION_REKEY_INTERVAL,
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
            identity_bytes: None,
            ml_dsa_seed: None,
            account_key: None,
            account_key_store: None,
            emotes,
            downloads,
            history,
            notifications,
            read_stdin: true,
            session_rekey_interval: SESSION_REKEY_INTERVAL,
            name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_timestamps_stay_near_reception() {
        let now = 1_000_000_000_000;
        assert_eq!(remote_timestamp(now - 60_000, now), now - 60_000);
        assert_eq!(remote_timestamp(now + 60_000, now), now + 60_000);
        assert_eq!(remote_timestamp(now + 365 * 86_400_000, now), now);
        assert_eq!(remote_timestamp(0, now), now);
    }

    #[test]
    fn auto_accept_never_covers_large_files() {
        let manifest = |filename: &str, size: u64| AttachmentManifest {
            attachment_id: [0; 32],
            filename: filename.into(),
            mime: "application/octet-stream".into(),
            size,
            chunk_size: 1,
            chunks: Vec::new(),
        };
        let all = HashSet::from(["*".to_owned()]);
        let images = HashSet::from(["jpg".to_owned()]);
        assert!(offer_auto_accepted(&manifest("foto.JPG", 1024), &images));
        assert!(!offer_auto_accepted(&manifest("doc.pdf", 1024), &images));
        assert!(offer_auto_accepted(&manifest("doc.pdf", 1024), &all));
        assert!(!offer_auto_accepted(
            &manifest("big.iso", AUTO_ACCEPT_MAX_BYTES + 1),
            &all
        ));
        assert!(!offer_auto_accepted(
            &manifest("foto.jpg", 5 * 1024 * 1024 * 1024),
            &images
        ));
    }

    async fn receive_until(
        events: &mut mpsc::UnboundedReceiver<ClientEvent>,
        predicate: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        receive_until_within(events, Duration::from_secs(15), predicate).await
    }

    async fn receive_until_within(
        events: &mut mpsc::UnboundedReceiver<ClientEvent>,
        limit: Duration,
        predicate: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        tokio::time::timeout(limit, async {
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
            id: [0; 16],
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
                id: [0; 16],
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
            admins: Vec::new(),
            silenced: Vec::new(),
            bans: Vec::new(),
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
    fn group_moderation_enforces_hierarchy_and_expiry() {
        let owner = PeerId::from(Keypair::generate_ed25519().public());
        let admin = PeerId::from(Keypair::generate_ed25519().public());
        let member = PeerId::from(Keypair::generate_ed25519().public());
        let mut group = GroupChatEntry {
            id: hex_group_id(&[1; 16]),
            name: "Amici".into(),
            owner_peer: owner.to_string(),
            members: vec![owner.to_string(), admin.to_string(), member.to_string()],
            admins: vec![admin.to_string()],
            silenced: Vec::new(),
            bans: Vec::new(),
            revision: 1,
        };
        let before_admin_update = group.clone();

        apply_group_moderation(
            &mut group,
            admin,
            member,
            GroupModeration::SetSilenced(true),
            1_000,
        )
        .unwrap();
        let definition = group_definition(&group).unwrap();
        assert!(admin_group_update_allowed(
            &before_admin_update,
            &definition,
            &admin.to_string()
        ));
        let mut privilege_escalation = definition;
        privilege_escalation.admins.clear();
        assert!(!admin_group_update_allowed(
            &before_admin_update,
            &privilege_escalation,
            &admin.to_string()
        ));
        assert!(!group_member_can_send(&group, &member.to_string(), 1_000));
        assert!(apply_group_moderation(
            &mut group,
            admin,
            member,
            GroupModeration::SetAdmin(true),
            1_000,
        )
        .is_err());

        apply_group_moderation(
            &mut group,
            owner,
            member,
            GroupModeration::Ban(Some(60_000)),
            1_000,
        )
        .unwrap();
        assert!(!group_member_can_send(&group, &member.to_string(), 60_999));
        assert!(group_member_can_send(&group, &member.to_string(), 61_000));
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
    fn incoming_file_offer_is_validated_before_user_consent() {
        let path = std::env::temp_dir().join(format!(
            "msnnext-offer-validation-{}-{}.sqlite3",
            std::process::id(),
            now_ms()
        ));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [5; 32]).unwrap();
        let peer = PeerId::from(Keypair::generate_ed25519().public());
        let local = PeerId::from(Keypair::generate_ed25519().public());
        let mut manifest = AttachmentManifest {
            attachment_id: *blake3::hash(&[]).as_bytes(),
            filename: "foto.png".into(),
            mime: "image/png".into(),
            size: 0,
            chunk_size: attachments::CHUNK_SIZE as u32,
            chunks: Vec::new(),
        };

        assert!(incoming_offer_details(
            peer,
            local,
            &ChatEvent::AttachmentOffer(manifest.clone()),
            &history
        )
        .unwrap()
        .is_some());

        manifest.filename = "../foto.png".into();
        assert!(incoming_offer_details(
            peer,
            local,
            &ChatEvent::AttachmentOffer(manifest),
            &history
        )
        .is_err());
        drop(history);
        fs::remove_file(path).ok();
    }

    fn test_peer() -> PeerId {
        PeerId::from(Keypair::generate_ed25519().public())
    }

    fn signed_presence(account: &AccountIdentity, device: PeerId) -> PresenceUpdate {
        presence_update("Phillip", "online", true, &account.proof(device).ok())
    }

    fn card(peer: PeerId, link: &str) -> ContactEntry {
        ContactEntry {
            peer: peer.to_string(),
            name: "Phillip".into(),
            link: link.into(),
            account_id: String::new(),
        }
    }

    #[test]
    fn presence_stays_compatible_with_clients_without_account_proof() {
        #[derive(Serialize)]
        struct LegacyPresence {
            display_name: String,
            online: bool,
            status: String,
        }
        #[derive(serde::Deserialize)]
        struct LegacyView {
            display_name: String,
            online: bool,
        }

        // Presence di un client vecchio: nessuna prova, nessun errore.
        let legacy = cbor4ii::serde::to_vec(
            Vec::new(),
            &LegacyPresence {
                display_name: "Alice".into(),
                online: true,
                status: "online".into(),
            },
        )
        .unwrap();
        let parsed: PresenceUpdate = cbor4ii::serde::from_slice(&legacy).unwrap();
        assert_eq!(parsed.display_name, "Alice");
        assert!(parsed.account.is_none());

        // La nostra presence firmata resta leggibile da un client vecchio.
        let account = AccountIdentity::from_account_key(&[13; 32]).unwrap();
        let device = test_peer();
        let current = presence_update("Alice", "online", true, &account.proof(device).ok());
        assert!(current.account.is_some());
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &current).unwrap();
        let view: LegacyView = cbor4ii::serde::from_slice(&bytes).unwrap();
        assert_eq!(view.display_name, "Alice");
        assert!(view.online);
    }

    #[test]
    fn a_linked_device_of_a_known_contact_needs_no_approval() {
        let account = AccountIdentity::from_account_key(&[9; 32]).unwrap();
        let contact = test_peer();
        let phone = test_peer();
        let stranger = test_peer();
        let known = HashSet::from([contact]);
        let contacts = vec![card(contact, "msnnext://add/abc")];
        let mut devices = ContactDevices::default();

        // Il contatto presenta la prova: il legame con l'account va salvato.
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            contact,
            &signed_presence(&account, contact),
        );
        assert_eq!(source.contact, Some(contact));
        assert!(source.bind.is_some());
        assert!(source.merge.is_none());

        // Un suo dispositivo nuovo: riconosciuto, nessuna richiesta di contatto.
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            phone,
            &signed_presence(&account, phone),
        );
        assert_eq!(source.contact, Some(contact));
        assert!(source.bind.is_none());
        assert_eq!(devices.canonical(phone), contact);
        assert!(devices.is_device(phone));

        // Prova rubata e rigiocata da un altro peer: resta uno sconosciuto.
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            stranger,
            &signed_presence(&account, phone),
        );
        assert_eq!(source.contact, None);

        // Sconosciuto senza prova: approvazione richiesta come prima.
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            stranger,
            &presence_update("Mallory", "online", true, &None),
        );
        assert_eq!(source.contact, None);
        assert!(source.account.is_none());

        // Un altro account non eredita nulla del contatto.
        let other = AccountIdentity::from_account_key(&[10; 32]).unwrap();
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            stranger,
            &signed_presence(&other, stranger),
        );
        assert_eq!(source.contact, None);
        assert!(source.account.is_some());
    }

    #[test]
    fn duplicate_contacts_of_the_same_person_are_merged() {
        let account = AccountIdentity::from_account_key(&[11; 32]).unwrap();
        let contact = test_peer();
        let duplicate = test_peer();
        let known = HashSet::from([contact, duplicate]);
        let contacts = vec![card(contact, "msnnext://add/abc"), card(duplicate, "")];
        let mut devices = ContactDevices::default();

        // Il duplicato (accettato per errore) lega per primo l'account.
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            duplicate,
            &signed_presence(&account, duplicate),
        );
        assert_eq!(source.contact, Some(duplicate));

        // Poi arriva il contatto vero: stessa persona, i due vanno fusi nel
        // contatto importato da contact card.
        let source = recognize_presence(
            &mut devices,
            &known,
            &contacts,
            contact,
            &signed_presence(&account, contact),
        );
        assert_eq!(source.merge, Some((duplicate, contact)));
        assert_eq!(source.contact, Some(contact));
        assert_eq!(devices.canonical(duplicate), contact);
    }

    #[test]
    fn merge_keeps_the_linked_contact_and_is_deterministic() {
        let first = test_peer();
        let second = test_peer();
        let contacts = vec![card(first, ""), card(second, "msnnext://add/abc")];
        assert_eq!(merge_target(&contacts, first, second), (second, first));
        assert_eq!(merge_target(&contacts, second, first), (second, first));

        // Nessun link da nessuna parte: ordine stabile su tutti i dispositivi.
        let contacts = vec![card(first, ""), card(second, "")];
        assert_eq!(
            merge_target(&contacts, first, second),
            merge_target(&contacts, second, first)
        );
    }

    #[test]
    fn messages_reach_the_contact_device_that_has_a_session() {
        let account = AccountIdentity::from_account_key(&[12; 32]).unwrap();
        let local = test_peer();
        let contact = test_peer();
        let phone = test_peer();
        let mut devices = ContactDevices::default();
        devices.bind(contact, "account".into());
        devices.attach(phone, "account");

        let mut sessions = HashMap::new();
        // Nessuna sessione: si continua a puntare al contatto (il messaggio
        // resta in coda finché non c'è un canale).
        assert_eq!(devices.route(contact, &sessions), contact);
        assert!(!devices.secure(contact, &sessions));

        // Solo il telefono è raggiungibile: il messaggio va lì.
        let (initiator, hello) = HybridInitiator::start(
            local,
            phone,
            &crypto::MlDsaIdentity::from_seed(&[1; 32]).unwrap(),
            false,
        )
        .unwrap();
        let (response, phone_key) = respond_hybrid(
            &hello,
            local,
            phone,
            &crypto::MlDsaIdentity::from_seed(&[2; 32]).unwrap(),
        )
        .unwrap();
        let local_key = initiator.finish(&response).unwrap().key;
        drop(phone_key);
        sessions.insert(phone, RatchetSession::new(local_key, local, phone));
        assert_eq!(devices.route(contact, &sessions), phone);
        assert!(devices.secure(contact, &sessions));

        // Cancellare la persona scollega tutti i suoi dispositivi.
        assert_eq!(devices.forget(contact), vec![phone]);
        assert_eq!(devices.canonical(phone), phone);
        let _ = account;
    }

    #[test]
    fn envelope_round_trips_through_the_session_ratchet() {
        let alice = PeerId::from(Keypair::generate_ed25519().public());
        let bob = PeerId::from(Keypair::generate_ed25519().public());
        let (initiator, hello) = HybridInitiator::start(
            alice,
            bob,
            &crypto::MlDsaIdentity::from_seed(&[1; 32]).unwrap(),
            false,
        )
        .unwrap();
        let (response, bob_key) = respond_hybrid(
            &hello,
            alice,
            bob,
            &crypto::MlDsaIdentity::from_seed(&[2; 32]).unwrap(),
        )
        .unwrap();
        let alice_key = initiator.finish(&response).unwrap().key;
        let mut alice_session = RatchetSession::new(alice_key, alice, bob);
        let mut bob_session = RatchetSession::new(bob_key.key, bob, alice);
        let envelope = Envelope {
            protocol_version: PROTOCOL_VERSION,
            conversation_id: conversation_id(alice, bob),
            sender_device_id: device_id(alice),
            message_number: 1,
            previous_message_number: 0,
            event: ChatEvent::Text(TextMessage {
                text: "segreto".into(),
                emoticons: vec![],
                id: [0; 16],
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
        // Sola connessione non autorizza più: serve consenso (contatto noto).
        assert!(!handshake_peer_authorized(false, false, true));
    }

    #[test]
    fn failed_secure_requests_have_bounded_retries() {
        assert_eq!(next_request_retry(0), Some(1));
        assert_eq!(next_request_retry(1), Some(2));
        assert_eq!(next_request_retry(2), None);
    }

    #[test]
    fn missing_chunk_requests_are_unique_and_in_bounds() {
        assert_eq!(
            validate_missing_chunks(vec![2, 0, 1], 3).unwrap(),
            VecDeque::from([2, 0, 1])
        );
        assert!(validate_missing_chunks(vec![0, 0], 2).is_err());
        assert!(validate_missing_chunks(vec![2], 2).is_err());
        assert!(validate_missing_chunks(vec![0, 1, 2], 2).is_err());
    }

    #[test]
    fn text_history_preserves_custom_emoticon_spans() {
        let message = TextMessage {
            text: "ciao :x:".into(),
            emoticons: vec![msnnext_protocol::EmoticonSpan {
                start: 5,
                end: 8,
                asset_id: [4; 32],
            }],
            id: [0; 16],
        };

        assert_eq!(
            decode_text_history(&encode_text_history(&message).unwrap()),
            message
        );
        assert_eq!(
            decode_text_history("messaggio precedente").text,
            "messaggio precedente"
        );
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

    /// Nodo di test su loopback: niente relay né bootstrap pubblici, così i
    /// test end-to-end non dipendono dalla rete esterna.
    fn loopback_config(name: &str, dir: &Path, listen: Option<Multiaddr>) -> ClientConfig {
        let mut config = ClientConfig::desktop(name.into(), dir.to_path_buf(), None, None).unwrap();
        config.listen = listen.unwrap_or_else(|| "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap());
        config.listen_tcp = "/ip4/127.0.0.1/tcp/0".parse().unwrap();
        config.relays.clear();
        config.bootstrap.clear();
        config.notifications = false;
        config
    }

    struct TestNode {
        commands: mpsc::UnboundedSender<ClientCommand>,
        events: mpsc::UnboundedReceiver<ClientEvent>,
        task: tokio::task::JoinHandle<Result<(), String>>,
        peer: PeerId,
    }

    impl TestNode {
        /// Va chiamato dentro un `LocalSet`: `run` non è `Send`.
        async fn start(config: ClientConfig) -> Self {
            Self::try_start(config)
                .await
                .expect("il nodo non è partito")
        }

        async fn try_start(config: ClientConfig) -> Result<Self, String> {
            let (commands, command_rx) = mpsc::unbounded_channel();
            let (event_tx, mut events) = mpsc::unbounded_channel();
            let mut task = tokio::task::spawn_local(async move {
                run(config, command_rx, event_tx)
                    .await
                    .map_err(|error| format!("{error:?}"))
            });
            let peer_id = loop {
                match events.recv().await {
                    Some(ClientEvent::Started { peer_id, .. }) => break peer_id,
                    Some(_) => {}
                    None => return Err(format!("{:?}", (&mut task).await)),
                }
            };
            Ok(Self {
                commands,
                events,
                task,
                peer: peer_id.parse().unwrap(),
            })
        }

        /// Riavvio sulla stessa porta: l'endpoint QUIC del nodo precedente può
        /// liberare la porta UDP con qualche istante di ritardo.
        async fn restart(make_config: impl Fn() -> ClientConfig) -> Self {
            let mut last_error = String::new();
            for _ in 0..40 {
                match Self::try_start(make_config()).await {
                    Ok(node) => return node,
                    Err(error) => last_error = error,
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            panic!("il nodo non è ripartito: {last_error}");
        }

        fn send(&self, command: ClientCommand) {
            self.commands.send(command).unwrap();
        }

        async fn until(&mut self, predicate: impl Fn(&ClientEvent) -> bool) -> ClientEvent {
            receive_until(&mut self.events, predicate).await
        }

        async fn until_secure(&mut self) {
            self.until(|event| {
                matches!(
                    event,
                    ClientEvent::ContactUpdated {
                        contact: ClientContact { secure: true, .. }
                    }
                )
            })
            .await;
        }

        async fn until_text(&mut self, text: &str) -> ClientMessage {
            let ClientEvent::Message { message } = self
                .until(|event| {
                    matches!(
                        event,
                        ClientEvent::Message { message } if message.direction == "in" && message.body == text
                    )
                })
                .await
            else {
                unreachable!()
            };
            message
        }

        async fn shutdown(self) {
            self.commands.send(ClientCommand::Shutdown).ok();
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("il nodo non si è arrestato")
                .unwrap()
                .unwrap();
        }
    }

    /// Bob importa il link di Alice e aspetta che il canale cifrato sia pronto
    /// da entrambe le parti.
    async fn connect_pair(alice: &mut TestNode, bob: &mut TestNode) {
        alice.send(ClientCommand::RequestContactLink);
        let ClientEvent::ContactLink { link, .. } = alice
            .until(|event| matches!(event, ClientEvent::ContactLink { .. }))
            .await
        else {
            unreachable!()
        };
        bob.send(ClientCommand::ImportContactLink { link });
        // Alice vede Bob come richiesta di contatto: finché non la accetta il
        // canale cifrato non parte.
        alice
            .until(|event| matches!(event, ClientEvent::ContactRequest { .. }))
            .await;
        alice.send(ClientCommand::AcceptContactRequest { peer: bob.peer });
        alice.until_secure().await;
        bob.until_secure().await;
    }

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "msnnext-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::remove_dir_all(&root).ok();
        root
    }

    /// Bob inserisce il codice di Alice: la DHT del mininodo li fa
    /// incontrare e SPAKE2 li aggiunge a vicenda, senza richiesta da accettare.
    async fn join_invite(bob: &mut TestNode, code: &str) -> ClientEvent {
        // L'invito arriva nella DHT poco dopo la creazione: finché non c'è
        // il codice risulta "non trovato".
        for _ in 0..30 {
            bob.send(ClientCommand::JoinInvite { code: code.into() });
            let event = bob
                .until(|event| {
                    matches!(
                        event,
                        ClientEvent::InviteCompleted { .. } | ClientEvent::InviteFailed { .. }
                    )
                })
                .await;
            match &event {
                ClientEvent::InviteFailed { message, .. } if message.contains("non trovato") => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                _ => return event,
            }
        }
        panic!("invito mai trovato nella DHT");
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn invite_code_adds_both_contacts_through_the_dht() {
        let root = test_root("invite");
        let relay_port = available_udp_port();
        let relay_listen: Multiaddr = format!("/ip4/127.0.0.1/udp/{relay_port}/quic-v1")
            .parse()
            .unwrap();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut relay_config =
                    loopback_config("Mininodo", &root.join("relay"), Some(relay_listen.clone()));
                relay_config.relay_server = true;
                let relay = TestNode::start(relay_config).await;
                let bootstrap = relay_listen.clone().with(Protocol::P2p(relay.peer));
                let client = |name: &str| {
                    let mut config = loopback_config(name, &root.join(name), None);
                    config.bootstrap = vec![bootstrap.clone()];
                    config
                };
                let mut alice = TestNode::start(client("alice")).await;
                let mut bob = TestNode::start(client("bob")).await;

                alice.send(ClientCommand::CreateInvite);
                let ClientEvent::Invite { code, .. } = alice
                    .until(|event| matches!(event, ClientEvent::Invite { .. }))
                    .await
                else {
                    unreachable!()
                };

                // Parola sbagliata: nessuna scheda passa, il contatto non c'è.
                let mut parts = code.split('-').map(str::to_owned).collect::<Vec<_>>();
                parts[3] = if parts[3] == "abaco" {
                    "zuppa"
                } else {
                    "abaco"
                }
                .into();
                let wrong = parts.join("-");
                let ClientEvent::InviteFailed { inviter, message } =
                    join_invite(&mut bob, &wrong).await
                else {
                    panic!("un codice sbagliato non deve aggiungere contatti");
                };
                assert!(!inviter);
                assert_eq!(message, invite::INVITE_WRONG_CODE);

                let ClientEvent::InviteCompleted {
                    peer_id,
                    name,
                    inviter,
                } = join_invite(&mut bob, &code).await
                else {
                    panic!("il codice giusto deve funzionare");
                };
                assert_eq!(peer_id, alice.peer.to_string());
                assert_eq!(name, "alice");
                assert!(!inviter);

                // Chi ha creato il codice deve comunque approvare: il codice
                // potrebbe essere stato rubato.
                let ClientEvent::ContactRequest {
                    peer_id,
                    fingerprint,
                    via_invite,
                    ..
                } = alice
                    .until(|event| matches!(event, ClientEvent::ContactRequest { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(peer_id, bob.peer.to_string());
                assert_eq!(fingerprint, peer_fingerprint(bob.peer));
                assert!(via_invite);
                assert!(
                    tokio::time::timeout(Duration::from_millis(1500), bob.until_secure())
                        .await
                        .is_err(),
                    "nessun canale cifrato prima dell'approvazione"
                );
                alice.send(ClientCommand::AcceptContactRequest { peer: bob.peer });

                // La chiave ML-DSA fissata dalla scheda coincide con quella
                // dell'handshake.
                alice.until_secure().await;
                bob.until_secure().await;
                bob.send(ClientCommand::SendText {
                    peer: alice.peer,
                    text: "ciao dal codice".into(),
                });
                alice.until_text("ciao dal codice").await;

                // Il codice è monouso.
                let ClientEvent::InviteFailed { .. } = join_invite(&mut bob, &code).await else {
                    panic!("un codice già usato non deve funzionare di nuovo");
                };

                alice.shutdown().await;
                bob.shutdown().await;
                relay.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn two_clients_exchange_messages_and_reconnect_after_restart() {
        let root = test_root("pair");
        let alice_dir = root.join("alice");
        let bob_dir = root.join("bob");
        let alice_port = available_udp_port();
        let alice_listen: Multiaddr = format!("/ip4/127.0.0.1/udp/{alice_port}/quic-v1")
            .parse()
            .unwrap();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config(
                    "Alice",
                    &alice_dir,
                    Some(alice_listen.clone()),
                ))
                .await;
                let mut bob = TestNode::start(loopback_config("Bob", &bob_dir, None)).await;
                connect_pair(&mut alice, &mut bob).await;

                bob.send(ClientCommand::SendText {
                    peer: alice.peer,
                    text: "prima".into(),
                });
                alice.until_text("prima").await;

                let alice_peer = alice.peer;
                alice.shutdown().await;
                tokio::select! {
                    _ = receive_until(&mut bob.events, |event| {
                        matches!(
                            event,
                            ClientEvent::ContactUpdated {
                                contact: ClientContact { online: false, .. }
                            }
                        )
                    }) => {}
                    result = &mut bob.task => {
                        panic!("Bob terminato durante la disconnessione: {result:?}");
                    }
                }

                let mut alice = TestNode::restart(|| {
                    loopback_config("Alice", &alice_dir, Some(alice_listen.clone()))
                })
                .await;
                assert_eq!(alice.peer, alice_peer);
                bob.until_secure().await;
                alice.until_secure().await;

                alice.send(ClientCommand::SendText {
                    peer: bob.peer,
                    text: "dopo il riavvio".into(),
                });
                bob.until_text("dopo il riavvio").await;

                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #18: le sessioni si rinnovano con un handshake nuovo mentre la
    /// conversazione continua, senza perdere messaggi in nessuna direzione.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn sessions_rekey_periodically_without_losing_messages() {
        let root = test_root("rekey");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let fast = Duration::from_millis(500);
                let mut alice = TestNode::start(
                    loopback_config("Alice", &root.join("alice"), None)
                        .with_session_rekey_interval(fast),
                )
                .await;
                let mut bob = TestNode::start(
                    loopback_config("Bob", &root.join("bob"), None)
                        .with_session_rekey_interval(fast),
                )
                .await;
                connect_pair(&mut alice, &mut bob).await;
                let before = SESSION_REKEYS.load(std::sync::atomic::Ordering::Relaxed);

                for round in 0..8 {
                    alice.send(ClientCommand::SendText {
                        peer: bob.peer,
                        text: format!("da Alice {round}"),
                    });
                    bob.until_text(&format!("da Alice {round}")).await;
                    bob.send(ClientCommand::SendText {
                        peer: alice.peer,
                        text: format!("da Bob {round}"),
                    });
                    alice.until_text(&format!("da Bob {round}")).await;
                    tokio::time::sleep(Duration::from_millis(900)).await;
                }

                // Ogni rinnovo conta due volte: iniziatore e responder.
                let rekeys = SESSION_REKEYS.load(std::sync::atomic::Ordering::Relaxed) - before;
                assert!(rekeys >= 4, "sessione rinnovata solo {rekeys} volte");
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn pq_identity_is_pinned_and_enforced() {
        let root = test_root("pq-pin");
        fs::create_dir_all(&root).unwrap();
        let history = History::open(&root.join("history.db"), [4; 32]).unwrap();
        let peer = test_peer();
        let first = crypto::MlDsaIdentity::from_seed(&[1; 32]).unwrap();
        let other = crypto::MlDsaIdentity::from_seed(&[2; 32]).unwrap();

        // Client vecchio mai visto firmare: accettato.
        assert!(pin_pq_identity(&history, peer, None).is_ok());
        // Primo handshake firmato: la chiave si fissa.
        assert!(pin_pq_identity(&history, peer, Some(first.public_key())).is_ok());
        assert!(pin_pq_identity(&history, peer, Some(first.public_key())).is_ok());
        // Chiave diversa o firma sparita (downgrade): rifiutati.
        assert!(pin_pq_identity(&history, peer, Some(other.public_key())).is_err());
        assert!(pin_pq_identity(&history, peer, None).is_err());
        // Rimuovere il contatto azzera la chiave fissata.
        history.delete_contact(&peer.to_string()).unwrap();
        assert!(pin_pq_identity(&history, peer, Some(other.public_key())).is_ok());
        drop(history);
        fs::remove_dir_all(root).ok();
    }

    /// Audit #1: un testo oltre 16 KB spegneva il nodo. Ora il mittente
    /// riceve un errore e la conversazione continua.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn oversized_text_reports_error_and_node_keeps_running() {
        let root = test_root("long-text");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice =
                    TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob =
                    TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                bob.send(ClientCommand::SendText {
                    peer: alice.peer,
                    text: "x".repeat(20 * 1024),
                });
                let ClientEvent::Error { message } = bob
                    .until(|event| matches!(event, ClientEvent::Error { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert!(message.contains("troppo lungo"), "{message}");

                bob.send(ClientCommand::SendText {
                    peer: alice.peer,
                    text: "ancora vivo".into(),
                });
                alice.until_text("ancora vivo").await;
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Collega `joining` all'account di `inviter` e aspetta il primo sync.
    async fn link_devices(inviter: &mut TestNode, joining: &mut TestNode) {
        inviter.send(ClientCommand::RequestDeviceLink);
        let ClientEvent::DeviceLink { link, .. } = inviter
            .until(|event| matches!(event, ClientEvent::DeviceLink { .. }))
            .await
        else {
            unreachable!()
        };
        joining.send(ClientCommand::ImportDeviceLink { link });
        joining
            .until(|event| {
                eprintln!("DBG {event:?}");
                matches!(event, ClientEvent::DeviceSynchronized { paired: true, .. })
            })
            .await;
    }

    fn history_key_for(dir: &Path) -> [u8; 32] {
        let bytes = fs::read(dir.join("identity.key")).unwrap();
        blake3::derive_key("msnnext local history v1", &bytes)
    }

    fn device_config(name: &str, dir: &Path) -> ClientConfig {
        loopback_config(name, dir, None).with_account_key(generate_secret(), |_| Ok(()))
    }

    /// "Usa codice" da un account già in uso univa le chat dei due account
    /// e cambiava utente. Ora il codice di un contatto e un dispositivo con
    /// dati propri vengono rifiutati, e l'account che invita resta intatto.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn pairing_never_mixes_two_accounts() {
        let root = test_root("pair-isolation");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(device_config("Alice", &root.join("alice"))).await;
                let mut bob = TestNode::start(device_config("Bob", &root.join("bob"))).await;
                let mut carol = TestNode::start(device_config("Carol", &root.join("carol"))).await;
                let mut dave = TestNode::start(device_config("Dave", &root.join("dave"))).await;
                connect_pair(&mut alice, &mut bob).await;
                connect_pair(&mut carol, &mut dave).await;
                dave.send(ClientCommand::SendText {
                    peer: carol.peer,
                    text: "solo per carol".into(),
                });
                carol.until_text("solo per carol").await;

                alice.send(ClientCommand::RequestDeviceLink);
                let ClientEvent::DeviceLink { link, .. } = alice
                    .until(|event| matches!(event, ClientEvent::DeviceLink { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(preview_device_link(&link).unwrap().name, "Alice");

                // Bob è un contatto di Alice: è un'altra persona.
                bob.send(ClientCommand::ImportDeviceLink { link: link.clone() });
                let ClientEvent::Error { message } = bob
                    .until(|event| matches!(event, ClientEvent::Error { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(message, PAIRING_WITH_CONTACT);

                // Carol ha già un suo account con chat: non le porta ad Alice.
                carol.send(ClientCommand::ImportDeviceLink { link: link.clone() });
                let ClientEvent::Error { message } = carol
                    .until(|event| matches!(event, ClientEvent::Error { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(message, PAIRING_WITH_LOCAL_DATA);

                // Il codice resta valido per un dispositivo nuovo, che riceve
                // le chat di Alice e nessuna di Carol.
                let mut laptop =
                    TestNode::start(device_config("Laptop", &root.join("laptop"))).await;
                laptop.send(ClientCommand::ImportDeviceLink { link });
                laptop
                    .until(|event| {
                        matches!(event, ClientEvent::DeviceSynchronized { paired: true, .. })
                    })
                    .await;

                let (carol_peer, dave_peer) = (carol.peer, dave.peer);
                for node in [alice, bob, carol, dave, laptop] {
                    node.shutdown().await;
                }
                let alice_history = History::open(
                    &root.join("alice").join("history.db"),
                    history_key_for(&root.join("alice")),
                )
                .unwrap();
                let foreign = [carol_peer.to_string(), dave_peer.to_string()];
                let leaked = alice_history
                    .contacts()
                    .unwrap()
                    .into_iter()
                    .filter(|contact| foreign.contains(&contact.peer))
                    .count();
                assert_eq!(leaked, 0, "i contatti di Carol sono finiti da Alice");
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #19: un dispositivo perso restava autorizzato per sempre. Ora si
    /// revoca: la chiave di account cambia e gli altri dispositivi la
    /// adottano dopo la conferma dell'utente.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn revoked_device_is_excluded_and_the_account_key_rotates() {
        let root = test_root("revoke");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut pc = TestNode::start(device_config("PC", &root.join("pc"))).await;
                let mut laptop = TestNode::start(device_config("Laptop", &root.join("laptop"))).await;
                let mut phone = TestNode::start(device_config("Telefono", &root.join("phone"))).await;
                link_devices(&mut pc, &mut laptop).await;
                link_devices(&mut pc, &mut phone).await;

                pc.send(ClientCommand::RevokeDevice { peer: phone.peer });
                let phone_id = phone.peer.to_string();
                pc.until(|event| matches!(event, ClientEvent::DeviceRevoked { peer_id } if *peer_id == phone_id))
                    .await;

                // Il laptop chiede conferma prima di adottare la nuova chiave.
                let ClientEvent::AccountRotationRequested {
                    request_id,
                    from_name,
                    revoked,
                    ..
                } = laptop
                    .until(|event| matches!(event, ClientEvent::AccountRotationRequested { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(from_name, "PC");
                assert_eq!(revoked, vec!["Telefono".to_owned()]);
                laptop.send(ClientCommand::AnswerAccountRotation {
                    request_id,
                    accept: true,
                });
                laptop
                    .until(|event| matches!(event, ClientEvent::DeviceRevoked { peer_id } if *peer_id == phone_id))
                    .await;
                // Il sync con il PC riesce: hanno di nuovo la stessa chiave.
                let pc_id = pc.peer.to_string();
                laptop
                    .until(|event| matches!(event, ClientEvent::DeviceSynchronized { peer_id, paired: false, .. } if *peer_id == pc_id))
                    .await;

                pc.shutdown().await;
                laptop.shutdown().await;
                phone.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Un dispositivo spento durante la revoca riceve la nuova chiave al
    /// primo sync, che arriva sigillato con la chiave ritirata.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn offline_device_learns_the_rotation_at_its_next_sync() {
        let root = test_root("rotate-offline");
        let laptop_dir = root.join("laptop");
        let laptop_key = Arc::new(std::sync::Mutex::new(generate_secret()));
        let laptop_config = || {
            let store = laptop_key.clone();
            let key = *laptop_key.lock().unwrap();
            loopback_config("Laptop", &laptop_dir, None).with_account_key(key, move |key| {
                *store.lock().unwrap() = key;
                Ok(())
            })
        };
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut pc = TestNode::start(device_config("PC", &root.join("pc"))).await;
                let mut laptop = TestNode::start(laptop_config()).await;
                let mut phone = TestNode::start(device_config("Telefono", &root.join("phone"))).await;
                link_devices(&mut pc, &mut laptop).await;
                link_devices(&mut pc, &mut phone).await;
                laptop.shutdown().await;

                pc.send(ClientCommand::RevokeDevice { peer: phone.peer });
                let phone_id = phone.peer.to_string();
                pc.until(|event| matches!(event, ClientEvent::DeviceRevoked { peer_id } if *peer_id == phone_id))
                    .await;

                let mut laptop = TestNode::restart(laptop_config).await;
                let ClientEvent::AccountRotationRequested { request_id, .. } = laptop
                    .until(|event| matches!(event, ClientEvent::AccountRotationRequested { .. }))
                    .await
                else {
                    unreachable!()
                };
                laptop.send(ClientCommand::AnswerAccountRotation {
                    request_id,
                    accept: true,
                });
                let pc_id = pc.peer.to_string();
                laptop
                    .until(|event| matches!(event, ClientEvent::DeviceSynchronized { peer_id, paired: false, .. } if *peer_id == pc_id))
                    .await;

                pc.shutdown().await;
                laptop.shutdown().await;
                phone.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Una rotazione che l'utente non riconosce revoca chi l'ha chiesta.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn refused_rotation_revokes_the_requesting_device() {
        let root = test_root("refuse-rotation");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut pc = TestNode::start(device_config("PC", &root.join("pc"))).await;
                let mut laptop = TestNode::start(device_config("Laptop", &root.join("laptop"))).await;
                let mut phone = TestNode::start(device_config("Telefono", &root.join("phone"))).await;
                link_devices(&mut pc, &mut laptop).await;
                link_devices(&mut pc, &mut phone).await;

                // Il telefono raggiunge da solo anche il laptop.
                let phone_id = phone.peer.to_string();
                laptop
                    .until(|event| matches!(event, ClientEvent::DeviceSynchronized { peer_id, .. } if *peer_id == phone_id))
                    .await;
                // Chi ha rubato il telefono prova a revocare il PC.
                phone.send(ClientCommand::RevokeDevice { peer: pc.peer });
                let ClientEvent::AccountRotationRequested { request_id, .. } = laptop
                    .until(|event| matches!(event, ClientEvent::AccountRotationRequested { .. }))
                    .await
                else {
                    unreachable!()
                };
                laptop.send(ClientCommand::AnswerAccountRotation {
                    request_id,
                    accept: false,
                });
                laptop
                    .until(|event| matches!(event, ClientEvent::DeviceRevoked { peer_id } if *peer_id == phone_id))
                    .await;

                pc.shutdown().await;
                laptop.shutdown().await;
                phone.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #1: anche un errore interno a metà gestione di un evento non deve
    /// fermare il loop.
    #[tokio::test(flavor = "current_thread")]
    async fn internal_error_in_event_is_reported_and_loop_continues() {
        let root = test_root("loop-error");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut node = TestNode::start(loopback_config("Solo", &root, None)).await;
                // Link di pairing valido ma con un Peer ID non valido: prima il
                // `?` sul parse terminava `run()`.
                let link = devices::create_pairing_link(
                    DeviceDescriptor {
                        peer_id: "non-un-peer".into(),
                        name: "x".into(),
                        addresses: Vec::new(),
                    },
                    now_ms(),
                )
                .unwrap()
                .0;
                node.send(ClientCommand::ImportDeviceLink { link });
                node.until(|event| matches!(event, ClientEvent::Error { .. }))
                    .await;
                node.send(ClientCommand::RequestContactLink);
                node.until(|event| matches!(event, ClientEvent::ContactLink { .. }))
                    .await;
                node.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #3: un messaggio rifiutato perché il destinatario non ha più la
    /// sessione veniva trattato come consegnato e spariva. Ora il mittente
    /// rifà l'handshake e lo reinvia.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn message_rejected_for_stale_session_is_redelivered() {
        let root = test_root("rejected");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                alice.send(ClientCommand::ForgetSession { peer: bob.peer });
                alice
                    .until(|event| matches!(event, ClientEvent::ContactStatus { status, .. } if status == "session-forgotten"))
                    .await;
                bob.send(ClientCommand::SendText {
                    peer: alice.peer,
                    text: "dopo la sessione persa".into(),
                });
                alice.until_text("dopo la sessione persa").await;
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #4: con le chiavi del ratchet fuori sincronia ogni messaggio in
    /// arrivo veniva rifiutato fino alla riconnessione. Ora la sessione si
    /// rinegozia e i messaggi arrivano.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn desynchronized_session_recovers_without_reconnecting() {
        let root = test_root("scrambled");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                alice.send(ClientCommand::ScrambleSession { peer: bob.peer });
                alice
                    .until(|event| matches!(event, ClientEvent::ContactStatus { status, .. } if status == "session-scrambled"))
                    .await;
                for index in 0..3 {
                    bob.send(ClientCommand::SendText {
                        peer: alice.peer,
                        text: format!("messaggio {index}"),
                    });
                }
                for index in 0..3 {
                    alice.until_text(&format!("messaggio {index}")).await;
                }
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #5: l'hash del file veniva calcolato nel thread del loop, che per
    /// tutto quel tempo non gestiva messaggi né ping. Ora un messaggio arriva
    /// mentre un file da 1 GB è ancora in preparazione.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn hashing_a_large_file_does_not_block_messages() {
        let root = test_root("big-file");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("grande.bin");
        // File sparso: non occupa disco ma va letto e hashato per intero.
        // 2 GB: con 1 GB, su una CI veloce e con
        // gli altri test in parallelo, l'hash poteva finire prima che il
        // testo facesse il giro della rete.
        fs::File::create(&source)
            .unwrap()
            .set_len(2 * 1024 * 1024 * 1024)
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                bob.send(ClientCommand::SendFile {
                    peer: alice.peer,
                    path: source.clone(),
                });
                alice.send(ClientCommand::SendText {
                    peer: bob.peer,
                    text: "mentre prepari il file".into(),
                });
                // Il testo deve arrivare a Bob prima che il suo file sia pronto.
                let first = receive_until_within(&mut bob.events, Duration::from_secs(120), |event| {
                    matches!(event, ClientEvent::Message { message } if message.direction == "out" && message.kind == "file")
                        || matches!(event, ClientEvent::Message { message } if message.direction == "in" && message.body == "mentre prepari il file")
                })
                .await;
                assert!(
                    matches!(&first, ClientEvent::Message { message } if message.direction == "in"),
                    "il loop era bloccato dall'hash del file: {first:?}"
                );

                let ClientEvent::IncomingAttachmentOffered { offer_id, .. } = receive_until_within(
                    &mut alice.events,
                    Duration::from_secs(120),
                    |event| matches!(event, ClientEvent::IncomingAttachmentOffered { .. }),
                )
                .await
                else {
                    unreachable!()
                };
                alice.send(ClientCommand::RejectAttachment { offer_id });
                bob.until(|event| matches!(event, ClientEvent::AttachmentFailed { reason, .. } if reason.contains("rifiutato")))
                    .await;
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #10: un messaggio senza conferma veniva ritentato solo dopo il
    /// timeout di 5 minuti delle offerte di file. Ora si reinvia dopo pochi
    /// secondi e il destinatario non lo mostra due volte.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn unacknowledged_message_is_resent_quickly_without_duplicates() {
        let root = test_root("ack-timeout");
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                bob.send(ClientCommand::WithholdAcks { enabled: true });
                alice.send(ClientCommand::SendText { peer: bob.peer, text: "conferma persa".into() });
                bob.until_text("conferma persa").await;
                bob.send(ClientCommand::WithholdAcks { enabled: false });
                // Il reinvio arriva ben prima dei 5 minuti ed è riconosciuto
                // come doppione; poi nessun altro messaggio uguale.
                let event = receive_until_within(&mut bob.events, Duration::from_secs(10), |event| {
                    matches!(event, ClientEvent::ContactStatus { status, .. } if status == "duplicate-ignored")
                        || matches!(event, ClientEvent::Message { message } if message.body == "conferma persa")
                })
                .await;
                assert!(
                    matches!(&event, ClientEvent::ContactStatus { .. }),
                    "messaggio mostrato due volte: {event:?}"
                );
                alice.send(ClientCommand::SendText { peer: bob.peer, text: "dopo".into() });
                bob.until_text("dopo").await;
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Ordina i due nodi per ruolo nell'handshake: il primo è quello che lo
    /// avvia quando manca la sessione.
    fn by_handshake_role<'a>(
        a: &'a mut TestNode,
        b: &'a mut TestNode,
    ) -> (&'a mut TestNode, &'a mut TestNode) {
        if should_initiate(a.peer, b.peer) {
            (a, b)
        } else {
            (b, a)
        }
    }

    async fn forget_session(node: &mut TestNode, peer: PeerId) {
        node.send(ClientCommand::ForgetSession { peer });
        node.until(|event| matches!(event, ClientEvent::ContactStatus { status, .. } if status == "session-forgotten"))
            .await;
    }

    /// Audit #7 e #9: trillo, emoticon personalizzata e "elimina per tutti"
    /// partivano una volta sola e senza sessione andavano persi in silenzio.
    /// Ora restano in coda e arrivano dopo il nuovo handshake.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn nudge_emoticon_and_delete_survive_a_lost_session() {
        let root = test_root("queued-events");
        fs::create_dir_all(&root).unwrap();
        let emoticon = root.join("gatto.png");
        fs::write(
            &emoticon,
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01",
        )
        .unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;
                // Chi invia avvia anche l'handshake: la coda si svuota da sola.
                let (sender, receiver) = by_handshake_role(&mut alice, &mut bob);

                forget_session(sender, receiver.peer).await;
                sender.send(ClientCommand::SendNudge { peer: receiver.peer });
                receiver
                    .until(|event| matches!(event, ClientEvent::Message { message } if message.direction == "in" && message.kind == "nudge"))
                    .await;

                sender.send(ClientCommand::CreateEmoticon { path: emoticon.clone(), trigger: ":gatto:".into() });
                sender.until(|event| matches!(event, ClientEvent::EmoticonCatalog { .. })).await;
                forget_session(sender, receiver.peer).await;
                sender.send(ClientCommand::SendText { peer: receiver.peer, text: "ciao :gatto:".into() });
                // Offerta e testo viaggiano su richieste separate: l'ordine di
                // arrivo non è garantito.
                let mut offered = false;
                let mut received = None;
                while !offered || received.is_none() {
                    match receiver
                        .until(|event| {
                            matches!(event, ClientEvent::EmoticonOffered { .. })
                                || matches!(event, ClientEvent::Message { message } if message.direction == "in" && message.body == "ciao :gatto:")
                        })
                        .await
                    {
                        ClientEvent::EmoticonOffered { .. } => offered = true,
                        ClientEvent::Message { message } => received = Some(message),
                        _ => unreachable!(),
                    }
                }
                let message = received.unwrap();
                assert_eq!(message.emoticons.len(), 1);
                let event_id = message.event_id.expect("id del messaggio");

                forget_session(sender, receiver.peer).await;
                sender.send(ClientCommand::DeleteMessageForEveryone { peer: receiver.peer, event_id: event_id.clone() });
                receiver
                    .until(|event| matches!(event, ClientEvent::MessageDeleted { event_id: deleted, .. } if *deleted == event_id))
                    .await;
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #6: se la sessione cadeva durante un invio il chunk successivo
    /// non partiva, senza errori, e la barra restava ferma per sempre.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn file_transfer_interrupted_by_lost_session_reports_failure() {
        let root = test_root("file-lost-session");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("doc.bin");
        fs::write(&source, vec![7u8; 2 * attachments::CHUNK_SIZE]).unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice =
                    TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob =
                    TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;
                // Chi invia non avvia l'handshake: la sessione resta assente
                // quando arriva la richiesta dei chunk.
                let (receiver, sender) = by_handshake_role(&mut alice, &mut bob);

                sender.send(ClientCommand::SendFile {
                    peer: receiver.peer,
                    path: source.clone(),
                });
                let ClientEvent::IncomingAttachmentOffered { offer_id, .. } = receiver
                    .until(|event| matches!(event, ClientEvent::IncomingAttachmentOffered { .. }))
                    .await
                else {
                    unreachable!()
                };
                forget_session(sender, receiver.peer).await;
                receiver.send(ClientCommand::AcceptAttachment { offer_id });
                let ClientEvent::AttachmentFailed { filename, reason } = sender
                    .until(|event| matches!(event, ClientEvent::AttachmentFailed { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(filename, "doc.bin");
                assert!(reason.contains("sessione"), "{reason}");
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #15 e #16: il rifiuto di una richiesta di contatto non
    /// sopravviveva al riavvio, e la UI lo annunciava come "contatto eliminato".
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn rejected_contact_request_stays_rejected_after_restart() {
        let root = test_root("reject-request");
        let alice_dir = root.join("alice");
        let alice_port = available_udp_port();
        let alice_listen: Multiaddr = format!("/ip4/127.0.0.1/udp/{alice_port}/quic-v1")
            .parse()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &alice_dir, Some(alice_listen.clone()))).await;
                let bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                alice.send(ClientCommand::RequestContactLink);
                let ClientEvent::ContactLink { link, .. } = alice
                    .until(|event| matches!(event, ClientEvent::ContactLink { .. }))
                    .await
                else {
                    unreachable!()
                };
                bob.send(ClientCommand::ImportContactLink { link: link.clone() });
                alice.until(|event| matches!(event, ClientEvent::ContactRequest { .. })).await;
                alice.send(ClientCommand::RejectContactRequest { peer: bob.peer });
                alice
                    .until(|event| matches!(event, ClientEvent::ContactRemoved { reason, .. } if reason == "rejected"))
                    .await;
                alice.shutdown().await;

                let mut alice = TestNode::restart(|| {
                    loopback_config("Alice", &alice_dir, Some(alice_listen.clone()))
                })
                .await;
                bob.send(ClientCommand::ImportContactLink { link });
                let again = tokio::time::timeout(
                    Duration::from_secs(6),
                    alice.until(|event| matches!(event, ClientEvent::ContactRequest { .. })),
                )
                .await;
                assert!(again.is_err(), "la richiesta rifiutata è ricomparsa: {again:?}");
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #14: "Annulla" fermava gli invii di tutte le chat e il
    /// destinatario restava con l'offerta aperta. Ora vale solo per la chat
    /// indicata e il destinatario viene avvisato.
    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn cancel_is_scoped_and_reaches_the_recipient() {
        let root = test_root("cancel-file");
        fs::create_dir_all(&root).unwrap();
        let first = root.join("primo.bin");
        let second = root.join("secondo.bin");
        fs::write(&first, vec![1u8; attachments::CHUNK_SIZE + 10]).unwrap();
        fs::write(&second, vec![2u8; attachments::CHUNK_SIZE + 20]).unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice = TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob = TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                // Annullare un'altra chat non tocca l'invio a Bob.
                alice.send(ClientCommand::SendFile { peer: bob.peer, path: first.clone() });
                let ClientEvent::IncomingAttachmentOffered { offer_id, .. } = bob
                    .until(|event| matches!(event, ClientEvent::IncomingAttachmentOffered { .. }))
                    .await
                else {
                    unreachable!()
                };
                alice.send(ClientCommand::CancelFileTransfers { conversation: Some(format!("group:{}", "ab".repeat(16))) });
                alice.until(|event| matches!(event, ClientEvent::AttachmentTransfersCancelled)).await;
                bob.send(ClientCommand::AcceptAttachment { offer_id });
                bob.until(|event| matches!(event, ClientEvent::AttachmentReceived { filename, .. } if filename == "primo.bin"))
                    .await;

                // Annullare la chat con Bob chiude la sua offerta.
                alice.send(ClientCommand::SendFile { peer: bob.peer, path: second.clone() });
                let ClientEvent::IncomingAttachmentOffered { offer_id, .. } = bob
                    .until(|event| matches!(event, ClientEvent::IncomingAttachmentOffered { .. }))
                    .await
                else {
                    unreachable!()
                };
                alice.send(ClientCommand::CancelFileTransfers { conversation: Some(bob.peer.to_string()) });
                let ClientEvent::IncomingAttachmentCancelled { offer_id: cancelled, .. } = bob
                    .until(|event| matches!(event, ClientEvent::IncomingAttachmentCancelled { .. }))
                    .await
                else {
                    unreachable!()
                };
                assert_eq!(cancelled, Some(offer_id));
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    /// Audit #12: l'anteprima in chat era il file intero in base64 (fino a
    /// 50 MB). Ora è una miniatura di pochi KB.
    #[test]
    fn inline_preview_is_a_small_thumbnail() {
        let photo = image::RgbImage::from_fn(3000, 2000, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(photo)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let original = png.into_inner();

        let data_url = thumbnail_data_url(&original).unwrap();
        let encoded = data_url.strip_prefix("data:image/png;base64,").unwrap();
        let thumbnail = image::load_from_memory(&BASE64.decode(encoded).unwrap()).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (640, 427));
        assert!(
            data_url.len() < original.len() / 4,
            "{} vs {}",
            data_url.len(),
            original.len()
        );
        assert!(thumbnail_data_url(b"non un'immagine").is_err());
    }

    /// Audit #8: le emoticon viaggiano col relay, ma una richiesta resta
    /// sotto il limite di libp2p.
    #[test]
    fn relay_carries_emoticons_within_budget() {
        let store =
            std::env::temp_dir().join(format!("msnnext-relay-emoticons-{}", std::process::id()));
        fs::create_dir_all(&store).unwrap();
        let mut triggers = Vec::new();
        let small = store.join("small.png");
        fs::write(&small, b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01").unwrap();
        create_emoticon(&small, ":piccola:", &store, &mut triggers).unwrap();
        let mut big = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x02".to_vec();
        big.resize(RELAY_EMOTICON_BUDGET, 0);
        fs::write(store.join("big.png"), &big).unwrap();
        create_emoticon(&store.join("big.png"), ":grande:", &store, &mut triggers).unwrap();

        let spans = resolve_emoticons(":piccola: :grande: :piccola:", &triggers).unwrap();
        assert_eq!(spans.len(), 3);
        let offers = relay_emoticon_offers(&spans, &triggers, &store);
        // Una sola copia per immagine, e la grande non ci sta più.
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].metadata.asset_id, spans[0].asset_id);
        fs::remove_dir_all(store).ok();
    }

    /// Audit #8: un dispositivo aggiornato deve capire il RelayText dei client
    /// precedenti, che non hanno i campi delle emoticon.
    #[test]
    fn relay_request_from_older_client_still_decodes() {
        #[derive(Serialize)]
        enum OldRequest {
            RelayText {
                target: String,
                id: [u8; 16],
                text: String,
            },
        }
        let key = [9u8; 32];
        let sealed = devices::seal(
            &key,
            &OldRequest::RelayText {
                target: "x".into(),
                id: [1; 16],
                text: "ciao".into(),
            },
        )
        .unwrap();
        let DeviceRequest::RelayText {
            text,
            emoticons,
            offers,
            ..
        } = devices::open::<DeviceRequest>(&key, &sealed).unwrap()
        else {
            panic!("variante inattesa")
        };
        assert_eq!(text, "ciao");
        assert!(emoticons.is_empty() && offers.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    #[cfg_attr(windows, ignore = "il teardown libp2p può bloccare l'harness Windows")]
    async fn sent_file_arrives_intact_and_sender_can_reopen_it() {
        let root = test_root("file");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("foto.bin");
        let bytes = (0..(3 * attachments::CHUNK_SIZE + 123))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&source, &bytes).unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut alice =
                    TestNode::start(loopback_config("Alice", &root.join("alice"), None)).await;
                let mut bob =
                    TestNode::start(loopback_config("Bob", &root.join("bob"), None)).await;
                connect_pair(&mut alice, &mut bob).await;

                bob.send(ClientCommand::SendFile {
                    peer: alice.peer,
                    path: source.clone(),
                });
                let ClientEvent::IncomingAttachmentOffered { offer_id, .. } = alice
                    .until(|event| matches!(event, ClientEvent::IncomingAttachmentOffered { .. }))
                    .await
                else {
                    unreachable!()
                };
                alice.send(ClientCommand::AcceptAttachment { offer_id });
                let ClientEvent::AttachmentReceived { id, .. } =
                    receive_until_within(&mut alice.events, Duration::from_secs(60), |event| {
                        matches!(event, ClientEvent::AttachmentReceived { .. })
                    })
                    .await
                else {
                    unreachable!()
                };
                let received = root.join("ricevuto.bin");
                alice.send(ClientCommand::ExportAttachment {
                    id: id.clone(),
                    path: received.clone(),
                });
                alice
                    .until(|event| matches!(event, ClientEvent::AttachmentExported { .. }))
                    .await;
                assert_eq!(fs::read(&received).unwrap(), bytes);

                // Il mittente riapre il proprio file dal riferimento, senza copia.
                let reopened = root.join("riaperto.bin");
                bob.send(ClientCommand::ExportAttachment {
                    id,
                    path: reopened.clone(),
                });
                bob.until(|event| matches!(event, ClientEvent::AttachmentExported { .. }))
                    .await;
                assert_eq!(fs::read(&reopened).unwrap(), bytes);
                alice.shutdown().await;
                bob.shutdown().await;
            })
            .await;
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_session_rejections_trigger_redelivery() {
        assert!(is_session_rejection(REJECT_NO_SESSION));
        assert!(is_session_rejection(REJECT_UNDECRYPTABLE));
        assert!(!is_session_rejection("contatto rimosso"));
        assert!(!is_session_rejection("testo non valido"));
    }

    /// Audit #2: se il nodo termina con un errore la UI deve ricevere
    /// `Stopped`, altrimenti i contatti restano verdi.
    #[tokio::test(flavor = "current_thread")]
    async fn failed_node_emits_error_then_stopped() {
        let root = test_root("stopped");
        fs::create_dir_all(root.join("history.db")).unwrap();
        let config = loopback_config("Rotto", &root, None);
        let (_commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();
        assert!(run(config, command_rx, event_tx).await.is_err());
        let mut received = Vec::new();
        while let Ok(event) = events.try_recv() {
            received.push(event);
        }
        assert!(
            matches!(received.last(), Some(ClientEvent::Stopped)),
            "{received:?}"
        );
        assert!(received
            .iter()
            .any(|event| matches!(event, ClientEvent::Error { .. })));
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
        let presence = ChatEvent::Presence(presence_update("Alice", "", true, &None));
        let text = ChatEvent::Text(TextMessage {
            text: "ciao".into(),
            emoticons: vec![],
            id: [0; 16],
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
            let config =
                ClientConfig::desktop("Alice".into(), data_dir.clone(), None, None).unwrap();
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

    #[test]
    fn desktop_relay_is_used_for_bootstrap_and_fallback() {
        let default =
            ClientConfig::desktop("Alice".into(), std::env::temp_dir(), None, None).unwrap();
        let relay_peer = PeerId::from(Keypair::generate_ed25519().public());
        let relay = format!("/dns4/relay.example.com/tcp/4001/p2p/{relay_peer}");
        let config = ClientConfig::desktop(
            "Alice".into(),
            std::env::temp_dir(),
            None,
            Some(relay.clone()),
        )
        .unwrap();

        assert_eq!(default.relays, vec![DEFAULT_RELAY_ADDRESS.parse().unwrap()]);
        assert_eq!(config.bootstrap, vec![relay.parse().unwrap()]);
        assert_eq!(config.relays, config.bootstrap);
        assert!(ClientConfig::desktop(
            "Alice".into(),
            std::env::temp_dir(),
            None,
            Some("/dns4/relay.example.com/tcp/4001".into()),
        )
        .is_err());

        let backup_peer = PeerId::from(Keypair::generate_ed25519().public());
        let backup = format!("/ip4/10.0.0.1/udp/4001/quic-v1/p2p/{backup_peer}");
        let several = ClientConfig::desktop(
            "Alice".into(),
            std::env::temp_dir(),
            None,
            Some(format!("{relay}, {backup}\n{relay}")),
        )
        .unwrap();
        assert_eq!(
            several.relays,
            vec![relay.parse().unwrap(), backup.parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn mininode_serves_the_dht() {
        let swarm = build_swarm(Keypair::generate_ed25519(), true).unwrap();
        let relay_peer = PeerId::from(Keypair::generate_ed25519().public());
        let relay = format!("/dns4/relay.example.com/tcp/4001/p2p/{relay_peer}")
            .parse::<Multiaddr>()
            .unwrap();
        let expected_route = format!("{relay}/p2p-circuit").parse::<Multiaddr>().unwrap();

        assert_eq!(swarm.behaviour().kad.mode(), kad::Mode::Server);
        assert_eq!(
            contact_addresses(&swarm, &[relay], *swarm.local_peer_id()).first(),
            Some(&expected_route)
        );
    }

    #[test]
    fn notification_mutes_expire_or_remain_permanent() {
        let mutes = HashMap::from([
            ("peer:temporary".to_owned(), Some(200)),
            ("group:permanent".to_owned(), None),
        ]);

        assert!(!notification_mute_allows(&mutes, "peer:temporary", 199));
        assert!(notification_mute_allows(&mutes, "peer:temporary", 200));
        assert!(!notification_mute_allows(&mutes, "group:permanent", 999));
        assert!(notification_mute_allows(&mutes, "peer:other", 0));
    }
}
