use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use libp2p::{identity::PublicKey, multiaddr::Protocol, Multiaddr, PeerId};
use qrcode::{render::unicode, QrCode};
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};

use crate::crypto::MlDsaIdentity;

#[derive(Serialize, Deserialize)]
pub struct ContactCard {
    version: u8,
    display_name: String,
    peer_id: String,
    classic_public_key: String,
    bootstrap_addresses: Vec<String>,
    #[serde(default)]
    ml_dsa_public_key: Option<String>,
    #[serde(default)]
    classic_signature: Option<String>,
    #[serde(default)]
    ml_dsa_signature: Option<String>,
}

pub fn export(
    path: &Path,
    display_name: &str,
    peer_id: PeerId,
    identity: &libp2p::identity::Keypair,
    ml_dsa_identity: &MlDsaIdentity,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<(), Box<dyn Error>> {
    let encoded = encode(display_name, peer_id, identity, ml_dsa_identity, addresses)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &encoded)?;
    println!("{}", encoded_link(&encoded));
    Ok(())
}

pub fn link(
    display_name: &str,
    peer_id: PeerId,
    identity: &libp2p::identity::Keypair,
    ml_dsa_identity: &MlDsaIdentity,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<String, Box<dyn Error>> {
    Ok(encoded_link(&encode(
        display_name,
        peer_id,
        identity,
        ml_dsa_identity,
        addresses,
    )?))
}

pub fn qr_link(
    display_name: &str,
    peer_id: PeerId,
    identity: &libp2p::identity::Keypair,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<String, Box<dyn Error>> {
    let mut card = ContactCard {
        version: 3,
        display_name: display_name.to_owned(),
        peer_id: peer_id.to_string(),
        classic_public_key: URL_SAFE_NO_PAD.encode(identity.public().encode_protobuf()),
        bootstrap_addresses: addresses
            .take(2)
            .map(|address| address.with(Protocol::P2p(peer_id)).to_string())
            .collect(),
        ml_dsa_public_key: None,
        classic_signature: None,
        ml_dsa_signature: None,
    };
    let payload = signing_payload(&card)?;
    card.classic_signature = Some(URL_SAFE_NO_PAD.encode(identity.sign(&payload)?));
    Ok(encoded_link(&cbor4ii::serde::to_vec(Vec::new(), &card)?))
}

pub fn share_links(
    display_name: &str,
    peer_id: PeerId,
    identity: &libp2p::identity::Keypair,
    ml_dsa_identity: &MlDsaIdentity,
    addresses: Vec<Multiaddr>,
) -> Result<(String, String), Box<dyn Error>> {
    Ok((
        link(
            display_name,
            peer_id,
            identity,
            ml_dsa_identity,
            addresses.clone().into_iter(),
        )?,
        qr_link(display_name, peer_id, identity, addresses.into_iter())?,
    ))
}

pub fn render_qr(payload: &str) -> Result<String, Box<dyn Error>> {
    let code = QrCode::new(payload.as_bytes())?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

fn encode(
    display_name: &str,
    peer_id: PeerId,
    identity: &libp2p::identity::Keypair,
    ml_dsa_identity: &MlDsaIdentity,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let addresses = addresses
        .take(4)
        .map(|address| address.with(Protocol::P2p(peer_id)).to_string())
        .collect();
    let mut card = ContactCard {
        version: 2,
        display_name: display_name.to_owned(),
        peer_id: peer_id.to_string(),
        classic_public_key: URL_SAFE_NO_PAD.encode(identity.public().encode_protobuf()),
        bootstrap_addresses: addresses,
        ml_dsa_public_key: Some(URL_SAFE_NO_PAD.encode(ml_dsa_identity.public_key())),
        classic_signature: None,
        ml_dsa_signature: None,
    };
    let payload = signing_payload(&card)?;
    card.classic_signature = Some(URL_SAFE_NO_PAD.encode(identity.sign(&payload)?));
    card.ml_dsa_signature = Some(URL_SAFE_NO_PAD.encode(ml_dsa_identity.sign(&payload)?));
    Ok(cbor4ii::serde::to_vec(Vec::new(), &card)?)
}

fn signing_payload(card: &ContactCard) -> Result<Vec<u8>, Box<dyn Error>> {
    Ok(cbor4ii::serde::to_vec(
        Vec::new(),
        &(
            card.version,
            &card.display_name,
            &card.peer_id,
            &card.classic_public_key,
            &card.bootstrap_addresses,
            &card.ml_dsa_public_key,
        ),
    )?)
}

fn encoded_link(encoded: &[u8]) -> String {
    format!("msnnext://add/{}", URL_SAFE_NO_PAD.encode(encoded))
}

pub fn import(path: &Path) -> Result<(String, PeerId, Vec<Multiaddr>), Box<dyn Error>> {
    let bytes = fs::read(path)?;
    decode(&bytes)
}

pub fn import_link(link: &str) -> Result<(String, PeerId, Vec<Multiaddr>), Box<dyn Error>> {
    let encoded = link
        .strip_prefix("msnnext://add/")
        .ok_or("link contatto non valido")?;
    decode(&URL_SAFE_NO_PAD.decode(encoded)?)
}

fn decode(bytes: &[u8]) -> Result<(String, PeerId, Vec<Multiaddr>), Box<dyn Error>> {
    let card: ContactCard = cbor4ii::serde::from_slice(bytes)?;
    if !matches!(card.version, 1..=3)
        || card.display_name.is_empty()
        || card.display_name.len() > 64
    {
        return Err("scheda contatto non valida".into());
    }
    let peer_id: PeerId = card.peer_id.parse()?;
    let public_key =
        PublicKey::try_decode_protobuf(&URL_SAFE_NO_PAD.decode(&card.classic_public_key)?)?;
    if PeerId::from_public_key(&public_key) != peer_id {
        return Err("chiave e Peer ID non corrispondono".into());
    }
    if matches!(card.version, 2 | 3) {
        let payload = signing_payload(&card)?;
        let classic_signature = URL_SAFE_NO_PAD.decode(
            card.classic_signature
                .as_deref()
                .ok_or("firma classica mancante")?,
        )?;
        if !public_key.verify(&payload, &classic_signature) {
            return Err("firma classica non valida".into());
        }
    }
    if card.version == 2 {
        let payload = signing_payload(&card)?;
        let ml_dsa_public_key = URL_SAFE_NO_PAD.decode(
            card.ml_dsa_public_key
                .as_deref()
                .ok_or("chiave ML-DSA mancante")?,
        )?;
        let ml_dsa_signature = URL_SAFE_NO_PAD.decode(
            card.ml_dsa_signature
                .as_deref()
                .ok_or("firma ML-DSA mancante")?,
        )?;
        if !MlDsaIdentity::verify(&ml_dsa_public_key, &payload, &ml_dsa_signature) {
            return Err("firma ML-DSA non valida".into());
        }
    } else if card.version == 3
        && (card.ml_dsa_public_key.is_some() || card.ml_dsa_signature.is_some())
    {
        return Err("scheda QR non valida".into());
    }
    let addresses = card
        .bootstrap_addresses
        .into_iter()
        .map(|address| address.parse())
        .collect::<Result<Vec<_>, _>>()?;
    if addresses.len() > 32 {
        return Err("troppe bootstrap address".into());
    }
    Ok((card.display_name, peer_id, addresses))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity::Keypair;

    #[test]
    fn contact_rejects_mismatched_identity() {
        let first = Keypair::generate_ed25519();
        let second = Keypair::generate_ed25519();
        let card = ContactCard {
            version: 1,
            display_name: "Alice".into(),
            peer_id: PeerId::from(first.public()).to_string(),
            classic_public_key: URL_SAFE_NO_PAD.encode(second.public().encode_protobuf()),
            bootstrap_addresses: vec![],
            ml_dsa_public_key: None,
            classic_signature: None,
            ml_dsa_signature: None,
        };
        let path =
            std::env::temp_dir().join(format!("msnnext-contact-{}.json", std::process::id()));
        fs::write(&path, cbor4ii::serde::to_vec(Vec::new(), &card).unwrap()).unwrap();
        assert!(import(&path).is_err());
        fs::remove_file(path).ok();
    }

    #[test]
    fn contact_link_round_trips() {
        let identity = Keypair::generate_ed25519();
        let ml_dsa = MlDsaIdentity::from_seed(&[7; 32]).unwrap();
        let peer_id = PeerId::from(identity.public());
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/4040".parse().unwrap();

        let link = link(
            "Alice",
            peer_id,
            &identity,
            &ml_dsa,
            std::iter::once(address),
        )
        .unwrap();
        let (name, imported_peer, addresses) = import_link(&link).unwrap();

        assert_eq!(name, "Alice");
        assert_eq!(imported_peer, peer_id);
        assert_eq!(addresses.len(), 1);
        assert!(addresses[0]
            .to_string()
            .ends_with(&format!("/p2p/{peer_id}")));
    }

    #[test]
    fn hybrid_contact_link_rejects_tampering() {
        let identity = Keypair::generate_ed25519();
        let ml_dsa = MlDsaIdentity::from_seed(&[8; 32]).unwrap();
        let peer_id = PeerId::from(identity.public());
        let link = link("Alice", peer_id, &identity, &ml_dsa, std::iter::empty()).unwrap();
        let encoded = link.strip_prefix("msnnext://add/").unwrap();
        let mut card: ContactCard =
            cbor4ii::serde::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        card.display_name = "Mallory".into();
        let tampered = encoded_link(&cbor4ii::serde::to_vec(Vec::new(), &card).unwrap());

        assert!(import_link(&tampered).is_err());
    }

    #[test]
    fn contact_link_renders_as_terminal_qr() {
        let identity = Keypair::generate_ed25519();
        let peer_id = PeerId::from(identity.public());
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/4040".parse().unwrap();
        let link = qr_link("Alice", peer_id, &identity, std::iter::once(address)).unwrap();
        let qr = render_qr(&link).unwrap();

        assert!(qr.lines().count() > 10);
        assert!(qr.contains('█'));
        assert_eq!(import_link(&link).unwrap().1, peer_id);

        let encoded = link.strip_prefix("msnnext://add/").unwrap();
        let mut card: ContactCard =
            cbor4ii::serde::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        card.display_name = "Mallory".into();
        let tampered = encoded_link(&cbor4ii::serde::to_vec(Vec::new(), &card).unwrap());
        assert!(import_link(&tampered).is_err());
    }
}
