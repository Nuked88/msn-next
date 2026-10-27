//! Inviti a codice corto: "7319-gatto-vela-sole".
//!
//! Chi invita pubblica nella DHT un record provider sotto una chiave derivata
//! dal solo numero iniziale (nameplate). Chi riceve il codice trova così il
//! Peer ID di chi invita, poi i due fanno SPAKE2 sulle parole: senza il codice
//! esatto la chiave non coincide e le schede contatto (firmate Ed25519 e
//! ML-DSA) non si aprono. Chi intercetta o indovina ha un solo tentativo per
//! sessione, e l'invito si brucia dopo pochi tentativi, dopo l'uso o dopo dieci
//! minuti.
//!
//! Le parole sono la lista BIP-39 italiana (MIT, bitcoin/bips): 2048 parole
//! senza accenti e con le prime quattro lettere tutte diverse, quindi bastano
//! quelle quattro per riconoscerle.

use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::{collections::HashMap, fmt, sync::OnceLock, time::Duration};
use zeroize::Zeroizing;

pub const INVITE_VALIDITY: Duration = Duration::from_secs(10 * 60);
/// Tentativi SPAKE2 accettati per invito: con 33 bit di parole, chi prova a
/// indovinare ha 3 possibilità su 8,6 miliardi prima che il codice si bruci.
pub const MAX_INVITE_ATTEMPTS: u8 = 3;
/// Provider da provare per un nameplate: due inviti con lo stesso numero sono
/// rari, ma possibili.
pub const MAX_INVITE_CANDIDATES: usize = 3;

const WORD_COUNT: usize = 3;
const NAMEPLATES: u32 = 10_000;
const WORDS: &str = include_str!("invite_words_it.txt");

fn words() -> &'static [&'static str] {
    static LIST: OnceLock<Vec<&'static str>> = OnceLock::new();
    LIST.get_or_init(|| {
        WORDS
            .lines()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .collect()
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InviteCode {
    nameplate: u16,
    words: [u16; WORD_COUNT],
}

impl InviteCode {
    pub fn generate() -> Self {
        let nameplate = (OsRng.next_u32() % NAMEPLATES) as u16;
        let mut words = [0u16; WORD_COUNT];
        for word in &mut words {
            // 2048 divide 2^32: nessuno sbilanciamento.
            *word = (OsRng.next_u32() % 2048) as u16;
        }
        Self { nameplate, words }
    }

    /// Accetta maiuscole, spazi o trattini e le sole prime quattro lettere di
    /// ogni parola.
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        const INVALID: &str = "codice d'invito non valido";
        let mut parts = input
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty());
        let nameplate = parts
            .next()
            .filter(|part| part.len() <= 4 && part.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|part| part.parse::<u16>().ok())
            .ok_or(INVALID)?;
        let mut words = [0u16; WORD_COUNT];
        for word in &mut words {
            *word = word_index(parts.next().ok_or(INVALID)?).ok_or(INVALID)?;
        }
        if parts.next().is_some() {
            return Err(INVALID);
        }
        Ok(Self { nameplate, words })
    }

    /// Chiave DHT dell'invito: dipende solo dal nameplate, così la DHT non
    /// permette di verificare le parole.
    pub fn rendezvous_key(&self) -> Vec<u8> {
        rendezvous_key(self.nameplate)
    }

    fn password(&self) -> Zeroizing<Vec<u8>> {
        let mut password = Zeroizing::new(b"msnnext invite v1".to_vec());
        password.extend_from_slice(&self.nameplate.to_be_bytes());
        for word in self.words {
            password.extend_from_slice(&word.to_be_bytes());
        }
        password
    }
}

impl fmt::Display for InviteCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}", self.nameplate)?;
        for word in self.words {
            write!(f, "-{}", words()[word as usize])?;
        }
        Ok(())
    }
}

fn word_index(input: &str) -> Option<u16> {
    let input = input.to_ascii_lowercase();
    if input.len() < 4 {
        return None;
    }
    let list = words();
    if let Some(index) = list.iter().position(|word| *word == input) {
        return Some(index as u16);
    }
    // Il prefisso di quattro lettere identifica una sola parola.
    let mut matches = list
        .iter()
        .enumerate()
        .filter(|(_, word)| word.starts_with(&input));
    match (matches.next(), matches.next()) {
        (Some((index, _)), None) if input.len() == 4 => Some(index as u16),
        _ => None,
    }
}

