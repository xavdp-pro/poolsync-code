use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::get,
    Json, Router,
};
use clap::Parser;
use futures_util::StreamExt;
use poolsync_core::{
    decode_message, encode_message, infer_neighbors, AgentMode, Message, MonitorInfo, Neighbor,
    PoolTopology, ScreenInfo, TopologyNode, DEFAULT_EDGE_TOLERANCE_PX,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info, warn};

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader};
use std::io::Cursor;

const TRAY_THUMB_MAX_PX: u32 = 64;

fn image_thumb_b64(data_b64: &str) -> Option<String> {
    let bytes = B64.decode(data_b64).ok()?;
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let img = reader.decode().ok()?;
    let thumb = img.resize(TRAY_THUMB_MAX_PX, TRAY_THUMB_MAX_PX, FilterType::Triangle);
    let mut out = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(B64.encode(out))
}

#[derive(Parser, Debug)]
#[command(
    name = "poolsync-hub",
    version,
    about = "PoolSync hub — presse-papiers + KVM maître dynamique"
)]
struct Args {
    /// Adresse d'écoute (0.0.0.0 pour LAN/VPN/public)
    #[arg(long, default_value = "0.0.0.0:9470")]
    listen: String,

    /// Jeton administrateur des API/Web (les agents peuvent avoir leur propre identité)
    #[arg(long, env = "POOLSYNC_TOKEN", hide_env_values = true)]
    token: String,

    /// Répertoire des fichiers statiques (dashboard web)
    #[arg(long)]
    web_dir: Option<PathBuf>,

    /// Fichier JSON de topologie KVM (mosaïque écrans)
    #[arg(long, default_value = "/var/lib/poolsync/topology.json")]
    topology_file: PathBuf,

    /// Identités par nœud, rechargées à chaque connexion (rotation/révocation à chaud).
    #[arg(long)]
    node_tokens_file: Option<PathBuf>,

    /// Certificat PEM TLS. Doit être fourni avec --tls-key.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// Clef privée PEM TLS. Doit être fournie avec --tls-cert.
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// Refuse tout presse-papiers en clair (à activer après migration des agents).
    #[arg(long, default_value_t = false)]
    require_e2e: bool,
}

#[derive(Clone)]
struct NodeInfo {
    mode: AgentMode,
    screen: ScreenInfo,
    neighbors: Vec<Neighbor>,
    kvm_enabled: bool,
    connected_at: u64,
    /// Synchro presse-papiers active sur ce nœud, telle qu'il la déclare.
    clipboard_sync: bool,
    /// PoolSync actif localement (pas en pause clavier).
    local_active: bool,
    /// Tous ses moniteurs RandR (vide si agent d'une version antérieure).
    monitors: Vec<MonitorInfo>,
    sender: broadcast::Sender<String>,
}

#[derive(Clone)]
struct HubState {
    token: String,
    node_tokens_file: Option<PathBuf>,
    require_e2e: bool,
    started_at: u64,
    topology_file: PathBuf,
    topology: Arc<RwLock<PoolTopology>>,
    nodes: Arc<RwLock<HashMap<String, NodeInfo>>>,
    master: Arc<RwLock<Option<String>>>,
    input_owner: Arc<RwLock<Option<String>>>,
    last_clipboard_hash: Arc<RwLock<Option<String>>>,
    last_clipboard_at: Arc<RwLock<Option<u64>>>,
    clipboard_history: Arc<RwLock<VecDeque<ClipboardHistoryEntry>>>,
    clipboard_history_revision: Arc<RwLock<u64>>,
    clipboard_events: broadcast::Sender<u64>,
}

const CLIPBOARD_HISTORY_MAX: usize = 50;

#[derive(Clone)]
struct ClipboardHistoryEntry {
    hash: String,
    mime: String,
    preview: String,
    data: String,
    thumb_b64: Option<String>,
    source_node: String,
    at: u64,
}

#[derive(Serialize)]
struct ClipboardHistoryItem {
    hash: String,
    mime: String,
    preview: String,
    source_node: String,
    at: u64,
    is_image: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thumb_b64: Option<String>,
}

#[derive(Serialize)]
struct ClipboardHistoryResponse {
    items: Vec<ClipboardHistoryItem>,
}

#[derive(Deserialize)]
struct ClipboardPickBody {
    hash: String,
    #[serde(default)]
    node: Option<String>,
}

#[derive(Deserialize)]
struct ClipboardDeleteBody {
    hashes: Vec<String>,
}

