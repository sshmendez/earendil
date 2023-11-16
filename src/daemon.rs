pub mod context;
mod control_protocol_impl;
pub mod global_rpc;
mod gossip;
mod inout_route;
mod link_connection;
mod link_protocol;
mod neightable;
mod reply_block_store;
mod udp_forward;

use anyhow::Context;
use bytes::Bytes;
use clone_macro::clone;
use earendil_crypt::Fingerprint;
use earendil_packet::ForwardInstruction;
use earendil_packet::{InnerPacket, PeeledPacket};
use earendil_topology::RelayGraph;
use futures_util::{stream::FuturesUnordered, StreamExt, TryFutureExt};
use moka::sync::Cache;
use nanorpc::{JrpcRequest, RpcService};
use nanorpc_http::server::HttpRpcServer;
use parking_lot::{Mutex, RwLock};
use smol::Task;
use smolscale::immortal::{Immortal, RespawnStrategy};
use smolscale::reaper::TaskReaper;
use stdcode::StdcodeSerializeExt;

use std::time::Instant;
use std::{sync::Arc, time::Duration};

use crate::config::ConfigFile;
use crate::control_protocol::SendMessageError;
use crate::daemon::context::DaemonContext;
use crate::daemon::udp_forward::udp_forward_loop;
use crate::havens::haven::{haven_loop, HAVEN_FORWARD_DOCK};
use crate::sockets::n2r_socket::N2rSocket;
use crate::sockets::socket::Endpoint;
use crate::{
    config::{InRouteConfig, OutRouteConfig},
    control_protocol::ControlService,
    daemon::{
        gossip::gossip_loop,
        inout_route::{in_route_obfsudp, out_route_obfsudp, InRouteContext, OutRouteContext},
    },
};

pub use self::control_protocol_impl::ControlProtErr;
use self::global_rpc::{GlobalRpcService, GLOBAL_RPC_DOCK};
use self::{control_protocol_impl::ControlProtocolImpl, global_rpc::server::GlobalRpcImpl};

pub struct Daemon {
    pub ctx: DaemonContext,
    _task: Task<anyhow::Result<()>>,
}

impl Daemon {
    /// Initializes the daemon and starts all background loops
    pub fn init(config: ConfigFile) -> anyhow::Result<Daemon> {
        let ctx = DaemonContext::new(config)?;
        let context = ctx.clone();
        let task = smolscale::spawn(async move { main_daemon(context) });
        Ok(Self { ctx, _task: task })
    }
}

fn log_error<E>(label: &str) -> impl FnOnce(E) + '_
where
    E: std::fmt::Debug,
{
    move |s| log::warn!("{label} restart, error: {:?}", s)
}

