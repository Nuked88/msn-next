use libp2p::{multiaddr::Protocol, Multiaddr, PeerId};
use std::collections::HashMap;

pub(crate) fn split_peer_address(address: &Multiaddr) -> Result<(PeerId, Multiaddr), String> {
    let mut base = address.clone();
    match base.pop() {
        Some(Protocol::P2p(peer)) => Ok((peer, base)),
        _ => Err("l'indirizzo deve terminare con /p2p/<peer-id>".into()),
    }
}

pub(crate) fn relay_route(relay: &Multiaddr, target: PeerId) -> Result<Multiaddr, String> {
    let (relay_peer, base) = split_peer_address(relay)?;
    if base
        .iter()
        .any(|protocol| matches!(protocol, Protocol::P2pCircuit))
    {
        return Err("l'indirizzo relay non deve contenere /p2p-circuit".into());
    }
    Ok(base
        .with(Protocol::P2p(relay_peer))
        .with(Protocol::P2pCircuit)
        .with(Protocol::P2p(target)))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    SearchDht(PeerId),
    DialPeer(PeerId),
    ViaRelay(Multiaddr),
    Exhausted,
}

pub(crate) struct FallbackPlanner {
    relays: Vec<Multiaddr>,
    stages: HashMap<PeerId, usize>,
}

impl FallbackPlanner {
    pub(crate) fn new(relays: Vec<Multiaddr>) -> Self {
        Self {
            relays,
            stages: HashMap::new(),
        }
    }

    pub(crate) fn after_failure(&mut self, peer: PeerId) -> Recovery {
        let stage = self.stages.entry(peer).or_default();
        if *stage == 0 {
            *stage = 1;
            return Recovery::SearchDht(peer);
        }

        let mut relay_index = stage.saturating_sub(2);
        while let Some(relay) = self.relays.get(relay_index) {
            *stage = relay_index + 3;
            if let Ok(route) = relay_route(relay, peer) {
                return Recovery::ViaRelay(route);
            }
            relay_index += 1;
        }
        Recovery::Exhausted
    }

    pub(crate) fn after_dht(&mut self, peer: PeerId) -> Recovery {
        self.stages.insert(peer, 2);
        Recovery::DialPeer(peer)
    }

    pub(crate) fn connected(&mut self, peer: PeerId) {
        self.stages.remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::{identity::Keypair, multiaddr::Protocol, Multiaddr, PeerId};

    fn peer_address(port: u16, peer: PeerId) -> Multiaddr {
        Multiaddr::empty()
            .with(Protocol::Ip4("127.0.0.1".parse().unwrap()))
            .with(Protocol::Tcp(port))
            .with(Protocol::P2p(peer))
    }

    #[test]
    fn peer_address_requires_trailing_peer_id() {
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/4001".parse().unwrap();
        assert!(split_peer_address(&address).is_err());
    }

    #[test]
    fn relay_route_appends_circuit_and_target() {
        let relay_peer = PeerId::from(Keypair::generate_ed25519().public());
        let target_peer = PeerId::from(Keypair::generate_ed25519().public());
        let relay = peer_address(4001, relay_peer);

        let route = relay_route(&relay, target_peer).unwrap();

        assert!(route
            .to_string()
            .ends_with(&format!("/p2p-circuit/p2p/{target_peer}")));
    }

    #[test]
    fn fallback_orders_dht_before_relay() {
        let relay_peer = PeerId::from(Keypair::generate_ed25519().public());
        let target_peer = PeerId::from(Keypair::generate_ed25519().public());
        let relay = peer_address(4001, relay_peer);
        let mut planner = FallbackPlanner::new(vec![relay.clone()]);

        assert_eq!(
            planner.after_failure(target_peer),
            Recovery::SearchDht(target_peer)
        );
        assert_eq!(
            planner.after_dht(target_peer),
            Recovery::DialPeer(target_peer)
        );
        assert_eq!(
            planner.after_failure(target_peer),
            Recovery::ViaRelay(relay_route(&relay, target_peer).unwrap())
        );
        assert_eq!(planner.after_failure(target_peer), Recovery::Exhausted);

        planner.connected(target_peer);
        assert_eq!(
            planner.after_failure(target_peer),
            Recovery::SearchDht(target_peer)
        );
    }
}
