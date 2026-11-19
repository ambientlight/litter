use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use codex_app_server_client::{
    AppServerClient, JsonRpcWire, RemoteAppServerClient, RemoteAppServerConnectArgs,
};
use codex_app_server_protocol::JSONRPCMessage;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
};

use crate::session::remote_transport::{Reconnected, RemoteTransport, SessionKeepalive};
use crate::transport::TransportError;

use super::{ConnectionTicket, DevTunnelError, DevTunnels, ResolvedTunnel, api};

const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_DEADLINE: Duration = Duration::from_secs(10);

struct CloseSignal(watch::Sender<bool>);

impl SessionKeepalive for CloseSignal {
    fn close(&self) {
        self.0.send_replace(true);
    }
}

impl Drop for CloseSignal {
    fn drop(&mut self) {
        self.close();
    }
}

/// A separate socket pump makes next_message cancellation-safe even when the
/// upstream dispatcher continuously selects between writes and notifications.
struct TunnelWire {
    outgoing: mpsc::Sender<JSONRPCMessage>,
    incoming: mpsc::Receiver<io::Result<JSONRPCMessage>>,
    close: Arc<CloseSignal>,
}

impl JsonRpcWire for TunnelWire {
    async fn send_message(&mut self, message: JSONRPCMessage, _label: &str) -> io::Result<()> {
        self.outgoing
            .send(message)
            .await
            .map_err(|_| io::Error::other("Dev Tunnel disconnected"))
    }

    async fn next_message(&mut self, _label: &str) -> io::Result<Option<JSONRPCMessage>> {
        self.incoming.recv().await.transpose()
    }

    async fn close(&mut self, _label: &str) -> io::Result<()> {
        self.close.close();
        Ok(())
    }
}

pub(crate) struct DevTunnelTransport {
    manager: Arc<DevTunnels>,
    ticket: ConnectionTicket,
    live: Mutex<Option<Arc<CloseSignal>>>,
}

impl DevTunnelTransport {
    pub fn new(manager: Arc<DevTunnels>, ticket: ConnectionTicket) -> Arc<Self> {
        Arc::new(Self {
            manager,
            ticket,
            live: Mutex::new(None),
        })
    }

    pub async fn connect(
        &self,
        args: &RemoteAppServerConnectArgs,
    ) -> Result<(Reconnected, super::DevTunnelProfile), DevTunnelError> {
        self.manager.wait_foreground(&self.ticket).await?;
        let mut stop = self.ticket.stop.subscribe();
        let connect = async {
            // Every reconnect re-resolves metadata and obtains a fresh scoped token.
            let resolved = self.manager.resolve(&self.ticket).await?;
            let (wire, close) = open_wire(&self.manager, &self.ticket, &resolved).await?;
            let client = tokio::time::timeout(
                Duration::from_secs(30),
                RemoteAppServerClient::connect_with_wire(
                    args.clone(),
                    "Microsoft Dev Tunnel".into(),
                    wire,
                ),
            )
            .await
            .map_err(|_| DevTunnelError::CodexHandshake)?
            .map_err(|_| DevTunnelError::CodexHandshake)?;
            self.manager.ensure_ticket(&self.ticket)?;
            *self.live.lock().unwrap_or_else(|e| e.into_inner()) = Some(close.clone());
            Ok((
                Reconnected {
                    client: AppServerClient::Remote(client),
                    keepalive: Some(close),
                },
                resolved.profile,
            ))
        };
        let result = tokio::select! {
            biased;
            _ = stop.changed() => Err(DevTunnelError::Cancelled),
            result = connect => result,
        };
        if let Err(error) = &result {
            self.manager.publish_error(&self.ticket, *error);
        }
        result
    }
}

#[async_trait]
impl RemoteTransport for DevTunnelTransport {
    async fn reconnect(
        &self,
        args: &RemoteAppServerConnectArgs,
        _url: &str,
    ) -> Result<Reconnected, TransportError> {
        self.connect(args)
            .await
            .map(|(connection, _)| connection)
            .map_err(TransportError::DevTunnel)
    }

    async fn notify_network_change(&self) {
        self.close_current_connection().await;
    }

    async fn close_current_connection(&self) {
        if let Some(close) = self.live.lock().unwrap_or_else(|e| e.into_inner()).take() {
            close.close();
        }
    }
}

