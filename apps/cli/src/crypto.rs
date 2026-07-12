use aws_lc_rs::{
    agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519},
    kem::{Ciphertext, DecapsulationKey, EncapsulationKey, ML_KEM_768},
    rand::SystemRandom,
};
use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng, Payload},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};
use zeroize::{ZeroizeOnDrop, Zeroizing};

const HYBRID_HANDSHAKE_VERSION: u16 = 1;
const RATCHET_VERSION: u16 = 1;
const MAX_SKIPPED_MESSAGE_KEYS: u64 = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HybridClientHello {
    pub(crate) version: u16,
    pub(crate) ml_kem_public_key: Vec<u8>,
    pub(crate) x25519_public_key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HybridServerHello {
    pub(crate) version: u16,
    pub(crate) ml_kem_ciphertext: Vec<u8>,
    pub(crate) x25519_public_key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum HybridResponse {
    Accepted(HybridServerHello),
    Rejected(String),
}

pub(crate) struct HybridInitiator {
    ml_kem_private_key: DecapsulationKey,
    x25519_private_key: EphemeralPrivateKey,
    client_hello: HybridClientHello,
    initiator_peer: PeerId,
    responder_peer: PeerId,
}

#[derive(ZeroizeOnDrop)]
pub(crate) struct SessionKey([u8; 32]);

impl SessionKey {
    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RatchetMessage {
    pub(crate) version: u16,
    pub(crate) number: u64,
    pub(crate) nonce: [u8; 24],
    pub(crate) ciphertext: Vec<u8>,
}

#[derive(ZeroizeOnDrop)]
pub(crate) struct RatchetSession {
    send_chain: [u8; 32],
    receive_chain: [u8; 32],
    send_number: u64,
    receive_number: u64,
    skipped_message_keys: Vec<(u64, [u8; 32])>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RatchetError {
    UnsupportedVersion,
    OutOfOrder,
    Authentication,
}

impl fmt::Display for RatchetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion => formatter.write_str("versione ratchet non supportata"),
            Self::OutOfOrder => formatter.write_str("numero messaggio ratchet non valido"),
            Self::Authentication => formatter.write_str("autenticazione messaggio fallita"),
        }
    }
}

impl Error for RatchetError {}

impl RatchetSession {
    pub(crate) fn new(root: SessionKey, local_peer: PeerId, remote_peer: PeerId) -> Self {
        let (initiator, responder) = if should_initiate(local_peer, remote_peer) {
            (local_peer, remote_peer)
        } else {
            (remote_peer, local_peer)
        };
        let mut material = Vec::with_capacity(32 + 2 * 64);
        material.extend_from_slice(&root.0);
        material.extend_from_slice(&initiator.to_bytes());
        material.extend_from_slice(&responder.to_bytes());
        let initiator_send = blake3::derive_key("msnnext ratchet initiator send v1", &material);
        let responder_send = blake3::derive_key("msnnext ratchet responder send v1", &material);
        let (send_chain, receive_chain) = if local_peer == initiator {
            (initiator_send, responder_send)
        } else {
            (responder_send, initiator_send)
        };
        Self {
            send_chain,
            receive_chain,
            send_number: 0,
            receive_number: 0,
            skipped_message_keys: Vec::new(),
        }
    }

    pub(crate) fn encrypt(
        &mut self,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> Result<RatchetMessage, RatchetError> {
        let number = self
            .send_number
            .checked_add(1)
            .ok_or(RatchetError::OutOfOrder)?;
        let message_key = Zeroizing::new(ratchet_message_key(&self.send_chain, number));
        let cipher = XChaCha20Poly1305::new((&*message_key).into());
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &ratchet_associated_data(number, associated_data),
                },
            )
            .map_err(|_| RatchetError::Authentication)?;
        self.send_chain = advance_chain(&self.send_chain);
        self.send_number = number;
        Ok(RatchetMessage {
            version: RATCHET_VERSION,
            number,
            nonce,
            ciphertext,
        })
    }

    pub(crate) fn decrypt(
        &mut self,
        message: &RatchetMessage,
        associated_data: &[u8],
    ) -> Result<Vec<u8>, RatchetError> {
        if message.version != RATCHET_VERSION {
            return Err(RatchetError::UnsupportedVersion);
        }
        if message.number <= self.receive_number {
            let position = self
                .skipped_message_keys
                .iter()
                .position(|(number, _)| *number == message.number)
                .ok_or(RatchetError::OutOfOrder)?;
            let message_key = Zeroizing::new(self.skipped_message_keys[position].1);
            let plaintext = decrypt_ratchet_message(message, associated_data, &message_key)?;
            self.skipped_message_keys.swap_remove(position);
            return Ok(plaintext);
        }

        let gap = message.number - self.receive_number;
        let skipped_count = gap.saturating_sub(1);
        if self.skipped_message_keys.len() as u64 + skipped_count > MAX_SKIPPED_MESSAGE_KEYS {
            return Err(RatchetError::OutOfOrder);
        }

        let mut next_chain = self.receive_chain;
        let mut skipped = Vec::with_capacity(skipped_count as usize);
        for number in self.receive_number + 1..message.number {
            skipped.push((number, ratchet_message_key(&next_chain, number)));
            next_chain = advance_chain(&next_chain);
        }
        let message_key = Zeroizing::new(ratchet_message_key(&next_chain, message.number));
        let plaintext = decrypt_ratchet_message(message, associated_data, &message_key)?;
        next_chain = advance_chain(&next_chain);

        self.receive_chain = next_chain;
        self.receive_number = message.number;
        self.skipped_message_keys.extend(skipped);
        Ok(plaintext)
    }
}

