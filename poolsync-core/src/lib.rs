pub mod mesh;
mod topology;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub use topology::{
    adapt_layout_geometry, infer_neighbors, layout_scale, snap_position, DEFAULT_EDGE_TOLERANCE_PX,
    DEFAULT_SNAP_GRID_PX,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Full,
    ClipboardOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighbor {
    pub direction: Direction,
    pub node: String,
    /// WebSocket direct LAN/VPN vers le voisin (clipboard sans relay hub).
    #[serde(default)]
    pub peer_url: Option<String>,
    /// Secours si peer_url (LAN) injoignable — for example, the neighbor VPN address.
    #[serde(default)]
    pub peer_url_vpn: Option<String>,
    /// Jeton propre au voisin pour authentifier ses connexions entrantes.
    /// Vide, le jeton historique du pool reste accepté pendant la migration.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Ancien jeton encore accepté pendant une rotation sans interruption.
    #[serde(default)]
    pub previous_auth_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub node: String,
    /// Use direct peers for presence, topology and KVM; never contact a hub.
    #[serde(default)]
    pub hubless: bool,
    pub hub_url: String,
    pub token: String,
    /// Identité secrète propre à ce nœud. Vide = jeton partagé historique.
    #[serde(default)]
    pub node_token: Option<String>,
    /// Clef de groupe base64 (32 octets) pour chiffrer le presse-papiers E2E.
    /// Le hub ne reçoit alors jamais le contenu en clair.
    #[serde(default)]
    pub e2e_key: Option<String>,
    /// Jetons publics au sein du pool (un secret attendu par pair), indexés par nœud.
    #[serde(default)]
    pub peer_tokens: HashMap<String, String>,
    /// Jetons précédents acceptés pendant une rotation.
    #[serde(default)]
    pub previous_peer_tokens: HashMap<String, String>,
    pub mode: AgentMode,
    pub screen: ScreenInfo,
    #[serde(default)]
    pub neighbors: Vec<Neighbor>,
    #[serde(default = "default_edge_px")]
    pub edge_px: u32,
    #[serde(default = "default_poll_ms")]
    pub clipboard_poll_ms: u64,
    #[serde(default = "default_input_poll_ms")]
    pub input_poll_ms: u64,
    /// Let native RDP own the client clipboard while its session is active.
    #[serde(default = "default_true")]
    pub pause_clipboard_when_rdp: bool,
    /// Display X11 (ex. ":10" pour session xrdp). Vide = auto via poolsync-agent-launch.sh
    #[serde(default)]
    pub display: Option<String>,
    /// Clavier/souris KVM (bords d'écran). Défaut : true si mode full, false si clipboard_only.
    #[serde(default)]
    pub kvm_enabled: Option<bool>,
    /// Capture souris/bords (primary Barrier). False = injection seule sur ce nœud.
    #[serde(default)]
    pub kvm_capture: Option<bool>,
    /// Nombre d'entrées presse-papiers dans le menu systray.
    #[serde(default = "default_tray_history_count")]
    pub tray_history_count: u32,
    /// Port d'écoute WS peer-to-peer (clipboard direct entre voisins).
    #[serde(default = "default_peer_listen_port")]
    pub peer_listen_port: u16,
    /// Certificat PEM du listener peer. Les deux champs activent WSS entrant.
    #[serde(default)]
    pub peer_tls_cert: Option<String>,
    #[serde(default)]
    pub peer_tls_key: Option<String>,
    /// Active le mesh clipboard direct vers les voisins configurés.
    #[serde(default = "default_true")]
    pub peer_direct_clipboard: bool,
    /// Relayer le presse-papiers via le legacy hub. False = peer mesh only (pas d'upload blob vers le VPS).
    #[serde(default = "default_true")]
    pub hub_clipboard: bool,
    /// true = synchroniser aussi text/html (gras, etc.). false = texte brut uniquement.
    #[serde(default)]
    pub keep_formatting: bool,
    /// Double-clic dans l'historique presse-papiers → coller sur ce poste.
    #[serde(default)]
    pub history_double_click_paste: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PoolTopology {
    #[serde(default)]
    pub nodes: HashMap<String, TopologyNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyNode {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_true")]
    pub kvm_enabled: bool,
    #[serde(default)]
    pub neighbors: HashMap<String, String>,
    /// Moniteur primaire KVM (position X11 absolue).
    #[serde(default)]
    pub monitor_x: i32,
    #[serde(default)]
    pub monitor_y: i32,
    /// Origine du bureau X11 complet (tous écrans).
    #[serde(default)]
    pub desktop_x: i32,
    #[serde(default)]
    pub desktop_y: i32,
    /// Bureau X11 complet (tous écrans) — souris distante peut aller sur HDMI.
    #[serde(default)]
    pub desktop_width: u32,
    #[serde(default)]
    pub desktop_height: u32,
}

/// Géométrie bureau / moniteur primaire (partagée hub ↔ agents).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct KvmDesktopInfo {
    pub monitor_x: i32,
    pub monitor_y: i32,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub desktop_width: u32,
    pub desktop_height: u32,
}

impl KvmDesktopInfo {
    pub fn desktop_bounds(&self, primary: ScreenInfo) -> KvmDisplayRect {
        let (x, y, w, h) = if self.desktop_width > 0 && self.desktop_height > 0 {
            (
                self.desktop_x,
                self.desktop_y,
                self.desktop_width,
                self.desktop_height,
            )
        } else {
            (
                self.monitor_x,
                self.monitor_y,
                primary.width,
                primary.height,
            )
        };
        KvmDisplayRect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    pub fn primary_bounds(&self, primary: ScreenInfo) -> KvmDisplayRect {
        KvmDisplayRect {
            x: self.monitor_x,
            y: self.monitor_y,
            width: primary.width,
            height: primary.height,
        }
    }

    pub fn desktop_size(&self, primary: ScreenInfo) -> ScreenInfo {
        ScreenInfo {
            width: if self.desktop_width > 0 {
                self.desktop_width
            } else {
                primary.width
            },
            height: if self.desktop_height > 0 {
                self.desktop_height
            } else {
                primary.height
            },
        }
    }
}

/// Rectangle écran X11 (pool ou bureau complet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvmDisplayRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl KvmDisplayRect {
    pub fn clamp(&self, px: i32, py: i32) -> (i32, i32) {
        (
            px.clamp(self.x, self.x + self.width as i32 - 1),
            py.clamp(self.y, self.y + self.height as i32 - 1),
        )
    }

    pub fn to_local(&self, px: i32, py: i32) -> (i32, i32) {
        (px - self.x, py - self.y)
    }

    pub fn to_root(&self, lx: i32, ly: i32) -> (i32, i32) {
        (lx + self.x, ly + self.y)
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && py >= self.y
            && px < self.x + self.width as i32
            && py < self.y + self.height as i32
    }
}

impl AgentConfig {
    pub fn authentication_token(&self) -> &str {
        self.node_token.as_deref().unwrap_or(&self.token)
    }

    pub fn kvm_active(&self) -> bool {
        self.kvm_enabled
            .unwrap_or(matches!(self.mode, AgentMode::Full))
    }

    pub fn kvm_capture_active(&self) -> bool {
        self.kvm_capture.unwrap_or(self.kvm_active())
    }
}

fn default_true() -> bool {
    true
}

fn default_edge_px() -> u32 {
    5
}

fn default_poll_ms() -> u64 {
    400
}

fn default_input_poll_ms() -> u64 {
    8
}

fn default_tray_history_count() -> u32 {
    15
}

fn default_peer_listen_port() -> u16 {
    9472
}

fn default_edge_flash_ms() -> u64 {
    2500
}

/// Un moniteur physique, tel que RandR le rapporte (coordonnées du bureau X11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Nom de la sortie RandR (« eDP-1 », « HDMI-1 »…), pour l'affichage.
    #[serde(default)]
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Moniteur primaire RandR — celui où l'utilisateur travaille.
    #[serde(default)]
    pub primary: bool,
}

/// Keep a remote pointer on a real output, including staggered displays with
/// empty regions inside their bounding rectangle. Ties use stable coordinates.
pub fn clamp_pointer_to_monitors(monitors: &[MonitorInfo], x: i32, y: i32) -> (i32, i32) {
    monitors
        .iter()
        .filter(|m| {
            m.width > 0
                && m.height > 0
                && m.width <= 65535
                && m.height <= 65535
                && m.x.unsigned_abs() < 10_000_000
                && m.y.unsigned_abs() < 10_000_000
        })
        .map(|m| {
            let px = (x as i64).clamp(m.x as i64, m.x as i64 + m.width as i64 - 1) as i32;
            let py = (y as i64).clamp(m.y as i64, m.y as i64 + m.height as i64 - 1) as i32;
            let dx = (x as i64 - px as i64).unsigned_abs();
            let dy = (y as i64 - py as i64).unsigned_abs();
            (
                dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy)),
                px,
                py,
            )
        })
        .min()
        .map(|(_, px, py)| (px, py))
        .unwrap_or((x, y))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        node: String,
        mode: AgentMode,
        screen: ScreenInfo,
        neighbors: Vec<Neighbor>,
        #[serde(default)]
        kvm_enabled: bool,
        #[serde(default)]
        kvm_desktop: KvmDesktopInfo,
        /// Synchro presse-papiers active sur ce nœud (case du menu systray).
        /// Un nœud « sourd » est invisible sans cette information : c'est ce qui
        /// a coûté une matinée de diagnostic le 02/09.
        #[serde(default = "default_true")]
        clipboard_sync: bool,
        /// PoolSync actif localement (raccourci de pause Ctrl+Alt+Shift+P).
        #[serde(default = "default_true")]
        local_active: bool,
        /// Tous les moniteurs RandR actifs, pour la mosaïque multi-écrans.
        /// Vide = agent d'une version antérieure : le hub retombe sur `screen`.
        #[serde(default)]
        monitors: Vec<MonitorInfo>,
    },
    Clipboard {
        msg_id: String,
        hash: String,
        mime: String,
        data: String,
        /// Nœud où la copie a réellement eu lieu (pas le dernier relais).
        /// Vide = émetteur d'une version antérieure au horodatage logique.
        #[serde(default)]
        origin: String,
        /// Horloge logique (Lamport) de `origin` au moment de la copie.
        /// Donne un ordre total sur le mesh : plus besoin de fenêtres de grâce.
        /// 0 = message legacy, appliqué sans contrôle d'ordre.
        #[serde(default)]
        seq: u64,
    },
    /// Presse-papiers XChaCha20-Poly1305. Seuls msg_id/origin/seq sont visibles
    /// afin que le hub puisse dédupliquer et relayer sans lire le contenu.
    EncryptedClipboard {
        msg_id: String,
        origin: String,
        seq: u64,
        nonce: String,
        ciphertext: String,
    },
    /// Signal hub : l'historique presse-papiers a changé (menu systray / SSE).
    ClipboardHistoryUpdated {
        revision: u64,
    },
    MasterClaim {
        node: String,
        ts: u64,
    },
    MasterChanged {
        node: String,
    },
    Input {
        target: String,
        kind: InputKind,
    },
    SwitchTo {
        node: String,
        x: i32,
        y: i32,
        /// Machine qui possède clavier/souris physiques (modèle Barrier).
        #[serde(default)]
        input_node: String,
    },
    TopologyUpdate {
        topology: PoolTopology,
    },
    /// Le hub demande à un nœud de matérialiser ses bords KVM à l'écran.
    ///
    /// Enregistrer une topologie ne dit pas si elle correspond au terrain :
    /// on découvrait l'erreur en promenant la souris. Ce message fait clignoter
    /// la bordure concernée sur la vraie machine, avant d'y croire.
    ShowEdges {
        /// Durée d'affichage en millisecondes.
        #[serde(default = "default_edge_flash_ms")]
        duration_ms: u64,
    },
    Ping,
    Pong,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputKind {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseMoveRelative {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: u8,
        pressed: bool,
        x: i32,
        y: i32,
    },
    MouseWheel {
        delta: i32,
        x: i32,
        y: i32,
    },
    Key {
        keycode: u32,
        pressed: bool,
    },
}