async fn open_wire(
    manager: &Arc<DevTunnels>,
    ticket: &ConnectionTicket,
    resolved: &ResolvedTunnel,
) -> Result<(TunnelWire, Arc<CloseSignal>), DevTunnelError> {
    let endpoint = api::forwarding_url(
        resolved
            .profile
            .cached_endpoint
            .as_deref()
            .ok_or(DevTunnelError::EndpointUnavailable)?,
    )?;
    let mut request = endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| DevTunnelError::InvalidEndpoint)?;
    let mut tunnel_header = HeaderValue::from_str(&format!("tunnel {}", resolved.token))
        .map_err(|_| DevTunnelError::ProtocolFailure)?;
    tunnel_header.set_sensitive(true);
    request
        .headers_mut()
        .insert("X-Tunnel-Authorization", tunnel_header);
    request.headers_mut().insert(
        "X-Tunnel-Skip-AntiPhishing-Page",
        HeaderValue::from_static("true"),
    );
    if let Some(token) = &resolved.app_server_token {
        let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| DevTunnelError::InvalidProfile)?;
        bearer.set_sensitive(true);
        request.headers_mut().insert("Authorization", bearer);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(64 * 1024 * 1024))
        .max_frame_size(Some(64 * 1024 * 1024));
    // connect_async does not follow HTTP redirects. Never use a retry/redirect
    // helper here: both credentials are restricted to this validated host.
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(30),
        tokio_tungstenite::connect_async_with_config(request, Some(config), true),
    )
    .await
    .map_err(|_| DevTunnelError::Network)?
    .map_err(|error| match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => match response.status().as_u16() {
            401 | 403 => DevTunnelError::ForwardingAuthRejected,
            300..=399 => DevTunnelError::InvalidEndpoint,
            404 => DevTunnelError::EndpointUnavailable,
            _ => DevTunnelError::Network,
        },
        _ => DevTunnelError::Network,
    })?;
    manager.ensure_ticket(ticket)?;
    Ok(start_wire(
        socket,
        ticket.stop.subscribe(),
        manager.foreground.subscribe(),
        Duration::from_secs(resolved.expires_at.saturating_sub(api::now() + 30)),
        PING_INTERVAL,
        PONG_DEADLINE,
    ))
}

