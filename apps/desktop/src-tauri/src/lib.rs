use std::{
    collections::HashSet,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use argon2::Argon2;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use msnnext_core::{ClientCommand, ClientConfig, ClientEvent, GroupModeration};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
#[cfg(desktop)]
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    WindowEvent,
};
use tokio::sync::mpsc;

type CommandSender = mpsc::UnboundedSender<ClientCommand>;
type RunningNode = (u64, CommandSender);

const MAX_ACCOUNT_HISTORY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ACCOUNT_BACKUP_BYTES: u64 = 256 * 1024 * 1024;
const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";

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
    #[serde(default = "enabled_by_default")]
    start_minimized: bool,
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
    start_minimized: bool,
}

#[derive(Deserialize, Serialize)]
struct StoredIdentity {
    version: u8,
    classic: Vec<u8>,
    ml_dsa_seed: [u8; 32],
    #[serde(default)]
    account_key: Option<[u8; 32]>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountBackup {
    version: u8,
    kdf: String,
    cipher: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountBackupPayload {
    identity: StoredIdentity,
    history: Option<String>,
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
        start_minimized: profile.start_minimized,
    })
}

fn parse_peer(value: &str) -> Result<libp2p_identity::PeerId, String> {
    msnnext_core::parse_peer_id(value)
}

// Secret storage is per-platform: OS keychain on Windows/Linux desktop, a
// machine-bound encrypted file on macOS, an app-private file on mobile.
//
// macOS uses a file instead of the Keychain because our app is ad-hoc signed
// (no Developer ID): the Keychain binds an item to the creating binary's code
// signature, so every rebuild/auto-update looks like a "different app" and
// macOS prompts for the login password on each launch. The file is encrypted
// at rest with a key derived from the machine's hardware UUID (never written to
// disk), so a copied file (backup, cloud sync, another Mac) is undecryptable.
// It is NOT protected against a process already running as this user — the
// derivation is public — which would require Keychain/Secure Enclave and bring
// back the prompt. This is a deliberate trade against that prompt.
#[cfg(all(desktop, not(target_os = "macos")))]
fn load_identity_secret(_data_dir: &Path) -> Result<Option<Vec<u8>>, String> {
    match identity_entry()?.get_secret() {
        Ok(bytes) => Ok(Some(bytes)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("failed to read keystore: {error}")),
    }
}

#[cfg(all(desktop, not(target_os = "macos")))]
fn save_identity_secret(_data_dir: &Path, bytes: &[u8]) -> Result<(), String> {
    let entry = identity_entry()?;
    entry
        .set_secret(bytes)
        .map_err(|error| format!("failed to save to keystore: {error}"))?;
    if entry.get_secret().map_err(|error| error.to_string())? != bytes {
        return Err("keystore verification failed".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_identity_path(data_dir: &Path) -> PathBuf {
    data_dir.join("identity.v1.enc")
}

// Key bound to this machine via the hardware UUID (ioreg). Never stored on disk.
#[cfg(target_os = "macos")]
fn macos_machine_key() -> Result<[u8; 32], String> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|error| format!("ioreg not runnable: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let uuid = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("\"IOPlatformUUID\" = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .ok_or("IOPlatformUUID not found")?;
    Ok(blake3::derive_key(
        "msnnext macos identity-at-rest key v1",
        uuid.as_bytes(),
    ))
}

#[cfg(target_os = "macos")]
fn macos_encrypt(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let key = macos_machine_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| "identity cipher init failed".to_owned())?;
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), bytes)
        .map_err(|_| "identity encryption failed".to_owned())?;
    let mut blob = Vec::with_capacity(25 + ciphertext.len());
    blob.push(1); // format version
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

#[cfg(target_os = "macos")]
fn macos_decrypt(blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.first() != Some(&1) || blob.len() < 25 {
        return Err("invalid encrypted identity file".into());
    }
    let key = macos_machine_key()?;
    XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| "identity cipher init failed".to_owned())?
        .decrypt(XNonce::from_slice(&blob[1..25]), &blob[25..])
        .map_err(|_| "identity not decryptable (different machine?)".to_owned())
}

