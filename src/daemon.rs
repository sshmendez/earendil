mod connection;
mod gossip;
mod inout_route;
mod n2n;
mod neightable;

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use async_trait::async_trait;
use clone_macro::clone;

use earendil_crypt::IdentitySecret;
use earendil_packet::{
    crypt::OnionSecret, ForwardInstruction, InnerPacket, PeeledPacket, RawPacket,
};
use earendil_topology::RelayGraph;
use futures_util::{stream::FuturesUnordered, StreamExt, TryFutureExt};
use nanorpc_http::server::HttpRpcServer;
use parking_lot::RwLock;
use smolscale::immortal::{Immortal, RespawnStrategy};

use crate::{
    config::{ConfigFile, InRouteConfig, OutRouteConfig},
    control_protocol::{ControlProtocol, ControlService, SendMessageArgs, SendMessageError},
    daemon::{
        gossip::gossip_loop,
        inout_route::{in_route_obfsudp, out_route_obfsudp, InRouteContext, OutRouteContext},
        neightable::NeighTable,
    },
};

fn label_error<E>(label: &str) -> impl FnOnce(E) + '_
where
    E: std::fmt::Debug,
{
    move |s| log::warn!("{label} restart, error: {:?}", s)
}

pub fn main_daemon(config: ConfigFile) -> anyhow::Result<()> {
    fn read_identity(path: &Path) -> anyhow::Result<IdentitySecret> {
        Ok(stdcode::deserialize(&hex::decode(std::fs::read(path)?)?)?)
    }

    fn write_identity(path: &Path, identity: &IdentitySecret) -> anyhow::Result<()> {
        let encoded_identity = hex::encode(stdcode::serialize(&identity)?);
        std::fs::write(path, encoded_identity)?;
        Ok(())
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("earendil=debug"))
        .init();
    let identity = loop {
        match read_identity(&config.identity) {
            Ok(id) => break id,
            Err(err) => {
                log::warn!(
                    "(re)writing identity file at {:?} due to error reading: {:?}",
                    config.identity,
                    err
                );
                let new_id = IdentitySecret::generate();
                write_identity(&config.identity, &new_id)?;
            }
        }
    };
    log::info!(
        "daemon starting with fingerprint {}",
        identity.public().fingerprint()
    );

    smolscale::block_on(async move {
        let mut subtasks = FuturesUnordered::new();
        let table = Arc::new(NeighTable::new());

        let daemon_ctx = DaemonContext {
            config: Arc::new(config),
            table: table.clone(),
            identity: identity.into(),
            onion_sk: OnionSecret::generate(),
            relay_graph: Arc::new(RwLock::new(RelayGraph::new())),
        };

        // Run the loops
        subtasks.push({
            let table = table.clone();
            smolscale::spawn(async move {
                loop {
                    smol::Timer::after(Duration::from_secs(60)).await;
                    table.garbage_collect();
                }
            })
        });

        let _peel_forward = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([daemon_ctx], move || peel_forward_loop(daemon_ctx.clone())
                .map_err(label_error("peel_forward"))),
        );
        let _gossip = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([daemon_ctx], move || gossip_loop(daemon_ctx.clone())
                .map_err(label_error("gossip"))),
        );
        let _control_protocol = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([daemon_ctx], move || control_protocol_loop(
                daemon_ctx.clone()
            )
            .map_err(label_error("control_protocol"))),
        );

        // For every in_routes block, spawn a task to handle incoming stuff
        for (in_route_name, config) in daemon_ctx.config.in_routes.iter() {
            let context = InRouteContext {
                in_route_name: in_route_name.clone(),
                daemon_ctx: daemon_ctx.clone(),
            };
            match config.clone() {
                InRouteConfig::Obfsudp { listen, secret } => {
                    subtasks.push(smolscale::spawn(in_route_obfsudp(context, listen, secret)));
                }
            }
        }

        // For every out_routes block, spawn a task to handle outgoing stuff
        for (out_route_name, config) in daemon_ctx.config.out_routes.iter() {
            match config {
                OutRouteConfig::Obfsudp {
                    fingerprint,
                    connect,
                    cookie,
                } => {
                    let context = OutRouteContext {
                        out_route_name: out_route_name.clone(),
                        remote_fingerprint: *fingerprint,
                        daemon_ctx: daemon_ctx.clone(),
                    };
                    subtasks.push(smolscale::spawn(out_route_obfsudp(
                        context, *connect, *cookie,
                    )));
                }
            }
        }

        while let Some(next) = subtasks.next().await {
            next?;
        }
        Ok(())
    })
}

/// Loop that handles the control protocol
async fn control_protocol_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    let http = HttpRpcServer::bind(ctx.config.control_listen).await?;
    let service = ControlService(ControlProtocolImpl { ctx });
    http.run(service).await?;
    Ok(())
}

/// Loop that takes incoming packets, peels them, and processes them
async fn peel_forward_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    loop {
        let pkt = ctx.table.recv_raw_packet().await;
        let peeled = pkt.peel(&ctx.onion_sk)?;
        match peeled {
            PeeledPacket::Forward(next_hop, inner) => {
                let conn = ctx
                    .table
                    .lookup(&next_hop)
                    .context("could not find this next hop")?;
                conn.send_raw_packet(inner).await;
            }
            PeeledPacket::Receive(raw) => {
                let (inner, source) = InnerPacket::open(&raw, &ctx.onion_sk)
                    .context("failed to interpret raw inner packet")?;
                anyhow::bail!(
                    "incoming message {:?} from {source}, but handling is not yet implemented",
                    inner
                )
            }
        }
    }
}

#[derive(Clone)]
pub struct DaemonContext {
    config: Arc<ConfigFile>,
    table: Arc<NeighTable>,
    identity: Arc<IdentitySecret>,
    onion_sk: OnionSecret,
    relay_graph: Arc<RwLock<RelayGraph>>,
}

struct ControlProtocolImpl {
    ctx: DaemonContext,
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
        let route = self
            .ctx
            .relay_graph
            .read()
            .find_shortest_path(&self.ctx.identity.public().fingerprint(), &args.destination)
            .ok_or(SendMessageError::NoRoute)?;
        let instructs: Result<Vec<_>, SendMessageError> = route
            .windows(2)
            .map(|wind| {
                let this = wind[0];
                let next = wind[1];
                let this_pubkey = self
                    .ctx
                    .relay_graph
                    .read()
                    .identity(&this)
                    .ok_or(SendMessageError::NoOnionPublic(this))?
                    .onion_pk;
                Ok(ForwardInstruction {
                    this_pubkey,
                    next_fingerprint: next,
                })
            })
            .collect();
        let instructs = instructs?;
        let their_opk = self
            .ctx
            .relay_graph
            .read()
            .identity(&args.destination)
            .ok_or(SendMessageError::NoOnionPublic(args.destination))?
            .onion_pk;
        let wrapped_onion = RawPacket::new(
            &instructs,
            &their_opk,
            &InnerPacket::Message(args.content)
                .seal(&self.ctx.identity, &their_opk)
                .ok()
                .ok_or(SendMessageError::MessageTooBig)?,
        )
        .ok()
        .ok_or(SendMessageError::TooFar)?;
        // we send the onion by treating it as a message addressed to ourselves
        self.ctx.table.inject_asif_incoming(wrapped_onion).await;
        Ok(())
    }
}