pub fn rendezvous_key(nameplate: u16) -> Vec<u8> {
    blake3::derive_key("msnnext invite rendezvous v1", &nameplate.to_be_bytes()).to_vec()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SealedCard {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InviteRequest {
    /// Primo messaggio SPAKE2 di chi ha ricevuto il codice.
    Start { nameplate: u16, message: Vec<u8> },
    /// Scheda di chi ha ricevuto il codice, cifrata con la chiave SPAKE2.
    Finish { nameplate: u16, card: SealedCard },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum InviteResponse {
    /// Messaggio SPAKE2 e scheda di chi invita: se si apre, il codice era
    /// giusto e la scheda viene davvero da chi l'ha mostrato.
    Started {
        message: Vec<u8>,
        card: SealedCard,
    },
    Finished,
    Rejected(String),
}

pub const INVITE_EXPIRED: &str = "codice d'invito scaduto o già usato";
pub const INVITE_WRONG_CODE: &str = "codice d'invito errato";
pub const INVITE_BURNED: &str = "troppi tentativi sbagliati: codice d'invito annullato";

struct SessionKeys {
    inviter_card: Zeroizing<[u8; 32]>,
    joiner_card: Zeroizing<[u8; 32]>,
}

fn session_keys(
    code: &InviteCode,
    spake: Spake2<Ed25519Group>,
    message: &[u8],
) -> Result<SessionKeys, &'static str> {
    let shared = Zeroizing::new(spake.finish(message).map_err(|_| INVITE_WRONG_CODE)?);
    let mut context = Vec::with_capacity(shared.len() + 2);
    context.extend_from_slice(&shared);
    context.extend_from_slice(&code.nameplate.to_be_bytes());
    let context = Zeroizing::new(context);
    Ok(SessionKeys {
        inviter_card: Zeroizing::new(blake3::derive_key(
            "msnnext invite inviter card v1",
            &context,
        )),
        joiner_card: Zeroizing::new(blake3::derive_key(
            "msnnext invite joiner card v1",
            &context,
        )),
    })
}

fn spake_identities(inviter: PeerId, joiner: PeerId) -> (Identity, Identity) {
    (
        Identity::new(&inviter.to_bytes()),
        Identity::new(&joiner.to_bytes()),
    )
}

fn seal(key: &[u8; 32], card: &[u8]) -> Result<SealedCard, &'static str> {
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), card)
        .map_err(|_| "cifratura invito fallita")?;
    Ok(SealedCard { nonce, ciphertext })
}

fn open(key: &[u8; 32], card: &SealedCard) -> Result<Vec<u8>, &'static str> {
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(&card.nonce), card.ciphertext.as_ref())
        .map_err(|_| INVITE_WRONG_CODE)
}

/// Invito mostrato da questo dispositivo.
pub struct ActiveInvite {
    pub code: InviteCode,
    pub expires_at_ms: u64,
    attempts: u8,
    sessions: HashMap<PeerId, SessionKeys>,
}

impl ActiveInvite {
    pub fn new(now_ms: u64) -> Self {
        Self {
            code: InviteCode::generate(),
            expires_at_ms: now_ms + INVITE_VALIDITY.as_millis() as u64,
            attempts: 0,
            sessions: HashMap::new(),
        }
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }

    /// Bruciato: scaduto o con tutti i tentativi consumati.
    pub fn exhausted(&self, now_ms: u64) -> bool {
        self.expired(now_ms) || self.attempts >= MAX_INVITE_ATTEMPTS
    }

