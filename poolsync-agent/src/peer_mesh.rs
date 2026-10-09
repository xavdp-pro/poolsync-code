//! Mesh clipboard direct entre voisins (LAN/VPN) — sans relay legacy hub.

use crate::clipboard_incoming::apply_incoming_clipboard;
use crate::peer_clip_transfer::{self, CAPABILITY_HEADER};
use crate::state::AgentState;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use poolsync_core::{decode_message, decrypt_clipboard, AgentConfig, Message, Neighbor};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::BufReader;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};
use tokio_rustls::{rustls::ServerConfig, TlsAcceptor};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::AUTHORIZATION, HeaderValue},
        Message as WsMessage,
    },
    WebSocketStream,
};
use tracing::{debug, info, warn};

const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const PEER_RECONNECT_INITIAL: Duration = Duration::from_secs(2);
const PEER_RECONNECT_MAX: Duration = Duration::from_secs(20);

/// Lance l'écoute + connexions sortantes ; retourne un canal pour diffuser le clipboard local.
pub fn spawn(state: Arc<AgentState>) -> Result<Option<mpsc::UnboundedSender<String>>> {
    if !state.config.peer_direct_clipboard {
        anyhow::ensure!(!state.config.hubless, "hubless mode requires direct peers");
        return Ok(None);
    }
    let has_peer = state
        .config
        .neighbors
        .iter()
        .any(|n| n.peer_url.is_some() || n.peer_url_vpn.is_some());
    if !has_peer && !state.config.hubless {
        return Ok(None);
    }
    let mut controller = state
        .config
        .hubless
        .then(|| crate::hubless::Hubless::new(state.clone()))
        .transpose()?;

    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<String>();
    let (peer_reg_tx, mut peer_reg_rx) = mpsc::unbounded_channel::<PeerLink>();
    let (peer_in_tx, mut peer_in_rx) = mpsc::unbounded_channel::<PeerInbound>();
    // Local selection application is serialized and coalesced independently of
    // control/forwarding. Slow X11/RDP owners cannot stall input lease renewals.
    let (clip_tx, mut clip_rx) = tokio::sync::watch::channel::<Option<(String, String, u64)>>(None);
    let clip_state = state.clone();
    tokio::spawn(async move {
        while clip_rx.changed().await.is_ok() {
            let clip = clip_rx.borrow_and_update().clone();
            if let Some((source, wire, epoch)) = clip {
                if epoch != clip_state.participation_epoch() {
                    continue;
                }
                if let Some(Message::Clipboard {
                    hash,
                    data,
                    mime,
                    origin,
                    seq,
                    ..
                }) = decode_peer_clipboard(&wire, &clip_state.config)
                {
                    if let Err(err) = apply_incoming_clipboard(
                        &clip_state,
                        &hash,
                        &data,
                        &mime,
                        &source,
                        false,
                        &origin,
                        seq,
                    )
                    .await
                    {
                        debug!("peer clipboard apply: {err:#}");
                    }
                }
            }
        }
    });

    let state_listen = state.clone();
    let reg_listen = peer_reg_tx.clone();
    let in_listen = peer_in_tx.clone();
    tokio::spawn(async move {
        if let Err(err) = run_listener(state_listen, reg_listen, in_listen).await {
            warn!("peer listener: {err:#}");
        }
    });

    for neighbor in state.config.neighbors.clone() {
        // A link is initiated by exactly one deterministic endpoint.  The
        // other endpoint accepts it and registers the same direct channel.
        // This avoids duplicate sessions and image echo/reconnect storms.
        if !should_initiate_link(&state.config.node, &neighbor.node) {
            continue;
        }
        let urls: Vec<String> = [neighbor.peer_url.clone(), neighbor.peer_url_vpn.clone()]
            .into_iter()
            .flatten()
            .collect();
        if urls.is_empty() {
            continue;
        }
        let state_out = state.clone();
        let node = neighbor.node.clone();
        let reg = peer_reg_tx.clone();
        let incoming = peer_in_tx.clone();
        tokio::spawn(async move {
            peer_outbound_loop(state_out, node, urls, reg, incoming).await;
        });
    }

    tokio::spawn(async move {
        let mut peers: HashMap<String, mpsc::Sender<PeerSend>> = HashMap::new();
        let mut seen_messages: HashSet<String> = HashSet::new();
        let mut seen_order = VecDeque::new();
        let mut queued_clipboard: Option<(u64, String)> = None;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(payload) = local_rx.recv() => {
                    let payload = if let Some(controller) = controller.as_mut() {
                        if clipboard_message_id(&payload).is_some() {
                            if state.pool_away() { continue; }
                            payload
                        } else {
                            match decode_message(&payload) {
                                Ok(message) => match controller.local(message).await {
                                    Ok(Some(wire)) => wire,
                                    Ok(None) => continue,
                                    Err(err) => { warn!("peer control local: {err:#}"); continue; }
                                },
                                Err(_) => continue,
                            }
                        }
                    } else { payload };
                    if let Some(id) = clipboard_message_id(&payload) {
                        remember_bounded(&mut seen_messages, &mut seen_order, id);
                        advance_clipboard_queue(&mut queued_clipboard,&payload,&state.config);
                    }
                    peers.retain(|_,tx| tx.try_send(PeerSend::new(payload.clone(), &state)).is_ok());
                }
                Some(incoming) = peer_in_rx.recv() => {
                    let clip_id = clipboard_message_id(&incoming.payload);
                    if clip_id.is_some() && incoming.participation_epoch != state.participation_epoch() { continue; }
                    let id = clip_id.clone().unwrap_or_else(|| poolsync_core::hash_text(&incoming.payload));
                    if seen_messages.contains(&id) { continue; }
                    if let Some(controller) = controller.as_mut() {
                        if clip_id.is_some() {
                            if state.pool_away() { continue; }
                            // Validate before forwarding, but apply only after the relay.
                            if decode_peer_clipboard(&incoming.payload, &state.config).is_none() { continue; }
                        } else {
                            match controller.incoming(&incoming.payload).await {
                                Ok(true) => {},
                                Ok(false) => continue,
                                Err(err) => { debug!("peer control rejected: {err:#}"); continue; }
                            }
                        }
                    } else if state.pool_away() || clip_id.is_none() { continue; }
                    remember_bounded(&mut seen_messages, &mut seen_order, id);
                    peers.retain(|node,tx| node == &incoming.source || tx.try_send(PeerSend::new(incoming.payload.clone(), &state)).is_ok());
                    if controller.is_some() && clip_id.is_some()
                        && advance_clipboard_queue(&mut queued_clipboard,&incoming.payload,&state.config) {
                        let _ = clip_tx.send(Some((incoming.source,incoming.payload,incoming.participation_epoch)));
                    }
                }
                Some(link) = peer_reg_rx.recv() => {
                    info!("peer mesh connecté: {}", link.node);
                    peers.insert(link.node, link.tx);
                }
                _ = heartbeat.tick() => {
                    peers.retain(|_,tx| !tx.is_closed());
                    if let Some(controller) = controller.as_mut() {
                        state.set_connected(!peers.is_empty());
                        match controller.tick().await {
                            Ok(messages) => for payload in messages { peers.retain(|_,tx| tx.try_send(PeerSend::new(payload.clone(), &state)).is_ok()); },
                            Err(err) => warn!("peer heartbeat: {err:#}"),
                        }
                    }
                }
            }
        }
    });

    Ok(Some(local_tx))
}