pub fn main_daemon(ctx: DaemonContext) -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("earendil=trace"))
        .init();
    log::info!(
        "daemon starting with fingerprint {}",
        ctx.identity.public().fingerprint()
    );

    let table = ctx.table.clone();
    smolscale::block_on(async move {
        // Run the loops
        let _table_gc = Immortal::spawn(clone!([table], async move {
            loop {
                smol::Timer::after(Duration::from_secs(60)).await;
                table.garbage_collect();
            }
        }));

        let _peel_forward = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([ctx], move || peel_forward_loop(ctx.clone())
                .map_err(log_error("peel_forward"))),
        );

        let _gossip = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([ctx], move || gossip_loop(ctx.clone())
                .map_err(log_error("gossip"))),
        );

        let _control_protocol = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([ctx], move || control_protocol_loop(ctx.clone())
                .map_err(log_error("control_protocol"))),
        );

        let _global_rpc_loop = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([ctx], move || global_rpc_loop(ctx.clone())
                .map_err(log_error("global_rpc_loop"))),
        );

        let _rendezvous_forward_loop = Immortal::respawn(
            RespawnStrategy::Immediate,
            clone!([ctx], move || rendezvous_forward_loop(ctx.clone())
                .map_err(log_error("haven_forward_loop"))),
        );

        let _haven_loops: Vec<Immortal> = ctx
            .config
            .havens
            .clone()
            .into_iter()
            .map(|cfg| {
                Immortal::respawn(
                    RespawnStrategy::Immediate,
                    clone!([ctx], move || haven_loop(ctx.clone(), cfg.clone())
                        .map_err(log_error("udp_haven_forward_loop"))),
                )
            })
            .collect();

        // app-level traffic tasks/processes
        let _udp_forward_loops: Vec<Immortal> = ctx
            .config
            .udp_forwards
            .clone()
            .into_iter()
            .map(|udp_fwd_cfg| {
                Immortal::respawn(
                    RespawnStrategy::Immediate,
                    clone!([ctx], move || udp_forward_loop(
                        ctx.clone(),
                        udp_fwd_cfg.clone()
                    )
                    .map_err(log_error("udp_forward_loop"))),
                )
            })
            .collect();

        let mut route_tasks = FuturesUnordered::new();

        // For every in_routes block, spawn a task to handle incoming stuff
        for (in_route_name, config) in ctx.config.in_routes.iter() {
            let context = InRouteContext {
                in_route_name: in_route_name.clone(),
                daemon_ctx: ctx.clone(),
            };
            match config.clone() {
                InRouteConfig::Obfsudp { listen, secret } => {
                    route_tasks.push(smolscale::spawn(in_route_obfsudp(context, listen, secret)));
                }
            }
        }

        // For every out_routes block, spawn a task to handle outgoing stuff
        for (out_route_name, config) in ctx.config.out_routes.iter() {
            match config {
                OutRouteConfig::Obfsudp {
                    fingerprint,
                    connect,
                    cookie,
                } => {
                    let context = OutRouteContext {
                        out_route_name: out_route_name.clone(),
                        remote_fingerprint: *fingerprint,
                        daemon_ctx: ctx.clone(),
                    };
                    route_tasks.push(smolscale::spawn(out_route_obfsudp(
                        context, *connect, *cookie,
                    )));
                }
            }
        }

        // Join all the tasks. If any of the tasks terminate with an error, that's fatal!
        while let Some(next) = route_tasks.next().await {
            next?;
        }
        Ok(())
    })
}

/// Loop that handles the control protocol
async fn control_protocol_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    let http = HttpRpcServer::bind(ctx.config.control_listen).await?;
    let service = ControlService(ControlProtocolImpl::new(ctx));
    http.run(service).await?;
    Ok(())
}

/// Loop that takes incoming packets, peels them, and processes them
async fn peel_forward_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    fn process_inner_pkt(
        ctx: &DaemonContext,
        inner: InnerPacket,
        src_fp: Fingerprint,
        dest_fp: Fingerprint,
    ) -> anyhow::Result<()> {
        match inner {
            InnerPacket::Message(msg) => {
                // log::debug!("received InnerPacket::Message: {:?}", msg);
                let dest = Endpoint::new(dest_fp, msg.dest_dock);
                if let Some(send_incoming) = ctx.socket_recv_queues.get(&dest) {
                    send_incoming.try_send((msg, src_fp))?;
                } else {
                    anyhow::bail!("No socket listening on destination {dest}")
                }
            }
            InnerPacket::ReplyBlocks(reply_blocks) => {
                log::debug!("received a batch of ReplyBlocks");
                for reply_block in reply_blocks {
                    ctx.anon_destinations.lock().insert(src_fp, reply_block);
                }
            }
        }
        Ok(())
    }

    loop {
        let pkt = ctx.table.recv_raw_packet().await;
        let now = Instant::now();
        log::debug!("received raw packet");
        let peeled = pkt.peel(&ctx.onion_sk)?;
        log::debug!("peeled packet!");

        scopeguard::defer!(log::debug!(
            "PEEL AND PROCESS MESSAGE TOOK:::::::::: {:?}",
            now.elapsed()
        ));
        match peeled {
            PeeledPacket::Forward {
                to: next_hop,
                pkt: inner,
            } => {
                let conn = ctx
                    .table
                    .lookup(&next_hop)
                    .context("could not find this next hop")?;
                conn.send_raw_packet(inner).await;
            }
            PeeledPacket::Received {
                from: src_fp,
                pkt: inner,
            } => process_inner_pkt(&ctx, inner, src_fp, ctx.identity.public().fingerprint())?,
            PeeledPacket::GarbledReply { id, mut pkt } => {
                log::debug!("received garbled packet");
                let reply_degarbler = ctx
                    .degarblers
                    .get(&id)
                    .context("no degarbler for this garbled pkt")?;
                let (inner, src_fp) = reply_degarbler.degarble(&mut pkt)?;
                log::debug!("packet has been degarbled!");
                process_inner_pkt(
                    &ctx,
                    inner,
                    src_fp,
                    reply_degarbler.my_anon_isk().public().fingerprint(),
                )?
            }
        }
    }
}