#[cfg(target_os = "macos")]
fn load_identity_secret(data_dir: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = macos_identity_path(data_dir);
    if path.exists() {
        let blob = std::fs::read(&path).map_err(|error| error.to_string())?;
        return Ok(Some(macos_decrypt(&blob)?));
    }
    // One-time migration: identity previously stored in the Keychain (this is
    // the last launch that prompts for the login password) → encrypted file.
    match identity_entry()?.get_secret() {
        Ok(bytes) => {
            save_identity_secret(data_dir, &bytes)?;
            let _ = identity_entry().and_then(|entry| {
                entry.delete_credential().map_err(|error| error.to_string())
            });
            Ok(Some(bytes))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("failed to read keystore: {error}")),
    }
}

#[cfg(target_os = "macos")]
fn save_identity_secret(data_dir: &Path, bytes: &[u8]) -> Result<(), String> {
    let blob = macos_encrypt(bytes)?;
    let path = macos_identity_path(data_dir);
    std::fs::write(&path, &blob).map_err(|error| error.to_string())?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    if macos_decrypt(&std::fs::read(&path).map_err(|error| error.to_string())?)? != bytes {
        return Err("encrypted identity verification failed".into());
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod macos_identity_tests {
    use super::*;

    #[test]
    fn encrypted_identity_round_trips_and_rejects_tampering() {
        let secret = b"nuked-identity-bytes-\x00\x01\x02\xff".to_vec();
        let blob = macos_encrypt(&secret).expect("encrypt");
        assert_ne!(&blob[25..], secret.as_slice(), "ciphertext must differ from plaintext");
        assert_eq!(macos_decrypt(&blob).expect("decrypt"), secret);
        // Truncated and tampered blobs are rejected, never mis-decrypted.
        assert!(macos_decrypt(&blob[..20]).is_err());
        let mut tampered = blob.clone();
        *tampered.last_mut().unwrap() ^= 0xff;
        assert!(macos_decrypt(&tampered).is_err());
    }
}

#[cfg(not(desktop))]
fn load_identity_secret(data_dir: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = data_dir.join("identity.v1.json");
    if path.exists() {
        Ok(Some(std::fs::read(path).map_err(|error| error.to_string())?))
    } else {
        Ok(None)
    }
}

#[cfg(not(desktop))]
fn save_identity_secret(data_dir: &Path, bytes: &[u8]) -> Result<(), String> {
    // ponytail: mobile secret sits unencrypted in app-private storage; wrap it
    // with an Android Keystore-held key in Phase 1.
    std::fs::write(data_dir.join("identity.v1.json"), bytes).map_err(|error| error.to_string())
}

fn desktop_identity(data_dir: &Path) -> Result<StoredIdentity, String> {
    let legacy_path = data_dir.join("identity.key");
    let identity = match load_identity_secret(data_dir)? {
        Some(bytes) => match serde_json::from_slice::<StoredIdentity>(&bytes) {
            Ok(identity) if matches!(identity.version, 1 | 2) => {
                let migrated = normalize_identity(identity);
                let encoded = serde_json::to_vec(&migrated).map_err(|error| error.to_string())?;
                if encoded != bytes {
                    save_identity_secret(data_dir, &encoded)?;
                }
                migrated
            }
            _ => {
                libp2p_identity::Keypair::from_protobuf_encoding(&bytes)
                    .map_err(|error| format!("invalid identity in keystore: {error}"))?;
                let identity = StoredIdentity {
                    version: 2,
                    classic: bytes,
                    ml_dsa_seed: msnnext_core::generate_secret(),
                    account_key: Some(msnnext_core::generate_secret()),
                };
                save_identity_secret(
                    data_dir,
                    &serde_json::to_vec(&identity).map_err(|error| error.to_string())?,
                )?;
                identity
            }
        },
        None => {
            let bytes = if legacy_path.exists() {
                std::fs::read(&legacy_path).map_err(|error| error.to_string())?
            } else {
                libp2p_identity::Keypair::generate_ed25519()
                    .to_protobuf_encoding()
                    .map_err(|error| error.to_string())?
            };
            libp2p_identity::Keypair::from_protobuf_encoding(&bytes)
                .map_err(|error| format!("invalid local identity: {error}"))?;
            let identity = StoredIdentity {
                version: 2,
                classic: bytes,
                ml_dsa_seed: msnnext_core::generate_secret(),
                account_key: Some(msnnext_core::generate_secret()),
            };
            let encoded = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
            save_identity_secret(data_dir, &encoded)?;
            identity
        }
    };
    libp2p_identity::Keypair::from_protobuf_encoding(&identity.classic)
        .map_err(|error| format!("invalid identity in keystore: {error}"))?;
    if legacy_path.exists() {
        let legacy = std::fs::read(&legacy_path).map_err(|error| error.to_string())?;
        if legacy != identity.classic {
            return Err("the identity in the keystore does not match identity.key".into());
        }
        std::fs::remove_file(legacy_path).map_err(|error| error.to_string())?;
    }
    Ok(identity)
}

fn normalize_identity(mut identity: StoredIdentity) -> StoredIdentity {
    if identity.account_key.is_none() {
        let mut legacy = identity.classic.clone();
        legacy.extend_from_slice(&identity.ml_dsa_seed);
        identity.account_key = Some(blake3::derive_key("msnnext account root v1", &legacy));
    }
    identity.version = 2;
    identity
}

fn store_account_key(data_dir: &Path, account_key: [u8; 32]) -> Result<(), String> {
    let encoded = load_identity_secret(data_dir)?
        .ok_or_else(|| "identity not initialized".to_owned())?;
    let mut identity: StoredIdentity = serde_json::from_slice(&encoded)
        .map(normalize_identity)
        .map_err(|_| "invalid identity in keystore".to_owned())?;
    identity.account_key = Some(account_key);
    identity.version = 2;
    let encoded = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
    save_identity_secret(data_dir, &encoded)
}

#[cfg(desktop)]
fn identity_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new("app.msnnext.desktop", "identity-v1")
        .map_err(|error| format!("keystore unavailable: {error}"))
}

fn derive_backup_key(password: &str, salt: &[u8; 16]) -> Result<[u8; 32], String> {
    if password.chars().count() < 12 {
        return Err("use a password with at least 12 characters".into());
    }
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|_| "password could not be processed".to_owned())?;
    Ok(key)
}

