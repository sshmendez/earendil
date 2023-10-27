use std::collections::BTreeMap;

use async_trait::async_trait;
use earendil_crypt::Fingerprint;
use earendil_packet::Message;
use nanorpc::RpcTransport;
use sosistab2::ObfsUdpSecret;

use crate::{
    config::{InRouteConfig, OutRouteConfig},
    control_protocol::{
        ControlProtocol, SendGlobalRpcArgs, SendGlobalRpcError, SendMessageArgs, SendMessageError,
    },
    daemon::DaemonContext,
};

use super::global_rpc_protocol::GlobalRpcTransport;

pub struct ControlProtocolImpl {
    ctx: DaemonContext,
}

impl ControlProtocolImpl {
    pub fn new(ctx: DaemonContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ControlProtocol for ControlProtocolImpl {
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

    async fn send_message(&self, args: SendMessageArgs) -> Result<(), SendMessageError> {
        self.ctx.send_message(args).await
    }

    async fn recv_message(&self) -> Option<(Message, Fingerprint)> {
        self.ctx.debug_queue.pop().ok()
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

    async fn send_global_rpc(
        &self,
        send_args: SendGlobalRpcArgs,
    ) -> Result<serde_json::Value, SendGlobalRpcError> {
        let client = GlobalRpcTransport::new(self.ctx.clone(), send_args.destination);
        let params: Vec<serde_json::Value> = send_args
            .args
            .iter()
            .map(|arg| serde_json::from_str(arg).unwrap())
            .collect();
        let res = if let Some(res) = client
            .call(&send_args.method, &params)
            .await
            .map_err(|_| SendGlobalRpcError::SendError)?
        {
            res.map_err(|_| SendGlobalRpcError::SendError)?
        } else {
            return Err(SendGlobalRpcError::SendError);
        };

        Ok(res)
    }
}