pub fn hash_text(data: &str) -> String {
    hash_bytes(data.as_bytes())
}

pub fn hash_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    format!("{:x}", digest)
}

pub fn encode_message(msg: &Message) -> anyhow::Result<String> {
    Ok(serde_json::to_string(msg)?)
}

fn decode_e2e_key(encoded: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = B64.decode(encoded.trim())?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("e2e_key must be base64 encoding exactly 32 bytes"))
}

pub fn encrypt_clipboard(message: &Message, key: &str) -> anyhow::Result<Message> {
    let Message::Clipboard {
        msg_id,
        origin,
        seq,
        ..
    } = message
    else {
        anyhow::bail!("only clipboard messages can be encrypted");
    };
    let key = decode_e2e_key(key)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let plaintext = encode_message(message)?;
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|_| anyhow::anyhow!("clipboard encryption failed"))?;
    Ok(Message::EncryptedClipboard {
        msg_id: msg_id.clone(),
        origin: origin.clone(),
        seq: *seq,
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ciphertext),
    })
}

pub fn decrypt_clipboard(message: &Message, key: &str) -> anyhow::Result<Message> {
    let Message::EncryptedClipboard {
        msg_id,
        origin,
        seq,
        nonce,
        ciphertext,
    } = message
    else {
        anyhow::bail!("message is not an encrypted clipboard");
    };
    let key = decode_e2e_key(key)?;
    let nonce = B64.decode(nonce)?;
    let nonce: [u8; 24] = nonce
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid clipboard nonce"))?;
    let ciphertext = B64.decode(ciphertext)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plaintext = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| anyhow::anyhow!("clipboard authentication failed"))?;
    let decrypted = decode_message(std::str::from_utf8(&plaintext)?)?;
    match &decrypted {
        Message::Clipboard {
            msg_id: inner_id,
            origin: inner_origin,
            seq: inner_seq,
            ..
        } if inner_id == msg_id && inner_origin == origin && inner_seq == seq => Ok(decrypted),
        _ => anyhow::bail!("encrypted clipboard metadata mismatch"),
    }
}

