use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{error::Error, time::Duration};

use crate::history::SyncRecord;

pub const PAIRING_VALIDITY: Duration = Duration::from_secs(10 * 60);
pub const MAX_SYNC_RECORDS: usize = 500;
const DEVICE_LINK_PREFIX: &str = "msnnext://device/";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceDescriptor {
    pub peer_id: String,
    pub name: String,
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PairingLink {
    pub version: u8,
    pub inviter: DeviceDescriptor,
    pub secret: [u8; 32],
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SealedDeviceMessage {
    pub version: u8,
    pub key_id: [u8; 16],
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DeviceRequest {
    Pair {
        device: DeviceDescriptor,
        records: Vec<SyncRecord>,
    },
    Sync {
        device: DeviceDescriptor,
        after_seq: u64,
        records: Vec<SyncRecord>,
    },
    /// Chiede a un dispositivo collegato di consegnare un messaggio di testo a
    /// un contatto che il dispositivo mittente non riesce a raggiungere.
    RelayText {
        target: String,
        id: [u8; 16],
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DeviceResponse {
    Paired {
        account_key: [u8; 32],
        devices: Vec<DeviceDescriptor>,
        records: Vec<SyncRecord>,
        latest_seq: u64,
        accepted_through: u64,
    },
    Synced {
        records: Vec<SyncRecord>,
        latest_seq: u64,
        accepted_through: u64,
    },
    Rejected(String),
    /// Esito di una richiesta RelayText: consegnato/accodato dal dispositivo.
    RelayResult { id: [u8; 16], delivered: bool },
}

pub fn create_pairing_link(
    inviter: DeviceDescriptor,
    now_ms: u64,
) -> Result<(String, PairingLink), Box<dyn Error>> {
    let mut secret = [0u8; 32];
    OsRng.fill_bytes(&mut secret);
    let link = PairingLink {
        version: 1,
        inviter,
        secret,
        expires_at_ms: now_ms + PAIRING_VALIDITY.as_millis() as u64,
    };
    let encoded = cbor4ii::serde::to_vec(Vec::new(), &link)?;
    Ok((
        format!("{DEVICE_LINK_PREFIX}{}", URL_SAFE_NO_PAD.encode(encoded)),
        link,
    ))
}

pub fn parse_pairing_link(value: &str, now_ms: u64) -> Result<PairingLink, Box<dyn Error>> {
    let encoded = value
        .trim()
        .strip_prefix(DEVICE_LINK_PREFIX)
        .ok_or("link dispositivo non valido")?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
    let link: PairingLink = cbor4ii::serde::from_slice(&bytes)?;
    if link.version != 1 || link.expires_at_ms < now_ms {
        return Err("link dispositivo scaduto o non supportato".into());
    }
    if link.inviter.peer_id.parse::<libp2p::PeerId>().is_err()
        || link.inviter.name.trim().is_empty()
        || link.inviter.addresses.is_empty()
    {
        return Err("link dispositivo incompleto".into());
    }
    for address in &link.inviter.addresses {
        address.parse::<libp2p::Multiaddr>()?;
    }
    Ok(link)
}

pub fn key_id(key: &[u8; 32]) -> [u8; 16] {
    let hash = blake3::hash(key);
    let mut id = [0u8; 16];
    id.copy_from_slice(&hash.as_bytes()[..16]);
    id
}

pub fn seal<T: Serialize>(
    key: &[u8; 32],
    value: &T,
) -> Result<SealedDeviceMessage, Box<dyn Error>> {
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let plaintext = cbor4ii::serde::to_vec(Vec::new(), value)?;
    let ciphertext = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
        .map_err(|_| "cifratura sincronizzazione fallita")?;
    Ok(SealedDeviceMessage {
        version: 1,
        key_id: key_id(key),
        nonce,
        ciphertext,
    })
}

pub fn open<T: DeserializeOwned>(
    key: &[u8; 32],
    message: &SealedDeviceMessage,
) -> Result<T, Box<dyn Error>> {
    if message.version != 1 || message.key_id != key_id(key) {
        return Err("chiave sincronizzazione non riconosciuta".into());
    }
    let plaintext = XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(&message.nonce),
            message.ciphertext.as_ref(),
        )
        .map_err(|_| "sincronizzazione non autenticata")?;
    Ok(cbor4ii::serde::from_slice(&plaintext)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_links_expire_and_round_trip() {
        let descriptor = DeviceDescriptor {
            peer_id: libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string(),
            name: "Portatile".into(),
            addresses: vec!["/ip4/127.0.0.1/udp/4040/quic-v1".into()],
        };
        let (encoded, link) = create_pairing_link(descriptor, 100).unwrap();
        assert_eq!(
            parse_pairing_link(&encoded, 101).unwrap().secret,
            link.secret
        );
        assert!(parse_pairing_link(&encoded, link.expires_at_ms + 1).is_err());
    }

    #[test]
    fn sealed_messages_require_the_same_key() {
        let message = seal(&[7; 32], &"cronologia").unwrap();
        assert_eq!(open::<String>(&[7; 32], &message).unwrap(), "cronologia");
        assert!(open::<String>(&[8; 32], &message).is_err());
    }
}