fn encrypt_account_backup(
    payload: &AccountBackupPayload,
    password: &str,
) -> Result<String, String> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = derive_backup_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plaintext = serde_json::to_vec(payload).map_err(|error| error.to_string())?;
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| "backup could not be created".to_owned())?;
    serde_json::to_string_pretty(&AccountBackup {
        version: 3,
        kdf: "argon2id-v1".into(),
        cipher: "xchacha20poly1305".into(),
        salt: BASE64.encode(salt),
        nonce: BASE64.encode(nonce),
        ciphertext: BASE64.encode(ciphertext),
    })
    .map_err(|error| error.to_string())
}

fn decrypt_account_backup(contents: &str, password: &str) -> Result<AccountBackupPayload, String> {
    let backup: AccountBackup =
        serde_json::from_str(contents).map_err(|_| "invalid account backup".to_owned())?;
    if !matches!(backup.version, 1..=3)
        || backup.kdf != "argon2id-v1"
        || backup.cipher != "xchacha20poly1305"
    {
        return Err("unsupported account backup format".into());
    }
    let salt: [u8; 16] = BASE64
        .decode(backup.salt)
        .map_err(|_| "invalid account backup".to_owned())?
        .try_into()
        .map_err(|_| "invalid account backup".to_owned())?;
    let nonce: [u8; 24] = BASE64
        .decode(backup.nonce)
        .map_err(|_| "invalid account backup".to_owned())?
        .try_into()
        .map_err(|_| "invalid account backup".to_owned())?;
    let ciphertext = BASE64
        .decode(backup.ciphertext)
        .map_err(|_| "invalid account backup".to_owned())?;
    let key = derive_backup_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plaintext = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_slice())
        .map_err(|_| "incorrect password or corrupted backup".to_owned())?;
    let mut payload = if backup.version == 1 {
        AccountBackupPayload {
            identity: serde_json::from_slice(&plaintext)
                .map_err(|_| "invalid identity in backup".to_owned())?,
            history: None,
        }
    } else {
        serde_json::from_slice(&plaintext)
            .map_err(|_| "invalid backup content".to_owned())?
    };
    payload.identity = normalize_identity(payload.identity);
    if payload.identity.version != 2
        || libp2p_identity::Keypair::from_protobuf_encoding(&payload.identity.classic).is_err()
    {
        return Err("invalid identity in backup".into());
    }
    if let Some(history) = payload.history.as_deref() {
        decode_backup_history(history)?;
    }
    Ok(payload)
}

