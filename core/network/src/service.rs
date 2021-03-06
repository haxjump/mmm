use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use async_trait::async_trait;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::stream::Stream;
use futures::task::AtomicWaker;
use log::{debug, error, info};
use protocol::traits::{
    Context, Gossip, MessageCodec, MessageHandler, Network, PeerTag, PeerTrust,
    Priority, Rpc, TrustFeedback,
};
use protocol::types::Hash;
use protocol::{Bytes, ProtocolResult};
use tentacle::secio::PeerId;

use crate::common::{socket_to_multi_addr, HeartBeat};
use crate::compression::Snappy;
use crate::connection::{ConnectionConfig, ConnectionService, ConnectionServiceKeeper};
use crate::endpoint::{Endpoint, EndpointScheme};
use crate::error::NetworkError;
use crate::event::{ConnectionEvent, PeerManagerEvent};
use crate::metrics::Metrics;
use crate::outbound::{NetworkGossip, NetworkRpc};
#[cfg(feature = "diagnostic")]
use crate::peer_manager::diagnostic::{Diagnostic, DiagnosticHookFn};
use crate::peer_manager::{
    PeerManager, PeerManagerConfig, PeerManagerHandle, SharedSessions,
};
use crate::protocols::{CoreProtocol, Transmitter};
use crate::reactor::MessageRouter;
use crate::selfcheck::SelfCheck;
use crate::traits::NetworkContext;
use crate::{NetworkConfig, PeerIdExt};

#[derive(Clone)]
pub struct NetworkServiceHandle {
    gossip: NetworkGossip,
    rpc: NetworkRpc,
    peer_trust: UnboundedSender<PeerManagerEvent>,
    peer_state: PeerManagerHandle,

    #[cfg(feature = "diagnostic")]
    pub diagnostic: Diagnostic,
}

#[async_trait]
impl Gossip for NetworkServiceHandle {
    async fn broadcast<M>(
        &self,
        cx: Context,
        end: &str,
        msg: M,
        p: Priority,
    ) -> ProtocolResult<()>
    where
        M: MessageCodec,
    {
        self.gossip.broadcast(cx, end, msg, p).await
    }

    async fn multicast<'a, M, P>(
        &self,
        cx: Context,
        end: &str,
        peer_ids: P,
        msg: M,
        p: Priority,
    ) -> ProtocolResult<()>
    where
        M: MessageCodec,
        P: AsRef<[Bytes]> + Send + 'a,
    {
        self.gossip.multicast(cx, end, peer_ids, msg, p).await
    }
}

#[async_trait]
impl Rpc for NetworkServiceHandle {
    async fn call<M, R>(
        &self,
        cx: Context,
        end: &str,
        msg: M,
        p: Priority,
    ) -> ProtocolResult<R>
    where
        M: MessageCodec,
        R: MessageCodec,
    {
        self.rpc.call(cx, end, msg, p).await
    }

    async fn response<M>(
        &self,
        cx: Context,
        end: &str,
        msg: ProtocolResult<M>,
        p: Priority,
    ) -> ProtocolResult<()>
    where
        M: MessageCodec,
    {
        self.rpc.response(cx, end, msg, p).await
    }
}

impl PeerTrust for NetworkServiceHandle {
    fn report(&self, ctx: Context, feedback: TrustFeedback) {
        let remote_peer_id = match ctx.remote_peer_id() {
            Ok(id) => id,
            Err(e) => {
                log::error!(
                    "peer id not found on trust report ctx, repoort {}, err {}",
                    feedback,
                    e
                );
                return;
            }
        };

        let feedback = PeerManagerEvent::TrustMetric {
            pid: remote_peer_id,
            feedback,
        };
        if let Err(e) = self.peer_trust.unbounded_send(feedback) {
            log::error!("peer manager offline {}", e);
        }
    }
}

impl Network for NetworkServiceHandle {
    fn tag(&self, _: Context, peer_id: Bytes, tag: PeerTag) -> ProtocolResult<()> {
        let peer_id = <PeerId as PeerIdExt>::from_bytes(peer_id)?;
        self.peer_state.tag(&peer_id, tag)?;

        Ok(())
    }

    fn untag(&self, _: Context, peer_id: Bytes, tag: &PeerTag) -> ProtocolResult<()> {
        let peer_id = <PeerId as PeerIdExt>::from_bytes(peer_id)?;
        self.peer_state.untag(&peer_id, tag);

        Ok(())
    }

