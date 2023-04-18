// Copyright 2022, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The entirety of a cluster node as a struct
use crate::{
    channel::{bounded, Sender},
    errors::Result,
    qsize,
    raft::{
        api::{self, ServerState},
        network::{raft, Raft as TarPCRaftService},
        store::{NodesRequest, Store, TremorRequest},
        Cluster, ClusterError, ClusterResult, Network, NodeId,
    },
    system::{Runtime, ShutdownMode, WorldConfig},
};
use futures::{future, prelude::*};
use openraft::{Config, Raft};
use std::{
    collections::BTreeMap,
    net::ToSocketAddrs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tarpc::{
    server::{self, Channel},
    tokio_serde::formats::Json,
};

use tokio::task::{self, JoinHandle};

use super::TremorRaftImpl;

#[derive(Clone, Debug)]
pub struct ClusterNodeKillSwitch {
    sender: Sender<ShutdownMode>,
}

impl ClusterNodeKillSwitch {
    /// Stop the running node with the given `mode`
    /// # Errors
    /// if the node is already stopped or failed to be stopped
    pub fn stop(&self, mode: ShutdownMode) -> ClusterResult<()> {
        self.sender
            .try_send(mode)
            .map_err(|_| ClusterError::from("Error stopping cluster node"))
    }
}

pub struct Running {
    node: Node,
    server_state: Arc<ServerState>,
    kill_switch_tx: Sender<ShutdownMode>,
    run_handle: JoinHandle<ClusterResult<()>>,
}

impl Running {
    #[must_use]
    pub fn node_data(&self) -> (crate::raft::NodeId, Addr) {
        (self.server_state.id(), self.server_state.addr().clone())
    }

    #[must_use]
    pub fn node(&self) -> &Node {
        &self.node
    }

    async fn start(
        node: Node,
        raft: TremorRaftImpl,
        api_worker_handle: JoinHandle<()>,
        server_state: Arc<ServerState>,
        runtime: Runtime,
        runtime_handle: JoinHandle<Result<()>>,
    ) -> ClusterResult<Self> {
        let node_id = server_state.id();
        let (kill_switch_tx, mut kill_switch_rx) = bounded(1);

        let tcp_server_state = Arc::new(raft.clone());
        let mut listener =
            tarpc::serde_transport::tcp::listen(&server_state.addr().rpc(), Json::default).await?;
        listener.config_mut().max_frame_length(usize::MAX);

        let http_api_addr = server_state.addr().api().to_string();
        let app = api::endpoints().with_state(server_state.clone());
        let http_api_server =
            axum::Server::bind(&http_api_addr.to_socket_addrs()?.next().ok_or("badaddr")?)
                .serve(app.into_make_service());

        let run_handle = task::spawn(async move {
            let mut tcp_future = Box::pin(
                listener
                    // Ignore accept errors.
                    .filter_map(|r| future::ready(r.ok()))
                    .map(server::BaseChannel::with_defaults)
                    // Limit channels to 1 per IP.
                    // TODO .max_channels_per_key(1, |t| t.transport().peer_addr().unwrap().ip())
                    // serve is generated by the service attribute. It takes as input any type implementing
                    // the generated World trait.
                    .map(|channel| {
                        let server = raft::Server::new(tcp_server_state.clone());
                        channel.execute(server.serve())
                    })
                    // Max 10 channels.
                    .buffer_unordered(10)
                    .for_each(|_| async {})
                    .fuse(),
            );
            let mut http_future = Box::pin(http_api_server.fuse());
            let mut runtime_future = Box::pin(runtime_handle.fuse());
            let mut kill_switch_future = Box::pin(kill_switch_rx.recv().fuse());
            futures::select! {
                _ = tcp_future => {
                    warn!("[Node {node_id}] TCP cluster API shutdown.");
                    // Important: this will free and drop the store and thus the rocksdb
                    api_worker_handle.abort();
                    raft.shutdown().await.map_err(|_| ClusterError::from("Error shutting down local raft node"))?;
                    runtime.stop(ShutdownMode::Graceful).await?;
                    runtime_future.await??;
                }
                http_res = http_future => {
                    if let Err(e) = http_res {
                        error!("[Node {node_id}] HTTP cluster API failed: {e}");
                    }
                    // Important: this will free and drop the store and thus the rocksdb
                    api_worker_handle.abort();
                    raft.shutdown().await.map_err(|_| ClusterError::from("Error shutting down local raft node"))?;
                    runtime.stop(ShutdownMode::Graceful).await?;
                    runtime_future.await??;

                }
                runtime_res = runtime_future => {
                    if let Err(e) = runtime_res {
                        error!("[Node {node_id}] Local runtime failed: {e}");
                    }
                    // Important: this will free and drop the store and thus the rocksdb
                    api_worker_handle.abort();
                    // runtime is already down, we only need to stop local raft
                    raft.shutdown().await.map_err(|_| ClusterError::from("Error shutting down local raft node"))?;
                }
                shutdown_mode = kill_switch_future => {
                    let shutdown_mode = shutdown_mode.unwrap_or(ShutdownMode::Forceful);
                    info!("[Node {node_id}] Node stopping in {shutdown_mode:?} mode");
                    // Important: this will free and drop the store and thus the rocksdb
                    api_worker_handle.abort();
                    // tcp and http api stopped listening as we don't poll them no more
                    raft.shutdown().await.map_err(|_| ClusterError::from("Error shutting down local raft node"))?;
                    info!("[Node {node_id}] Raft engine did stop.");
                    info!("[Node {node_id}] Stopping the Tremor runtime...");
                    runtime.stop(shutdown_mode).await?;
                    runtime_future.await??;
                    info!("[Node {node_id}] Tremor runtime stopped.");
                }
            }
            info!("[Node {node_id}] Tremor cluster node stopped");

            Ok::<(), ClusterError>(())
        });

        Ok(Self {
            node,
            server_state,
            kill_switch_tx,
            run_handle,
        })
    }

    /// get a kill-switch
    #[must_use]
    pub fn kill_switch(&self) -> ClusterNodeKillSwitch {
        ClusterNodeKillSwitch {
            sender: self.kill_switch_tx.clone(),
        }
    }

    /// block until the cluster node is done noodling
    ///
    /// # Errors
    /// if the node failed to run
    pub async fn join(self) -> ClusterResult<()> {
        self.run_handle.await?
    }
}

/// internal struct carrying all data to start a cluster node
/// and keeps all the state for an ordered clean shutdown
#[derive(Clone, Debug)]
pub struct Node {
    db_dir: PathBuf,
    raft_config: Arc<Config>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Hash, Eq, PartialEq)]