fn read_backup_history(data_dir: &Path) -> Result<Option<String>, String> {
    let path = data_dir.join("history.db");
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(&path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_ACCOUNT_HISTORY_BYTES {
        return Err("history is too large for the account backup".into());
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if !bytes.starts_with(SQLITE_HEADER) {
        return Err("invalid history database".into());
    }
    Ok(Some(BASE64.encode(bytes)))
}

fn decode_backup_history(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| "invalid history in backup".to_owned())?;
    if bytes.len() as u64 > MAX_ACCOUNT_HISTORY_BYTES || !bytes.starts_with(SQLITE_HEADER) {
        return Err("invalid history in backup".into());
    }
    Ok(bytes)
}

fn archive_identity_data(
    data_dir: &Path,
    include_downloads: bool,
) -> Result<Option<PathBuf>, String> {
    let history_names = [
        "history.db",
        "history.db-journal",
        "history.db-shm",
        "history.db-wal",
    ];
    let all_names = [
        "history.db",
        "history.db-journal",
        "history.db-shm",
        "history.db-wal",
        "downloads",
    ];
    let names = if include_downloads {
        all_names.as_slice()
    } else {
        history_names.as_slice()
    };
    if !names.iter().any(|name| data_dir.join(name).exists()) {
        return Ok(None);
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    let archive = data_dir.join(format!(
        "account-data-before-restore-{timestamp}-{}",
        std::process::id()
    ));
    std::fs::create_dir(&archive).map_err(|error| error.to_string())?;
    let mut moved = Vec::new();
    for &name in names {
        let source = data_dir.join(name);
        if !source.exists() {
            continue;
        }
        let destination = archive.join(name);
        if let Err(error) = std::fs::rename(&source, &destination) {
            for (original, archived) in moved.iter().rev() {
                let _ = std::fs::rename(archived, original);
            }
            let _ = std::fs::remove_dir(&archive);
            return Err(format!("archiviazione dei dati locali fallita: {error}"));
        }
        moved.push((source, destination));
    }
    Ok(Some(archive))
}

fn write_backup_history(data_dir: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = data_dir.join(format!(".history-restore-{}.db", std::process::id()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("cronologia non ripristinata: {error}"))?;
    if let Err(error) = std::fs::rename(&temporary, data_dir.join("history.db")) {
        let _ = std::fs::remove_file(temporary);
        return Err(format!("cronologia non ripristinata: {error}"));
    }
    Ok(())
}

fn restore_archived_identity_data(archive: &Path, data_dir: &Path) {
    for name in [
        "history.db",
        "history.db-journal",
        "history.db-shm",
        "history.db-wal",
        "downloads",
    ] {
        let archived = archive.join(name);
        if archived.exists() {
            let destination = data_dir.join(name);
            if destination.is_dir() {
                let _ = std::fs::remove_dir_all(&destination);
            } else {
                let _ = std::fs::remove_file(&destination);
            }
            let _ = std::fs::rename(archived, destination);
        }
    }
    let _ = std::fs::remove_dir(archive);
}

fn worker_is_current(current_generation: Option<u64>, worker_generation: u64) -> bool {
    match current_generation {
        None => true,
        Some(current) => current == worker_generation,
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        #[cfg(desktop)]
        let _ = window.unminimize();
        let _ = window.set_focus();
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
        .map_err(|_| "worker state unavailable")?;
    let (running, _) = workers
        .1
        .wait_timeout_while(running, timeout, |workers| workers.contains(&generation))
        .map_err(|_| "node shutdown unavailable")?;
    Ok(!running.contains(&generation))
}

fn decode_qr_image(path: &Path) -> Result<String, String> {
    let image = image::open(path)
        .map_err(|error| format!("image could not be read: {error}"))?
        .to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(image);
    for grid in prepared.detect_grids() {
        if let Ok((_, content)) = grid.decode() {
            if content.starts_with("msnnext://add/") || content.starts_with("msnnext://device/") {
                return Ok(content);
            }
        }
    }
    Err("no msnnext QR code found".into())
}

fn send_command(state: &NodeState, command: ClientCommand) -> Result<(), String> {
    let commands = state
        .commands
        .lock()
        .map_err(|_| "node state unavailable")?
        .as_ref()
        .map(|(_, commands)| commands.clone())
        .ok_or_else(|| "start the node first".to_owned())?;
    commands
        .send(command)
        .map_err(|_| "the node is no longer running".to_owned())
}

#[tauri::command]
async fn node_start(
    app: AppHandle,
    state: State<'_, NodeState>,
    config: NodeConfig,
) -> Result<(), String> {
    let mut command_slot = state
        .commands
        .lock()
        .map_err(|_| "node state unavailable")?;
    if command_slot
        .as_ref()
        .is_some_and(|(_, commands)| !commands.is_closed())
    {
        return Err("the node is already running".into());
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let identity = desktop_identity(&data_dir)?;
    let account_key = identity
        .account_key
        .ok_or_else(|| "account key unavailable".to_owned())?;
    let account_key_dir = data_dir.clone();
    let client_config = ClientConfig::desktop(config.name, data_dir, config.connect, config.relay)
        .and_then(|config| config.with_identity_bytes(identity.classic, identity.ml_dsa_seed))
        .map(|config| {
            config.with_account_key(account_key, move |key| store_account_key(&account_key_dir, key))
        })
        .map_err(|error| error.to_string())?;
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let generation = state.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
    state
        .workers
        .0
        .lock()
        .map_err(|_| "worker state unavailable")?
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
        return Err("the message is empty".into());
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
        return Err("invalid conversation".into());
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
fn node_accept_contact_request(state: State<'_, NodeState>, peer_id: String) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::AcceptContactRequest {
            peer: parse_peer(&peer_id)?,
        },
    )
}

#[tauri::command]
fn node_reject_contact_request(state: State<'_, NodeState>, peer_id: String) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::RejectContactRequest {
            peer: parse_peer(&peer_id)?,
        },
    )
}

#[tauri::command]
fn node_delete_message_for_me(state: State<'_, NodeState>, event_id: String) -> Result<(), String> {
    send_command(&state, ClientCommand::DeleteMessageForMe { event_id })
}

#[tauri::command]
fn node_delete_message_for_everyone(
    state: State<'_, NodeState>,
    peer_id: String,
    event_id: String,
) -> Result<(), String> {
    send_command(
        &state,
        ClientCommand::DeleteMessageForEveryone {
            peer: parse_peer(&peer_id)?,
            event_id,
        },
    )
}

#[tauri::command]
fn node_set_auto_accept_extensions(
    state: State<'_, NodeState>,
    extensions: Vec<String>,
) -> Result<(), String> {
    send_command(&state, ClientCommand::SetAutoAcceptExtensions { extensions })
}

#[tauri::command]
fn node_set_presence_status(state: State<'_, NodeState>, status: String) -> Result<(), String> {
    send_command(&state, ClientCommand::SetPresenceStatus { status })
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
            duration_ms.ok_or("choose a temporary ban duration")?,
        )),
        "permaBan" => GroupModeration::Ban(None),
        "unban" => GroupModeration::Unban,
        _ => return Err("invalid moderation action".into()),
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
        return Err("the message is empty".into());
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
fn node_request_device_link(state: State<'_, NodeState>) -> Result<(), String> {
    send_command(&state, ClientCommand::RequestDeviceLink)
}

#[tauri::command]
fn node_import_device_link(state: State<'_, NodeState>, link: String) -> Result<(), String> {
    send_command(&state, ClientCommand::ImportDeviceLink { link })
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
        .ok_or_else(|| "invalid QR code".to_owned())?;
    let bytes = BASE64
        .decode(encoded)
        .map_err(|_| "invalid QR code".to_owned())?;
    if bytes.len() > MAX_QR_BYTES
        || image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).is_err()
    {
        return Err("invalid QR code".into());
    }
    std::fs::write(path, bytes).map_err(|error| format!("QR code could not be saved: {error}"))
}

