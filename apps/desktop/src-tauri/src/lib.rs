use std::{
    collections::HashSet,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use msnnext_core::{ClientCommand, ClientConfig, ClientEvent, GroupModeration};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

type CommandSender = mpsc::UnboundedSender<ClientCommand>;
type RunningNode = (u64, CommandSender);

#[derive(Clone, Default)]
struct NodeState {
    commands: Arc<Mutex<Option<RunningNode>>>,
    next_generation: Arc<AtomicU64>,
    workers: Arc<(Mutex<HashSet<u64>>, Condvar)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeConfig {
    name: String,
    connect: Option<String>,
    relay: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredProfile {
    name: String,
    avatar_file: Option<String>,
    #[serde(default = "enabled_by_default")]
    preview_sent_images: bool,
    #[serde(default)]
    preview_received_images: bool,
    #[serde(default = "enabled_by_default")]
    nudge_sound: bool,
    #[serde(default)]
    relay_address: String,
    #[serde(default = "default_font_scale")]
    font_scale: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileView {
    name: String,
    avatar_data_url: Option<String>,
    preview_sent_images: bool,
    preview_received_images: bool,
    nudge_sound: bool,
    relay_address: String,
    font_scale: u16,
}

#[derive(Deserialize, Serialize)]
struct StoredIdentity {
    version: u8,
    classic: Vec<u8>,
    ml_dsa_seed: [u8; 32],
}

fn enabled_by_default() -> bool {
    true
}

fn default_font_scale() -> u16 {
    125
}

fn valid_font_scale(value: u16) -> bool {
    matches!(value, 100 | 115 | 125 | 140)
}

fn profile_view(data_dir: &Path, profile: StoredProfile) -> Result<ProfileView, String> {
    let avatar_data_url = profile
        .avatar_file
        .as_ref()
        .map(|file| {
            std::fs::read(data_dir.join(file))
                .map(|bytes| format!("data:image/png;base64,{}", BASE64.encode(bytes)))
                .map_err(|error| error.to_string())
        })
        .transpose()?;
    Ok(ProfileView {
        name: profile.name,
        avatar_data_url,
        preview_sent_images: profile.preview_sent_images,
        preview_received_images: profile.preview_received_images,
        nudge_sound: profile.nudge_sound,
        relay_address: profile.relay_address,
        font_scale: profile.font_scale,
    })
}

fn parse_peer(value: &str) -> Result<libp2p_identity::PeerId, String> {
    msnnext_core::parse_peer_id(value)
}

fn desktop_identity(data_dir: &Path) -> Result<StoredIdentity, String> {
    let entry = keyring::Entry::new("app.msnnext.desktop", "identity-v1")
        .map_err(|error| format!("keystore non disponibile: {error}"))?;
    let legacy_path = data_dir.join("identity.key");
    let identity = match entry.get_secret() {
        Ok(bytes) => match serde_json::from_slice::<StoredIdentity>(&bytes) {
            Ok(identity) if identity.version == 1 => identity,
            _ => {
                libp2p_identity::Keypair::from_protobuf_encoding(&bytes)
                    .map_err(|error| format!("identità nel keystore non valida: {error}"))?;
                let identity = StoredIdentity {
                    version: 1,
                    classic: bytes,
                    ml_dsa_seed: msnnext_core::generate_secret(),
                };
                entry
                    .set_secret(&serde_json::to_vec(&identity).map_err(|error| error.to_string())?)
                    .map_err(|error| format!("aggiornamento del keystore fallito: {error}"))?;
                identity
            }
        },
        Err(keyring::Error::NoEntry) => {
            let bytes = if legacy_path.exists() {
                std::fs::read(&legacy_path).map_err(|error| error.to_string())?
            } else {
                libp2p_identity::Keypair::generate_ed25519()
                    .to_protobuf_encoding()
                    .map_err(|error| error.to_string())?
            };
            libp2p_identity::Keypair::from_protobuf_encoding(&bytes)
                .map_err(|error| format!("identità locale non valida: {error}"))?;
            let identity = StoredIdentity {
                version: 1,
                classic: bytes,
                ml_dsa_seed: msnnext_core::generate_secret(),
            };
            let encoded = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
            entry
                .set_secret(&encoded)
                .map_err(|error| format!("salvataggio nel keystore fallito: {error}"))?;
            if entry.get_secret().map_err(|error| error.to_string())? != encoded {
                return Err("verifica del keystore fallita".into());
            }
            identity
        }
        Err(error) => return Err(format!("lettura del keystore fallita: {error}")),
    };
    libp2p_identity::Keypair::from_protobuf_encoding(&identity.classic)
        .map_err(|error| format!("identità nel keystore non valida: {error}"))?;
    if legacy_path.exists() {
        let legacy = std::fs::read(&legacy_path).map_err(|error| error.to_string())?;
        if legacy != identity.classic {
            return Err("l'identità nel keystore non coincide con identity.key".into());
        }
        std::fs::remove_file(legacy_path).map_err(|error| error.to_string())?;
    }
    Ok(identity)
}

fn worker_is_current(current_generation: Option<u64>, worker_generation: u64) -> bool {
    match current_generation {
        None => true,
        Some(current) => current == worker_generation,
    }
}

fn wait_for_worker(
    workers: &(Mutex<HashSet<u64>>, Condvar),
    generation: u64,
    timeout: Duration,
) -> Result<bool, String> {
    let running = workers
        .0
        .lock()
        .map_err(|_| "stato worker non disponibile")?;
    let (running, _) = workers
        .1
        .wait_timeout_while(running, timeout, |workers| workers.contains(&generation))
        .map_err(|_| "arresto del nodo non disponibile")?;
    Ok(!running.contains(&generation))
}

fn decode_qr_image(path: &Path) -> Result<String, String> {
    let image = image::open(path)
        .map_err(|error| format!("immagine non leggibile: {error}"))?
        .to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(image);
    for grid in prepared.detect_grids() {
        if let Ok((_, content)) = grid.decode() {
            if content.starts_with("msnnext://add/") {
                return Ok(content);
            }
        }
    }
    Err("nessun QR contatto msnnext trovato".into())
}

fn send_command(state: &NodeState, command: ClientCommand) -> Result<(), String> {
    let commands = state
        .commands
        .lock()
        .map_err(|_| "stato del nodo non disponibile")?
        .as_ref()
        .map(|(_, commands)| commands.clone())
        .ok_or_else(|| "avvia prima il nodo".to_owned())?;
    commands
        .send(command)
        .map_err(|_| "il nodo non è più attivo".to_owned())
}

#[tauri::command]
fn node_start(
    app: AppHandle,
    state: State<'_, NodeState>,
    config: NodeConfig,
) -> Result<(), String> {
    let mut command_slot = state
        .commands
        .lock()
        .map_err(|_| "stato del nodo non disponibile")?;
    if command_slot
        .as_ref()
        .is_some_and(|(_, commands)| !commands.is_closed())
    {
        return Err("il nodo è già in esecuzione".into());
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let identity = desktop_identity(&data_dir)?;
    let client_config = ClientConfig::desktop(config.name, data_dir, config.connect, config.relay)
        .and_then(|config| config.with_identity_bytes(identity.classic, identity.ml_dsa_seed))
        .map_err(|error| error.to_string())?;
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let generation = state.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
    state
        .workers
        .0
        .lock()
        .map_err(|_| "stato worker non disponibile")?
        .insert(generation);
    *command_slot = Some((generation, command_tx));
    drop(command_slot);

    let event_app = app.clone();
    let event_state = state.commands.clone();
    thread::spawn(move || {
        while let Some(event) = event_rx.blocking_recv() {
            let current_generation = event_state
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|(generation, _)| *generation));
            if worker_is_current(current_generation, generation) {
                let _ = event_app.emit("client-event", event);
            }
        }
    });

    let state_slot = state.commands.clone();
    let workers = state.workers.clone();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let current_generation = state_slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|(current, _)| *current));
                if worker_is_current(current_generation, generation) {
                    let _ = app.emit(
                        "client-event",
                        ClientEvent::Error {
                            message: error.to_string(),
                        },
                    );
                }
                if let Ok(mut commands) = state_slot.lock() {
                    if commands
                        .as_ref()
                        .is_some_and(|(current, _)| *current == generation)
                    {
                        *commands = None;
                    }
                }
                if let Ok(mut running) = workers.0.lock() {
                    running.remove(&generation);
                    workers.1.notify_all();
                }
                return;
            }
        };
        let result = runtime.block_on(msnnext_core::run(client_config, command_rx, event_tx));
        runtime.shutdown_background();
        if let Err(error) = result {
            let current_generation = state_slot
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|(current, _)| *current));
            if worker_is_current(current_generation, generation) {
                let _ = app.emit(
                    "client-event",
                    ClientEvent::Error {
                        message: error.to_string(),
                    },
                );
            }
        }
        if let Ok(mut commands) = state_slot.lock() {
            if commands
                .as_ref()
                .is_some_and(|(current, _)| *current == generation)
            {
                *commands = None;
            }
        }
        if let Ok(mut running) = workers.0.lock() {
            running.remove(&generation);
            workers.1.notify_all();
        }
    });
    Ok(())
}