    fn tag_consensus(&self, _: Context, peer_ids: Vec<Bytes>) -> ProtocolResult<()> {
        let peer_ids = peer_ids
            .into_iter()
            .map(<PeerId as PeerIdExt>::from_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        self.peer_state.tag_consensus(peer_ids);

        Ok(())
    }
}

enum NetworkConnectionService {
    NoListen(ConnectionService<CoreProtocol>), // no listen address yet
    Ready(ConnectionService<CoreProtocol>),
}

pub struct NetworkService {
    sys_rx: UnboundedReceiver<NetworkError>,

    // Heart beats
    conn_tx: UnboundedSender<ConnectionEvent>,
    mgr_tx: UnboundedSender<PeerManagerEvent>,
    heart_beat: Option<HeartBeat>,
    hb_waker: Arc<AtomicWaker>,

    // Config backup
    config: NetworkConfig,

    // Public service components
    gossip: NetworkGossip,
    rpc: NetworkRpc,
    transmitter: Transmitter,

    // Core service
    net_conn_srv: Option<NetworkConnectionService>,
    peer_mgr: Option<PeerManager>,
    peer_mgr_handle: PeerManagerHandle,

    // Metrics
    metrics: Option<Metrics<SharedSessions>>,

    // Self check
    selfcheck: Option<SelfCheck<SharedSessions>>,

    // Diagnostic
    #[cfg(feature = "diagnostic")]
    diagnostic: Diagnostic,
}

impl NetworkService {
    pub fn new(config: NetworkConfig) -> Self {
        let (mgr_tx, mgr_rx) = unbounded();
        let (conn_tx, conn_rx) = unbounded();
        let (sys_tx, sys_rx) = unbounded();

        let hb_waker = Arc::new(AtomicWaker::new());
        let heart_beat =
            HeartBeat::new(Arc::clone(&hb_waker), config.heart_beat_interval);

        let mgr_config = PeerManagerConfig::from(&config);
        let conn_config = ConnectionConfig::from(&config);

        // Build peer manager
        let mut peer_mgr = PeerManager::new(mgr_config, mgr_rx, conn_tx.clone());
        let peer_mgr_handle = peer_mgr.handle();
        let session_book = peer_mgr.share_session_book((&config).into());
        #[cfg(feature = "diagnostic")]
        let diagnostic = peer_mgr.diagnostic();

        if config.enable_save_restore {
            peer_mgr.enable_save_restore();
        }

        if let Err(err) = peer_mgr.restore_peers() {
            error!("network: peer manager: load peers failure: {}", err);
        }

        if !config.bootstraps.is_empty() {
            peer_mgr.bootstrap();
        }

        // Build service protocol
        let disc_sync_interval = config.discovery_sync_interval;
        let message_router = MessageRouter::new(mgr_tx.clone(), Snappy);
        let proto = CoreProtocol::build()
            .ping(config.ping_interval, config.ping_timeout, mgr_tx.clone())
            .identify(peer_mgr_handle.clone(), mgr_tx.clone())
            .discovery(peer_mgr_handle.clone(), mgr_tx.clone(), disc_sync_interval)
            .transmitter(message_router, peer_mgr_handle.clone())
            .build();
        let transmitter = proto.transmitter();

        // Build connection service
        let keeper = ConnectionServiceKeeper::new(mgr_tx.clone(), sys_tx);
        let conn_srv =
            ConnectionService::<CoreProtocol>::new(proto, conn_config, keeper, conn_rx);
        let conn_ctrl = conn_srv.control();

        transmitter
            .behaviour
            .init(conn_ctrl, mgr_tx.clone(), session_book.clone());

        // Build public service components
        let gossip = NetworkGossip::new(transmitter.clone());
        let rpc = NetworkRpc::new(transmitter.clone(), (&config).into());

        // Build metrics service
        let metrics = Metrics::new(session_book.clone());

        // Build selfcheck service
        let selfcheck = SelfCheck::new(session_book, (&config).into());

        NetworkService {
            sys_rx,
            conn_tx,
            mgr_tx,
            hb_waker,

            heart_beat: Some(heart_beat),

            config,

            gossip,
            rpc,
            transmitter,

            net_conn_srv: Some(NetworkConnectionService::NoListen(conn_srv)),
            peer_mgr: Some(peer_mgr),
            peer_mgr_handle,

            metrics: Some(metrics),

            selfcheck: Some(selfcheck),

            #[cfg(feature = "diagnostic")]
            diagnostic,
        }
    }

