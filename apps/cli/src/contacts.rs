use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use libp2p::{identity::PublicKey, multiaddr::Protocol, Multiaddr, PeerId};
use qrcode::{render::unicode, QrCode};
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};

#[derive(Serialize, Deserialize)]
pub struct ContactCard {
    version: u8,
    display_name: String,
    peer_id: String,
    classic_public_key: String,
    bootstrap_addresses: Vec<String>,
}

pub fn export(
    path: &Path,
    display_name: &str,
    peer_id: PeerId,
    public_key: &PublicKey,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<(), Box<dyn Error>> {
    let encoded = encode(display_name, peer_id, public_key, addresses)?;
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
    public_key: &PublicKey,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<String, Box<dyn Error>> {
    Ok(encoded_link(&encode(
        display_name,
        peer_id,
        public_key,
        addresses,
    )?))
}

pub fn render_qr(payload: &str) -> Result<String, Box<dyn Error>> {
    let code = QrCode::new(payload.as_bytes())?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

fn encode(
    display_name: &str,
    peer_id: PeerId,
    public_key: &PublicKey,
    addresses: impl Iterator<Item = Multiaddr>,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let addresses = addresses
        .take(4)
        .map(|address| address.with(Protocol::P2p(peer_id)).to_string())
        .collect();
    let card = ContactCard {
        version: 1,
        display_name: display_name.to_owned(),
        peer_id: peer_id.to_string(),
        classic_public_key: URL_SAFE_NO_PAD.encode(public_key.encode_protobuf()),
        bootstrap_addresses: addresses,
    };
    Ok(cbor4ii::serde::to_vec(Vec::new(), &card)?)
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
    if card.version != 1 || card.display_name.is_empty() || card.display_name.len() > 64 {
        return Err("scheda contatto non valida".into());
    }
    let peer_id: PeerId = card.peer_id.parse()?;
    let public_key =
        PublicKey::try_decode_protobuf(&URL_SAFE_NO_PAD.decode(card.classic_public_key)?)?;
    if PeerId::from_public_key(&public_key) != peer_id {
        return Err("chiave e Peer ID non corrispondono".into());
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
        let peer_id = PeerId::from(identity.public());
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/4040".parse().unwrap();

        let link = link(
            "Alice",
            peer_id,
            &identity.public(),
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
    fn contact_link_renders_as_terminal_qr() {
        let qr = render_qr("msnnext://add/example").unwrap();

        assert!(qr.lines().count() > 10);
        assert!(qr.contains('█'));
    }
}