#[tauri::command]
fn node_send_text(
    state: State<'_, NodeState>,
    peer_id: String,
    text: String,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("il messaggio è vuoto".into());
    }
    send_command(
        &state,
        ClientCommand::SendText {
            peer: parse_peer(&peer_id)?,
            text,
        },
    )
}

#[tauri::command]
fn node_send_nudge(state: State<'_, NodeState>, peer_id: String) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::SendNudge {
            peer: parse_peer(&peer_id)?,
        },
    )
}

#[tauri::command]
fn node_set_notification_mute(
    state: State<'_, NodeState>,
    conversation: String,
    muted: bool,
    until_ms: Option<u64>,
) -> Result<(), String> {
    let valid = conversation
        .strip_prefix("peer:")
        .is_some_and(|peer| parse_peer(peer).is_ok())
        || conversation.strip_prefix("group:").is_some_and(|group| {
            group.len() == 32 && group.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if !valid {
        return Err("conversazione non valida".into());
    }
    send_command(
        &state,
        ClientCommand::SetNotificationMute {
            conversation,
            muted,
            until_ms,
        },
    )
}

#[tauri::command]
fn node_send_file(
    state: State<'_, NodeState>,
    peer_id: String,
    path: String,
) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::SendFile {
            peer: parse_peer(&peer_id)?,
            path: PathBuf::from(path),
        },
    )
}