pub struct Addr {
    /// Address for API access (from outside of the cluster)
    api: String,
    /// Address for RPC access (inter-node)
    rpc: String,
}

impl Default for Addr {
    fn default() -> Self {
        Self {
            api: String::from("127.0.0.1:8888"),
            rpc: String::from("127.0.0.1:9999"),
        }
    }
}

impl std::fmt::Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Addr")
            .field("api", &self.api)
            .field("rpc", &self.rpc)
            .finish()
    }
}

impl Addr {
    /// constructor
    pub fn new(api: impl Into<String>, rpc: impl Into<String>) -> Self {
        Self {
            api: api.into(),
            rpc: rpc.into(),
        }
    }

    /// get the api addr
    #[must_use]
    pub fn api(&self) -> &str {
        &self.api
    }

    /// get the rpc addr
    #[must_use]
    pub fn rpc(&self) -> &str {
        &self.rpc
    }
}

impl Node {
    pub fn new(db_dir: impl AsRef<Path>, raft_config: Config) -> Self {
        Self {
            db_dir: PathBuf::from(db_dir.as_ref()),
            raft_config: Arc::new(raft_config),
        }
    }
    /// Load the latest state from `db_dir`
    /// and start the cluster with it
    ///
    /// # Errors
    /// if the store does not exist, is not properly initialized
    pub async fn load_from_store(
        db_dir: impl AsRef<Path>,
        raft_config: Config,
    ) -> ClusterResult<Running> {
        let db = Store::init_db(&db_dir)?;
        // ensure we have working node data
        let (node_id, addr) = Store::get_self(&db)?;

        let world_config = WorldConfig::default(); // TODO: make configurable
        let (runtime, runtime_handle) = Runtime::start(world_config).await?;
        let (store_tx, store_rx) = bounded(qsize());

        let store: Store = Store::load(Arc::new(db), runtime.clone()).await?;
        let node = Self::new(db_dir, raft_config.clone());

        let network = Network::new();
        let raft = Raft::new(node_id, node.raft_config.clone(), network, store.clone()).await?;
        let manager = Cluster::new(node_id, store_tx.clone(), raft.clone());
        *(runtime
            .cluster_manager
            .write()
            .map_err(|_| "Failed to set world manager")?) = Some(manager);
        let (api_worker_handle, server_state) = api::initialize(
            node_id,
            addr,
            raft.clone(),
            store.clone(),
            store_tx,
            store_rx,
        );
        Running::start(
            node,
            raft,
            api_worker_handle,
            server_state,
            runtime,
            runtime_handle,
        )
        .await
    }

