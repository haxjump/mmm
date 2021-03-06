#![allow(clippy::mutable_key_type)]

use derive_more::{Display, From};

use protocol::{ProtocolError, ProtocolErrorKind};

use std::collections::HashMap;
use std::convert::TryFrom;
use std::panic;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use backtrace::Backtrace;
use bytes::Bytes;
use futures::stream::StreamExt;
use futures::{future, lock::Mutex};
use futures_timer::Delay;
#[cfg(unix)]
use tokio::signal::unix::{self as os_impl};

use common_config_parser::types::Config;
use common_crypto::{
    BlsCommonReference, BlsPrivateKey, BlsPublicKey, PublicKey, Secp256k1,
    Secp256k1PrivateKey, ToPublicKey, UncompressedPublicKey,
};
use core_api::adapter::DefaultAPIAdapter;
use core_api::config::{GraphQLConfig, GraphQLTLS};
use core_consensus::fixed_types::{FixedBlock, FixedProof, FixedSignedTxs};
use core_consensus::message::{
    ChokeMessageHandler, ProposalMessageHandler, PullBlockRpcHandler,
    PullProofRpcHandler, PullTxsRpcHandler, QCMessageHandler,
    RemoteHeightMessageHandler, VoteMessageHandler, BROADCAST_HEIGHT,
    END_GOSSIP_AGGREGATED_VOTE, END_GOSSIP_SIGNED_CHOKE, END_GOSSIP_SIGNED_PROPOSAL,
    END_GOSSIP_SIGNED_VOTE, RPC_RESP_SYNC_PULL_BLOCK, RPC_RESP_SYNC_PULL_PROOF,
    RPC_RESP_SYNC_PULL_TXS, RPC_SYNC_PULL_BLOCK, RPC_SYNC_PULL_PROOF, RPC_SYNC_PULL_TXS,
};
use core_consensus::status::{CurrentConsensusStatus, StatusAgent};
use core_consensus::util::OverlordCrypto;
use core_consensus::{
    ConsensusWal, DurationConfig, Node, OverlordConsensus, OverlordConsensusAdapter,
    OverlordSynchronization, RichBlock, SignedTxsWAL,
};
use core_mempool::{
    DefaultMemPoolAdapter, HashMemPool, MsgPushTxs, NewTxsHandler, PullTxsHandler,
    END_GOSSIP_NEW_TXS, RPC_PULL_TXS, RPC_RESP_PULL_TXS, RPC_RESP_PULL_TXS_SYNC,
};
use core_network::{NetworkConfig, NetworkService, PeerId, PeerIdExt};
use core_storage::{adapter::rocks::RocksAdapter, ImplStorage, StorageError};
use framework::binding::state::RocksTrieDB;
use framework::executor::{ServiceExecutor, ServiceExecutorFactory};
use protocol::traits::{
    APIAdapter, CommonStorage, Context, MemPool, Network, NodeInfo, ServiceMapping,
    Storage,
};
use protocol::types::{
    Address, Block, BlockHeader, Genesis, Hash, Metadata, Proof, Validator,
};
use protocol::{fixed_codec::FixedCodec, ProtocolResult};

use common_apm::muta_apm;

pub struct Muta<Mapping>
where
    Mapping: ServiceMapping,
{
    config: Config,
    genesis: Genesis,
    service_mapping: Arc<Mapping>,
}