fn start_wire<S>(
    mut socket: tokio_tungstenite::WebSocketStream<S>,
    mut cancelled: watch::Receiver<bool>,
    mut foreground: watch::Receiver<bool>,
    expires_in: Duration,
    ping_interval: Duration,
    pong_deadline: Duration,
) -> (TunnelWire, Arc<CloseSignal>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (outgoing, mut outgoing_rx) = mpsc::channel::<JSONRPCMessage>(256);
    let (incoming_tx, incoming) = mpsc::channel(256);
    let (stop_tx, mut stop) = watch::channel(false);
    let close = Arc::new(CloseSignal(stop_tx));
    tokio::spawn(async move {
        if *stop.borrow() || *cancelled.borrow() {
            return;
        }
        let mut pings = tokio::time::interval_at(Instant::now() + ping_interval, ping_interval);
        pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let expiry = tokio::time::sleep(expires_in);
        tokio::pin!(expiry);
        let mut pong: Option<(Vec<u8>, Instant)> = None;
        let pump = async {
            loop {
                let pong_at = pong
                    .as_ref()
                    .map(|(_, time)| *time)
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(86400));
                tokio::select! {
                    _ = &mut expiry => break,
                    _ = foreground.changed() => {
                        // A suspended TCP socket may look alive. Force a new
                        // handshake when foregrounded rather than trusting it.
                        if *foreground.borrow_and_update() { break; }
                        pong = None;
                    },
                    _ = tokio::time::sleep_until(pong_at), if pong.is_some() => break,
                    _ = pings.tick(), if *foreground.borrow() && pong.is_none() => {
                        let payload = uuid::Uuid::new_v4().as_bytes().to_vec();
                        if !tokio::time::timeout(pong_deadline, socket.send(Message::Ping(payload.clone().into()))).await.is_ok_and(|r| r.is_ok()) { break; }
                        pong = Some((payload, Instant::now() + pong_deadline));
                    },
                    message = outgoing_rx.recv() => {
                        let Some(message) = message else { break; };
                        let Ok(json) = serde_json::to_string(&message) else { break; };
                        if !tokio::time::timeout(pong_deadline, socket.send(Message::Text(json.into()))).await.is_ok_and(|r| r.is_ok()) { break; }
                    },
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let message = serde_json::from_str::<JSONRPCMessage>(&text)
                                    .map_err(|_| io::Error::other("Invalid Dev Tunnel JSON-RPC response"));
                                let invalid = message.is_err();
                                if !tokio::time::timeout(pong_deadline, incoming_tx.send(message)).await.is_ok_and(|r| r.is_ok()) || invalid { break; }
                            },
                            Some(Ok(Message::Pong(payload))) => {
                                if pong.as_ref().is_some_and(|(expected, _)| expected.as_slice() == payload.as_ref()) { pong = None; }
                            },
                            Some(Ok(Message::Ping(payload))) => {
                                if !tokio::time::timeout(pong_deadline, socket.send(Message::Pong(payload))).await.is_ok_and(|r| r.is_ok()) { break; }
                            },
                            Some(Ok(Message::Frame(_))) => {},
                            _ => break,
                        }
                    },
                }
            }
        };
        // Cancellation must also interrupt blocked socket/channel writes.
        tokio::select! {
            biased;
            _ = stop.changed() => {},
            _ = cancelled.changed() => {},
            _ = pump => {},
        }
        // Do not wait indefinitely for a dead peer's close acknowledgement.
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
        // Dropping the receiver causes upstream's dispatcher to fail pending
        // requests; the session worker decides which reads can be replayed.
    });
    (
        TunnelWire {
            outgoing,
            incoming,
            close: close.clone(),
        },
        close,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::{WebSocketStream, tungstenite::protocol::Role};

    async fn pair(
        expires: Duration,
    ) -> (
        TunnelWire,
        WebSocketStream<tokio::io::DuplexStream>,
        watch::Sender<bool>,
        watch::Sender<bool>,
    ) {
        let (client, server) = tokio::io::duplex(128);
        let (client, server) = tokio::join!(
            WebSocketStream::from_raw_socket(client, Role::Client, None),
            WebSocketStream::from_raw_socket(server, Role::Server, None)
        );
        let (cancelled, receiver) = watch::channel(false);
        let (foreground, active) = watch::channel(true);
        let (wire, _) = start_wire(
            client,
            receiver,
            active,
            expires,
            Duration::from_millis(30),
            Duration::from_millis(80),
        );
        (wire, server, cancelled, foreground)
    }

    #[tokio::test]
    async fn pings_keep_idle_wire_alive_and_pongs_are_not_json_rpc() {
        let (mut wire, mut server, _cancelled, _foreground) = pair(Duration::from_secs(30)).await;
        for _ in 0..3 {
            let ping = tokio::time::timeout(Duration::from_secs(1), server.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Message::Ping(payload) = ping else {
                panic!("expected a ping");
            };
            server.send(Message::Pong(payload)).await.unwrap();
        }
        server
            .send(Message::Text(r#"{"id":1,"result":{}}"#.into()))
            .await
            .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(1), wire.next_message("test"))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(message, Some(JSONRPCMessage::Response(_))));
    }

    #[tokio::test]
    async fn missing_pong_and_credential_expiry_close_the_wire() {
        for expiry in [Duration::from_secs(30), Duration::from_millis(20)] {
            // An unpolled peer never flushes the automatic pong response.
            let (mut wire, _server, _cancelled, _foreground) = pair(expiry).await;
            assert!(
                tokio::time::timeout(Duration::from_secs(2), wire.next_message("test"))
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn foreground_resume_discards_the_old_socket() {
        let (mut wire, _server, _cancelled, foreground) = pair(Duration::from_secs(30)).await;
        foreground.send_replace(false);
        tokio::task::yield_now().await;
        foreground.send_replace(true);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), wire.next_message("test"))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_blocked_socket_write() {
        let (mut wire, _server, cancelled, _foreground) = pair(Duration::from_secs(30)).await;
        let message = serde_json::from_value(
            serde_json::json!({"method":"test", "params":{"text":"x".repeat(16_384)}}),
        )
        .unwrap();
        wire.send_message(message, "test").await.unwrap();
        tokio::task::yield_now().await;
        cancelled.send_replace(true);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), wire.next_message("test"))
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }
}