    /// Just start the cluster node and let it do whatever it does
    /// # Errors
    /// when the node can't be started
    pub async fn try_join(
        &mut self,
        addr: Addr,
        endpoints: Vec<String>,
        promote_to_voter: bool,
    ) -> ClusterResult<Running> {
        if endpoints.is_empty() {
            return Err(ClusterError::Other(
                "No join endpoints provided".to_string(),
            ));
        }

        // for now we infinitely try to join until it succeeds
        let mut join_wait = Duration::from_secs(2);
        let (client, node_id) = 'outer: loop {
            for endpoint in &endpoints {
                info!("Trying to join existing cluster via {endpoint}...");
                let client = api::client::Tremor::new(endpoint)?;
                // TODO: leader will start replication stream to this node and fail, until we start our machinery
                let node_id = match client.add_node(&addr).await {
                    Ok(node_id) => node_id,
                    Err(e) => {
                        // TODO: ensure we don't error here, when we are already learner
                        warn!("Error connecting to {endpoint}: {e}");
                        continue;
                    }
                };
                break 'outer (client, node_id);
            }
            // exponential backoff
            join_wait *= 2;
            info!(
                "Waiting for {}s before retrying to join...",
                join_wait.as_secs()
            );
            tokio::time::sleep(join_wait).await;
        };

        let world_config = WorldConfig::default(); // TODO: make configurable
        let (runtime, runtime_handle) = Runtime::start(world_config).await?;
        let (store_tx, store_rx) = bounded(qsize());
        let store = Store::bootstrap(node_id, &addr, &self.db_dir, runtime.clone()).await?;
        let network = Network::new();
        let raft = Raft::new(node_id, self.raft_config.clone(), network, store.clone()).await?;
        let manager = Cluster::new(node_id, store_tx.clone(), raft.clone());
        *(runtime
            .cluster_manager
            .write()
            .map_err(|_| "Failed to set world manager")?) = Some(manager);
        let (api_worker_handle, server_state) =
            api::initialize(node_id, addr, raft.clone(), store, store_tx, store_rx);
        let running = Running::start(
            self.clone(),
            raft,
            api_worker_handle,
            server_state,
            runtime,
            runtime_handle,
        )
        .await?;

        // only when the node is started (listens for HTTP, TCP etc)
        // we can add it as learner and optionally promote it to a voter

        info!("Adding Node {node_id} as Learner...");
        let res = client.add_learner(&node_id).await?;
        info!("Node {node_id} successully added as learner");
        if let Some(log_id) = res {
            info!("Learner {node_id} has applied the log up to {log_id}.");
        }

        if promote_to_voter {
            info!("Promoting Node {node_id} to Voter...");
            client.promote_voter(&node_id).await?;
            // FIXME: wait for the node to be a voter
            info!("Node {node_id} became Voter.");
        }
        Ok(running)
    }

    /// Bootstrap and start this cluster node as a single node cluster
    /// of which it immediately becomes the leader.
    /// # Errors
    /// if bootstrapping a a leader fails
    pub async fn bootstrap_as_single_node_cluster(&mut self, addr: Addr) -> ClusterResult<Running> {
        let node_id = crate::raft::NodeId::default();
        let world_config = WorldConfig::default(); // TODO: make configurable
        let (runtime, runtime_handle) = Runtime::start(world_config).await?;
        let (store_tx, store_rx) = bounded(qsize());

        let store = Store::bootstrap(node_id, &addr, &self.db_dir, runtime.clone()).await?;
        let network = Network::new();

        let raft = Raft::new(node_id, self.raft_config.clone(), network, store.clone()).await?;
        let manager = Cluster::new(node_id, store_tx.clone(), raft.clone());
        *(runtime
            .cluster_manager
            .write()
            .map_err(|_| "Failed to set world manager")?) = Some(manager);
        let mut nodes = BTreeMap::new();
        nodes.insert(node_id, addr.clone());
        // this is the crucial bootstrapping step
        raft.initialize(nodes).await?;
        raft.wait(None)
            .state(
                openraft::ServerState::Leader,
                "waiting for bootstrap node to become leader",
            )
            .await?;
        // lets make ourselves known to the cluster state as first operation, so new joiners will see us as well
        // this is critical
        match raft
            .client_write(TremorRequest::Nodes(NodesRequest::AddNode {
                addr: addr.clone(),
            }))
            .await
        {
            Ok(r) => {
                let assigned_node_id = NodeId::try_from(r.data)?;
                debug_assert_eq!(node_id, assigned_node_id, "Adding initial leader resulted in a differing node_id: {assigned_node_id}, expected: {node_id}");
                let (worker_handle, server_state) =
                    api::initialize(node_id, addr, raft.clone(), store, store_tx, store_rx);

                Running::start(
                    self.clone(),
                    raft,
                    worker_handle,
                    server_state,
                    runtime,
                    runtime_handle,
                )
                .await
            }
            Err(e) => Err(ClusterError::Other(format!(
                "Error adding myself to the bootstrapped cluster: {e}"
            ))),
        }
    }
}