fn decode_peer_clipboard(payload: &str, config: &AgentConfig) -> Option<Message> {
    let message = decode_message(payload).ok()?;
    match (&message, config.e2e_key.as_deref()) {
        (Message::EncryptedClipboard { .. }, Some(key)) => decrypt_clipboard(&message, key).ok(),
        (Message::Clipboard { .. }, None) => Some(message),
        _ => None,
    }
}

fn advance_clipboard_queue(
    last: &mut Option<(u64, String)>,
    wire: &str,
    config: &AgentConfig,
) -> bool {
    let Some(Message::Clipboard { origin, seq, .. }) = decode_peer_clipboard(wire, config) else {
        return false;
    };
    if seq == 0 || origin.is_empty() {
        return true;
    }
    if !crate::clip_order::remote_seq_is_plausible(seq) {
        return false;
    }
    let candidate = (seq, origin);
    if last.as_ref().is_some_and(|old| old >= &candidate) {
        return false;
    }
    *last = Some(candidate);
    true
}

fn remember_bounded(seen: &mut HashSet<String>, order: &mut VecDeque<String>, id: String) {
    if !seen.insert(id.clone()) {
        return;
    }
    order.push_back(id);
    while order.len() > MAX_SEEN_MESSAGES {
        if let Some(old) = order.pop_front() {
            seen.remove(&old);
        }
    }
}

