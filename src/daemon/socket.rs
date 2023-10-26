use crate::{control_protocol::SendMessageArgs, daemon::DaemonContext};
use bytes::Bytes;
use earendil_crypt::Fingerprint;
use earendil_packet::{Dock, Message};
use smol::channel::Receiver;

pub struct Socket {
    id: Option<String>,
    dock: Dock,
    recv_incoming: Receiver<(Message, Fingerprint)>,
}

pub struct Endpoint {
    fingerprint: Fingerprint,
    dock: Dock,
}

impl Socket {
    fn bind(ctx: DaemonContext, id: Option<String>, dock: Dock) -> Socket {
        let (send_outgoing, recv_incoming) = smol::channel::bounded(1000);
        ctx.socket_recv_queues.insert(dock, send_outgoing);

        Socket {
            id,
            dock,
            recv_incoming,
        }
    }

    async fn send_to(
        &self,
        ctx: DaemonContext,
        body: Bytes,
        endpoint: Endpoint,
    ) -> anyhow::Result<()> {
        ctx.send_message(SendMessageArgs {
            id: self.id.clone(),
            source_dock: self.dock,
            dest_dock: endpoint.dock,
            destination: endpoint.fingerprint,
            content: body,
        })
        .await?;

        Ok(())
    }

    async fn recv_from(&self) -> anyhow::Result<(Message, Fingerprint)> {
        let message = self.recv_incoming.recv().await?;

        Ok(message)
    }
}