#[derive(Serialize)]
struct ClipboardItemResponse {
    hash: String,
    mime: String,
    preview: String,
    data: String,
    source_node: String,
    at: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct NodeCredential {
    token: String,
    #[serde(default)]
    previous_tokens: Vec<String>,
    #[serde(default)]
    revoked: bool,
}

#[derive(Debug, Default, Deserialize)]
struct NodeCredentials {
    #[serde(default)]
    nodes: HashMap<String, NodeCredential>,
}

#[derive(Serialize)]
struct StatusResponse {
    hub: HubInfo,
    master: Option<String>,
    clipboard: ClipboardInfo,
    nodes: Vec<NodeStatus>,
}

#[derive(Serialize)]
struct HubInfo {
    version: &'static str,
    started_at: u64,
    node_count: usize,
    listen: String,
}

#[derive(Serialize)]
struct ClipboardInfo {
    last_hash: Option<String>,
    last_at: Option<u64>,
}

#[derive(Serialize)]
struct NodeStatus {
    name: String,
    mode: AgentMode,
    screen: ScreenInfo,
    neighbors: Vec<Neighbor>,
    kvm_enabled: bool,
    connected_at: u64,
    online: bool,
    is_master: bool,
    /// Synchro presse-papiers : `false` = nœud « sourd », il ne réplique rien.
    clipboard_sync: bool,
    /// `false` = PoolSync en pause sur ce poste (raccourci clavier).
    local_active: bool,
    /// Moniteurs de ce nœud, pour la mosaïque multi-écrans.
    monitors: Vec<MonitorInfo>,
    /// Dernière copie venue de ce nœud (aperçu, mime, horodatage).
    #[serde(skip_serializing_if = "Option::is_none")]
    last_clip: Option<NodeClip>,
}

#[derive(Serialize)]
struct NodeClip {
    preview: String,
    mime: String,
    at: u64,
    is_image: bool,
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers.get(AUTHORIZATION) {
        let value = value.to_str().ok()?;
        let (scheme, credential) = value.split_once(' ')?;
        return scheme.eq_ignore_ascii_case("bearer").then_some(credential);
    }
    None
}

fn request_authorized(headers: &HeaderMap, state: &HubState) -> bool {
    let Some(token) = bearer_token(headers) else {
        return false;
    };
    if token == state.token {
        return true;
    }
    let Some(node) = headers
        .get("x-poolsync-node")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(path) = state.node_tokens_file.as_ref() else {
        return false;
    };
    load_node_credentials(path)
        .ok()
        .and_then(|credentials| credentials.nodes.get(node).cloned())
        .is_some_and(|credential| credential_accepts(&credential, token))
}

fn load_node_credentials(path: &PathBuf) -> Result<NodeCredentials> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read node credentials {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse node credentials {}", path.display()))
}

fn credential_accepts(credential: &NodeCredential, token: &str) -> bool {
    !credential.revoked
        && (credential.token == token || credential.previous_tokens.iter().any(|old| old == token))
}

#[derive(Clone)]
struct AuthenticatedNode {
    name: String,
    token: String,
}

fn node_identity_still_valid(identity: &AuthenticatedNode, state: &HubState) -> bool {
    if let Some(path) = state.node_tokens_file.as_ref() {
        return load_node_credentials(path)
            .ok()
            .and_then(|credentials| credentials.nodes.get(&identity.name).cloned())
            .is_some_and(|credential| credential_accepts(&credential, &identity.token));
    }
    identity.token == state.token
}

fn authenticated_node(
    headers: &HeaderMap,
    state: &HubState,
) -> Result<AuthenticatedNode, StatusCode> {
    let node = headers
        .get("x-poolsync-node")
        .and_then(|value| value.to_str().ok())
        .filter(|node| !node.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let token = bearer_token(headers).ok_or(StatusCode::UNAUTHORIZED)?;

    if let Some(path) = state.node_tokens_file.as_ref() {
        let credentials = load_node_credentials(path).map_err(|err| {
            error!("node credentials unavailable: {err:#}");
            StatusCode::SERVICE_UNAVAILABLE
        })?;
        let credential = credentials
            .nodes
            .get(node)
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if !credential_accepts(credential, token) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    } else if token != state.token {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(AuthenticatedNode {
        name: node.to_string(),
        token: token.to_string(),
    })
}

fn load_topology(path: &PathBuf) -> PoolTopology {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|err| {
            warn!(
                "invalid topology {}: {err:#} — using empty default",
                path.display()
            );
            PoolTopology::default()
        }),
        Err(_) => {
            let topo = PoolTopology::default();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(&topo) {
                let _ = std::fs::write(path, json);
            }
            topo
        }
    }
}

fn write_topology_file(state: &HubState, topology: &PoolTopology) -> Result<()> {
    let json = serde_json::to_string_pretty(topology)?;
    if let Some(parent) = state.topology_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pending = state.topology_file.with_extension("pending");
    std::fs::write(&pending, &json)?;
    std::fs::rename(pending, &state.topology_file)?;
    Ok(())
}

async fn save_topology(state: &HubState, topology: PoolTopology) -> Result<()> {
    let mut current = state.topology.write().await;
    write_topology_file(state, &topology)?;
    *current = topology;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Le hub n'est qu'un relais : il transmet l'`origin` de la copie tel quel,
/// sinon les nœuds perdent l'ordre d'origine et voient le hub comme auteur.
/// Un agent d'une version antérieure n'en envoie pas : on attribue alors la
/// copie au nœud qui l'a poussée, ce qui reste un identifiant stable.
fn relay_origin(origin: String, from: &str) -> String {
    if origin.is_empty() {
        from.to_string()
    } else {
        origin
    }
}

/// Horloge logique des messages presse-papiers émis par le hub lui-même.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tokio::main]
async fn main() -> Result<()> {
    // The workspace also contains a ring-based TLS client. When Cargo builds
    // the hub and agent together, rustls sees both providers and cannot choose
    // one implicitly. Pin the process provider before creating any TLS config.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "poolsync_hub=info".into()),
        )
        .init();

    let args = Args::parse();
    let listen: SocketAddr = args
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", args.listen))?;

    let started_at = now_secs();
    let topology = load_topology(&args.topology_file);
    let (clipboard_events, _) = broadcast::channel(64);
    let state = HubState {
        token: args.token.clone(),
        node_tokens_file: args.node_tokens_file.clone(),
        require_e2e: args.require_e2e,
        started_at,
        topology_file: args.topology_file.clone(),
        topology: Arc::new(RwLock::new(topology)),
        nodes: Arc::new(RwLock::new(HashMap::new())),
        master: Arc::new(RwLock::new(None)),
        input_owner: Arc::new(RwLock::new(None)),
        last_clipboard_hash: Arc::new(RwLock::new(None)),
        last_clipboard_at: Arc::new(RwLock::new(None)),
        clipboard_history: Arc::new(RwLock::new(VecDeque::new())),
        clipboard_history_revision: Arc::new(RwLock::new(0)),
        clipboard_events,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/status", get(api_status))
        .route(
            "/api/topology",
            get(api_topology_get).post(api_topology_post),
        )
        .route("/api/clipboard/history", get(api_clipboard_history))
        .route("/api/clipboard/item", get(api_clipboard_item))
        .route("/api/clipboard/events", get(api_clipboard_events))
        .route("/api/edges/show", axum::routing::post(api_edges_show))
        .route(
            "/api/clipboard/pick",
            axum::routing::post(api_clipboard_pick),
        )
        .route(
            "/api/clipboard/clear",
            axum::routing::post(api_clipboard_clear),
        )
        .route(
            "/api/clipboard/delete",
            axum::routing::post(api_clipboard_delete),
        )
        .route("/ws", get(ws_handler))
        .with_state(state.clone());

    let mut app = app;
    if let Some(web_dir) = args.web_dir.as_ref() {
        let index = web_dir.join("index.html");
        if web_dir.is_dir() && index.is_file() {
            let serve = ServeDir::new(web_dir).not_found_service(ServeFile::new(index));
            app = app.fallback_service(serve);
            info!("serving web dashboard from {}", web_dir.display());
        } else {
            warn!(
                "web_dir {} missing or no index.html — dashboard disabled",
                web_dir.display()
            );
        }
    }

    match (args.tls_cert.as_ref(), args.tls_key.as_ref()) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .context("load TLS certificate/key")?;
            info!("poolsync-hub TLS listening on {listen}");
            axum_server::bind_rustls(listen, tls)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            info!("poolsync-hub listening on {listen}");
            let listener = tokio::net::TcpListener::bind(listen).await?;
            axum::serve(listener, app).await?;
        }
        _ => anyhow::bail!("--tls-cert and --tls-key must be provided together"),
    }
    Ok(())
}

