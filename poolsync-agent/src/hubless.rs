//! Direct peer control. The mesh actor owns this object; no dedicated leader
//! coordinates membership or layout, and the input lease expires locally.

use crate::state::AgentState;
use anyhow::{Context, Result};
use poolsync_core::{
    infer_neighbors,
    mesh::{self, Control, Event, LeaseAnswer, LeaseQuery, Packet, Presence},
    Direction, InputKind, Message, PoolTopology, TopologyNode, DEFAULT_EDGE_TOLERANCE_PX,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct LayoutDocument {
    pub revision: u64,
    pub origin: String,
    pub topology: PoolTopology,
}

fn layout_path(state: &AgentState) -> PathBuf {
    state.config_path.with_extension("topology.json")
}
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn load_layout(state: &AgentState) -> LayoutDocument {
    std::fs::read(layout_path(state))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn save_json(path: &std::path::Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path.with_extension("json.new");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    f.write_all(&serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

pub struct Hubless {
    state: Arc<AgentState>,
    control: Control,
    layout: LayoutDocument,
    boot: String,
    seq: u64,
    started: Instant,
    allowed: HashSet<String>,
    held_keys: HashSet<u32>,
    held_buttons: HashSet<u8>,
    applied_session: Option<(String, u64, String)>,
    lease_queries: HashMap<String, (String, LeaseQuery)>,
}

impl Hubless {
    pub fn new(state: Arc<AgentState>) -> Result<Self> {
        anyhow::ensure!(
            state.config.e2e_key.is_some(),
            "hubless mode requires the existing pool E2E key"
        );
        mesh::validate_key(state.config.e2e_key.as_deref().unwrap_or_default())
            .context("invalid pool E2E key for hubless mode")?;
        if !crate::kvm_wayland::active() && std::env::var_os("DISPLAY").is_some() {
            crate::kvm_x11::recover_injected_state(&state.config.node)
                .context("recovering abandoned remote inputs")?;
        }
        let mut allowed: HashSet<String> = state.config.peer_tokens.keys().cloned().collect();
        allowed.extend(state.config.neighbors.iter().map(|n| n.node.clone()));
        allowed.insert(state.config.node.clone());
        let mut layout = load_layout(&state);
        if layout.topology.nodes.is_empty() {
            // Initial installation only. Production migration imports the saved
            // topology before enabling hubless mode. Later edits use an LWW document.
            let local = node_for(
                state.config.screen,
                Default::default(),
                0,
                0,
                state.config.kvm_active(),
            );
            layout
                .topology
                .nodes
                .insert(state.config.node.clone(), local.clone());
            for n in &state.config.neighbors {
                let (x, y) = match n.direction {
                    Direction::Left => (-(local.width as i32), 0),
                    Direction::Right => (local.width as i32, 0),
                    Direction::Up => (0, -(local.height as i32)),
                    Direction::Down => (0, local.height as i32),
                };
                layout.topology.nodes.insert(
                    n.node.clone(),
                    node_for(state.config.screen, Default::default(), x, y, true),
                );
            }
            layout.revision = 1;
            layout.origin = state.config.node.clone();
            save_json(&layout_path(&state), &layout)?;
        }
        Ok(Self {
            state,
            layout,
            allowed,
            control: Control::default(),
            boot: uuid::Uuid::new_v4().to_string(),
            seq: 0,
            started: Instant::now(),
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            applied_session: None,
            lease_queries: HashMap::new(),
        })
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    fn packet(&mut self, event: Event) -> Packet {
        self.seq += 1;
        Packet {
            id: format!("{}:{}", self.boot, self.seq),
            origin: self.state.config.node.clone(),
            boot: self.boot.clone(),
            seq: self.seq,
            sent_ms: wall_ms(),
            event,
        }
    }
    fn wire(&self, p: &Packet) -> Result<String> {
        mesh::encrypt(
            p,
            self.state
                .config
                .e2e_key
                .as_deref()
                .context("pool key missing")?,
        )
    }
    fn presence(&self) -> Presence {
        Presence {
            mode: self.state.config.mode,
            screen: crate::kvm_x11::kvm_display()
                .map(|d| d.screen_info())
                .unwrap_or(self.state.config.screen),
            desktop: crate::kvm_x11::kvm_layout_snapshot().unwrap_or_default(),
            monitors: crate::kvm_x11::described_monitors().unwrap_or_default(),
            active: self.state.local_poolsync_active(),
            // Advertise local capability independently of received runtime
            // presence, but honor the persisted pool permission.
            kvm: self.state.kvm_enabled() && self.layout_allows(&self.state.config.node),
            control_clock: self.control.clock,
            lease_query: self.control.lease_query(),
            lease_answers: self
                .control
                .lease
                .as_ref()
                .filter(|l| l.owner == self.state.config.node)
                .map(|lease| {
                    self.lease_queries
                        .iter()
                        .filter_map(|(recipient, (boot, query))| {
                            if query.owner == lease.owner
                                && query.min_term <= lease.term
                                && self
                                    .control
                                    .members
                                    .get(recipient)
                                    .is_some_and(|m| m.boot == *boot && m.presence.active)
                            {
                                Some(LeaseAnswer {
                                    recipient: recipient.clone(),
                                    recipient_boot: boot.clone(),
                                    nonce: query.nonce.clone(),
                                    term: lease.term,
                                    focus: lease.focus.clone(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub async fn tick(&mut self) -> Result<Vec<String>> {
        let now = self.now();
        self.control.expire(now);
        let p = self.packet(Event::Presence {
            presence: self.presence(),
        });
        self.control.accept(&p, now);
        self.lease_queries.clear();
        let mut out = vec![self.wire(&p)?];
        if let Some(l) = self
            .control
            .lease
            .as_ref()
            .filter(|l| l.owner == self.state.config.node)
            .cloned()
        {
            let renew = self.packet(Event::Renew {
                term: l.term,
                focus: l.focus,
            });
            if self.control.accept(&renew, now) {
                out.push(self.wire(&renew)?);
            }
        }
        if let Some((topology, ack)) = self.state.take_layout_request() {
            let layout = LayoutDocument {
                revision: self.layout.revision.saturating_add(1),
                origin: self.state.config.node.clone(),
                topology,
            };
            match save_json(&layout_path(&self.state), &layout) {
                Ok(()) => {
                    self.layout = layout;
                    let _ = ack.send(Ok(()));
                }
                Err(err) => {
                    let _ = ack.send(Err(format!("layout persistence: {err:#}")));
                }
            }
        }
        // Gossip the latest document, not an edit history, on every heartbeat.
        // Returning peers receive current positions without replaying clipboard.
        let p = self.packet(Event::Layout {
            revision: self.layout.revision,
            author: self.layout.origin.clone(),
            topology: self.layout.topology.clone(),
        });
        self.control.accept(&p, now);
        out.push(self.wire(&p)?);
        self.sync().await?;
        let peers: HashMap<_,_> = self.control.members.iter().map(|(node,m)| (node.clone(), serde_json::json!({"active":m.presence.active,"kvm":m.presence.can_kvm(),"monitors":m.presence.monitors}))).collect();
        let lease = self
            .control
            .lease
            .as_ref()
            .map(|l| serde_json::json!({"owner":l.owner,"focus":l.focus,"term":l.term}));
        save_json(
            &self.state.config_path.with_extension("status.json"),
            &serde_json::json!({"hubless":true,"boot":self.boot,"peers":peers,"lease":lease,"topology":self.state.topology()}),
        )?;
        Ok(out)
    }

    pub async fn local(&mut self, message: Message) -> Result<Option<String>> {
        let event = match message {
            Message::MasterClaim { .. } if self.state.kvm_effective() => Event::Claim {
                term: self.control.next_term(),
            },
            Message::SwitchTo { node, x, y, .. } => {
                let Some(l) = self
                    .control
                    .lease
                    .as_ref()
                    .filter(|l| l.owner == self.state.config.node)
                else {
                    return Ok(None);
                };
                Event::Switch {
                    term: l.term,
                    target: node,
                    x,
                    y,
                }
            }
            Message::Input { target, kind } => {
                let Some(l) = self
                    .control
                    .lease
                    .as_ref()
                    .filter(|l| l.owner == self.state.config.node)
                else {
                    return Ok(None);
                };
                Event::Input {
                    term: l.term,
                    target,
                    kind,
                }
            }
            // Hello is replaced by a periodic presence containing no credentials.
            _ => return Ok(None),
        };
        let p = self.packet(event);
        if !self.control.accept(&p, self.now()) {
            return Ok(None);
        }
        self.apply(&p).await?;
        Ok(Some(self.wire(&p)?))
    }

    pub async fn incoming(&mut self, wire: &str) -> Result<bool> {
        let packet = mesh::decrypt(
            wire,
            self.state
                .config
                .e2e_key
                .as_deref()
                .context("pool key missing")?,
        )?;
        let wall = wall_ms();
        anyhow::ensure!(
            packet.sent_ms.abs_diff(wall) <= 5_000,
            "stale peer control or peer clock outside five-second tolerance"
        );
        if !self.allowed.contains(&packet.origin) {
            return Ok(false);
        }
        let now = self.now();
        if !self.control.accept(&packet, now) {
            if matches!(&packet.event, Event::Renew { .. }) {
                self.control.request_lease(
                    &packet,
                    &self.state.config.node,
                    &self.boot,
                    uuid::Uuid::new_v4().to_string(),
                    now,
                );
            }
            return Ok(false);
        }
        if let Event::Presence { presence } = &packet.event {
            if let Some(query) = &presence.lease_query {
                if query.owner == self.state.config.node && presence.active {
                    self.lease_queries
                        .insert(packet.origin.clone(), (packet.boot.clone(), query.clone()));
                }
            }
        }
        self.apply(&packet).await?;
        Ok(true)
    }

    async fn apply(&mut self, p: &Packet) -> Result<()> {
        if let Event::Layout {
            revision,
            author,
            topology,
        } = &p.event
        {
            anyhow::ensure!(
                topology.nodes.len() <= 64
                    && topology
                        .nodes
                        .iter()
                        .all(|(name, n)| self.allowed.contains(name)
                            && n.width > 0
                            && n.height > 0
                            && n.width <= 65535
                            && n.height <= 65535
                            && n.x.unsigned_abs() < 10_000_000
                            && n.y.unsigned_abs() < 10_000_000),
                "invalid peer layout"
            );
            if (*revision, author) > (self.layout.revision, &self.layout.origin)
                && self.allowed.contains(author)
            {
                self.layout = LayoutDocument {
                    revision: *revision,
                    origin: author.clone(),
                    topology: topology.clone(),
                };
                save_json(&layout_path(&self.state), &self.layout)?;
            }
        }
        self.sync().await?;
        match &p.event {
            Event::Switch { target, x, y, .. }
                if target == &self.state.config.node && self.state.kvm_effective() =>
            {
                let msg = Message::SwitchTo {
                    node: target.clone(),
                    x: *x,
                    y: *y,
                    input_node: p.origin.clone(),
                };
                crate::agent::handle_incoming(
                    &self.state,
                    &poolsync_core::encode_message(&msg)?,
                    &self.state.last_clip_hash_handle(),
                )
                .await?;
            }
            Event::Input { target, kind, .. }
                if target == &self.state.config.node && self.state.kvm_effective() =>
            {
                self.state.note_kvm_inject(kind);
                if let InputKind::Key { keycode, .. } = kind {
                    if !crate::kvm_wayland::active() {
                        anyhow::ensure!((8..=255).contains(keycode), "invalid remote X11 keycode");
                    }
                }
                // Confirm the recovery marker before sending any down event.
                // Confirm the up event before clearing its marker. A process
                // dying between these steps leaves an idempotent release.
                match kind {
                    InputKind::Key {
                        keycode,
                        pressed: true,
                    } => {
                        self.held_keys.insert(*keycode);
                        self.store_held()?;
                    }
                    InputKind::MouseButton {
                        button,
                        pressed: true,
                        ..
                    } => {
                        anyhow::ensure!(*button != 0, "invalid remote mouse button");
                        self.held_buttons.insert(*button);
                        self.store_held()?;
                    }
                    InputKind::MouseWheel { delta, .. } => {
                        self.held_buttons.insert(if *delta > 0 { 4 } else { 5 });
                        self.store_held()?;
                    }
                    _ => {}
                }
                crate::kvm::inject_input(kind).await?;
                match kind {
                    InputKind::Key {
                        keycode,
                        pressed: false,
                    } => {
                        self.held_keys.remove(keycode);
                        self.store_held()?;
                    }
                    InputKind::MouseButton {
                        button,
                        pressed: false,
                        ..
                    } => {
                        self.held_buttons.remove(button);
                        self.store_held()?;
                    }
                    InputKind::MouseWheel { delta, .. } => {
                        self.held_buttons.remove(&(if *delta > 0 { 4 } else { 5 }));
                        self.store_held()?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn layout_allows(&self, node: &str) -> bool {
        self.layout
            .topology
            .nodes
            .get(node)
            .is_none_or(|n| n.kvm_enabled)
    }

    async fn sync(&mut self) -> Result<()> {
        self.state
            .set_layout_kvm_allowed(self.layout_allows(&self.state.config.node));
        if self
            .control
            .lease
            .as_ref()
            .is_some_and(|l| !self.layout_allows(&l.owner) || !self.layout_allows(&l.focus))
        {
            self.control.lease = None;
        }
        self.state.set_peer_monitors(
            self.control
                .members
                .iter()
                .map(|(name, m)| (name.clone(), m.presence.monitors.clone()))
                .collect(),
        );
        let mut topology = self.layout.topology.clone();
        for node in topology.nodes.values_mut() {
            node.kvm_enabled = false;
        }
        for (name, m) in &self.control.members {
            let (x, y) = topology
                .nodes
                .get(name)
                .map(|n| (n.x, n.y))
                .unwrap_or((0, 0));
            topology.nodes.insert(
                name.clone(),
                node_for(
                    m.presence.screen,
                    m.presence.desktop,
                    x,
                    y,
                    m.presence.can_kvm() && self.layout_allows(name),
                ),
            );
        }
        self.state.set_topology(infer_neighbors(
            &poolsync_core::adapt_layout_geometry(&self.layout.topology, &topology),
            DEFAULT_EDGE_TOLERANCE_PX,
        ));
        let session = self
            .control
            .lease
            .as_ref()
            .map(|l| (l.owner.clone(), l.term, l.focus.clone()));
        if session != self.applied_session {
            self.release_held().await?;
            self.applied_session = session;
        }
        if let Some(l) = &self.control.lease {
            self.state.set_master(&l.owner);
            self.state.set_kvm_input_node(&l.owner);
            self.state.set_kvm_focus(&l.focus);
        } else {
            self.state.set_master("—");
            self.state.set_kvm_input_node(&self.state.config.node);
            self.state.set_kvm_focus(&self.state.config.node);
        }
        Ok(())
    }

    fn store_held(&self) -> Result<()> {
        if crate::kvm_wayland::active() {
            return Ok(());
        }
        crate::kvm_x11::store_injected_state(
            &self.state.config.node,
            &self.held_keys,
            &self.held_buttons,
        )
    }

    async fn release_held(&mut self) -> Result<()> {
        for keycode in self.held_keys.clone() {
            crate::kvm::inject_input(&InputKind::Key {
                keycode,
                pressed: false,
            })
            .await?;
            self.held_keys.remove(&keycode);
            self.store_held()?;
        }
        for button in self.held_buttons.clone() {
            if crate::kvm_wayland::active() {
                let (x, y) = crate::kvm_x11::mouse_location().unwrap_or((0, 0));
                crate::kvm::inject_input(&InputKind::MouseButton {
                    button,
                    pressed: false,
                    x,
                    y,
                })
                .await?;
            } else {
                // A release does not need coordinates and must not warp an
                // already recovered local pointer.
                crate::kvm_x11::mouse_button(button, false)?;
            }
            self.held_buttons.remove(&button);
            self.store_held()?;
        }
        Ok(())
    }
}

fn node_for(
    screen: poolsync_core::ScreenInfo,
    d: poolsync_core::KvmDesktopInfo,
    x: i32,
    y: i32,
    kvm_enabled: bool,
) -> TopologyNode {
    let size = d.desktop_size(screen);
    TopologyNode {
        x,
        y,
        width: size.width,
        height: size.height,
        kvm_enabled,
        neighbors: HashMap::new(),
        monitor_x: d.monitor_x,
        monitor_y: d.monitor_y,
        desktop_x: d.desktop_x,
        desktop_y: d.desktop_y,
        desktop_width: d.desktop_width,
        desktop_height: d.desktop_height,
    }
}