#[tauri::command]
fn node_cancel_file_transfers(state: State<'_, NodeState>) -> Result<(), String> {
    send_command(&state, ClientCommand::CancelFileTransfers)
}

#[tauri::command]
fn node_accept_attachment(state: State<'_, NodeState>, offer_id: u64) -> Result<(), String> {
    send_command(&state, ClientCommand::AcceptAttachment { offer_id })
}

#[tauri::command]
fn node_reject_attachment(state: State<'_, NodeState>, offer_id: u64) -> Result<(), String> {
    send_command(&state, ClientCommand::RejectAttachment { offer_id })
}

#[tauri::command]
fn node_create_emoticon(
    state: State<'_, NodeState>,
    path: String,
    trigger: String,
) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::CreateEmoticon {
            path: PathBuf::from(path),
            trigger,
        },
    )
}

#[tauri::command]
fn node_save_emoticon(
    state: State<'_, NodeState>,
    asset_id: String,
    trigger: String,
) -> Result<(), String> {
    send_command(&state, ClientCommand::SaveEmoticon { asset_id, trigger })
}

#[tauri::command]
fn node_update_emoticon(
    state: State<'_, NodeState>,
    asset_id: String,
    trigger: String,
) -> Result<(), String> {
    send_command(&state, ClientCommand::UpdateEmoticon { asset_id, trigger })
}