async fn health() -> impl IntoResponse {
    "ok"
}

async fn api_status(
    headers: HeaderMap,
    State(state): State<HubState>,
) -> Result<Json<StatusResponse>, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let nodes_map = state.nodes.read().await;
    let master = state.master.read().await.clone();
    let last_hash = state.last_clipboard_hash.read().await.clone();
    let last_at = state.last_clipboard_at.read().await;
    let node_count = nodes_map.len();

    // Dernière copie par nœud : l'historique la porte déjà, il suffit de
    // prendre la plus récente de chaque source. L'interface peut alors montrer
    // « desk-b a copié tel texte il y a 3 min » sans requête supplémentaire.
    let mut last_clip_by_node: HashMap<String, NodeClip> = {
        let history = state.clipboard_history.read().await;
        let mut map: HashMap<String, NodeClip> = HashMap::new();
        for entry in history.iter() {
            let slot = map
                .entry(entry.source_node.clone())
                .or_insert_with(|| NodeClip {
                    preview: entry.preview.clone(),
                    mime: entry.mime.clone(),
                    at: entry.at,
                    is_image: entry.mime.starts_with("image/"),
                });
            if entry.at > slot.at {
                *slot = NodeClip {
                    preview: entry.preview.clone(),
                    mime: entry.mime.clone(),
                    at: entry.at,
                    is_image: entry.mime.starts_with("image/"),
                };
            }
        }
        map
    };

    let nodes: Vec<NodeStatus> = nodes_map
        .iter()
        .map(|(name, info)| NodeStatus {
            name: name.clone(),
            mode: info.mode,
            screen: info.screen,
            neighbors: info.neighbors.clone(),
            kvm_enabled: info.kvm_enabled,
            connected_at: info.connected_at,
            online: true,
            is_master: master.as_deref() == Some(name.as_str()),
            clipboard_sync: info.clipboard_sync,
            local_active: info.local_active,
            monitors: info.monitors.clone(),
            last_clip: last_clip_by_node.remove(name),
        })
        .collect();

    Ok(Json(StatusResponse {
        hub: HubInfo {
            version: env!("CARGO_PKG_VERSION"),
            started_at: state.started_at,
            node_count,
            listen: "0.0.0.0:9470".into(),
        },
        master,
        clipboard: ClipboardInfo {
            last_hash,
            last_at: *last_at,
        },
        nodes,
    }))
}

async fn api_topology_get(
    headers: HeaderMap,
    State(state): State<HubState>,
) -> Result<Json<PoolTopology>, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(state.topology.read().await.clone()))
}

fn clip_preview_hub(mime: &str, data: &str) -> String {
    if mime.starts_with("image/") {
        let label = mime.strip_prefix("image/").unwrap_or(mime);
        let bytes = data.len().saturating_mul(3) / 4;
        let size = if bytes >= 1024 * 1024 {
            format!("{:.1} Mo", bytes as f64 / (1024.0 * 1024.0))
        } else if bytes >= 1024 {
            format!("{} Ko", bytes / 1024)
        } else {
            format!("{bytes} o")
        };
        format!("[Image {label} — {size}]")
    } else {
        let one_line: String = data.chars().take(80).collect();
        if data.len() > 80 {
            format!("{one_line}…")
        } else {
            one_line
        }
    }
}

