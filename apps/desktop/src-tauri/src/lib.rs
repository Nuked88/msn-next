use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use msnnext_core::{ClientCommand, ClientConfig, ClientEvent};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

type CommandSender = mpsc::UnboundedSender<ClientCommand>;
type RunningNode = (u64, CommandSender);

#[derive(Clone, Default)]
struct NodeState {
    commands: Arc<Mutex<Option<RunningNode>>>,
    next_generation: Arc<AtomicU64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeConfig {
    name: String,
    connect: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredProfile {
    name: String,
    avatar_file: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileView {
    name: String,
    avatar_data_url: Option<String>,
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
    })
}

fn parse_peer(value: &str) -> Result<libp2p_identity::PeerId, String> {
    msnnext_core::parse_peer_id(value)
}

fn worker_is_current(current_generation: Option<u64>, worker_generation: u64) -> bool {
    match current_generation {
        None => true,
        Some(current) => current == worker_generation,
    }
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
    let client_config = ClientConfig::desktop(config.name, data_dir, config.connect)
        .map_err(|error| error.to_string())?;
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let generation = state.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
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
fn profile_save(
    app: AppHandle,
    state: State<'_, NodeState>,
    name: String,
    avatar_path: Option<String>,
    clear_avatar: bool,
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
    if let Some((_, commands)) = commands {
        let _ = commands.send(ClientCommand::Shutdown);
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
            node_send_file,
            node_create_emoticon,
            node_save_emoticon,
            node_update_emoticon,
            node_delete_emoticon,
            node_rename_contact,
            node_delete_contact,
            node_clear_conversation,
            node_import_contact,
            node_request_contact_link,
            scan_contact_qr,
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
    fn stale_worker_events_are_ignored_after_a_restart() {
        assert!(worker_is_current(None, 1));
        assert!(worker_is_current(Some(2), 2));
        assert!(!worker_is_current(Some(2), 1));
    }

    #[test]
    fn qr_images_yield_contact_links() {
        let payload = "msnnext://add/test-contact";
        let path = std::env::temp_dir().join("msnnext-contact-qr.png");
        let image = qrcode::QrCode::new(payload)
            .unwrap()
            .render::<image::Luma<u8>>()
            .min_dimensions(240, 240)
            .build();
        image.save(&path).unwrap();

        assert_eq!(decode_qr_image(&path).unwrap(), payload);

        std::fs::remove_file(path).ok();
    }
}
