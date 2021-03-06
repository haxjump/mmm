use super::{time, PeerAddrSet, Retry, Tags, TrustMetric, MAX_RETRY_COUNT};

use std::{
    borrow::Borrow,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use derive_more::Display;
use parking_lot::RwLock;
use protocol::traits::PeerTag;
use tentacle::{
    secio::{PeerId, PublicKey},
    SessionId,
};

use crate::error::ErrorKind;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Clone, Copy, Display)]
#[repr(usize)]
pub enum Connectedness {
    #[display(fmt = "not connected")]
    NotConnected = 0,

    #[display(fmt = "can connect")]
    CanConnect = 1,

    #[display(fmt = "connected")]
    Connected = 2,

    #[display(fmt = "unconnectable")]
    Unconnectable = 3,

    #[display(fmt = "connecting")]
    Connecting = 4,
}

impl From<usize> for Connectedness {
    fn from(src: usize) -> Connectedness {
        use self::Connectedness::{
            CanConnect, Connected, Connecting, NotConnected, Unconnectable,
        };

        match src {
            0 => NotConnected,
            1 => CanConnect,
            2 => Connected,
            3 => Unconnectable,
            4 => Connecting,
            _ => NotConnected,
        }
    }
}

impl From<Connectedness> for usize {
    fn from(src: Connectedness) -> usize {
        src as usize
    }
}

#[derive(Debug)]
pub struct Peer {
    pub id: PeerId,
    pub multiaddrs: PeerAddrSet,
    pub retry: Retry,
    pub tags: Tags,
    pubkey: RwLock<Option<PublicKey>>,
    trust_metric: RwLock<Option<TrustMetric>>,
    connectedness: AtomicUsize,
    session_id: AtomicUsize,
    connected_at: AtomicU64,
    disconnected_at: AtomicU64,
    alive: AtomicU64,
}

impl Peer {
    pub fn new(peer_id: PeerId) -> Self {
        Peer {
            id: peer_id.clone(),
            multiaddrs: PeerAddrSet::new(peer_id),
            retry: Retry::new(MAX_RETRY_COUNT),
            tags: Tags::default(),
            pubkey: RwLock::new(None),
            trust_metric: RwLock::new(None),
            connectedness: AtomicUsize::new(Connectedness::NotConnected as usize),
            session_id: AtomicUsize::new(0),
            connected_at: AtomicU64::new(0),
            disconnected_at: AtomicU64::new(0),
            alive: AtomicU64::new(0),
        }
    }

    pub fn from_pubkey(pubkey: PublicKey) -> Result<Self, ErrorKind> {
        let peer = Peer::new(pubkey.peer_id());
        peer.set_pubkey(pubkey)?;

        Ok(peer)
    }

    pub fn owned_id(&self) -> PeerId {
        self.id.to_owned()
    }

    pub fn has_pubkey(&self) -> bool {
        self.pubkey.read().is_some()
    }

    pub fn owned_pubkey(&self) -> Option<PublicKey> {
        self.pubkey.read().clone()
    }

    pub fn set_pubkey(&self, pubkey: PublicKey) -> Result<(), ErrorKind> {
        if pubkey.peer_id() != self.id {
            Err(ErrorKind::PublicKeyNotMatchId {
                pubkey,
                id: self.id.clone(),
            })
        } else {
            *self.pubkey.write() = Some(pubkey);
            Ok(())
        }
    }

    pub fn trust_metric(&self) -> Option<TrustMetric> {
        self.trust_metric.read().clone()
    }

    pub fn set_trust_metric(&self, metric: TrustMetric) {
        *self.trust_metric.write() = Some(metric);
    }

    #[cfg(test)]
    pub fn remove_trust_metric(&self) {
        *self.trust_metric.write() = None;
    }

    pub fn connectedness(&self) -> Connectedness {
        Connectedness::from(self.connectedness.load(Ordering::SeqCst))
    }

    pub fn set_connectedness(&self, flag: Connectedness) {
        self.connectedness
            .store(usize::from(flag), Ordering::SeqCst);
    }

    pub fn set_session_id(&self, sid: SessionId) {
        self.session_id.store(sid.value(), Ordering::SeqCst);
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id.load(Ordering::SeqCst).into()
    }

    pub fn connected_at(&self) -> u64 {
        self.connected_at.load(Ordering::SeqCst)
    }

    pub(super) fn set_connected_at(&self, at: u64) {
        self.connected_at.store(at, Ordering::SeqCst);
    }

    pub fn disconnected_at(&self) -> u64 {
        self.disconnected_at.load(Ordering::SeqCst)
    }

    pub(super) fn set_disconnected_at(&self, at: u64) {
        self.disconnected_at.store(at, Ordering::SeqCst);
    }