    /// Risponde al primo messaggio SPAKE2 di `joiner` con il proprio e con la
    /// scheda `card` cifrata. Ogni chiamata consuma un tentativo, anche se il
    /// codice è sbagliato: chi invita non può saperlo finché l'altro non
    /// manda `Finish`.
    pub fn start(
        &mut self,
        inviter: PeerId,
        joiner: PeerId,
        nameplate: u16,
        message: &[u8],
        card: &[u8],
        now_ms: u64,
    ) -> Result<(Vec<u8>, SealedCard), &'static str> {
        if self.exhausted(now_ms) || nameplate != self.code.nameplate {
            return Err(INVITE_EXPIRED);
        }
        self.attempts += 1;
        let (id_inviter, id_joiner) = spake_identities(inviter, joiner);
        let password = self.code.password();
        let (spake, reply) =
            Spake2::<Ed25519Group>::start_a(&Password::new(&password), &id_inviter, &id_joiner);
        let keys = session_keys(&self.code, spake, message)?;
        let sealed = seal(&keys.inviter_card, card)?;
        self.sessions.insert(joiner, keys);
        Ok((reply, sealed))
    }

    /// Apre la scheda di `joiner`: riesce solo se ha usato il codice giusto.
    pub fn finish(
        &mut self,
        joiner: PeerId,
        nameplate: u16,
        card: &SealedCard,
        now_ms: u64,
    ) -> Result<Vec<u8>, &'static str> {
        if self.expired(now_ms) || nameplate != self.code.nameplate {
            return Err(INVITE_EXPIRED);
        }
        let keys = self.sessions.remove(&joiner).ok_or(INVITE_EXPIRED)?;
        open(&keys.joiner_card, card)
    }
}

/// Lato di chi ha ricevuto il codice, per un singolo candidato.
pub struct JoinSession {
    code: InviteCode,
    spake: Spake2<Ed25519Group>,
}

impl JoinSession {
    pub fn start(code: InviteCode, inviter: PeerId, joiner: PeerId) -> (Self, InviteRequest) {
        let (id_inviter, id_joiner) = spake_identities(inviter, joiner);
        let password = code.password();
        let (spake, message) =
            Spake2::<Ed25519Group>::start_b(&Password::new(&password), &id_inviter, &id_joiner);
        (
            Self { code, spake },
            InviteRequest::Start {
                nameplate: code.nameplate,
                message,
            },
        )
    }