#[tauri::command]
fn node_delete_emoticon(state: State<'_, NodeState>, asset_id: String) -> Result<(), String> {
    send_command(&state, ClientCommand::DeleteEmoticon { asset_id })
}

#[tauri::command]
fn node_rename_contact(
    state: State<'_, NodeState>,
    peer_id: String,
    name: String,
) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::RenameContact {
            peer: parse_peer(&peer_id)?,
            name,
        },
    )
}

#[tauri::command]
fn node_delete_contact(state: State<'_, NodeState>, peer_id: String) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::DeleteContact {
            peer: parse_peer(&peer_id)?,
        },
    )
}

#[tauri::command]
fn node_clear_conversation(state: State<'_, NodeState>, peer_id: String) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::ClearConversation {
            peer: parse_peer(&peer_id)?,
        },
    )
}

#[tauri::command]
fn node_create_chat_group(
    state: State<'_, NodeState>,
    name: String,
    members: Vec<String>,
) -> Result<(), String> {
    let members = members
        .iter()
        .map(|member| parse_peer(member))
        .collect::<Result<Vec<_>, _>>()?;
    send_command(&state, ClientCommand::CreateChatGroup { name, members })
}

#[tauri::command]
fn node_moderate_group(
    state: State<'_, NodeState>,
    group_id: String,
    peer_id: String,
    action: String,
    duration_ms: Option<u64>,
) -> Result<(), String> {
    let action = match action.as_str() {
        "admin" => GroupModeration::SetAdmin(true),
        "member" => GroupModeration::SetAdmin(false),
        "silence" => GroupModeration::SetSilenced(true),
        "unsilence" => GroupModeration::SetSilenced(false),
        "tempBan" => GroupModeration::Ban(Some(
            duration_ms.ok_or("scegli la durata del ban temporaneo")?,
        )),
        "permaBan" => GroupModeration::Ban(None),
        "unban" => GroupModeration::Unban,
        _ => return Err("azione di moderazione non valida".into()),
    };
    send_command(
        &state,
        ClientCommand::ModerateGroup {
            group_id,
            peer: parse_peer(&peer_id)?,
            action,
        },
    )
}

#[tauri::command]
fn node_send_group_text(
    state: State<'_, NodeState>,
    group_id: String,
    text: String,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("il messaggio è vuoto".into());
    }
    send_command(&state, ClientCommand::SendGroupText { group_id, text })
}

#[tauri::command]
fn node_send_group_file(
    state: State<'_, NodeState>,
    group_id: String,
    path: String,
) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::SendGroupFile {
            group_id,
            path: PathBuf::from(path),
        },
    )
}

#[tauri::command]
fn node_clear_group_conversation(
    state: State<'_, NodeState>,
    group_id: String,
) -> Result<(), String> {
    send_command(&state, ClientCommand::ClearGroupConversation { group_id })
}

#[tauri::command]
fn node_delete_chat_group(state: State<'_, NodeState>, group_id: String) -> Result<(), String> {
    send_command(&state, ClientCommand::DeleteChatGroup { group_id })
}

#[tauri::command]
fn node_read_attachment(
    state: State<'_, NodeState>,
    id: String,
    mime: String,
) -> Result<(), String> {
    send_command(&state, ClientCommand::ReadAttachment { id, mime })
}

#[tauri::command]
fn node_export_attachment(
    state: State<'_, NodeState>,
    id: String,
    path: PathBuf,
) -> Result<(), String> {
    send_command(&state, ClientCommand::ExportAttachment { id, path })
}

#[tauri::command]
fn node_import_contact(state: State<'_, NodeState>, link: String) -> Result<(), String> {
    send_command(&state, ClientCommand::ImportContactLink { link })
}

#[tauri::command]
fn node_request_contact_link(state: State<'_, NodeState>) -> Result<(), String> {
    send_command(&state, ClientCommand::RequestContactLink)
}