fn decrypt_ratchet_message(
    message: &RatchetMessage,
    associated_data: &[u8],
    message_key: &[u8; 32],
) -> Result<Vec<u8>, RatchetError> {
    let cipher = XChaCha20Poly1305::new(message_key.into());
    cipher
        .decrypt(
            XNonce::from_slice(&message.nonce),
            Payload {
                msg: &message.ciphertext,
                aad: &ratchet_associated_data(message.number, associated_data),
            },
        )
        .map_err(|_| RatchetError::Authentication)
}

fn ratchet_message_key(chain: &[u8; 32], number: u64) -> [u8; 32] {
    let mut material = Vec::with_capacity(40);
    material.extend_from_slice(chain);
    material.extend_from_slice(&number.to_be_bytes());
    blake3::derive_key("msnnext ratchet message key v1", &material)
}

fn advance_chain(chain: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("msnnext ratchet chain step v1", chain)
}

fn ratchet_associated_data(number: u64, associated_data: &[u8]) -> Vec<u8> {
    let mut bound = Vec::with_capacity(10 + associated_data.len());
    bound.extend_from_slice(&RATCHET_VERSION.to_be_bytes());
    bound.extend_from_slice(&number.to_be_bytes());
    bound.extend_from_slice(associated_data);
    bound
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HybridError {
    UnsupportedVersion,
    CryptoOperation,
}

impl fmt::Display for HybridError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion => formatter.write_str("versione handshake non supportata"),
            Self::CryptoOperation => formatter.write_str("operazione crittografica fallita"),
        }
    }
}

impl Error for HybridError {}

pub(crate) fn should_initiate(local: PeerId, remote: PeerId) -> bool {
    local < remote
}

pub(crate) fn accepts_inbound(local: PeerId, remote: PeerId) -> bool {
    should_initiate(remote, local)
}

pub(crate) fn needs_outbound_handshake(
    local: PeerId,
    remote: PeerId,
    established: bool,
    pending: bool,
) -> bool {
    should_initiate(local, remote) && !established && !pending
}

impl HybridInitiator {
    pub(crate) fn start(
        initiator_peer: PeerId,
        responder_peer: PeerId,
    ) -> Result<(Self, HybridClientHello), HybridError> {
        let ml_kem_private_key =
            DecapsulationKey::generate(&ML_KEM_768).map_err(|_| HybridError::CryptoOperation)?;
        let ml_kem_public_key = ml_kem_private_key
            .encapsulation_key()
            .and_then(|key| key.key_bytes())
            .map_err(|_| HybridError::CryptoOperation)?
            .as_ref()
            .to_vec();

        let random = SystemRandom::new();
        let x25519_private_key = EphemeralPrivateKey::generate(&X25519, &random)
            .map_err(|_| HybridError::CryptoOperation)?;
        let x25519_public_key = x25519_private_key
            .compute_public_key()
            .map_err(|_| HybridError::CryptoOperation)?
            .as_ref()
            .try_into()
            .map_err(|_| HybridError::CryptoOperation)?;

        let client_hello = HybridClientHello {
            version: HYBRID_HANDSHAKE_VERSION,
            ml_kem_public_key,
            x25519_public_key,
        };
        Ok((
            Self {
                ml_kem_private_key,
                x25519_private_key,
                client_hello: client_hello.clone(),
                initiator_peer,
                responder_peer,
            },
            client_hello,
        ))
    }

