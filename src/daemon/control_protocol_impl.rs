use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use earendil_crypt::{Fingerprint, IdentitySecret};
use earendil_packet::{Dock, Message};
use moka::sync::Cache;
use nanorpc::RpcTransport;
use parking_lot::Mutex;
use smol_timeout::TimeoutExt;
use sosistab2::ObfsUdpSecret;

use crate::{
    config::{InRouteConfig, OutRouteConfig},
    control_protocol::{
        ControlProtocol, DhtError, GlobalRpcArgs, GlobalRpcError, SendMessageArgs, SendMessageError,
    },
    daemon::{
        global_rpc::GlobalRpcClient, haven::RegisterHavenReq, haven_socket::HavenSocket,
        DaemonContext,
    },
};

use super::{global_rpc::transport::GlobalRpcTransport, haven::HavenLocator, n2r_socket::Endpoint};

pub struct ControlProtocolImpl {
    anon_identities: Arc<Mutex<AnonIdentities>>,
    ctx: DaemonContext,
}

impl ControlProtocolImpl {
    pub fn new(ctx: DaemonContext) -> Self {
        Self {
            ctx,
            anon_identities: Arc::new(Mutex::new(AnonIdentities::new())),
        }
    }
}

#[async_trait]
impl ControlProtocol for ControlProtocolImpl {
    async fn send_haven_message(
        &self,
        message: Bytes,
        identity_sk: Option<IdentitySecret>,
        endpoint: Endpoint,
    ) -> Result<(), SendMessageError> {
        let haven_socket = HavenSocket::bind(self.ctx.clone(), identity_sk, None, None);
        let _ = haven_socket.send_to(message, endpoint).await;
        Ok(())
    }

    async fn recv_haven_message(
        &self,
        identity_sk: Option<IdentitySecret>,
        dock: Option<Dock>,
        rendezvous_point: Fingerprint,
    ) -> (Bytes, Endpoint) {
        let haven_socket =
            HavenSocket::bind(self.ctx.clone(), identity_sk, dock, Some(rendezvous_point));
        haven_socket.recv_from().await.unwrap()
    }

    async fn register_haven(&self, identity_sk: IdentitySecret, rendezvous_fp: Fingerprint) {
        let req = RegisterHavenReq::new(identity_sk);

        let gclient = GlobalRpcClient(GlobalRpcTransport::new(self.ctx.clone(), rendezvous_fp));
        gclient
            .alloc_forward(req)
            .timeout(Duration::from_secs(30))
            .await;
    }

    async fn send_message(&self, args: SendMessageArgs) -> Result<(), SendMessageError> {
        let id = args.id.map(|id| self.anon_identities.lock().get(&id));
        self.ctx
            .send_message(
                id,
                args.source_dock,
                args.destination,
                args.dest_dock,
                args.content,
            )
            .await
    }

    async fn recv_message(&self) -> Option<(Message, Fingerprint)> {
        self.ctx.unhandled_incoming.pop().ok()
    }

    async fn my_routes(&self) -> serde_json::Value {
        let lala: BTreeMap<String, OutRouteConfig> = self
            .ctx
            .config
            .in_routes
            .iter()
            .map(|(k, v)| match v {
                InRouteConfig::Obfsudp { listen, secret } => {
                    let secret =
                        ObfsUdpSecret::from_bytes(*blake3::hash(secret.as_bytes()).as_bytes());
                    (
                        k.clone(),
                        OutRouteConfig::Obfsudp {
                            fingerprint: self.ctx.identity.public().fingerprint(),
                            connect: *listen,
                            cookie: *secret.to_public().as_bytes(),
                        },
                    )
                }
            })
            .collect();
        serde_json::to_value(lala).unwrap()
    }

    async fn graph_dump(&self) -> String {
        let mut out = String::new();
        out.push_str("graph G {\n");
        for adj in self.ctx.relay_graph.read().all_adjacencies() {
            out.push_str(&format!(
                "{:?} -- {:?}\n",
                adj.left.to_string(),
                adj.right.to_string()
            ));
        }
        out.push_str("}\n");
        out
    }

    async fn send_global_rpc(
        &self,
        send_args: GlobalRpcArgs,
    ) -> Result<serde_json::Value, GlobalRpcError> {
        let client = GlobalRpcTransport::new(self.ctx.clone(), send_args.destination);
        let res = if let Some(res) = client
            .call(&send_args.method, &send_args.args)
            .await
            .map_err(|_| GlobalRpcError::SendError)?
        {
            res.map_err(|_| GlobalRpcError::SendError)?
        } else {
            return Err(GlobalRpcError::SendError);
        };

        Ok(res)
    }

    async fn insert_rendezvous(&self, locator: HavenLocator) -> Result<(), DhtError> {
        self.ctx.dht_insert(locator).await;
        Ok(())
    }

    async fn get_rendezvous(
        &self,
        fingerprint: Fingerprint,
    ) -> Result<Option<HavenLocator>, DhtError> {
        self.ctx.dht_get(fingerprint).await
    }
}

struct AnonIdentities {
    map: Cache<String, IdentitySecret>,
}

impl AnonIdentities {
    pub fn new() -> Self {
        let map = Cache::builder()
            .max_capacity(100_000)
            .time_to_idle(Duration::from_secs(3600))
            .build();
        Self { map }
    }

    pub fn get(&mut self, id: &str) -> IdentitySecret {
        self.map.get_with_by_ref(id, IdentitySecret::generate)
    }
}