impl<Mapping: 'static + ServiceMapping> Muta<Mapping> {
    pub fn new(config: Config, genesis: Genesis, service_mapping: Arc<Mapping>) -> Self {
        Self {
            config,
            genesis,
            service_mapping,
        }
    }

    pub fn run(self) -> ProtocolResult<()> {
        if let Some(apm_config) = &self.config.apm {
            muta_apm::global_tracer_register(
                &apm_config.service_name,
                apm_config.tracing_address,
                apm_config.tracing_batch_size,
            );

            log::info!("muta_apm start");
        }
        // run muta
        let mut rt = tokio::runtime::Runtime::new().expect("new tokio runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&mut rt, async move {
            self.create_genesis().await?;

            self.start().await
        })?;

        Ok(())
    }

    pub async fn create_genesis(&self) -> ProtocolResult<Block> {
        log::info!("Genesis data: {:?}", self.genesis);

        let metadata_payload = self.genesis.get_payload("metadata");

        let hrp = Metadata::get_hrp_from_json(metadata_payload.to_string());

        // Set bech32 address hrp
        if !protocol::address_hrp_inited() {
            protocol::init_address_hrp(hrp.into());
        }

        // Init Block db
        let path_block = self.config.data_path_for_block();
        let rocks_adapter = Arc::new(RocksAdapter::new(
            path_block,
            self.config.rocksdb.max_open_files,
        )?);
        let storage = Arc::new(ImplStorage::new(rocks_adapter));

        match storage.get_latest_block(Context::new()).await {
            Ok(genesis_block) => {
                log::info!("The Genesis block has been initialized.");
                return Ok(genesis_block);
            }
            Err(e) => {
                if !e.to_string().contains("GetNone") {
                    return Err(e);
                }
            }
        };

        // Init trie db
        let path_state = self.config.data_path_for_state();
        let trie_db = Arc::new(RocksTrieDB::new(
            path_state,
            self.config.executor.light,
            self.config.rocksdb.max_open_files,
            self.config.executor.triedb_cache_size,
        )?);

        let metadata: Metadata =
            serde_json::from_str(self.genesis.get_payload("metadata"))
                .expect("Decode metadata failed!");

        let validators: Vec<Validator> = metadata
            .verifier_list
            .iter()
            .map(|v| Validator {
                pub_key: v.pub_key.decode(),
                propose_weight: v.propose_weight,
                vote_weight: v.vote_weight,
            })
            .collect();

        // Init genesis
        let genesis_state_root = ServiceExecutor::create_genesis(
            self.genesis.services.clone(),
            Arc::clone(&trie_db),
            Arc::clone(&storage),
            Arc::clone(&self.service_mapping),
        )?;

        // Build genesis block.
        let proposer =
            Address::from_hash(Hash::digest(protocol::address_hrp().as_str()))?;
        let genesis_block_header = BlockHeader {
            chain_id: metadata.chain_id.clone(),
            height: 0,
            exec_height: 0,
            prev_hash: Hash::from_empty(),
            timestamp: self.genesis.timestamp,
            order_root: Hash::from_empty(),
            order_signed_transactions_hash: Hash::from_empty(),
            confirm_root: vec![],
            state_root: genesis_state_root,
            receipt_root: vec![],
            cycles_used: vec![],
            proposer,
            proof: Proof {
                height: 0,
                round: 0,
                block_hash: Hash::from_empty(),
                signature: Bytes::new(),
                bitmap: Bytes::new(),
            },
            validator_version: 0,
            validators,
        };
        let latest_proof = genesis_block_header.proof.clone();
        let genesis_block = Block {
            header: genesis_block_header,
            ordered_tx_hashes: vec![],
        };
        storage
            .insert_block(Context::new(), genesis_block.clone())
            .await?;
        storage
            .update_latest_proof(Context::new(), latest_proof)
            .await?;

        log::info!("The genesis block is created {:?}", genesis_block);
        Ok(genesis_block)
    }

    pub async fn start(self) -> ProtocolResult<()> {
        log::info!("node starts");
        let config = self.config;
        let service_mapping = self.service_mapping;
        // Init Block db
        let path_block = config.data_path_for_block();
        log::info!("Data path for block: {:?}", path_block);

        let rocks_adapter = Arc::new(RocksAdapter::new(
            path_block.clone(),
            config.rocksdb.max_open_files,
        )?);
        let storage = Arc::new(ImplStorage::new(Arc::clone(&rocks_adapter)));

        // Init network
        let network_config = NetworkConfig::new()
            .max_connections(config.network.max_connected_peers)?
            .same_ip_conn_limit(config.network.same_ip_conn_limit)
            .inbound_conn_limit(config.network.inbound_conn_limit)?
            .allowlist_only(config.network.allowlist_only)
            .peer_trust_metric(
                config.network.trust_interval_duration,
                config.network.trust_max_history_duration,
            )?
            .peer_soft_ban(config.network.soft_ban_duration)
            .peer_fatal_ban(config.network.fatal_ban_duration)
            .rpc_timeout(config.network.rpc_timeout)
            .ping_interval(config.network.ping_interval)
            .selfcheck_interval(config.network.selfcheck_interval)
            .max_wait_streams(config.network.max_wait_streams)
            .max_frame_length(config.network.max_frame_length)
            .send_buffer_size(config.network.send_buffer_size)
            .write_timeout(config.network.write_timeout)
            .recv_buffer_size(config.network.recv_buffer_size);

        let network_privkey = config.privkey.as_string_trim0x();

        let mut bootstrap_pairs = vec![];
        if let Some(bootstrap) = &config.network.bootstraps {
            for bootstrap in bootstrap.iter() {
                bootstrap_pairs
                    .push((bootstrap.peer_id.to_owned(), bootstrap.address.to_owned()));
            }
        }

        let allowlist = config.network.allowlist.clone().unwrap_or_default();
        let network_config = network_config
            .bootstraps(bootstrap_pairs)?
            .allowlist(allowlist)?
            .secio_keypair(network_privkey)?;

        let mut network_service = NetworkService::new(network_config);
        network_service
            .listen(config.network.listening_address)
            .await?;

        // Init trie db
        let path_state = config.data_path_for_state();
        let trie_db = Arc::new(RocksTrieDB::new(
            path_state,
            config.executor.light,
            config.rocksdb.max_open_files,
            config.executor.triedb_cache_size,
        )?);

        // Init full transactions wal
        let txs_wal_path = config.data_path_for_txs_wal().to_str().unwrap().to_string();
        let txs_wal = Arc::new(SignedTxsWAL::new(txs_wal_path));

        // Init consensus wal
        let consensus_wal_path = config
            .data_path_for_consensus_wal()
            .to_str()
            .unwrap()
            .to_string();
        let consensus_wal = Arc::new(ConsensusWal::new(consensus_wal_path));

        // Recover signed transactions of current height
        let current_block = storage.get_latest_block(Context::new()).await?;
        let current_stxs = txs_wal.load_by_height(current_block.header.height + 1);
        log::info!(
            "Recover {} tx of height {} from wal",
            current_stxs.len(),
            current_block.header.height + 1
        );

        // Init mempool
        let mempool_adapter =
            DefaultMemPoolAdapter::<ServiceExecutorFactory, Secp256k1, _, _, _, _>::new(
                network_service.handle(),
                Arc::clone(&storage),
                Arc::clone(&trie_db),
                Arc::clone(&service_mapping),
                config.mempool.broadcast_txs_size,
                config.mempool.broadcast_txs_interval,
            );
        let mempool = Arc::new(
            HashMemPool::new(
                config.mempool.pool_size as usize,
                mempool_adapter,
                current_stxs,
            )
            .await,
        );

        let monitor_mempool = Arc::clone(&mempool);
        tokio::spawn(async move {
            let interval = Duration::from_millis(1000);
            loop {
                Delay::new(interval).await;
                common_apm::metrics::mempool::MEMPOOL_LEN_GAUGE
                    .set(monitor_mempool.get_tx_cache().len().await as i64);
            }
        });

        // self private key
        let hex_privkey = hex::decode(config.privkey.as_string_trim0x())
            .map_err(MainError::FromHex)?;
        let my_privkey = Secp256k1PrivateKey::try_from(hex_privkey.as_ref())
            .map_err(MainError::Crypto)?;
        let my_pubkey = my_privkey.pub_key();
        let my_address = Address::from_pubkey_bytes(my_pubkey.to_uncompressed_bytes())?;

        // Get metadata
        let api_adapter = DefaultAPIAdapter::<ServiceExecutorFactory, _, _, _, _>::new(
            Arc::clone(&mempool),
            Arc::clone(&storage),
            Arc::clone(&trie_db),
            Arc::clone(&service_mapping),
        );

        let exec_resp = api_adapter
            .query_service(
                Context::new(),
                current_block.header.height,
                u64::max_value(),
                1,
                my_address.clone(),
                "metadata".to_string(),
                "get_metadata".to_string(),
                "".to_string(),
            )
            .await?;

        let metadata: Metadata = serde_json::from_str(&exec_resp.succeed_data)
            .expect("Decode metadata failed!");

        // Set bech32 address hrp
        if !protocol::address_hrp_inited() {
            protocol::init_address_hrp(metadata.bech32_address_hrp.into());
        }

        // set chain id in network
        network_service.set_chain_id(metadata.chain_id.clone());

        // set args in mempool
        mempool.set_args(
            metadata.timeout_gap,
            metadata.cycles_limit,
            metadata.max_tx_size,
        );

        // register broadcast new transaction
        network_service.register_endpoint_handler(
            END_GOSSIP_NEW_TXS,
            NewTxsHandler::new(Arc::clone(&mempool)),
        )?;

        // register pull txs from other node
        network_service.register_endpoint_handler(
            RPC_PULL_TXS,
            PullTxsHandler::new(
                Arc::new(network_service.handle()),
                Arc::clone(&mempool),
            ),
        )?;
        network_service.register_rpc_response::<MsgPushTxs>(RPC_RESP_PULL_TXS)?;

        network_service.register_rpc_response::<MsgPushTxs>(RPC_RESP_PULL_TXS_SYNC)?;

        // Init Consensus
        let validators: Vec<Validator> = metadata
            .verifier_list
            .iter()
            .map(|v| Validator {
                pub_key: v.pub_key.decode(),
                propose_weight: v.propose_weight,
                vote_weight: v.vote_weight,
            })
            .collect();

        let node_info = NodeInfo {
            chain_id: metadata.chain_id.clone(),
            self_address: my_address.clone(),
            self_pub_key: my_pubkey.to_bytes(),
        };
        let current_header = &current_block.header;
        let block_hash = Hash::digest(current_block.header.encode_fixed()?);
        let current_height = current_block.header.height;
        let exec_height = current_block.header.exec_height;
        let proof = if let Ok(temp) = storage.get_latest_proof(Context::new()).await {
            temp
        } else {
            current_header.proof.clone()
        };

        let current_consensus_status = CurrentConsensusStatus {
            cycles_price: metadata.cycles_price,
            cycles_limit: metadata.cycles_limit,
            latest_committed_height: current_block.header.height,
            exec_height: current_block.header.exec_height,
            current_hash: block_hash,
            latest_committed_state_root: current_header.state_root.clone(),
            list_confirm_root: vec![],
            list_state_root: vec![],
            list_receipt_root: vec![],
            list_cycles_used: vec![],
            current_proof: proof,
            validators: validators.clone(),
            consensus_interval: metadata.interval,
            propose_ratio: metadata.propose_ratio,
            prevote_ratio: metadata.prevote_ratio,
            precommit_ratio: metadata.precommit_ratio,
            brake_ratio: metadata.brake_ratio,
            max_tx_size: metadata.max_tx_size,
            tx_num_limit: metadata.tx_num_limit,
        };

        let consensus_interval = current_consensus_status.consensus_interval;
        let status_agent = StatusAgent::new(current_consensus_status);

        let mut bls_pub_keys = HashMap::new();
        for validator_extend in metadata.verifier_list.iter() {
            let address = validator_extend.pub_key.decode();
            let hex_pubkey =
                hex::decode(validator_extend.bls_pub_key.as_string_trim0x())
                    .map_err(MainError::FromHex)?;
            let pub_key = BlsPublicKey::try_from(hex_pubkey.as_ref())
                .map_err(MainError::Crypto)?;
            bls_pub_keys.insert(address, pub_key);
        }

        let mut priv_key = Vec::new();
        priv_key.extend_from_slice(&[0u8; 16]);
        let mut tmp = hex::decode(config.privkey.as_string_trim0x()).unwrap();
        priv_key.append(&mut tmp);
        let bls_priv_key =
            BlsPrivateKey::try_from(priv_key.as_ref()).map_err(MainError::Crypto)?;

        let hex_common_ref = hex::decode(metadata.common_ref.as_string_trim0x())
            .map_err(MainError::FromHex)?;
        let common_ref: BlsCommonReference =
            std::str::from_utf8(hex_common_ref.as_ref())
                .map_err(MainError::Utf8)?
                .into();

        let crypto =
            Arc::new(OverlordCrypto::new(bls_priv_key, bls_pub_keys, common_ref));

        let mut consensus_adapter =
            OverlordConsensusAdapter::<ServiceExecutorFactory, _, _, _, _, _>::new(
                Arc::new(network_service.handle()),
                Arc::clone(&mempool),
                Arc::clone(&storage),
                Arc::clone(&trie_db),
                Arc::clone(&service_mapping),
                status_agent.clone(),
                Arc::clone(&crypto),
                config.consensus.overlord_gap,
            )?;

        let exec_demon = consensus_adapter.take_exec_demon();
        let consensus_adapter = Arc::new(consensus_adapter);

        let lock = Arc::new(Mutex::new(()));

        let overlord_consensus = Arc::new(OverlordConsensus::new(
            status_agent.clone(),
            node_info,
            Arc::clone(&crypto),
            Arc::clone(&txs_wal),
            Arc::clone(&consensus_adapter),
            Arc::clone(&lock),
            Arc::clone(&consensus_wal),
        ));

        consensus_adapter
            .set_overlord_handler(overlord_consensus.get_overlord_handler());

        let synchronization = Arc::new(OverlordSynchronization::<_>::new(
            config.consensus.sync_txs_chunk_size,
            consensus_adapter,
            status_agent.clone(),
            crypto,
            lock,
        ));

        let peer_ids = metadata
            .verifier_list
            .iter()
            .map(|v| {
                PeerId::from_pubkey_bytes(v.pub_key.decode())
                    .map(PeerIdExt::into_bytes_ext)
            })
            .collect::<Result<Vec<_>, _>>()?;

        network_service
            .handle()
            .tag_consensus(Context::new(), peer_ids)?;

        // Re-execute block from exec_height + 1 to current_height, so that init the
        // lost current status.
        log::info!("Re-execute from {} to {}", exec_height + 1, current_height);
        for height in exec_height + 1..=current_height {
            let block = storage
                .get_block(Context::new(), height)
                .await?
                .ok_or(StorageError::GetNone)?;
            let txs = storage
                .get_transactions(
                    Context::new(),
                    block.header.height,
                    &block.ordered_tx_hashes,
                )
                .await?
                .into_iter()
                .filter_map(|opt_stx| opt_stx)
                .collect::<Vec<_>>();
            if txs.len() != block.ordered_tx_hashes.len() {
                return Err(StorageError::GetNone.into());
            }
            let rich_block = RichBlock { block, txs };
            let _ = synchronization
                .exec_block(Context::new(), rich_block, status_agent.clone())
                .await?;
        }

        // register consensus
        network_service.register_endpoint_handler(
            END_GOSSIP_SIGNED_PROPOSAL,
            ProposalMessageHandler::new(Arc::clone(&overlord_consensus)),
        )?;
        network_service.register_endpoint_handler(
            END_GOSSIP_AGGREGATED_VOTE,
            QCMessageHandler::new(Arc::clone(&overlord_consensus)),
        )?;
        network_service.register_endpoint_handler(
            END_GOSSIP_SIGNED_VOTE,
            VoteMessageHandler::new(Arc::clone(&overlord_consensus)),
        )?;
        network_service.register_endpoint_handler(
            END_GOSSIP_SIGNED_CHOKE,
            ChokeMessageHandler::new(Arc::clone(&overlord_consensus)),
        )?;
        network_service.register_endpoint_handler(
            BROADCAST_HEIGHT,
            RemoteHeightMessageHandler::new(Arc::clone(&synchronization)),
        )?;
        network_service.register_endpoint_handler(
            RPC_SYNC_PULL_BLOCK,
            PullBlockRpcHandler::new(
                Arc::new(network_service.handle()),
                Arc::clone(&storage),
            ),
        )?;

        network_service.register_endpoint_handler(
            RPC_SYNC_PULL_PROOF,
            PullProofRpcHandler::new(
                Arc::new(network_service.handle()),
                Arc::clone(&storage),
            ),
        )?;

        network_service.register_endpoint_handler(
            RPC_SYNC_PULL_TXS,
            PullTxsRpcHandler::new(
                Arc::new(network_service.handle()),
                Arc::clone(&storage),
            ),
        )?;
        network_service.register_rpc_response::<FixedBlock>(RPC_RESP_SYNC_PULL_BLOCK)?;
        network_service.register_rpc_response::<FixedProof>(RPC_RESP_SYNC_PULL_PROOF)?;
        network_service
            .register_rpc_response::<FixedSignedTxs>(RPC_RESP_SYNC_PULL_TXS)?;

        // Run network
        tokio::spawn(network_service);

        // Run sync
        tokio::spawn(async move {
            if let Err(e) = synchronization.polling_broadcast().await {
                log::error!("synchronization: {:?}", e);
            }
        });

        // Run consensus
        let authority_list = validators
            .iter()
            .map(|v| Node {
                address: v.pub_key.clone(),
                propose_weight: v.propose_weight,
                vote_weight: v.vote_weight,
            })
            .collect::<Vec<_>>();

        let timer_config = DurationConfig {
            propose_ratio: metadata.propose_ratio,
            prevote_ratio: metadata.prevote_ratio,
            precommit_ratio: metadata.precommit_ratio,
            brake_ratio: metadata.brake_ratio,
        };

        tokio::spawn(async move {
            if let Err(e) = overlord_consensus
                .run(
                    current_height,
                    consensus_interval,
                    authority_list,
                    Some(timer_config),
                )
                .await
            {
                log::error!("muta-consensus: {:?} error", e);
            }
        });

        let (abortable_demon, abort_handle) = future::abortable(exec_demon.run());
        let exec_handler = tokio::task::spawn_local(abortable_demon);

        // Init graphql
        let mut graphql_config = GraphQLConfig::default();
        graphql_config.listening_address = config.graphql.listening_address;
        graphql_config.graphql_uri = config.graphql.graphql_uri.clone();
        graphql_config.graphiql_uri = config.graphql.graphiql_uri.clone();
        if config.graphql.workers != 0 {
            graphql_config.workers = config.graphql.workers;
        }
        if config.graphql.maxconn != 0 {
            graphql_config.maxconn = config.graphql.maxconn;
        }
        if config.graphql.max_payload_size != 0 {
            graphql_config.max_payload_size = config.graphql.max_payload_size;
        }
        if let Some(tls) = config.graphql.tls {
            graphql_config.tls = Some(GraphQLTLS {
                private_key_file_path: tls.private_key_file_path,
                certificate_chain_file_path: tls.certificate_chain_file_path,
            })
        }
        graphql_config.enable_dump_profile =
            config.graphql.enable_dump_profile.unwrap_or(false);

        tokio::task::spawn_local(async move {
            let local = tokio::task::LocalSet::new();
            let actix_rt = actix_rt::System::run_in_tokio("muta-graphql", &local);
            tokio::task::spawn_local(actix_rt);

            core_api::start_graphql(graphql_config, api_adapter).await;
        });

        let ctrl_c_handler = tokio::task::spawn_local(async {
            #[cfg(windows)]
            let _ = tokio::signal::ctrl_c().await;
            #[cfg(unix)]
            {
                let mut sigtun_int =
                    os_impl::signal(os_impl::SignalKind::interrupt()).unwrap();
                let mut sigtun_term =
                    os_impl::signal(os_impl::SignalKind::terminate()).unwrap();
                tokio::select! {
                    _ = sigtun_int.recv() => {}
                    _ = sigtun_term.recv() => {}
                };
            }
        });

        // register channel of panic
        let (panic_sender, mut panic_receiver) = tokio::sync::mpsc::channel::<()>(1);

        panic::set_hook(Box::new(move |info: &panic::PanicInfo| {
            let mut panic_sender = panic_sender.clone();
            Self::panic_log(info);
            panic_sender.try_send(()).expect("panic_receiver is droped");
        }));

        tokio::select! {
            _ = exec_handler =>{log::error!("exec_daemon is down, quit.")},
            _ = ctrl_c_handler =>{log::info!("ctrl + c is pressed, quit.")},
            _ = panic_receiver.next() =>{log::info!("child thraed panic, quit.")},
        };
        abort_handle.abort();
        Ok(())
    }

    fn panic_log(info: &panic::PanicInfo) {
        let backtrace = Backtrace::new();
        let thread = thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let location = info.location().unwrap(); // The current implementation always returns Some
        let msg = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &*s,
                None => "Box<Any>",
            },
        };
        log::error!(
            target: "panic", "thread '{}' panicked at '{}': {}:{} {:?}",
            name,
            msg,
            location.file(),
            location.line(),
            backtrace,
        );
    }
}

#[derive(Debug, Display, From)]
pub enum MainError {
    #[display(fmt = "The muta configuration read failed {:?}", _0)]
    ConfigParse(common_config_parser::ParseError),

    #[display(fmt = "{:?}", _0)]
    Io(std::io::Error),

    #[display(fmt = "Toml fails to parse genesis {:?}", _0)]
    GenesisTomlDe(toml::de::Error),

    #[display(fmt = "hex error {:?}", _0)]
    FromHex(hex::FromHexError),

    #[display(fmt = "crypto error {:?}", _0)]
    Crypto(common_crypto::Error),

    #[display(fmt = "{:?}", _0)]
    Utf8(std::str::Utf8Error),

    #[display(fmt = "{:?}", _0)]
    JSONParse(serde_json::error::Error),

    #[display(fmt = "other error {:?}", _0)]
    Other(String),
}

impl std::error::Error for MainError {}

impl From<MainError> for ProtocolError {
    fn from(error: MainError) -> ProtocolError {
        ProtocolError::new(ProtocolErrorKind::Main, Box::new(error))
    }
}