#[tauri::command]
fn scan_contact_qr(path: String) -> Result<String, String> {
    decode_qr_image(Path::new(&path))
}

#[tauri::command]
fn save_contact_qr(path: PathBuf, data_url: String) -> Result<(), String> {
    const MAX_QR_BYTES: usize = 5 * 1024 * 1024;
    let encoded = data_url
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(|| "QR non valido".to_owned())?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| "QR non valido".to_owned())?;
    if bytes.len() > MAX_QR_BYTES
        || image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).is_err()
    {
        return Err("QR non valido".into());
    }
    std::fs::write(path, bytes).map_err(|error| format!("QR non salvato: {error}"))
}

#[tauri::command]
fn image_preview(path: String) -> Result<String, String> {
    const MAX_PREVIEW_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
    let path = Path::new(&path);
    if std::fs::metadata(path)
        .map_err(|error| format!("immagine non leggibile: {error}"))?
        .len()
        > MAX_PREVIEW_SOURCE_BYTES
    {
        return Err("immagine troppo grande per l’anteprima".into());
    }
    let image = image::open(path)
        .map_err(|error| format!("immagine non leggibile: {error}"))?
        .thumbnail(1280, 1280);
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|error| format!("anteprima non creata: {error}"))?;
    Ok(format!(
        "data:image/png;base64,{}",
        BASE64.encode(bytes.into_inner())
    ))
}

#[tauri::command]
fn profile_load(app: AppHandle) -> Result<Option<ProfileView>, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let path = data_dir.join("profile.json");
    if !path.exists() {
        return Ok(None);
    }
    let profile = serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    profile_view(&data_dir, profile).map(Some)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri espone questi campi come argomenti nominati alla WebView.