pub fn decode_message(raw: &str) -> anyhow::Result<Message> {
    Ok(serde_json::from_str(raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_in_a_staggered_display_gap_lands_on_a_real_monitor() {
        let monitors = vec![
            MonitorInfo {
                name: "left".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 400,
                primary: true,
            },
            MonitorInfo {
                name: "right".into(),
                x: 800,
                y: 200,
                width: 600,
                height: 600,
                primary: false,
            },
        ];
        assert_eq!(clamp_pointer_to_monitors(&monitors, 1000, 100), (1000, 200));
        assert_eq!(clamp_pointer_to_monitors(&monitors, 1000, 300), (1000, 300));
        assert_eq!(clamp_pointer_to_monitors(&monitors, -1000, 50), (0, 50));
        assert_eq!(clamp_pointer_to_monitors(&[], 1000, 100), (1000, 100));
    }

    fn cfg(mode: AgentMode, kvm_enabled: Option<bool>, kvm_capture: Option<bool>) -> AgentConfig {
        AgentConfig {
            node: "n".into(),
            hubless: false,
            hub_url: "ws://x/ws".into(),
            token: "t".into(),
            node_token: None,
            e2e_key: None,
            peer_tokens: HashMap::new(),
            previous_peer_tokens: HashMap::new(),
            mode,
            screen: ScreenInfo {
                width: 100,
                height: 100,
            },
            neighbors: vec![],
            edge_px: default_edge_px(),
            clipboard_poll_ms: default_poll_ms(),
            input_poll_ms: default_input_poll_ms(),
            pause_clipboard_when_rdp: true,
            display: None,
            kvm_enabled,
            kvm_capture,
            tray_history_count: default_tray_history_count(),
            peer_listen_port: default_peer_listen_port(),
            peer_tls_cert: None,
            peer_tls_key: None,
            peer_direct_clipboard: true,
            hub_clipboard: true,
            keep_formatting: false,
            history_double_click_paste: false,
        }
    }

    #[test]
    fn kvm_active_defaults_from_mode() {
        assert!(cfg(AgentMode::Full, None, None).kvm_active());
        assert!(!cfg(AgentMode::ClipboardOnly, None, None).kvm_active());
    }

    #[test]
    fn kvm_enabled_overrides_mode() {
        assert!(!cfg(AgentMode::Full, Some(false), None).kvm_active());
        assert!(cfg(AgentMode::ClipboardOnly, Some(true), None).kvm_active());
    }

    #[test]
    fn kvm_capture_falls_back_to_kvm_active() {
        let c = cfg(AgentMode::Full, None, None);
        assert_eq!(c.kvm_capture_active(), c.kvm_active());
        assert!(!cfg(AgentMode::Full, None, Some(false)).kvm_capture_active());
    }

    #[test]
    fn node_token_overrides_shared_token_with_migration_fallback() {
        let mut config = cfg(AgentMode::Full, None, None);
        assert_eq!(config.authentication_token(), "t");
        config.node_token = Some("node-secret".into());
        assert_eq!(config.authentication_token(), "node-secret");
    }

    #[test]
    fn message_round_trip() {
        let msg = Message::Clipboard {
            msg_id: "id".into(),
            hash: "h".into(),
            mime: "text/plain".into(),
            data: "hello".into(),
            origin: "desk-a".into(),
            seq: 42,
        };
        let raw = encode_message(&msg).unwrap();
        match decode_message(&raw).unwrap() {
            Message::Clipboard {
                data,
                mime,
                origin,
                seq,
                ..
            } => {
                assert_eq!(data, "hello");
                assert_eq!(mime, "text/plain");
                assert_eq!(origin, "desk-a");
                assert_eq!(seq, 42);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn encrypted_clipboard_round_trip_hides_payload_and_detects_wrong_key() {
        let key = B64.encode([7u8; 32]);
        let wrong = B64.encode([8u8; 32]);
        let plain = Message::Clipboard {
            msg_id: "copy-1".into(),
            hash: "hash-secret".into(),
            mime: "text/plain".into(),
            data: "very secret clipboard".into(),
            origin: "desk-a".into(),
            seq: 42,
        };
        let encrypted = encrypt_clipboard(&plain, &key).unwrap();
        let wire = encode_message(&encrypted).unwrap();
        assert!(!wire.contains("very secret clipboard"));
        assert!(!wire.contains("hash-secret"));
        assert!(decrypt_clipboard(&encrypted, &wrong).is_err());
        let decrypted = decrypt_clipboard(&encrypted, &key).unwrap();
        assert_eq!(
            encode_message(&decrypted).unwrap(),
            encode_message(&plain).unwrap()
        );
    }

    /// Un agent d'une version antérieure n'envoie ni l'état de sa synchro ni
    /// ses moniteurs : le Hello doit rester décodable, et l'absence
    /// d'information ne doit pas le faire passer pour « sourd ».
    #[test]
    fn hello_from_an_older_agent_defaults_to_active() {
        let raw = r#"{"type":"hello","node":"desk-b","mode":"full",
            "screen":{"width":1366,"height":768},"neighbors":[]}"#;
        match decode_message(raw).unwrap() {
            Message::Hello {
                clipboard_sync,
                local_active,
                monitors,
                ..
            } => {
                assert!(
                    clipboard_sync,
                    "sans information, on suppose la synchro active"
                );
                assert!(local_active);
                assert!(monitors.is_empty(), "le hub retombe alors sur `screen`");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Les moniteurs font l'aller-retour, drapeau primaire compris.
    #[test]
    fn monitors_round_trip_with_their_primary_flag() {
        let msg = Message::Hello {
            node: "desk-a".into(),
            mode: AgentMode::Full,
            screen: ScreenInfo {
                width: 1344,
                height: 756,
            },
            neighbors: vec![],
            kvm_enabled: true,
            kvm_desktop: KvmDesktopInfo::default(),
            clipboard_sync: false,
            local_active: true,
            monitors: vec![
                MonitorInfo {
                    name: "eDP-1".into(),
                    x: 1920,
                    y: 614,
                    width: 1344,
                    height: 756,
                    primary: true,
                },
                MonitorInfo {
                    name: "HDMI-1".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    primary: false,
                },
            ],
        };
        let raw = encode_message(&msg).unwrap();
        match decode_message(&raw).unwrap() {
            Message::Hello {
                clipboard_sync,
                monitors,
                ..
            } => {
                assert!(!clipboard_sync, "un nœud sourd doit être visible comme tel");
                assert_eq!(monitors.len(), 2);
                assert_eq!(monitors[0].name, "eDP-1");
                assert!(monitors[0].primary);
                assert!(!monitors[1].primary);
                assert_eq!(monitors[1].width, 1920);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Un agent non encore mis à jour n'envoie ni `origin` ni `seq` : le
    /// message doit rester décodable, avec l'ordre neutre (0 / vide).
    #[test]
    fn clipboard_from_an_older_agent_still_decodes() {
        let raw =
            r#"{"type":"clipboard","msg_id":"id","hash":"h","mime":"text/plain","data":"hello"}"#;
        match decode_message(raw).unwrap() {
            Message::Clipboard { origin, seq, .. } => {
                assert!(origin.is_empty());
                assert_eq!(seq, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn clipboard_history_updated_round_trip() {
        let msg = Message::ClipboardHistoryUpdated { revision: 42 };
        let raw = encode_message(&msg).unwrap();
        match decode_message(&raw).unwrap() {
            Message::ClipboardHistoryUpdated { revision } => assert_eq!(revision, 42),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn input_kind_tag_encoding() {
        let raw = encode_message(&Message::Input {
            target: "n".into(),
            kind: InputKind::Key {
                keycode: 65,
                pressed: true,
            },
        })
        .unwrap();
        assert!(raw.contains("\"type\":\"input\""), "{raw}");
        assert!(raw.contains("\"kind\":\"key\""), "{raw}");
    }

    #[test]
    fn hash_is_stable_and_hex() {
        // SHA-256("abc")
        assert_eq!(
            hash_text("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn topology_default_is_empty() {
        assert!(PoolTopology::default().nodes.is_empty());
    }
}