    /// Apre la scheda di chi invita e cifra la propria. Restituisce la scheda
    /// ricevuta e la richiesta `Finish` da mandare.
    pub fn complete(
        self,
        message: &[u8],
        inviter_card: &SealedCard,
        own_card: &[u8],
    ) -> Result<(Vec<u8>, InviteRequest), &'static str> {
        let keys = session_keys(&self.code, self.spake, message)?;
        let card = open(&keys.inviter_card, inviter_card)?;
        let sealed = seal(&keys.joiner_card, own_card)?;
        Ok((
            card,
            InviteRequest::Finish {
                nameplate: self.code.nameplate,
                card: sealed,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    fn peer() -> PeerId {
        PeerId::from(Keypair::generate_ed25519().public())
    }

    #[test]
    fn wordlist_has_unique_four_letter_prefixes() {
        let list = words();
        assert_eq!(list.len(), 2048);
        let prefixes = list
            .iter()
            .map(|word| &word[..4])
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(prefixes.len(), 2048);
        assert!(list
            .iter()
            .all(|word| word.bytes().all(|b| b.is_ascii_lowercase())));
    }

    #[test]
    fn codes_round_trip_and_accept_sloppy_typing() {
        let code = InviteCode::generate();
        let text = code.to_string();
        assert_eq!(InviteCode::parse(&text).unwrap(), code);
        assert_eq!(InviteCode::parse(&text.to_uppercase()).unwrap(), code);
        assert_eq!(
            InviteCode::parse(&format!("  {}  ", text.replace('-', " "))).unwrap(),
            code
        );
        let short = text
            .split('-')
            .enumerate()
            .map(|(i, part)| if i == 0 { part } else { &part[..4] })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(InviteCode::parse(&short).unwrap(), code);
    }

    #[test]
    fn malformed_codes_are_rejected() {
        for input in [
            "",
            "1234",
            "12345-abaco-abaco-abaco",
            "1234-abaco-abaco",
            "1234-abaco-abaco-abaco-abaco",
            "1234-abc-abaco-abaco",
            "1234-nonesiste-abaco-abaco",
            "abaco-1234-abaco-abaco",
        ] {
            assert!(InviteCode::parse(input).is_err(), "{input}");
        }
        assert!(InviteCode::parse("0007-abaco-abaco-zuppa").is_ok());
    }

    #[test]
    fn rendezvous_key_ignores_the_secret_words() {
        let first = InviteCode::parse("0042-abaco-abaco-abaco").unwrap();
        let second = InviteCode::parse("0042-zuppa-zuppa-zuppa").unwrap();
        let other = InviteCode::parse("0043-abaco-abaco-abaco").unwrap();
        assert_eq!(first.rendezvous_key(), second.rendezvous_key());
        assert_ne!(first.rendezvous_key(), other.rendezvous_key());
    }

    #[test]
    fn right_code_exchanges_both_cards() {
        let (inviter, joiner) = (peer(), peer());
        let mut invite = ActiveInvite::new(0);
        let (session, request) = JoinSession::start(invite.code, inviter, joiner);
        let InviteRequest::Start { nameplate, message } = request else {
            unreachable!()
        };
        let (reply, sealed) = invite
            .start(inviter, joiner, nameplate, &message, b"alice", 1)
            .unwrap();
        let (card, finish) = session.complete(&reply, &sealed, b"bob").unwrap();
        assert_eq!(card, b"alice");
        let InviteRequest::Finish { nameplate, card } = finish else {
            unreachable!()
        };
        assert_eq!(invite.finish(joiner, nameplate, &card, 2).unwrap(), b"bob");
        // La sessione si consuma: una seconda Finish non passa.
        assert!(invite.finish(joiner, nameplate, &card, 3).is_err());
    }

    #[test]
    fn wrong_words_reveal_nothing_and_burn_the_invite() {
        let (inviter, joiner) = (peer(), peer());
        let mut invite = ActiveInvite::new(0);
        let mut guess = invite.code;
        guess.words[0] ^= 1;
        for attempt in 0..MAX_INVITE_ATTEMPTS {
            let (session, request) = JoinSession::start(guess, inviter, joiner);
            let InviteRequest::Start { nameplate, message } = request else {
                unreachable!()
            };
            let (reply, sealed) = invite
                .start(inviter, joiner, nameplate, &message, b"alice", 1)
                .unwrap_or_else(|error| panic!("tentativo {attempt}: {error}"));
            assert_eq!(
                session.complete(&reply, &sealed, b"mallory").err(),
                Some(INVITE_WRONG_CODE)
            );
        }
        // Tentativi finiti: neppure il codice giusto viene più accettato.
        let (_, request) = JoinSession::start(invite.code, inviter, joiner);
        let InviteRequest::Start { nameplate, message } = request else {
            unreachable!()
        };
        assert!(invite.exhausted(1));
        assert_eq!(
            invite
                .start(inviter, joiner, nameplate, &message, b"alice", 1)
                .err(),
            Some(INVITE_EXPIRED)
        );
    }

    #[test]
    fn keys_are_bound_to_both_peers() {
        // Un relay che inoltra i messaggi a un terzo peer non ottiene la
        // stessa chiave: le identità SPAKE2 sono i Peer ID autenticati.
        let (inviter, joiner, mallory) = (peer(), peer(), peer());
        let mut invite = ActiveInvite::new(0);
        let (session, request) = JoinSession::start(invite.code, inviter, joiner);
        let InviteRequest::Start { nameplate, message } = request else {
            unreachable!()
        };
        let (reply, sealed) = invite
            .start(inviter, mallory, nameplate, &message, b"alice", 1)
            .unwrap();
        assert!(session.complete(&reply, &sealed, b"bob").is_err());
    }

    #[test]
    fn expired_invites_are_refused() {
        let (inviter, joiner) = (peer(), peer());
        let mut invite = ActiveInvite::new(0);
        let (_, request) = JoinSession::start(invite.code, inviter, joiner);
        let InviteRequest::Start { nameplate, message } = request else {
            unreachable!()
        };
        let late = INVITE_VALIDITY.as_millis() as u64;
        assert_eq!(
            invite
                .start(inviter, joiner, nameplate, &message, b"alice", late)
                .err(),
            Some(INVITE_EXPIRED)
        );
    }
}