fn profile_save(
    app: AppHandle,
    state: State<'_, NodeState>,
    name: String,
    avatar_path: Option<String>,
    clear_avatar: bool,
    preview_sent_images: Option<bool>,
    preview_received_images: Option<bool>,
    nudge_sound: Option<bool>,
    relay_address: Option<String>,
    font_scale: Option<u16>,
) -> Result<ProfileView, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err("nome non valido".into());
    }
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let profile_path = data_dir.join("profile.json");
    let previous: Option<StoredProfile> = std::fs::read(&profile_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let sent_previews = preview_sent_images
        .or_else(|| previous.as_ref().map(|profile| profile.preview_sent_images))
        .unwrap_or(true);
    let received_previews = preview_received_images
        .or_else(|| {
            previous
                .as_ref()
                .map(|profile| profile.preview_received_images)
        })
        .unwrap_or(false);
    let nudge_sound = nudge_sound
        .or_else(|| previous.as_ref().map(|profile| profile.nudge_sound))
        .unwrap_or(true);
    let font_scale = font_scale
        .or_else(|| previous.as_ref().map(|profile| profile.font_scale))
        .unwrap_or_else(default_font_scale);
    if !valid_font_scale(font_scale) {
        return Err("dimensione testo non valida".into());
    }
    let relay_address = relay_address
        .or_else(|| {
            previous
                .as_ref()
                .map(|profile| profile.relay_address.clone())
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    if relay_address.len() > 512
        || ClientConfig::desktop(
            name.to_owned(),
            data_dir.clone(),
            None,
            (!relay_address.is_empty()).then_some(relay_address.clone()),
        )
        .is_err()
    {
        return Err("indirizzo relay non valido".into());
    }
    let avatar_file = if clear_avatar {
        if let Some(file) = previous
            .as_ref()
            .and_then(|profile| profile.avatar_file.as_ref())
        {
            std::fs::remove_file(data_dir.join(file)).ok();
        }
        None
    } else if let Some(path) = avatar_path {
        let image = image::open(path).map_err(|error| format!("avatar non leggibile: {error}"))?;
        let file = "profile-avatar.png".to_owned();
        image
            .thumbnail(256, 256)
            .save(data_dir.join(&file))
            .map_err(|error| format!("avatar non salvato: {error}"))?;
        Some(file)
    } else {
        previous.and_then(|profile| profile.avatar_file)
    };
    let profile = StoredProfile {
        name: name.to_owned(),
        avatar_file,
        preview_sent_images: sent_previews,
        preview_received_images: received_previews,
        nudge_sound,
        relay_address,
        font_scale,
    };
    std::fs::write(
        &profile_path,
        serde_json::to_vec(&profile).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if node_status(state.clone()).unwrap_or(false) {
        send_command(
            &state,
            ClientCommand::UpdateDisplayName {
                name: name.to_owned(),
            },
        )?;
    }
    profile_view(&data_dir, profile)
}

#[tauri::command]
fn node_status(state: State<'_, NodeState>) -> Result<bool, String> {
    let mut command_slot = state
        .commands
        .lock()
        .map_err(|_| "stato del nodo non disponibile")?;
    let running = command_slot
        .as_ref()
        .is_some_and(|(_, commands)| !commands.is_closed());
    if !running {
        *command_slot = None;
    }
    Ok(running)
}

#[tauri::command]
fn node_stop(state: State<'_, NodeState>) -> Result<(), String> {
    let commands = state
        .commands
        .lock()
        .map_err(|_| "stato del nodo non disponibile")?
        .take();
    if let Some((generation, commands)) = commands {
        let _ = commands.send(ClientCommand::Shutdown);
        if !wait_for_worker(&state.workers, generation, Duration::from_secs(5))? {
            return Err("il nodo non si è arrestato in tempo; riprova".into());
        }
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(NodeState::default())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            node_start,
            node_send_text,
            node_send_nudge,
            node_set_notification_mute,
            node_send_file,
            node_cancel_file_transfers,
            node_accept_attachment,
            node_reject_attachment,
            node_create_emoticon,
            node_save_emoticon,
            node_update_emoticon,
            node_delete_emoticon,
            node_rename_contact,
            node_delete_contact,
            node_clear_conversation,
            node_create_chat_group,
            node_moderate_group,
            node_send_group_text,
            node_send_group_file,
            node_clear_group_conversation,
            node_delete_chat_group,
            node_read_attachment,
            node_export_attachment,
            node_import_contact,
            node_request_contact_link,
            scan_contact_qr,
            save_contact_qr,
            image_preview,
            profile_load,
            profile_save,
            node_status,
            node_stop
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_commands_reject_invalid_peer_ids() {
        assert!(parse_peer("non-un-peer-id").is_err());
    }

    #[test]
    fn font_scale_accepts_only_ui_choices() {
        assert!(valid_font_scale(100));
        assert!(valid_font_scale(140));
        assert!(!valid_font_scale(99));
    }

    #[test]
    fn stale_worker_events_are_ignored_after_a_restart() {
        assert!(worker_is_current(None, 1));
        assert!(worker_is_current(Some(2), 2));
        assert!(!worker_is_current(Some(2), 1));
    }

    #[test]
    fn reconnect_waits_for_previous_worker_to_stop() {
        let workers = Arc::new((Mutex::new(HashSet::from([1])), Condvar::new()));
        let background = workers.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            background.0.lock().unwrap().remove(&1);
            background.1.notify_all();
        });
        assert!(wait_for_worker(&workers, 1, Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn qr_images_yield_contact_links() {
        let payload = "msnnext://add/test-contact";
        let path =
            std::env::temp_dir().join(format!("msnnext-contact-qr-{}.png", std::process::id()));
        let image = qrcode::QrCode::new(payload)
            .unwrap()
            .render::<image::Luma<u8>>()
            .min_dimensions(1024, 1024)
            .build();
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        save_contact_qr(
            path.clone(),
            format!("data:image/png;base64,{}", BASE64.encode(png.into_inner())),
        )
        .unwrap();

        assert_eq!(decode_qr_image(&path).unwrap(), payload);

        std::fs::remove_file(path).ok();
    }
}