#[tauri::command]
fn image_preview(path: String) -> Result<String, String> {
    const MAX_PREVIEW_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
    let path = Path::new(&path);
    if std::fs::metadata(path)
        .map_err(|error| format!("image could not be read: {error}"))?
        .len()
        > MAX_PREVIEW_SOURCE_BYTES
    {
        return Err("image is too large for preview".into());
    }
    let image = image::open(path)
        .map_err(|error| format!("image could not be read: {error}"))?
        .thumbnail(1280, 1280);
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|error| format!("preview could not be created: {error}"))?;
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
    start_minimized: Option<bool>,
) -> Result<ProfileView, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err("invalid name".into());
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
        return Err("invalid text size".into());
    }
    let start_minimized = start_minimized
        .or_else(|| previous.as_ref().map(|profile| profile.start_minimized))
        .unwrap_or(true);
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
        return Err("invalid relay address".into());
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
        let image = image::open(path).map_err(|error| format!("avatar could not be read: {error}"))?;
        let file = "profile-avatar.png".to_owned();
        image
            .thumbnail(256, 256)
            .save(data_dir.join(&file))
            .map_err(|error| format!("avatar could not be saved: {error}"))?;
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
        start_minimized,
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
fn account_backup_export(
    app: AppHandle,
    state: State<'_, NodeState>,
    password: String,
    path: PathBuf,
) -> Result<(), String> {
    if node_status(state)? {
        return Err("go offline before creating an account backup".into());
    }
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let payload = AccountBackupPayload {
        identity: desktop_identity(&data_dir)?,
        history: read_backup_history(&data_dir)?,
    };
    let backup = encrypt_account_backup(&payload, &password)?;
    std::fs::write(path, backup).map_err(|error| format!("backup could not be saved: {error}"))
}