async fn push_clipboard_history(
    state: &HubState,
    source_node: &str,
    hash: &str,
    mime: &str,
    data: &str,
) {
    let entry = ClipboardHistoryEntry {
        hash: hash.to_string(),
        mime: mime.to_string(),
        preview: clip_preview_hub(mime, data),
        data: data.to_string(),
        thumb_b64: mime
            .starts_with("image/")
            .then(|| image_thumb_b64(data))
            .flatten(),
        source_node: source_node.to_string(),
        at: now_secs(),
    };
    let mut hist = state.clipboard_history.write().await;
    hist.retain(|e| e.hash != entry.hash);
    hist.push_front(entry);
    while hist.len() > CLIPBOARD_HISTORY_MAX {
        hist.pop_back();
    }
}

async fn notify_clipboard_history(state: &HubState) {
    let revision = {
        let mut rev = state.clipboard_history_revision.write().await;
        *rev += 1;
        *rev
    };
    let _ = state.clipboard_events.send(revision);
    if let Ok(payload) = encode_message(&Message::ClipboardHistoryUpdated { revision }) {
        broadcast_all(state, &payload).await;
    }
}

async fn api_clipboard_events(
    headers: HeaderMap,
    State(state): State<HubState>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>>,
    StatusCode,
> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let initial = *state.clipboard_history_revision.read().await;
    let initial_event = futures_util::stream::once(async move {
        Ok(Event::default().data(format!("{{\"revision\":{initial}}}")))
    });
    let updates = BroadcastStream::new(state.clipboard_events.subscribe())
        .filter_map(|r| async move { r.ok() })
        .map(|revision| Ok(Event::default().data(format!("{{\"revision\":{revision}}}"))));
    let stream = initial_event.chain(updates);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

async fn api_clipboard_history(
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
    State(state): State<HubState>,
) -> Result<Json<ClipboardHistoryResponse>, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let limit = query.limit.unwrap_or(50).min(CLIPBOARD_HISTORY_MAX);
    let hist = state.clipboard_history.read().await;
    let items = hist
        .iter()
        .take(limit)
        .map(|e| ClipboardHistoryItem {
            hash: e.hash.clone(),
            mime: e.mime.clone(),
            preview: e.preview.clone(),
            source_node: e.source_node.clone(),
            at: e.at,
            is_image: e.mime.starts_with("image/"),
            thumb_b64: e.thumb_b64.clone(),
        })
        .collect();
    Ok(Json(ClipboardHistoryResponse { items }))
}

#[derive(Deserialize)]
struct ItemQuery {
    hash: String,
}

async fn api_clipboard_item(
    headers: HeaderMap,
    Query(query): Query<ItemQuery>,
    State(state): State<HubState>,
) -> Result<Json<ClipboardItemResponse>, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let hist = state.clipboard_history.read().await;
    let entry = hist
        .iter()
        .find(|e| e.hash == query.hash)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(ClipboardItemResponse {
        hash: entry.hash.clone(),
        mime: entry.mime.clone(),
        preview: entry.preview.clone(),
        data: entry.data.clone(),
        source_node: entry.source_node.clone(),
        at: entry.at,
    }))
}

#[derive(Deserialize)]
struct EdgesShowBody {
    /// Nœud visé ; absent = tous les nœuds du pool.
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

/// Demande aux agents de matérialiser leurs bords KVM à l'écran.
///
/// Enregistrer une topologie ne dit pas si elle correspond au terrain : cette
/// route permet de le vérifier sans promener la souris de bord en bord.
async fn api_edges_show(
    headers: HeaderMap,
    State(state): State<HubState>,
    Json(body): Json<EdgesShowBody>,
) -> Result<StatusCode, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let payload = encode_message(&Message::ShowEdges {
        duration_ms: body.duration_ms.unwrap_or(2500),
    })
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let nodes = state.nodes.read().await;
    let mut sent = 0;
    for (name, info) in nodes.iter() {
        if body.node.as_deref().is_some_and(|n| n != name) {
            continue;
        }
        if info.sender.send(payload.clone()).is_ok() {
            sent += 1;
        }
    }
    info!("bords : demande envoyée à {sent} nœud(s)");
    if sent == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::OK)
}

async fn api_clipboard_pick(
    headers: HeaderMap,
    State(state): State<HubState>,
    Json(body): Json<ClipboardPickBody>,
) -> Result<StatusCode, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let entry = {
        let hist = state.clipboard_history.read().await;
        hist.iter()
            .find(|e| e.hash == body.hash)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    *state.last_clipboard_hash.write().await = Some(entry.hash.clone());
    *state.last_clipboard_at.write().await = Some(now_secs());
    push_clipboard_history(&state, "pick", &entry.hash, &entry.mime, &entry.data).await;
    // Un pick est une nouvelle intention utilisateur : il doit gagner sur tout
    // ce que les agents ont déjà vu, d'où une horloge basée sur l'heure mur.
    let payload = encode_message(&Message::Clipboard {
        msg_id: uuid::Uuid::new_v4().to_string(),
        hash: entry.hash,
        mime: entry.mime,
        data: entry.data,
        origin: "hub".to_string(),
        seq: now_ms(),
    })
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Some(ref node) = body.node {
        broadcast_except(&state, node, &payload).await;
    } else {
        broadcast_all(&state, &payload).await;
    }
    notify_clipboard_history(&state).await;
    Ok(StatusCode::OK)
}