    pub fn register_endpoint_handler<M>(
        &mut self,
        end: &str,
        handler: impl MessageHandler<Message = M>,
    ) -> ProtocolResult<()>
    where
        M: MessageCodec,
    {
        let endpoint = end.parse::<Endpoint>()?;
        if endpoint.scheme() == EndpointScheme::RpcResponse {
            let err = "use register_rpc_response() instead".to_owned();

            return Err(NetworkError::UnexpectedScheme(err).into());
        }

        self.transmitter.router.register_reactor(endpoint, handler);
        Ok(())
    }

    // Currently rpc response dont invoke message handler, so we create a dummy
    // for it.
    pub fn register_rpc_response<M>(&mut self, end: &str) -> ProtocolResult<()>
    where
        M: MessageCodec,
    {
        let endpoint = end.parse::<Endpoint>()?;
        if endpoint.scheme() != EndpointScheme::RpcResponse {
            return Err(NetworkError::UnexpectedScheme(end.to_owned()).into());
        }

        self.transmitter.router.register_rpc_response(endpoint);
        Ok(())
    }

    #[cfg(feature = "diagnostic")]
    pub fn register_diagnostic_hook(&mut self, f: DiagnosticHookFn) {
        if let Some(peer_mgr) = self.peer_mgr.as_mut() {
            peer_mgr.register_diagnostic_hook(f);
        }
    }

    pub fn handle(&self) -> NetworkServiceHandle {
        NetworkServiceHandle {
            gossip: self.gossip.clone(),
            rpc: self.rpc.clone(),
            peer_trust: self.mgr_tx.clone(),
            peer_state: self.peer_mgr_handle.clone(),

            #[cfg(feature = "diagnostic")]
            diagnostic: self.diagnostic.clone(),
        }
    }

    pub fn peer_id(&self) -> PeerId {
        self.config.secio_keypair.peer_id()
    }

    pub fn set_chain_id(&self, chain_id: Hash) {
        self.peer_mgr_handle.set_chain_id(chain_id);
    }

    pub async fn listen(&mut self, socket_addr: SocketAddr) -> ProtocolResult<()> {
        if let Some(NetworkConnectionService::NoListen(conn_srv)) =
            &mut self.net_conn_srv
        {
            debug!("network: listen to {}", socket_addr);

            let addr = socket_to_multi_addr(socket_addr);

            conn_srv.listen(addr.clone()).await?;

            // Update service state
            if let Some(NetworkConnectionService::NoListen(conn_srv)) =
                self.net_conn_srv.take()
            {
                self.net_conn_srv = Some(NetworkConnectionService::Ready(conn_srv));
            } else {
                unreachable!("connection service must be there");
            }
        }

        Ok(())
    }
}

impl Future for NetworkService {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, ctx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        self.hb_waker.register(ctx.waker());

        macro_rules! service_ready {
            ($poll:expr) => {
                match $poll {
                    Poll::Pending => break,
                    Poll::Ready(Some(v)) => v,
                    Poll::Ready(None) => {
                        info!("network shutdown");

                        return Poll::Ready(());
                    }
                }
            };
        }

        // Preflight
        if let Some(conn_srv) = self.net_conn_srv.take() {
            let default_listen = self.config.default_listen.clone();

            tokio::spawn(async move {
                let conn_srv = match conn_srv {
                    NetworkConnectionService::NoListen(mut conn_srv) => {
                        conn_srv
                            .listen(default_listen)
                            .await
                            .expect("fail to listen default address");

                        conn_srv
                    }
                    NetworkConnectionService::Ready(conn_srv) => conn_srv,
                };

                conn_srv.await
            });
        }

        if let Some(peer_mgr) = self.peer_mgr.take() {
            tokio::spawn(peer_mgr);
        }

        if let Some(metrics) = self.metrics.take() {
            tokio::spawn(metrics);
        }

        if let Some(selfcheck) = self.selfcheck.take() {
            tokio::spawn(selfcheck);
        }

        // Heart beats
        if let Some(heart_beat) = self.heart_beat.take() {
            tokio::spawn(heart_beat);
        }

        // TODO: Reboot ceased service? Right now we just assume that it's
        // normal shutdown, simple log it and let it go.
        //
        // let it go ~~~ , let it go ~~~
        // i am one with the wind and sky
        // let it go, let it go
        // you'll never see me cry
        // bla bla bal ~~~
        if self.conn_tx.is_closed() {
            info!("network: connection service closed");
        }

        if self.mgr_tx.is_closed() {
            info!("network: peer manager closed");
        }

        // Process system error report
        loop {
            let sys_rx = &mut self.as_mut().sys_rx;
            futures::pin_mut!(sys_rx);

            let sys_err = service_ready!(sys_rx.poll_next(ctx));
            error!("network: system error: {}", sys_err);
        }

        Poll::Pending
    }
}