fn should_initiate_link(local: &str, remote: &str) -> bool {
    !local.is_empty() && !remote.is_empty() && local < remote
}

struct PeerLink {
    node: String,
    tx: mpsc::Sender<PeerSend>,
}

struct PeerSend {
    payload: String,
    participation_epoch: u64,
}
impl PeerSend {
    fn new(payload: String, state: &AgentState) -> Self {
        Self {
            payload,
            participation_epoch: state.participation_epoch(),
        }
    }
}

struct PeerInbound {
    source: String,
    payload: String,
    participation_epoch: u64,
}

const MAX_SEEN_MESSAGES: usize = 4096;

fn clipboard_message_id(payload: &str) -> Option<String> {
    match decode_message(payload).ok()? {
        Message::Clipboard { msg_id, .. } if !msg_id.is_empty() => Some(msg_id),
        Message::EncryptedClipboard { msg_id, .. } if !msg_id.is_empty() => Some(msg_id),
        _ => None,
    }
}

async fn run_listener(
    state: Arc<AgentState>,
    reg: mpsc::UnboundedSender<PeerLink>,
    incoming: mpsc::UnboundedSender<PeerInbound>,
) -> Result<()> {
    let port = state.config.peer_listen_port;
    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind peer listen {addr}"))?;
    let tls = peer_tls_acceptor(&state.config)?;
    info!(
        "peer mesh écoute sur {addr} ({})",
        if tls.is_some() { "wss" } else { "ws" }
    );

    loop {
        let (stream, peer_addr) = listener.accept().await.context("peer accept")?;
        let socket_fd = stream.as_raw_fd();
        let state_in = state.clone();
        let reg_in = reg.clone();
        let incoming_in = incoming.clone();
        let tls_in = tls.clone();
        tokio::spawn(async move {
            let result = if let Some(acceptor) = tls_in {
                match acceptor.accept(stream).await {
                    Ok(stream) => {
                        handle_inbound_stream(
                            state_in,
                            stream,
                            peer_addr.to_string(),
                            reg_in,
                            incoming_in,
                            socket_fd,
                        )
                        .await
                    }
                    Err(err) => Err(anyhow::anyhow!("peer TLS accept: {err}")),
                }
            } else {
                handle_inbound_stream(
                    state_in,
                    stream,
                    peer_addr.to_string(),
                    reg_in,
                    incoming_in,
                    socket_fd,
                )
                .await
            };
            if let Err(err) = result {
                debug!("peer inbound {peer_addr}: {err:#}");
            }
        });
    }
}