async fn api_clipboard_clear(
    headers: HeaderMap,
    State(state): State<HubState>,
) -> Result<StatusCode, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state.clipboard_history.write().await.clear();
    *state.last_clipboard_hash.write().await = None;
    *state.last_clipboard_at.write().await = None;
    notify_clipboard_history(&state).await;
    info!("clipboard history cleared");
    Ok(StatusCode::OK)
}

async fn api_clipboard_delete(
    headers: HeaderMap,
    State(state): State<HubState>,
    Json(body): Json<ClipboardDeleteBody>,
) -> Result<StatusCode, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if body.hashes.is_empty() {
        return Ok(StatusCode::OK);
    }
    let to_remove: std::collections::HashSet<String> = body.hashes.iter().cloned().collect();
    let mut hist = state.clipboard_history.write().await;
    hist.retain(|e| !to_remove.contains(&e.hash));
    let last = state.last_clipboard_hash.read().await.clone();
    if last.as_ref().is_some_and(|h| to_remove.contains(h)) {
        *state.last_clipboard_hash.write().await = hist.front().map(|e| e.hash.clone());
        *state.last_clipboard_at.write().await = hist.front().map(|e| e.at);
    }
    drop(hist);
    notify_clipboard_history(&state).await;
    info!("clipboard history deleted {} item(s)", to_remove.len());
    Ok(StatusCode::OK)
}

async fn api_topology_post(
    headers: HeaderMap,
    State(state): State<HubState>,
    Json(body): Json<PoolTopology>,
) -> Result<StatusCode, StatusCode> {
    if !request_authorized(&headers, &state) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    save_topology(&state, body.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    broadcast_runtime_topology(&state)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    info!("topology saved and broadcast");
    Ok(StatusCode::OK)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<HubState>,
) -> impl IntoResponse {
    let authenticated_node = match authenticated_node(&headers, &state) {
        Ok(identity) => identity,
        Err(status) => return (status, "invalid node credentials").into_response(),
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, authenticated_node))
}

async fn handle_socket(socket: WebSocket, state: HubState, authenticated_node: AuthenticatedNode) {
    if let Err(err) = run_session(socket, state, authenticated_node).await {
        error!("session ended: {err:#}");
    }
}