#[tauri::command]
fn account_backup_import(
    app: AppHandle,
    state: State<'_, NodeState>,
    password: String,
    path: PathBuf,
) -> Result<(), String> {
    if node_status(state)? {
        return Err("go offline before restoring an account".into());
    }
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("account backup could not be read: {error}"))?;
    if metadata.len() > MAX_ACCOUNT_BACKUP_BYTES {
        return Err("account backup is too large".into());
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("account backup could not be read: {error}"))?;
    let imported = decrypt_account_backup(&contents, &password)?;
    let imported_history = imported
        .history
        .as_deref()
        .map(decode_backup_history)
        .transpose()?;
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let current = desktop_identity(&data_dir)?;
    let restored_identity = StoredIdentity {
        version: 2,
        classic: current.classic.clone(),
        ml_dsa_seed: current.ml_dsa_seed,
        account_key: imported.identity.account_key,
    };
    let identity_changed = current.account_key != restored_identity.account_key;
    if !identity_changed && imported_history.is_none() {
        return Ok(());
    }

    let current_encoded = identity_changed
        .then(|| serde_json::to_vec(&current).map_err(|error| error.to_string()))
        .transpose()?;
    let imported_encoded = identity_changed
        .then(|| serde_json::to_vec(&restored_identity).map_err(|error| error.to_string()))
        .transpose()?;
    let archived = archive_identity_data(&data_dir, identity_changed)?;

    let update_result = if let Some(imported_encoded) = imported_encoded.as_ref() {
        // save_identity_secret scrive e verifica sul backend della piattaforma.
        save_identity_secret(&data_dir, imported_encoded)
    } else {
        Ok(())
    }
    .and_then(|_| {
        imported_history
            .as_deref()
            .map(|history| {
                write_backup_history(&data_dir, history)?;
                msnnext_core::rekey_history_database(
                    &data_dir.join("history.db"),
                    &imported.identity.classic,
                    &current.classic,
                )
                .map_err(|error| format!("history could not be re-encrypted: {error}"))
            })
            .transpose()
            .map(|_| ())
    });

    if let Err(error) = update_result {
        let rollback_error = current_encoded
            .as_ref()
            .and_then(|current| save_identity_secret(&data_dir, current).err());
        if let Some(archive) = archived.as_deref() {
            restore_archived_identity_data(archive, &data_dir);
        }
        return match rollback_error {
            Some(rollback) => Err(format!(
                "{error}; restoring the previous identity also failed: {rollback}"
            )),
            None => Err(error),
        };
    }
    Ok(())
}