fn peer_tls_acceptor(config: &AgentConfig) -> Result<Option<TlsAcceptor>> {
    let (cert_path, key_path) = match (&config.peer_tls_cert, &config.peer_tls_key) {
        (None, None) => return Ok(None),
        (Some(cert), Some(key)) => (cert, key),
        _ => anyhow::bail!("peer_tls_cert and peer_tls_key must be configured together"),
    };
    let mut cert_reader = BufReader::new(
        File::open(cert_path).with_context(|| format!("open peer TLS cert {cert_path}"))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("read peer TLS certificates")?;
    let mut key_reader = BufReader::new(
        File::open(key_path).with_context(|| format!("open peer TLS key {key_path}"))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("read peer TLS private key")?
        .context("peer TLS private key missing")?;
    let server = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build peer TLS configuration")?;
    Ok(Some(TlsAcceptor::from(Arc::new(server))))
}

#[allow(clippy::result_large_err)]
async fn handle_inbound_stream<S>(
    state: Arc<AgentState>,
    stream: S,
    peer_addr: String,
    reg: mpsc::UnboundedSender<PeerLink>,
    incoming: mpsc::UnboundedSender<PeerInbound>,
    socket_fd: std::os::fd::RawFd,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut remote_node: Option<String> = None;
    let mut fragments = false;
    let config = state.config.clone();
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         mut res: tokio_tungstenite::tungstenite::handshake::server::Response| {
            let Some(node) = peer_request_identity(req, &config) else {
                let err_res = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(
                    Some("Invalid peer credentials".to_string()),
                );
                return Err(err_res);
            };
            remote_node = Some(node);
            fragments = req
                .headers()
                .get(CAPABILITY_HEADER)
                .is_some_and(|v| v == "1");
            if fragments {
                peer_clip_transfer::pace_socket(socket_fd).map_err(|_| {
                    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(Some(
                        "Peer socket pacing unavailable".into(),
                    ))
                })?;
                res.headers_mut()
                    .insert(CAPABILITY_HEADER, HeaderValue::from_static("1"));
            }
            Ok(res)
        },
    )
    .await
    .context("peer ws accept")?;
    let label = remote_node.clone().unwrap_or(peer_addr);
    serve_peer_session(state, ws, remote_node, label, reg, incoming, fragments).await
}

fn neighbor_accepts_token(neighbor: &Neighbor, token: &str, shared_token: &str) -> bool {
    let current = neighbor.auth_token.as_deref().unwrap_or(shared_token);
    token == current || neighbor.previous_auth_token.as_deref() == Some(token)
}

fn config_accepts_peer_token(config: &AgentConfig, neighbor: &Neighbor, token: &str) -> bool {
    if let Some(expected) = config.peer_tokens.get(&neighbor.node) {
        return token == expected
            || config
                .previous_peer_tokens
                .get(&neighbor.node)
                .is_some_and(|old| old == token);
    }
    neighbor_accepts_token(neighbor, token, &config.token)
}

fn peer_request_identity(
    req: &tokio_tungstenite::tungstenite::handshake::server::Request,
    config: &AgentConfig,
) -> Option<String> {
    let node = req
        .headers()
        .get("x-poolsync-node")?
        .to_str()
        .ok()?
        .to_string();
    let authorization = req.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = authorization.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || node == config.node {
        return None;
    }
    let neighbor = config.neighbors.iter().find(|peer| peer.node == node)?;
    config_accepts_peer_token(config, neighbor, token).then_some(node)
}

async fn peer_outbound_loop(
    state: Arc<AgentState>,
    neighbor: String,
    urls: Vec<String>,
    reg: mpsc::UnboundedSender<PeerLink>,
    incoming: mpsc::UnboundedSender<PeerInbound>,
) {
    let mut backoff = PEER_RECONNECT_INITIAL;
    loop {
        let mut session_ended = false;
        for url in &urls {
            match timeout_connect(url, state.config.authentication_token(), &state.config.node)
                .await
            {
                Ok((ws, fragments)) => {
                    info!("peer mesh → {neighbor} via {url}");
                    if serve_peer_session(
                        state.clone(),
                        ws,
                        Some(neighbor.clone()),
                        url.clone(),
                        reg.clone(),
                        incoming.clone(),
                        fragments,
                    )
                    .await
                    .is_ok()
                    {
                        // A clean WebSocket close still means the session is gone.  Without
                        // this pause the outer loop reconnects immediately, creating thousands
                        // of sockets and starving clipboard work on every peer.
                        session_ended = true;
                        break;
                    }
                }
                Err(err) => {
                    debug!("peer connect {neighbor} {url}: {err:#}");
                }
            }
        }
        if session_ended {
            backoff = PEER_RECONNECT_INITIAL;
        }
        sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, PEER_RECONNECT_MAX);
    }
}