    pub fn alive(&self) -> u64 {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn update_alive(&self) {
        let connected_at =
            UNIX_EPOCH + Duration::from_secs(self.connected_at.load(Ordering::SeqCst));
        let alive = time::duration_since(SystemTime::now(), connected_at).as_secs();

        self.alive.store(alive, Ordering::SeqCst);
    }

    pub(super) fn set_alive(&self, live: u64) {
        self.alive.store(live, Ordering::SeqCst);
    }

    pub fn mark_connected(&self, sid: SessionId) {
        self.set_connectedness(Connectedness::Connected);
        self.set_session_id(sid);
        self.retry.reset();
        self.update_connected();
    }

    pub fn mark_disconnected(&self) {
        self.set_connectedness(Connectedness::CanConnect);
        self.set_session_id(0.into());
        self.update_disconnected();
        self.update_alive();
    }

    pub fn banned(&self) -> bool {
        if let Some(until) = self.tags.get_banned_until() {
            if time::now() < until {
                return true;
            }

            self.tags.remove(&PeerTag::ban_key());
            if let Some(trust_metric) = self.trust_metric() {
                // TODO: Reset just in case, may remove in
                // the future.
                trust_metric.reset_history();
            }
        }

        false
    }

    fn update_connected(&self) {
        self.connected_at.store(time::now(), Ordering::SeqCst);
    }

    fn update_disconnected(&self) {
        self.disconnected_at.store(time::now(), Ordering::SeqCst);
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} multiaddr {:?} tags {:?} last connected at {} alive {} retry {} current {}",
            self.id,
            self.multiaddrs.all(),
            self.tags,
            self.connected_at.load(Ordering::SeqCst),
            self.alive.load(Ordering::SeqCst),
            self.retry.count(),
            Connectedness::from(self.connectedness.load(Ordering::SeqCst))
        )
    }
}

#[derive(Debug, Display, Clone)]
#[display(fmt = "{}", _0)]
pub struct ArcPeer(Arc<Peer>);

impl ArcPeer {
    pub fn new(peer_id: PeerId) -> Self {
        ArcPeer(Arc::new(Peer::new(peer_id)))
    }

    pub fn from_pubkey(pubkey: PublicKey) -> Result<Self, ErrorKind> {
        Ok(ArcPeer(Arc::new(Peer::from_pubkey(pubkey)?)))
    }
}

impl Deref for ArcPeer {
    type Target = Peer;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Borrow<PeerId> for ArcPeer {
    fn borrow(&self) -> &PeerId {
        &self.id
    }
}

impl PartialEq for ArcPeer {
    fn eq(&self, other: &ArcPeer) -> bool {
        self.id == other.id
    }
}

impl Eq for ArcPeer {}

impl Hash for ArcPeer {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArcPeer, Connectedness};
    use crate::peer_manager::{time, TrustMetric, TrustMetricConfig};

    use tentacle::secio::SecioKeyPair;

    use std::sync::Arc;

    #[test]
    fn should_reset_trust_metric_history_after_unban() {
        let keypair = SecioKeyPair::secp256k1_generated();
        let pubkey = keypair.public_key();
        let peer = ArcPeer::from_pubkey(pubkey).expect("make peer");
        let peer_trust_config = Arc::new(TrustMetricConfig::default());

        let trust_metric = TrustMetric::new(Arc::clone(&peer_trust_config));
        peer.set_trust_metric(trust_metric.clone());
        for _ in 0..2 {
            trust_metric.bad_events(10);
            trust_metric.enter_new_interval();
        }
        assert!(trust_metric.trust_score() < 40, "should lower score");

        peer.tags.set_ban_until(time::now() - 20);
        assert!(!peer.banned(), "should unban");

        assert_eq!(
            trust_metric.intervals(),
            0,
            "should reset peer trust history"
        );
    }

    #[test]
    fn should_be_able_to_convert_between_connectedness_and_usize() {
        assert_eq!(usize::from(Connectedness::NotConnected), 0usize);
        assert_eq!(usize::from(Connectedness::CanConnect), 1usize);
        assert_eq!(usize::from(Connectedness::Connected), 2usize);
        assert_eq!(usize::from(Connectedness::Unconnectable), 3usize);
        assert_eq!(usize::from(Connectedness::Connecting), 4usize);

        assert_eq!(Connectedness::from(0usize), Connectedness::NotConnected);
        assert_eq!(Connectedness::from(1usize), Connectedness::CanConnect);
        assert_eq!(Connectedness::from(2usize), Connectedness::Connected);
        assert_eq!(Connectedness::from(3usize), Connectedness::Unconnectable);
        assert_eq!(Connectedness::from(4usize), Connectedness::Connecting);
        assert_eq!(Connectedness::from(5usize), Connectedness::NotConnected);
    }
}