#[tauri::command]
fn node_status(state: State<'_, NodeState>) -> Result<bool, String> {
    let mut command_slot = state
        .commands
        .lock()
        .map_err(|_| "node state unavailable")?;
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
        .map_err(|_| "node state unavailable")?
        .take();
    if let Some((generation, commands)) = commands {
        let _ = commands.send(ClientCommand::Shutdown);
        if !wait_for_worker(&state.workers, generation, Duration::from_secs(5))? {
            return Err("the node did not stop in time; try again".into());
        }
    }
    Ok(())
}

/// True se l'app è stata lanciata dall'avvio automatico del sistema (arg passato
/// dal plugin autostart), non da un avvio manuale dell'utente.
#[cfg(desktop)]
fn launched_at_startup() -> bool {
    std::env::args().any(|arg| arg == "--autostarted")
}

/// Preferenza "avvia minimizzato" dal profilo salvato (default: attiva).
#[cfg(desktop)]
fn start_minimized_pref(app: &AppHandle) -> bool {
    let Ok(data_dir) = app.path().app_data_dir() else {
        return true;
    };
    std::fs::read(data_dir.join("profile.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<StoredProfile>(&bytes).ok())
        .map(|profile| profile.start_minimized)
        .unwrap_or(true)
}

#[cfg(desktop)]
#[tauri::command]
fn autostart_get(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|error| error.to_string())
}

#[cfg(desktop)]
#[tauri::command]
fn autostart_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled { manager.enable() } else { manager.disable() }.map_err(|error| error.to_string())
}