/// Loop that listens to and handles incoming GlobalRpc requests
async fn global_rpc_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    let socket = Arc::new(N2rSocket::bind(ctx.clone(), None, Some(GLOBAL_RPC_DOCK)));
    let service = Arc::new(GlobalRpcService(GlobalRpcImpl::new(ctx)));
    let group: TaskReaper<anyhow::Result<()>> = TaskReaper::new();

    loop {
        let socket = socket.clone();
        if let Ok((req, endpoint)) = socket.recv_from().await {
            let service = service.clone();
            group.attach(smolscale::spawn(async move {
                let req: JrpcRequest = serde_json::from_str(&String::from_utf8(req.to_vec())?)?;
                let resp = service.respond_raw(req).await;
                socket
                    .send_to(
                        Bytes::from(serde_json::to_string(&resp)?.into_bytes()),
                        endpoint,
                    )
                    .await?;

                Ok(())
            }));
        }
    }
}

const DHT_REDUNDANCY: usize = 3;
/// Loop that listens to and handles incoming haven forwarding requests
async fn rendezvous_forward_loop(ctx: DaemonContext) -> anyhow::Result<()> {
    let seen_srcs: Cache<(Endpoint, Endpoint), ()> = Cache::builder()
        .max_capacity(100_000)
        .time_to_idle(Duration::from_secs(60 * 60))
        .build();
    let socket = Arc::new(N2rSocket::bind(ctx.clone(), None, Some(HAVEN_FORWARD_DOCK)));

    loop {
        if let Ok((msg, src_endpoint)) = socket.recv_from().await {
            let ctx = ctx.clone();
            let (inner, dest_ep): (Bytes, Endpoint) = stdcode::deserialize(&msg)?;
            log::debug!(
                "received forward msg {:?}, from {}, to {}",
                inner,
                src_endpoint,
                dest_ep
            );

            let is_valid_dest = ctx.registered_havens.contains_key(&dest_ep.fingerprint);
            let is_seen_src = seen_srcs.contains_key(&(dest_ep, src_endpoint));

            if is_valid_dest {
                seen_srcs.insert((src_endpoint, dest_ep), ());
            }
            if is_valid_dest || is_seen_src {
                let body: Bytes = (inner, src_endpoint).stdcode().into();
                socket.send_to(body, dest_ep).await?;
            } else {
                log::warn!("haven {} is not registered with me!", dest_ep.fingerprint);
            }
        };
    }
}

fn route_to_instructs(
    route: Vec<Fingerprint>,
    relay_graph: Arc<RwLock<RelayGraph>>,
) -> Result<Vec<ForwardInstruction>, SendMessageError> {
    route
        .windows(2)
        .map(|wind| {
            let this = wind[0];
            let next = wind[1];
            let this_pubkey = relay_graph
                .read()
                .identity(&this)
                .ok_or(SendMessageError::NoOnionPublic(this))?
                .onion_pk;
            Ok(ForwardInstruction {
                this_pubkey,
                next_fingerprint: next,
            })
        })
        .collect()
}