async fn timeout_connect(
    url: &str,
    token: &str,
    node: &str,
) -> Result<(
    WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    bool,
)> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    request
        .headers_mut()
        .insert("x-poolsync-node", HeaderValue::from_str(node)?);
    request
        .headers_mut()
        .insert(CAPABILITY_HEADER, HeaderValue::from_static("1"));
    let (ws, response) = timeout(PEER_CONNECT_TIMEOUT, connect_async(request))
        .await
        .context("peer connect timeout")?
        .with_context(|| format!("peer connect {url}"))?;
    let fragments = response
        .headers()
        .get(CAPABILITY_HEADER)
        .is_some_and(|v| v == "1");
    if fragments {
        let stream = match ws.get_ref() {
            tokio_tungstenite::MaybeTlsStream::Plain(stream) => stream,
            tokio_tungstenite::MaybeTlsStream::NativeTls(stream) => {
                stream.get_ref().get_ref().get_ref()
            }
            _ => anyhow::bail!("unsupported peer stream for TCP pacing"),
        };
        peer_clip_transfer::pace_socket(stream.as_raw_fd())?;
    }
    Ok((ws, fragments))
}

async fn serve_peer_session<S>(
    state: Arc<AgentState>,
    ws: WebSocketStream<S>,
    remote_node: Option<String>,
    label: String,
    reg: mpsc::UnboundedSender<PeerLink>,
    incoming: mpsc::UnboundedSender<PeerInbound>,
    fragments: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut write, mut read) = ws.split();
    let (peer_tx, mut peer_rx) = mpsc::channel::<PeerSend>(128);
    let node_name = remote_node.clone().unwrap_or_else(|| label.clone());
    // Inbound handshakes include their node name.  With one deterministic
    // dialer per pair, both endpoints may register this single channel.
    if remote_node.is_some() {
        let _ = reg.send(PeerLink {
            node: node_name.clone(),
            tx: peer_tx,
        });
    }

    let state_read = state.clone();
    let remote = remote_node.clone();
    let mut ping_interval = tokio::time::interval(Duration::from_secs(10));
    let mut outgoing_clipboard: Option<peer_clip_transfer::Outgoing> = None;
    let mut incoming_clipboard = peer_clip_transfer::Incoming::default();
    let mut participation_epoch = state.participation_epoch();
    info!(peer = %node_name, clipboard_fragments = fragments, "peer transport negotiated");

    loop {
        let epoch = state.participation_epoch();
        if epoch != participation_epoch || !state.local_poolsync_active() {
            outgoing_clipboard = None;
            incoming_clipboard.cancel();
            participation_epoch = epoch;
        }
        incoming_clipboard.expire(std::time::Instant::now());
        if outgoing_clipboard
            .as_ref()
            .is_some_and(|outgoing| outgoing.expired(std::time::Instant::now()))
        {
            debug!(peer = %node_name, "peer clipboard fragmented transfer stalled; control remains connected");
            outgoing_clipboard = None;
        }
        tokio::select! {
            biased;
            _ = ping_interval.tick() => {
                if write.send(WsMessage::Ping(vec![].into())).await.is_err() {
                    warn!("peer mesh ping échoué vers {node_name}");
                    break;
                }
            }
            maybe = peer_rx.recv() => {
                match maybe {
                    Some(queued) => {
                        let payload = queued.payload;
                        if state_read.pool_away() && !state_read.config.hubless { continue; }
                        let clipboard = clipboard_message_id(&payload).is_some();
                        if clipboard && (queued.participation_epoch != state.participation_epoch() || !state.local_poolsync_active()) { continue; }
                        let decoded = decode_message(&payload).ok().and_then(|message| {
                            if matches!(message, Message::EncryptedClipboard { .. }) {
                                state
                                    .config
                                    .e2e_key
                                    .as_deref()
                                    .and_then(|key| decrypt_clipboard(&message, key).ok())
                            } else if state.config.e2e_key.is_none() {
                                Some(message)
                            } else {
                                None
                            }
                        });
                        if let Some(Message::Clipboard { hash, ref mime, ref data, .. }) = decoded {
                            if mime.starts_with("image/") {
                                info!(
                                    "image-trace PEER-SEND id={} to={} mime={} wire_bytes={}",
                                    crate::clipboard::trace_id(&hash),
                                    node_name,
                                    mime,
                                    data.len()
                                );
                            }
                        }
                        if clipboard && fragments {
                            // A newer copy cancels the incomplete outgoing image.
                            outgoing_clipboard = None;
                            if payload.len() > peer_clip_transfer::CHUNK_BYTES {
                                debug!(peer = %node_name, wire_bytes = payload.len(), "peer clipboard fragmented transfer started");
                                outgoing_clipboard = Some(peer_clip_transfer::Outgoing::new(payload)?);
                                continue;
                            }
                        }
                        if !matches!(timeout(Duration::from_secs(1), write.send(WsMessage::Text(payload.into()))).await, Ok(Ok(()))) {
                            debug!(peer = %node_name, "peer send failed or timed out");
                            break;
                        }
                    }
                    None => break,
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if clipboard_message_id(&text).is_some() { incoming_clipboard.cancel(); }
                        if state_read.config.hubless {
                            let _ = incoming.send(PeerInbound { source: node_name.clone(), payload: text.to_string(), participation_epoch });
                            continue;
                        }
                        let wire = decode_message(&text);
                        let decoded = wire.as_ref().ok().and_then(|message| {
                            if matches!(message, Message::EncryptedClipboard { .. }) {
                                state_read
                                    .config
                                    .e2e_key
                                    .as_deref()
                                    .and_then(|key| decrypt_clipboard(message, key).ok())
                            } else if state_read.config.e2e_key.is_none() {
                                Some(message.clone())
                            } else {
                                None
                            }
                        });
                        if let Some(Message::Clipboard {
                            hash, data, mime, origin, seq, ..
                        }) = decoded {
                            // Forward before touching the local X11 selection. A
                            // slow paste owner must not stall downstream peers.
                            let _ = incoming.send(PeerInbound {
                                source: node_name.clone(),
                                payload: text.to_string(),
                                participation_epoch,
                            });
                            let source = remote.as_deref().unwrap_or("peer");
                            // Toujours relayer : `(origin, seq)` voyage avec le
                            // message, donc chaque nœud tranche lui-même. Filtrer
                            // ici priverait un voisin plus lointain d'un message
                            // qui est peut-être le plus récent pour lui.
                            if let Err(err) = apply_incoming_clipboard(
                                &state_read, &hash, &data, &mime, source, false, &origin, seq,
                            ).await {
                                debug!("peer clipboard apply: {err:#}");
                            }
                        } else if matches!(wire, Ok(Message::EncryptedClipboard { .. })) {
                            warn!("peer clipboard chiffré rejeté depuis {node_name}: clef absente ou invalide");
                        }
                    }
                    Some(Ok(WsMessage::Binary(frame))) => {
                        anyhow::ensure!(fragments, "unnegotiated clipboard fragment");
                        if peer_clip_transfer::is_ack(&frame) {
                            if let Some(outgoing) = outgoing_clipboard.as_mut() {
                                outgoing.acknowledge(&frame, std::time::Instant::now())?;
                            }
                            continue;
                        }
                        let ack = peer_clip_transfer::acknowledgement(&frame)?;
                        if !matches!(timeout(Duration::from_secs(1), write.send(WsMessage::Binary(ack.into()))).await, Ok(Ok(()))) { break; }
                        if !state.local_poolsync_active() { incoming_clipboard.cancel(); continue; }
                        if let Some(payload) = incoming_clipboard.push(&frame, std::time::Instant::now())? {
                            anyhow::ensure!(clipboard_message_id(&payload).is_some(), "fragmented payload is not a clipboard message");
                            if !state_read.config.hubless && decode_peer_clipboard(&payload, &state_read.config).is_none() { continue; }
                            let _ = incoming.send(PeerInbound { source: node_name.clone(), payload, participation_epoch });
                        }
                    }
                    Some(Ok(WsMessage::Ping(payload))) => {
                        if write.send(WsMessage::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            _ = std::future::ready(()), if outgoing_clipboard.as_ref().is_some_and(|outgoing| outgoing.ready()) => {
                let outgoing = outgoing_clipboard.as_mut().expect("pending clipboard");
                if let Some(frame) = outgoing.next_frame() {
                    if !matches!(timeout(Duration::from_secs(1), write.send(WsMessage::Binary(frame.into()))).await, Ok(Ok(()))) {
                        debug!(peer = %node_name, "peer clipboard fragment send failed or timed out");
                        break;
                    }
                }
                if outgoing.finished() {
                    debug!(peer = %node_name, "peer clipboard fragmented transfer finished");
                    outgoing_clipboard = None;
                }
            }
        }
    }
    debug!("peer session ended: {node_name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reordered_copies_cannot_replace_the_newest_pending_clipboard() {
        let config: AgentConfig = toml::from_str("node='a'\nhub_url='ws://localhost/ws'\ntoken='t'\nmode='full'\n[screen]\nwidth=800\nheight=600").unwrap();
        let mut last = None;
        for (seq, expected) in [
            (20, true),
            (10, false),
            (21, true),
            (u64::MAX, false),
            (22, true),
        ] {
            let wire = poolsync_core::encode_message(&Message::Clipboard {
                msg_id: seq.to_string(),
                hash: "hash".into(),
                mime: "text/plain".into(),
                data: "fixture".into(),
                origin: "b".into(),
                seq,
            })
            .unwrap();
            assert_eq!(advance_clipboard_queue(&mut last, &wire, &config), expected);
        }
        assert_eq!(last, Some((22, "b".into())));
    }

    #[test]
    fn exactly_one_endpoint_dials_each_pair() {
        for (a, b) in [
            ("desk-a", "work-a"),
            ("work-a", "work-b"),
            ("desk-b", "desk-a"),
        ] {
            assert_ne!(should_initiate_link(a, b), should_initiate_link(b, a));
        }
    }

    #[test]
    fn invalid_or_self_links_are_never_dialed() {
        assert!(!should_initiate_link("desk-a", "desk-a"));
        assert!(!should_initiate_link("", "work-a"));
        assert!(!should_initiate_link("work-a", ""));
    }

    #[test]
    fn peer_credentials_support_individual_rotation() {
        let neighbor = Neighbor {
            direction: poolsync_core::Direction::Left,
            node: "desk-b".into(),
            peer_url: None,
            peer_url_vpn: None,
            auth_token: Some("new".into()),
            previous_auth_token: Some("old".into()),
        };
        assert!(neighbor_accepts_token(&neighbor, "new", "shared"));
        assert!(neighbor_accepts_token(&neighbor, "old", "shared"));
        assert!(!neighbor_accepts_token(&neighbor, "shared", "shared"));
    }

    #[test]
    fn clipboard_message_id_drives_mesh_deduplication() {
        let payload = poolsync_core::encode_message(&Message::Clipboard {
            msg_id: "copy-42".into(),
            hash: "hash".into(),
            mime: "text/plain".into(),
            data: "hello".into(),
            origin: "desk-a".into(),
            seq: 7,
        })
        .unwrap();
        assert_eq!(clipboard_message_id(&payload).as_deref(), Some("copy-42"));

        let mut seen = HashSet::new();
        assert!(seen.insert(clipboard_message_id(&payload).unwrap()));
        assert!(!seen.insert(clipboard_message_id(&payload).unwrap()));
    }
}