    pub(crate) fn finish(
        self,
        server_hello: &HybridServerHello,
    ) -> Result<SessionKey, HybridError> {
        ensure_version(server_hello.version)?;
        let ml_kem_secret = self
            .ml_kem_private_key
            .decapsulate(Ciphertext::from(server_hello.ml_kem_ciphertext.as_slice()))
            .map_err(|_| HybridError::CryptoOperation)?;
        let x25519_secret = agree_x25519(self.x25519_private_key, &server_hello.x25519_public_key)?;
        Ok(derive_session_key(
            &self.client_hello,
            server_hello,
            ml_kem_secret.as_ref(),
            &x25519_secret,
            self.initiator_peer,
            self.responder_peer,
        ))
    }
}

pub(crate) fn respond(
    client_hello: &HybridClientHello,
    initiator_peer: PeerId,
    responder_peer: PeerId,
) -> Result<(HybridServerHello, SessionKey), HybridError> {
    ensure_version(client_hello.version)?;
    let ml_kem_public_key = EncapsulationKey::new(&ML_KEM_768, &client_hello.ml_kem_public_key)
        .map_err(|_| HybridError::CryptoOperation)?;
    let (ml_kem_ciphertext, ml_kem_secret) = ml_kem_public_key
        .encapsulate()
        .map_err(|_| HybridError::CryptoOperation)?;

    let random = SystemRandom::new();
    let x25519_private_key = EphemeralPrivateKey::generate(&X25519, &random)
        .map_err(|_| HybridError::CryptoOperation)?;
    let x25519_public_key = x25519_private_key
        .compute_public_key()
        .map_err(|_| HybridError::CryptoOperation)?
        .as_ref()
        .try_into()
        .map_err(|_| HybridError::CryptoOperation)?;
    let x25519_secret = agree_x25519(x25519_private_key, &client_hello.x25519_public_key)?;

    let server_hello = HybridServerHello {
        version: HYBRID_HANDSHAKE_VERSION,
        ml_kem_ciphertext: ml_kem_ciphertext.as_ref().to_vec(),
        x25519_public_key,
    };
    let session_key = derive_session_key(
        client_hello,
        &server_hello,
        ml_kem_secret.as_ref(),
        &x25519_secret,
        initiator_peer,
        responder_peer,
    );
    Ok((server_hello, session_key))
}

fn ensure_version(version: u16) -> Result<(), HybridError> {
    if version == HYBRID_HANDSHAKE_VERSION {
        Ok(())
    } else {
        Err(HybridError::UnsupportedVersion)
    }
}

fn agree_x25519(
    private_key: EphemeralPrivateKey,
    peer_public_key: &[u8; 32],
) -> Result<Vec<u8>, HybridError> {
    agreement::agree_ephemeral(
        private_key,
        UnparsedPublicKey::new(&X25519, peer_public_key),
        HybridError::CryptoOperation,
        |secret| Ok(secret.to_vec()),
    )
}