#[cfg(not(desktop))]
#[tauri::command]
fn autostart_get(_app: AppHandle) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(desktop))]
#[tauri::command]
fn autostart_set(_app: AppHandle, _enabled: bool) -> Result<(), String> {
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .manage(NodeState::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // Log su file anche in release (necessario su Windows: l'app GUI non
            // scrive su terminale). File in <log-dir>/msnnext.log.
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .targets([
                        tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                            file_name: Some("msnnext".into()),
                        }),
                        tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    ])
                    .build(),
            )?;
            // Scanner QR con fotocamera nativa: solo mobile.
            #[cfg(mobile)]
            app.handle().plugin(tauri_plugin_barcode_scanner::init())?;
            // Tray e chiusura-in-tray sono solo desktop; su mobile non esistono.
            #[cfg(desktop)]
            {
                use tauri_plugin_autostart::ManagerExt;
                // Primo avvio: abilita l'avvio automatico col sistema (default on).
                // Un marker evita di riabilitarlo se l'utente lo disattiva.
                if let Ok(data_dir) = app.path().app_data_dir() {
                    let marker = data_dir.join("autostart.init");
                    if !marker.exists() {
                        let _ = app.autolaunch().enable();
                        let _ = std::fs::create_dir_all(&data_dir);
                        let _ = std::fs::write(&marker, b"1");
                    }
                }
                let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
                let open =
                    MenuItem::with_id(app, "tray-open", "Open msnnext", true, None::<&str>)?;
                let autostart_item = CheckMenuItem::with_id(
                    app,
                    "tray-autostart",
                    "Avvia con il sistema",
                    true,
                    autostart_on,
                    None::<&str>,
                )?;
                let quit = MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?;
                let menu = Menu::with_items(app, &[&open, &autostart_item, &quit])?;
                let mut tray = TrayIconBuilder::with_id("main")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .tooltip("msnnext")
                    .on_menu_event(move |app, event| match event.id().as_ref() {
                        "tray-open" => show_main_window(app),
                        "tray-autostart" => {
                            let manager = app.autolaunch();
                            let next = !manager.is_enabled().unwrap_or(false);
                            let _ = if next { manager.enable() } else { manager.disable() };
                            let _ = autostart_item.set_checked(next);
                        }
                        "tray-quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if matches!(
                            event,
                            TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            }
                        ) {
                            show_main_window(tray.app_handle());
                        }
                    });
                if let Some(icon) = app.default_window_icon() {
                    tray = tray.icon(icon.clone());
                }
                tray.build(app)?;
            }
            Ok(())
        });

    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostarted"]),
        ))
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        });

    builder
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
            node_accept_contact_request,
            node_reject_contact_request,
            node_delete_message_for_me,
            node_delete_message_for_everyone,
            node_set_auto_accept_extensions,
            node_set_presence_status,
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
            node_request_device_link,
            node_import_device_link,
            scan_contact_qr,
            save_contact_qr,
            image_preview,
            profile_load,
            profile_save,
            account_backup_export,
            account_backup_import,
            autostart_get,
            autostart_set,
            node_status,
            node_stop
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if matches!(event, RunEvent::Ready) {
                // All'avvio automatico col sistema, se l'utente ha scelto "avvia
                // minimizzato" (default), non mostriamo la finestra: resta nel tray.
                #[cfg(desktop)]
                let minimized = launched_at_startup() && start_minimized_pref(app);
                #[cfg(not(desktop))]
                let minimized = false;
                if !minimized {
                    show_main_window(app);
                }
            }
        });
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
    fn encrypted_account_backup_round_trips_and_rejects_wrong_password() {
        let identity = StoredIdentity {
            version: 1,
            classic: libp2p_identity::Keypair::generate_ed25519()
                .to_protobuf_encoding()
                .unwrap(),
            ml_dsa_seed: [7; 32],
            account_key: None,
        };
        let history = [SQLITE_HEADER, b"test-history"].concat();
        let payload = AccountBackupPayload {
            identity,
            history: Some(BASE64.encode(&history)),
        };
        let backup = encrypt_account_backup(&payload, "password lunga 123").unwrap();
        let restored = decrypt_account_backup(&backup, "password lunga 123").unwrap();

        assert_eq!(restored.identity.classic, payload.identity.classic);
        assert_eq!(restored.identity.ml_dsa_seed, payload.identity.ml_dsa_seed);
        assert_eq!(
            decode_backup_history(restored.history.as_deref().unwrap()).unwrap(),
            history
        );
        assert!(decrypt_account_backup(&backup, "password errata 123").is_err());
        assert!(encrypt_account_backup(&payload, "corta").is_err());
    }

    #[test]
    fn account_history_is_archived_and_restored() {
        let data_dir = std::env::temp_dir().join(format!(
            "msnnext-account-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&data_dir).unwrap();
        let history = [SQLITE_HEADER, b"test-history"].concat();
        std::fs::write(data_dir.join("history.db"), &history).unwrap();

        let encoded = read_backup_history(&data_dir).unwrap().unwrap();
        let archive = archive_identity_data(&data_dir, false).unwrap().unwrap();
        assert!(!data_dir.join("history.db").exists());
        write_backup_history(&data_dir, &decode_backup_history(&encoded).unwrap()).unwrap();

        assert_eq!(std::fs::read(data_dir.join("history.db")).unwrap(), history);
        std::fs::remove_dir_all(archive).ok();
        std::fs::remove_dir_all(data_dir).ok();
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