async fn run_session(
    mut socket: WebSocket,
    state: HubState,
    authenticated_node: AuthenticatedNode,
) -> Result<()> {
    let (tx, mut rx) = broadcast::channel::<String>(256);
    let mut node_name: Option<String> = None;
    let mut credential_check = tokio::time::interval(std::time::Duration::from_secs(5));

    let result: Result<()> = async {
    loop {
        tokio::select! {
            maybe_in = socket.recv() => {
                let Some(frame) = maybe_in else { break; };
                match frame? {
                    WsMessage::Text(text) => {
                        if let Some(name) = node_name.as_deref() {
                            handle_message(&state, name, &text, &tx).await?;
                        } else {
                            node_name = register_node(
                                &state,
                                &text,
                                tx.clone(),
                                &authenticated_node.name,
                            )
                            .await?;
                            info!("node registered: {}", node_name.as_deref().unwrap_or("?"));
                        }
                    }
                    WsMessage::Ping(payload) => {
                        socket.send(WsMessage::Pong(payload)).await?;
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
            Ok(outgoing) = rx.recv() => {
                socket.send(WsMessage::Text(outgoing.into())).await?;
            }
            _ = credential_check.tick() => {
                if !node_identity_still_valid(&authenticated_node, &state) {
                    warn!("node credentials revoked or rotated out: {}", authenticated_node.name);
                    break;
                }
            }
        }
    }

        Ok(())
    }.await;

    if let Some(name) = node_name {
        unregister_node(&state, &name).await;
        info!("node disconnected: {name}");
    }
    result
}

/// Applique géométrie écran / bureau (connexion initiale ou hotplug HDMI).
async fn apply_hello_geometry(
    state: &HubState,
    node: &str,
    screen: &ScreenInfo,
    kvm_desktop: &poolsync_core::KvmDesktopInfo,
    kvm_enabled: bool,
) -> Result<PoolTopology> {
    let topology_update = {
        let mut topo = state.topology.write().await;
        let mut geometry_changed = true;
        match topo.nodes.get_mut(node) {
            Some(n) => {
                geometry_changed = n.kvm_enabled != kvm_enabled
                    || n.width != screen.width
                    || n.height != screen.height;
                if n.width != screen.width || n.height != screen.height {
                    info!(
                        "topology {node}: {}x{} → {}x{}",
                        n.width, n.height, screen.width, screen.height
                    );
                    n.width = screen.width;
                    n.height = screen.height;
                }
                n.kvm_enabled = kvm_enabled;
                n.monitor_x = kvm_desktop.monitor_x;
                n.monitor_y = kvm_desktop.monitor_y;
                n.desktop_x = kvm_desktop.desktop_x;
                n.desktop_y = kvm_desktop.desktop_y;
                n.desktop_width = kvm_desktop.desktop_width;
                n.desktop_height = kvm_desktop.desktop_height;
                // Pause locale : garder x/y mosaïque. Clip-only à l'init est déjà à y=100000.
                if !kvm_enabled && n.y < 50_000 && geometry_changed {
                    info!(
                        "topology {node}: KVM off (pause) — bords recalculés, position conservée"
                    );
                }
            }
            None => {
                let (x, y) = if kvm_enabled {
                    (
                        topo.nodes
                            .values()
                            .filter(|n| n.kvm_enabled)
                            .map(|n| n.x + n.width as i32)
                            .max()
                            .unwrap_or(0),
                        0,
                    )
                } else {
                    (0, 100_000)
                };
                info!(
                    "topology: nouveau nœud {node} ({}x{}) @ ({x},{y}) kvm={kvm_enabled}",
                    screen.width, screen.height
                );
                topo.nodes.insert(
                    node.to_string(),
                    TopologyNode {
                        x,
                        y,
                        width: screen.width,
                        height: screen.height,
                        kvm_enabled,
                        neighbors: HashMap::new(),
                        monitor_x: kvm_desktop.monitor_x,
                        monitor_y: kvm_desktop.monitor_y,
                        desktop_x: kvm_desktop.desktop_x,
                        desktop_y: kvm_desktop.desktop_y,
                        desktop_width: kvm_desktop.desktop_width,
                        desktop_height: kvm_desktop.desktop_height,
                    },
                );
            }
        }
        if geometry_changed {
            *topo = infer_neighbors(&topo, DEFAULT_EDGE_TOLERANCE_PX);
        }
        // Keep mutation and persistence under one lock: concurrent logins must
        // never replace each other's device with a stale cloned snapshot.
        if let Err(error) = write_topology_file(state, &topo) {
            warn!("topology save: {error:#}");
        }
        topo.clone()
    };
    Ok(topology_update)
}

async fn register_node(
    state: &HubState,
    text: &str,
    sender: broadcast::Sender<String>,
    authenticated_node: &str,
) -> Result<Option<String>> {
    let msg = decode_message(text)?;
    match msg {
        Message::Hello {
            node,
            mode,
            screen,
            neighbors,
            kvm_enabled,
            kvm_desktop,
            clipboard_sync,
            local_active,
            monitors,
        } => {
            if node != authenticated_node {
                return Err(anyhow!(
                    "hello node {node:?} does not match authenticated identity {authenticated_node:?}"
                ));
            }
            let topology_update =
                apply_hello_geometry(state, &node, &screen, &kvm_desktop, kvm_enabled).await?;

            {
                let mut nodes = state.nodes.write().await;
                nodes.insert(
                    node.clone(),
                    NodeInfo {
                        mode,
                        screen,
                        neighbors,
                        kvm_enabled,
                        connected_at: now_secs(),
                        clipboard_sync,
                        local_active,
                        monitors,
                        sender: sender.clone(),
                    },
                );
            }

            let _ = topology_update;
            broadcast_runtime_topology(state).await?;

            let owner = {
                let input_owner = state.input_owner.read().await.clone();
                if input_owner.is_some() {
                    input_owner
                } else {
                    state.master.read().await.clone()
                }
            };
            if let Some(owner_node) = owner {
                let payload = encode_message(&Message::MasterChanged { node: owner_node })?;
                let _ = sender.send(payload);
            }
            let revision = *state.clipboard_history_revision.read().await;
            if revision > 0 {
                let payload = encode_message(&Message::ClipboardHistoryUpdated { revision })?;
                let _ = sender.send(payload);
            }
            Ok(Some(node))
        }
        _ => Err(anyhow!("first message must be hello")),
    }
}

// Runtime routes exclude unavailable devices while saved desk positions stay intact.
fn available_topology(saved: &PoolTopology, online: &HashMap<String, NodeInfo>) -> PoolTopology {
    let mut topology = saved.clone();
    for (name, node) in &mut topology.nodes {
        node.kvm_enabled &= online.get(name).is_some_and(|info| {
            info.kvm_enabled && info.local_active && info.mode == AgentMode::Full
        });
    }
    infer_neighbors(&topology, DEFAULT_EDGE_TOLERANCE_PX)
}

async fn broadcast_runtime_topology(state: &HubState) -> Result<()> {
    let saved = state.topology.read().await.clone();
    let runtime = available_topology(&saved, &*state.nodes.read().await);
    broadcast_all(
        state,
        &encode_message(&Message::TopologyUpdate { topology: runtime })?,
    )
    .await;
    Ok(())
}

async fn release_unavailable_master(state: &HubState) -> Result<()> {
    let online = state.nodes.read().await;
    let active = |node: &str| {
        online.get(node).is_some_and(|info| {
            info.mode == AgentMode::Full && info.kvm_enabled && info.local_active
        })
    };
    let mut master = state.master.write().await;
    let mut owner = state.input_owner.write().await;
    let changed = master.as_deref().is_some_and(|node| !active(node))
        || owner.as_deref().is_some_and(|node| !active(node));
    if changed {
        *master = None;
        *owner = None;
    }
    drop(owner);
    drop(master);
    drop(online);
    if changed {
        broadcast_all(
            state,
            &encode_message(&Message::MasterChanged {
                node: String::new(),
            })?,
        )
        .await;
    }
    Ok(())
}

async fn active_kvm_node(state: &HubState, node: &str) -> bool {
    state
        .nodes
        .read()
        .await
        .get(node)
        .is_some_and(|info| info.mode == AgentMode::Full && info.kvm_enabled && info.local_active)
}

async fn unregister_node(state: &HubState, node: &str) {
    state.nodes.write().await.remove(node);
    if let Err(error) = release_unavailable_master(state).await {
        warn!("master release after disconnect: {error:#}");
    }
    if let Err(error) = broadcast_runtime_topology(state).await {
        warn!("topology update after disconnect: {error:#}");
    }
}

async fn handle_message(
    state: &HubState,
    from: &str,
    text: &str,
    local_tx: &broadcast::Sender<String>,
) -> Result<()> {
    let msg = decode_message(text)?;
    match msg {
        Message::Clipboard {
            msg_id,
            hash,
            mime,
            data,
            origin,
            seq,
        } => {
            if state.require_e2e {
                warn!("unencrypted clipboard rejected from {from}");
                return Ok(());
            }
            let duplicate = {
                let last = state.last_clipboard_hash.read().await;
                last.as_deref() == Some(&hash)
            };
            if !duplicate {
                *state.last_clipboard_hash.write().await = Some(hash.clone());
            }
            *state.last_clipboard_at.write().await = Some(now_secs());
            // Toujours remonter en tête (même hash recopié) ; broadcast seulement si nouveau.
            push_clipboard_history(state, from, &hash, &mime, &data).await;
            notify_clipboard_history(state).await;
            if duplicate {
                return Ok(());
            }

            let payload = encode_message(&Message::Clipboard {
                msg_id,
                hash,
                mime,
                data,
                origin: relay_origin(origin, from),
                seq,
            })?;
            broadcast_except(state, from, &payload).await;
        }
        Message::EncryptedClipboard { msg_id, .. } => {
            // E2E mode: the hub deliberately cannot inspect hash, MIME, data or
            // preview. It only relays the authenticated ciphertext unchanged.
            info!("encrypted clipboard relayed id={msg_id} from={from}");
            broadcast_except(state, from, text).await;
        }
        Message::MasterClaim { node, ts: _ } => {
            if node != from || !active_kvm_node(state, from).await {
                return Ok(());
            }
            let changed = {
                let mut owner = state.input_owner.write().await;
                let changed = owner.as_deref() != Some(node.as_str());
                *owner = Some(node.clone());
                changed
            };
            if changed {
                *state.master.write().await = Some(node.clone());
                let payload = encode_message(&Message::MasterChanged { node })?;
                broadcast_all(state, &payload).await;
            }
        }
        Message::Input { target, kind } => {
            if !active_kvm_node(state, from).await || !active_kvm_node(state, &target).await {
                return Ok(());
            }
            let payload = encode_message(&Message::Input {
                target: target.clone(),
                kind,
            })?;
            route_to_node(state, &target, &payload).await;
        }
        Message::SwitchTo {
            node,
            x,
            y,
            input_node,
        } => {
            let input = if input_node.is_empty() {
                from.to_string()
            } else {
                input_node
            };
            if !active_kvm_node(state, from).await
                || !active_kvm_node(state, &node).await
                || !active_kvm_node(state, &input).await
            {
                return Ok(());
            }
            let payload = encode_message(&Message::SwitchTo {
                node: node.clone(),
                x,
                y,
                input_node: input.clone(),
            })?;
            route_to_node(state, &node, &payload).await;
            broadcast_all(state, &payload).await;
            *state.input_owner.write().await = Some(input.clone());
            let mut master = state.master.write().await;
            if master.as_deref() != Some(input.as_str()) {
                *master = Some(input.clone());
                broadcast_all(
                    state,
                    &encode_message(&Message::MasterChanged { node: input })?,
                )
                .await;
            }
        }
        Message::Hello {
            node,
            mode,
            screen,
            neighbors,
            kvm_enabled,
            kvm_desktop,
            clipboard_sync,
            local_active,
            monitors,
        } => {
            // Mise à jour hotplug (HDMI etc.) — même message Hello après la connexion initiale.
            if node != from {
                warn!("hello update ignored: node={node} from={from}");
                return Ok(());
            }
            apply_hello_geometry(state, &node, &screen, &kvm_desktop, kvm_enabled).await?;
            {
                let mut nodes = state.nodes.write().await;
                if let Some(info) = nodes.get_mut(&node) {
                    info.mode = mode;
                    info.screen = screen;
                    info.neighbors = neighbors;
                    info.kvm_enabled = kvm_enabled;
                    // Sans ces trois lignes, un nœud qui coupe sa synchro ou
                    // débranche un écran reste affiché comme avant : c'est le
                    // renvoi d'état de l'agent qui deviendrait inutile.
                    info.clipboard_sync = clipboard_sync;
                    info.local_active = local_active;
                    info.monitors = monitors;
                }
            }
            release_unavailable_master(state).await?;
            broadcast_runtime_topology(state).await?;
            info!(
                "screen/layout update from {from}: {}x{}",
                screen.width, screen.height
            );
        }
        Message::Ping => {
            local_tx.send(encode_message(&Message::Pong)?)?;
        }
        _ => warn!("ignored message from {from}: {msg:?}"),
    }
    Ok(())
}

async fn broadcast_except(state: &HubState, except: &str, payload: &str) {
    let nodes = state.nodes.read().await;
    for (name, info) in nodes.iter() {
        if name != except {
            let _ = info.sender.send(payload.to_string());
        }
    }
}

async fn broadcast_all(state: &HubState, payload: &str) {
    let nodes = state.nodes.read().await;
    for info in nodes.values() {
        let _ = info.sender.send(payload.to_string());
    }
}

async fn route_to_node(state: &HubState, target: &str, payload: &str) {
    let nodes = state.nodes.read().await;
    if let Some(info) = nodes.get(target) {
        let _ = info.sender.send(payload.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hub() -> HubState {
        let file = std::env::temp_dir().join(format!("poolsync-hub-{}.json", uuid::Uuid::new_v4()));
        HubState {
            token: "test-only".into(),
            node_tokens_file: None,
            require_e2e: false,
            started_at: now_secs(),
            topology_file: file,
            topology: Arc::new(RwLock::new(PoolTopology::default())),
            nodes: Arc::new(RwLock::new(HashMap::new())),
            master: Arc::new(RwLock::new(None)),
            input_owner: Arc::new(RwLock::new(None)),
            last_clipboard_hash: Arc::new(RwLock::new(None)),
            last_clipboard_at: Arc::new(RwLock::new(None)),
            clipboard_history: Arc::new(RwLock::new(VecDeque::new())),
            clipboard_history_revision: Arc::new(RwLock::new(0)),
            clipboard_events: broadcast::channel(64).0,
        }
    }

    async fn join(state: &HubState, name: &str, width: u32, mode: AgentMode, active: bool) {
        let (sender, _rx) = broadcast::channel(32);
        let message = Message::Hello {
            node: name.into(),
            mode,
            screen: ScreenInfo { width, height: 600 },
            neighbors: vec![],
            kvm_enabled: mode == AgentMode::Full && active,
            kvm_desktop: Default::default(),
            clipboard_sync: true,
            local_active: active,
            monitors: vec![],
        };
        register_node(state, &encode_message(&message).unwrap(), sender, name)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_logins_cannot_erase_other_device_positions() {
        let state = test_hub();
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for number in 0..8 {
            let state = state.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                join(
                    &state,
                    &format!("desk-{number}"),
                    800,
                    AgentMode::Full,
                    true,
                )
                .await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(state.topology.read().await.nodes.len(), 8);
        let persisted: PoolTopology =
            serde_json::from_slice(&std::fs::read(&state.topology_file).unwrap()).unwrap();
        assert_eq!(persisted.nodes.len(), 8);
        let _ = std::fs::remove_file(&state.topology_file);
    }

    #[tokio::test]
    async fn departure_removes_routes_and_master_but_preserves_desk_positions() {
        let state = test_hub();
        join(&state, "a", 800, AgentMode::Full, true).await;
        join(&state, "b", 800, AgentMode::Full, true).await;
        *state.master.write().await = Some("b".into());
        *state.input_owner.write().await = Some("b".into());
        let before = state.topology.read().await.clone();
        unregister_node(&state, "b").await;
        let runtime = available_topology(&*state.topology.read().await, &*state.nodes.read().await);
        assert!(!runtime.nodes["b"].kvm_enabled);
        assert!(runtime.nodes["a"].neighbors.is_empty());
        assert_eq!(
            state.topology.read().await.nodes["b"].x,
            before.nodes["b"].x
        );
        assert!(state.master.read().await.is_none());
        join(&state, "b", 800, AgentMode::Full, true).await;
        let runtime = available_topology(&*state.topology.read().await, &*state.nodes.read().await);
        assert_eq!(runtime.nodes["a"].neighbors["right"], "b");
        let _ = std::fs::remove_file(&state.topology_file);
    }

    #[tokio::test]
    async fn resizing_an_active_screen_removes_a_stale_edge() {
        let state = test_hub();
        join(&state, "a", 800, AgentMode::Full, true).await;
        join(&state, "b", 800, AgentMode::Full, true).await;
        apply_hello_geometry(
            &state,
            "a",
            &ScreenInfo {
                width: 1200,
                height: 600,
            },
            &Default::default(),
            true,
        )
        .await
        .unwrap();
        assert!(state.topology.read().await.nodes["a"].neighbors.is_empty());
        assert_eq!(state.topology.read().await.nodes["b"].x, 800);
        let _ = std::fs::remove_file(&state.topology_file);
    }

    #[tokio::test]
    async fn clipboard_only_and_paused_nodes_cannot_claim_the_keyboard() {
        let state = test_hub();
        join(&state, "clip", 800, AgentMode::ClipboardOnly, true).await;
        join(&state, "paused", 800, AgentMode::Full, false).await;
        let (sender, _rx) = broadcast::channel(32);
        for name in ["clip", "paused"] {
            let text = encode_message(&Message::MasterClaim {
                node: name.into(),
                ts: 0,
            })
            .unwrap();
            handle_message(&state, name, &text, &sender).await.unwrap();
        }
        assert!(state.master.read().await.is_none());
        let _ = std::fs::remove_file(&state.topology_file);
    }

    #[test]
    fn status_auth_rejects_missing_or_invalid_credentials() {
        let headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert_ne!(bearer_token(&headers), Some("secret"));
    }

    #[test]
    fn status_auth_accepts_bearer_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("secret"));
    }

    #[test]
    fn node_credentials_support_rotation_and_revocation() {
        let mut credential = NodeCredential {
            token: "new".into(),
            previous_tokens: vec!["old".into()],
            revoked: false,
        };
        assert!(credential_accepts(&credential, "new"));
        assert!(credential_accepts(&credential, "old"));
        assert!(!credential_accepts(&credential, "other"));
        credential.revoked = true;
        assert!(!credential_accepts(&credential, "new"));
    }

    #[test]
    fn relaying_keeps_the_node_where_the_copy_actually_happened() {
        // work-b a copié, work-a relaie au hub : l'auteur reste work-b.
        assert_eq!(relay_origin("work-b".into(), "work-a"), "work-b");
    }

    #[test]
    fn a_copy_from_an_older_agent_is_attributed_to_the_sending_node() {
        assert_eq!(relay_origin(String::new(), "desk-b"), "desk-b");
    }

    #[test]
    fn the_hub_clock_is_in_milliseconds_so_it_outranks_agent_clocks_of_the_same_epoch() {
        assert!(now_ms() >= now_secs() * 1_000);
    }
}