fn derive_session_key(
    client_hello: &HybridClientHello,
    server_hello: &HybridServerHello,
    ml_kem_secret: &[u8],
    x25519_secret: &[u8],
    initiator_peer: PeerId,
    responder_peer: PeerId,
) -> SessionKey {
    let initiator_peer = initiator_peer.to_bytes();
    let responder_peer = responder_peer.to_bytes();
    let mut material = Vec::with_capacity(
        ml_kem_secret.len()
            + x25519_secret.len()
            + client_hello.ml_kem_public_key.len()
            + server_hello.ml_kem_ciphertext.len()
            + initiator_peer.len()
            + responder_peer.len()
            + 68,
    );
    material.extend_from_slice(ml_kem_secret);
    material.extend_from_slice(x25519_secret);
    material.extend_from_slice(&initiator_peer);
    material.extend_from_slice(&responder_peer);
    material.extend_from_slice(&client_hello.version.to_be_bytes());
    material.extend_from_slice(&client_hello.ml_kem_public_key);
    material.extend_from_slice(&client_hello.x25519_public_key);
    material.extend_from_slice(&server_hello.version.to_be_bytes());
    material.extend_from_slice(&server_hello.ml_kem_ciphertext);
    material.extend_from_slice(&server_hello.x25519_public_key);
    SessionKey(blake3::derive_key(
        "msnnext X25519MLKEM768 session key v1",
        &material,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::{identity::Keypair, PeerId};

    fn peers() -> (PeerId, PeerId) {
        (
            PeerId::from(Keypair::generate_ed25519().public()),
            PeerId::from(Keypair::generate_ed25519().public()),
        )
    }

    #[test]
    fn hybrid_handshake_derives_the_same_session_key() {
        let (alice, bob) = peers();
        let (initiator, client_hello) = HybridInitiator::start(alice, bob).unwrap();
        let (server_hello, responder_key) = respond(&client_hello, alice, bob).unwrap();
        let initiator_key = initiator.finish(&server_hello).unwrap();

        assert_eq!(initiator_key.as_bytes(), responder_key.as_bytes());
        assert_ne!(initiator_key.as_bytes(), &[0; 32]);
    }

    #[test]
    fn hybrid_handshake_rejects_unknown_version() {
        let (alice, bob) = peers();
        let (_, mut client_hello) = HybridInitiator::start(alice, bob).unwrap();
        client_hello.version += 1;

        match respond(&client_hello, alice, bob) {
            Err(error) => assert_eq!(error, HybridError::UnsupportedVersion),
            Ok(_) => panic!("una versione sconosciuta non deve essere accettata"),
        }
    }

    #[test]
    fn hybrid_handshake_messages_round_trip_on_the_wire() {
        let (alice, bob) = peers();
        let (_, client_hello) = HybridInitiator::start(alice, bob).unwrap();
        let encoded = cbor4ii::serde::to_vec(Vec::new(), &client_hello).unwrap();
        let decoded: HybridClientHello = cbor4ii::serde::from_slice(&encoded).unwrap();

        assert_eq!(decoded, client_hello);
    }

    #[test]
    fn hybrid_handshake_binds_both_peer_ids() {
        let (alice, bob) = peers();
        let mallory = PeerId::from(Keypair::generate_ed25519().public());
        let (initiator, client_hello) = HybridInitiator::start(alice, bob).unwrap();
        let (server_hello, wrong_peer_key) = respond(&client_hello, mallory, bob).unwrap();
        let initiator_key = initiator.finish(&server_hello).unwrap();

        assert_ne!(initiator_key.as_bytes(), wrong_peer_key.as_bytes());
    }

    #[test]
    fn hybrid_handshake_rejects_all_zero_x25519_key() {
        let (alice, bob) = peers();
        let (_, mut client_hello) = HybridInitiator::start(alice, bob).unwrap();
        client_hello.x25519_public_key = [0; 32];

        assert!(respond(&client_hello, alice, bob).is_err());
    }

    #[test]
    fn hybrid_handshake_rejects_malformed_ml_kem_key() {
        let (alice, bob) = peers();
        let (_, mut client_hello) = HybridInitiator::start(alice, bob).unwrap();
        client_hello.ml_kem_public_key.truncate(32);

        assert!(respond(&client_hello, alice, bob).is_err());
    }

    #[test]
    fn exactly_one_peer_initiates_the_handshake() {
        let first = PeerId::from(Keypair::generate_ed25519().public());
        let second = PeerId::from(Keypair::generate_ed25519().public());

        assert_ne!(
            should_initiate(first, second),
            should_initiate(second, first)
        );
    }

    #[test]
    fn responder_only_accepts_the_deterministic_initiator() {
        let (first, second) = peers();
        let (initiator, responder) = if should_initiate(first, second) {
            (first, second)
        } else {
            (second, first)
        };

        assert!(accepts_inbound(responder, initiator));
        assert!(!accepts_inbound(initiator, responder));
    }

    #[test]
    fn valid_peer_event_starts_a_missing_handshake() {
        let (first, second) = peers();
        let (initiator, responder) = if should_initiate(first, second) {
            (first, second)
        } else {
            (second, first)
        };

        assert!(needs_outbound_handshake(initiator, responder, false, false));
        assert!(!needs_outbound_handshake(initiator, responder, false, true));
        assert!(!needs_outbound_handshake(initiator, responder, true, false));
    }

    #[test]
    fn symmetric_ratchet_encrypts_in_both_directions() {
        let (alice, bob) = peers();
        let mut alice_session = RatchetSession::new(SessionKey([7; 32]), alice, bob);
        let mut bob_session = RatchetSession::new(SessionKey([7; 32]), bob, alice);

        let to_bob = alice_session.encrypt(b"ciao Bob", b"conversation").unwrap();
        assert_eq!(
            bob_session.decrypt(&to_bob, b"conversation").unwrap(),
            b"ciao Bob"
        );

        let to_alice = bob_session.encrypt(b"ciao Alice", b"conversation").unwrap();
        assert_eq!(
            alice_session.decrypt(&to_alice, b"conversation").unwrap(),
            b"ciao Alice"
        );
    }

    #[test]
    fn symmetric_ratchet_rejects_replayed_messages() {
        let (alice, bob) = peers();
        let mut alice_session = RatchetSession::new(SessionKey([9; 32]), alice, bob);
        let mut bob_session = RatchetSession::new(SessionKey([9; 32]), bob, alice);
        let encrypted = alice_session
            .encrypt(b"una volta", b"conversation")
            .unwrap();

        assert!(bob_session.decrypt(&encrypted, b"conversation").is_ok());
        assert_eq!(
            bob_session.decrypt(&encrypted, b"conversation"),
            Err(RatchetError::OutOfOrder)
        );
    }

    #[test]
    fn symmetric_ratchet_authenticates_context_without_advancing_on_failure() {
        let (alice, bob) = peers();
        let mut alice_session = RatchetSession::new(SessionKey([11; 32]), alice, bob);
        let mut bob_session = RatchetSession::new(SessionKey([11; 32]), bob, alice);
        let encrypted = alice_session
            .encrypt(b"segreto", b"conversation-a")
            .unwrap();

        assert_eq!(
            bob_session.decrypt(&encrypted, b"conversation-b"),
            Err(RatchetError::Authentication)
        );
        assert_eq!(
            bob_session.decrypt(&encrypted, b"conversation-a").unwrap(),
            b"segreto"
        );
    }

    #[test]
    fn symmetric_ratchet_accepts_bounded_out_of_order_delivery() {
        let (alice, bob) = peers();
        let mut alice_session = RatchetSession::new(SessionKey([13; 32]), alice, bob);
        let mut bob_session = RatchetSession::new(SessionKey([13; 32]), bob, alice);
        let first = alice_session.encrypt(b"primo", b"conversation").unwrap();
        let second = alice_session.encrypt(b"secondo", b"conversation").unwrap();

        assert_eq!(
            bob_session.decrypt(&second, b"conversation").unwrap(),
            b"secondo"
        );
        assert_eq!(
            bob_session.decrypt(&first, b"conversation").unwrap(),
            b"primo"
        );
        assert_eq!(
            bob_session.decrypt(&first, b"conversation"),
            Err(RatchetError::OutOfOrder)
        );
    }

    #[test]
    fn symmetric_ratchet_limits_total_skipped_keys() {
        let (alice, bob) = peers();
        let mut alice_session = RatchetSession::new(SessionKey([15; 32]), alice, bob);
        let mut bob_session = RatchetSession::new(SessionKey([15; 32]), bob, alice);
        let messages = (0..130)
            .map(|_| alice_session.encrypt(b"x", b"conversation").unwrap())
            .collect::<Vec<_>>();

        for index in (1..128).step_by(2) {
            assert!(bob_session
                .decrypt(&messages[index], b"conversation")
                .is_ok());
        }
        assert_eq!(
            bob_session.decrypt(&messages[129], b"conversation"),
            Err(RatchetError::OutOfOrder)
        );
    }
}
