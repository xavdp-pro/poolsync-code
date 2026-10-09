//! Encrypted peer control and deterministic, expiring input ownership.
//! All clocks below are process-local monotonic milliseconds, not wall time.

use crate::{AgentMode, InputKind, KvmDesktopInfo, MonitorInfo, PoolTopology, ScreenInfo};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const PRESENCE_TIMEOUT_MS: u64 = 4_000;
pub const CONTROL_TIMEOUT_MS: u64 = 3_000;
const DOMAIN: &[u8] = b"poolsync-peer-control-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseQuery {
    pub owner: String,
    pub min_term: u64,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseAnswer {
    pub recipient: String,
    pub recipient_boot: String,
    pub nonce: String,
    pub term: u64,
    pub focus: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub mode: AgentMode,
    pub screen: ScreenInfo,
    pub desktop: KvmDesktopInfo,
    pub monitors: Vec<MonitorInfo>,
    pub active: bool,
    pub kvm: bool,
    #[serde(default)]
    pub control_clock: u64,
    // Optional presence metadata lets older peers relay the encrypted packet
    // unchanged without understanding the reconnect handshake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_query: Option<LeaseQuery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lease_answers: Vec<LeaseAnswer>,
}

impl Presence {
    pub fn can_kvm(&self) -> bool {
        self.active && self.kvm && self.mode == AgentMode::Full
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub id: String,
    pub origin: String,
    pub boot: String,
    pub seq: u64,
    pub sent_ms: u64,
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Presence {
        presence: Presence,
    },
    Claim {
        term: u64,
    },
    Renew {
        term: u64,
        focus: String,
    },
    Switch {
        term: u64,
        target: String,
        x: i32,
        y: i32,
    },
    Input {
        term: u64,
        target: String,
        kind: InputKind,
    },
    Layout {
        revision: u64,
        author: String,
        topology: PoolTopology,
    },
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    peer_control: u8,
    nonce: String,
    ciphertext: String,
}

pub fn validate_key(key: &str) -> anyhow::Result<()> {
    crate::decode_e2e_key(key).map(|_| ())
}

pub fn encrypt(packet: &Packet, key: &str) -> anyhow::Result<String> {
    let key = crate::decode_e2e_key(key)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut nonce = [0; 24];
    OsRng.fill_bytes(&mut nonce);
    let plain = serde_json::to_vec(packet)?;
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &plain,
                aad: DOMAIN,
            },
        )
        .map_err(|_| anyhow::anyhow!("peer control encryption failed"))?;
    Ok(serde_json::to_string(&Envelope {
        peer_control: 1,
        nonce: B64.encode(nonce),
        ciphertext: B64.encode(ciphertext),
    })?)
}

pub fn decrypt(wire: &str, key: &str) -> anyhow::Result<Packet> {
    let env: Envelope = serde_json::from_str(wire)?;
    anyhow::ensure!(env.peer_control == 1, "unsupported peer control version");
    let nonce: [u8; 24] = B64
        .decode(env.nonce)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid peer control nonce"))?;
    let cipher = XChaCha20Poly1305::new((&crate::decode_e2e_key(key)?).into());
    let ciphertext = B64.decode(env.ciphertext)?;
    let plain = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: DOMAIN,
            },
        )
        .map_err(|_| anyhow::anyhow!("peer control authentication failed"))?;
    Ok(serde_json::from_slice(&plain)?)
}

#[derive(Clone)]
pub struct Member {
    pub presence: Presence,
    pub boot: String,
    pub last_seen: u64,
    presence_seq: u64,
    seen: HashSet<u64>,
    max_seq: u64,
}

#[derive(Clone, Debug)]
pub struct Lease {
    pub owner: String,
    pub boot: String,
    pub term: u64,
    pub renewed_at: u64,
    pub focus: String,
    switch_seq: u64,
    input_seq: HashMap<String, u64>,
}

#[derive(Default)]
pub struct Control {
    pub members: HashMap<String, Member>,
    pub lease: Option<Lease>,
    pub clock: u64,
    // Remember retired boots, including while the member is absent. A delayed
    // packet from a dead process must not impersonate its restarted successor.
    retired: HashMap<String, HashSet<String>>,
    boots: HashMap<String, String>,
    last_claim: (u64, String),
    pending_lease: Option<PendingLease>,
}

struct PendingLease {
    requester: String,
    requester_boot: String,
    owner_boot: String,
    query: LeaseQuery,
    requested_at: u64,
}

impl Control {
    pub fn next_term(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    pub fn expire(&mut self, now: u64) {
        if self.lease.is_some()
            || self
                .pending_lease
                .as_ref()
                .is_some_and(|p| now.saturating_sub(p.requested_at) >= CONTROL_TIMEOUT_MS)
        {
            self.pending_lease = None;
        }
        self.members
            .retain(|_, m| now.saturating_sub(m.last_seen) < PRESENCE_TIMEOUT_MS);
        if let Some(l) = &self.lease {
            if now.saturating_sub(l.renewed_at) >= CONTROL_TIMEOUT_MS
                || !self.enabled(&l.owner)
                || !self.enabled(&l.focus)
                || self.members.get(&l.owner).is_none_or(|m| m.boot != l.boot)
            {
                self.lease = None;
            }
        }
    }

    pub fn enabled(&self, node: &str) -> bool {
        self.members.get(node).is_some_and(|m| m.presence.can_kvm())
    }

    /// An expired lease needs a fresh challenge response, not a late renewal.
    pub fn request_lease(
        &mut self,
        renewal: &Packet,
        requester: &str,
        requester_boot: &str,
        nonce: String,
        now: u64,
    ) -> bool {
        let Event::Renew { term, focus } = &renewal.event else {
            return false;
        };
        if self.lease.is_some()
            || self.pending_lease.is_some()
            || nonce.is_empty()
            || nonce.len() > 128
            || !self.enabled(&renewal.origin)
            || !self.enabled(focus)
            || (*term, &renewal.origin) < (self.last_claim.0, &self.last_claim.1)
            || self
                .members
                .get(&renewal.origin)
                .is_none_or(|m| m.boot != renewal.boot || renewal.seq != m.max_seq)
            || self
                .members
                .get(requester)
                .is_none_or(|m| m.boot != requester_boot || !m.presence.active)
        {
            return false;
        }
        self.pending_lease = Some(PendingLease {
            requester: requester.into(),
            requester_boot: requester_boot.into(),
            owner_boot: renewal.boot.clone(),
            query: LeaseQuery {
                owner: renewal.origin.clone(),
                min_term: *term,
                nonce,
            },
            requested_at: now,
        });
        true
    }

    pub fn lease_query(&self) -> Option<LeaseQuery> {
        self.pending_lease.as_ref().map(|p| p.query.clone())
    }

    fn accept_lease_answer(&mut self, packet: &Packet, answers: &[LeaseAnswer], now: u64) {
        let Some(pending) = self.pending_lease.as_ref() else {
            return;
        };
        if self.lease.is_some()
            || now.saturating_sub(pending.requested_at) >= CONTROL_TIMEOUT_MS
            || packet.origin != pending.query.owner
            || packet.boot != pending.owner_boot
            || !self.enabled(&packet.origin)
            || self
                .members
                .get(&pending.requester)
                .is_none_or(|m| m.boot != pending.requester_boot || !m.presence.active)
        {
            return;
        }
        let Some(answer) = answers.iter().find(|a| {
            a.recipient == pending.requester
                && a.recipient_boot == pending.requester_boot
                && a.nonce == pending.query.nonce
                && a.term >= pending.query.min_term
                && (a.term, &packet.origin) >= (self.last_claim.0, &self.last_claim.1)
                && self.enabled(&a.focus)
        }) else {
            return;
        };
        self.clock = self.clock.max(answer.term);
        self.last_claim = (answer.term, packet.origin.clone());
        self.lease = Some(Lease {
            owner: packet.origin.clone(),
            boot: packet.boot.clone(),
            term: answer.term,
            renewed_at: now,
            focus: answer.focus.clone(),
            // Input/switches queued before this fresh answer cannot be replayed.
            switch_seq: packet.seq,
            input_seq: HashMap::new(),
        });
        self.pending_lease = None;
    }

    /// Returns true for a fresh, authorized event. Consumers may then relay it.
    /// Input events are accepted only for the current lease and focused target.
    pub fn accept(&mut self, packet: &Packet, now: u64) -> bool {
        self.expire(now);
        if packet.origin.is_empty()
            || packet.boot.is_empty()
            || packet.id.is_empty()
            || packet.seq == 0
        {
            return false;
        }
        if let Event::Presence { presence } = &packet.event {
            if presence.screen.width == 0
                || presence.screen.height == 0
                || presence.screen.width > 65535
                || presence.screen.height > 65535
                || presence.monitors.len() > 32
                || presence.monitors.iter().any(|m| {
                    m.width == 0
                        || m.height == 0
                        || m.width > 65535
                        || m.height > 65535
                        || m.x.unsigned_abs() >= 10_000_000
                        || m.y.unsigned_abs() >= 10_000_000
                })
                || presence.desktop.desktop_width > 65535
                || presence.desktop.desktop_height > 65535
                || presence.lease_answers.len() > 64
                || presence.lease_query.as_ref().is_some_and(|q| {
                    q.owner.is_empty() || q.nonce.is_empty() || q.nonce.len() > 128
                })
                || [
                    presence.desktop.monitor_x,
                    presence.desktop.monitor_y,
                    presence.desktop.desktop_x,
                    presence.desktop.desktop_y,
                ]
                .iter()
                .any(|v| v.unsigned_abs() >= 10_000_000)
                || self
                    .retired
                    .get(&packet.origin)
                    .is_some_and(|boots| boots.contains(&packet.boot))
            {
                return false;
            }
            if let Some(old_boot) = self.boots.get(&packet.origin) {
                if old_boot != &packet.boot {
                    self.retired
                        .entry(packet.origin.clone())
                        .or_default()
                        .insert(old_boot.clone());
                    self.members.remove(&packet.origin);
                    if self
                        .lease
                        .as_ref()
                        .is_some_and(|l| l.owner == packet.origin)
                    {
                        self.lease = None;
                    }
                }
            }
            self.boots
                .insert(packet.origin.clone(), packet.boot.clone());
            self.clock = self.clock.max(presence.control_clock);
            let m = self
                .members
                .entry(packet.origin.clone())
                .or_insert_with(|| Member {
                    presence: presence.clone(),
                    boot: packet.boot.clone(),
                    last_seen: now,
                    presence_seq: 0,
                    max_seq: 0,
                    seen: HashSet::new(),
                });
            if packet.seq <= m.presence_seq {
                return false;
            }
            m.presence_seq = packet.seq;
            m.presence = presence.clone();
            m.last_seen = now;
        }
        let Some(m) = self.members.get_mut(&packet.origin) else {
            return false;
        };
        if m.boot != packet.boot
            || packet.seq.saturating_add(256) < m.max_seq
            || !m.seen.insert(packet.seq)
        {
            return false;
        }
        m.max_seq = m.max_seq.max(packet.seq);
        let floor = m.max_seq.saturating_sub(256);
        m.seen.retain(|s| *s >= floor);
        match &packet.event {
            Event::Presence { presence } => {
                self.accept_lease_answer(packet, &presence.lease_answers, now);
                true
            }
            Event::Layout { .. } => true,
            Event::Claim { term } => {
                if !self.enabled(&packet.origin)
                    || (*term, &packet.origin) <= (self.last_claim.0, &self.last_claim.1)
                {
                    return false;
                }
                self.clock = self.clock.max(*term);
                self.last_claim = (*term, packet.origin.clone());
                self.pending_lease = None;
                self.lease = Some(Lease {
                    owner: packet.origin.clone(),
                    boot: packet.boot.clone(),
                    term: *term,
                    renewed_at: now,
                    focus: packet.origin.clone(),
                    switch_seq: 0,
                    input_seq: HashMap::new(),
                });
                true
            }
            Event::Renew { term, focus } => {
                // A newly joined peer may learn an existing lease; an expired
                // lease already observed by this process cannot be resurrected.
                if (*term, &packet.origin) > (self.last_claim.0, &self.last_claim.1)
                    && self.enabled(&packet.origin)
                    && self.enabled(focus)
                {
                    self.clock = self.clock.max(*term);
                    self.last_claim = (*term, packet.origin.clone());
                    self.lease = Some(Lease {
                        owner: packet.origin.clone(),
                        boot: packet.boot.clone(),
                        term: *term,
                        renewed_at: now,
                        focus: focus.clone(),
                        switch_seq: packet.seq,
                        input_seq: HashMap::new(),
                    });
                    return true;
                }
                if let Some(l) = self.lease.as_mut().filter(|l| {
                    l.owner == packet.origin && l.boot == packet.boot && l.term == *term
                }) {
                    l.renewed_at = now;
                    true
                } else {
                    false
                }
            }
            Event::Switch { term, target, .. } => {
                if !self.enabled(target) {
                    return false;
                }
                if let Some(l) = self.lease.as_mut().filter(|l| {
                    l.owner == packet.origin
                        && l.boot == packet.boot
                        && l.term == *term
                        && packet.seq > l.switch_seq
                }) {
                    l.switch_seq = packet.seq;
                    l.focus = target.clone();
                    l.input_seq.clear();
                    true
                } else {
                    false
                }
            }
            Event::Input { term, target, kind } => {
                if !self.enabled(target) {
                    return false;
                }
                let Some(l) = self.lease.as_mut().filter(|l| {
                    l.owner == packet.origin
                        && l.boot == packet.boot
                        && l.term == *term
                        && l.focus == *target
                        && packet.seq > l.switch_seq
                }) else {
                    return false;
                };
                // Reordering motion must not suppress an unrelated key release.
                let stream = match kind {
                    InputKind::Key { keycode, .. } => format!("key:{keycode}"),
                    InputKind::MouseButton { button, .. } => format!("button:{button}"),
                    InputKind::MouseWheel { .. } => "wheel".into(),
                    _ => "motion".into(),
                };
                let last = l.input_seq.entry(stream).or_default();
                if packet.seq <= *last {
                    return false;
                }
                *last = packet.seq;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(node: &str, seq: u64, event: Event) -> Packet {
        Packet {
            id: format!("{node}:{seq}"),
            origin: node.into(),
            boot: "boot-1".into(),
            seq,
            sent_ms: 0,
            event,
        }
    }
    fn join(c: &mut Control, node: &str, full: bool) {
        assert!(c.accept(
            &packet(
                node,
                1,
                Event::Presence {
                    presence: Presence {
                        mode: if full {
                            AgentMode::Full
                        } else {
                            AgentMode::ClipboardOnly
                        },
                        screen: ScreenInfo {
                            width: 800,
                            height: 600
                        },
                        desktop: KvmDesktopInfo::default(),
                        monitors: vec![],
                        active: true,
                        kvm: full,
                        control_clock: 0,
                        lease_query: None,
                        lease_answers: Vec::new(),
                    }
                }
            ),
            0
        ));
    }
    #[test]
    fn simultaneous_claims_converge_in_both_arrival_orders() {
        for reverse in [false, true] {
            let mut c = Control::default();
            join(&mut c, "a", true);
            join(&mut c, "b", true);
            let a = packet("a", 2, Event::Claim { term: 1 });
            let b = packet("b", 2, Event::Claim { term: 1 });
            for p in if reverse { [&b, &a] } else { [&a, &b] } {
                c.accept(p, 10);
            }
            assert_eq!(c.lease.unwrap().owner, "b");
        }
    }
    #[test]
    fn owner_timeout_cannot_be_resurrected_by_a_late_renewal() {
        let mut c = Control::default();
        join(&mut c, "a", true);
        assert!(c.accept(&packet("a", 2, Event::Claim { term: 1 }), 1));
        c.expire(3_002);
        assert!(c.lease.is_none());
        assert!(!c.accept(
            &packet(
                "a",
                3,
                Event::Renew {
                    term: 1,
                    focus: "a".into()
                }
            ),
            3_003
        ));
        assert!(c.accept(&packet("a", 4, Event::Claim { term: 2 }), 3_004));
    }

    fn expired_remote_lease() -> Control {
        let mut c = Control::default();
        for node in ["a", "b", "c"] {
            join(&mut c, node, true);
        }
        assert!(c.accept(&packet("a", 2, Event::Claim { term: 1 }), 1));
        assert!(c.accept(
            &packet(
                "a",
                3,
                Event::Switch {
                    term: 1,
                    target: "c".into(),
                    x: 20,
                    y: 20,
                }
            ),
            2
        ));
        c.expire(3_003);
        assert!(c.lease.is_none());
        c
    }

    fn query(c: &mut Control) {
        let renewal = packet(
            "a",
            4,
            Event::Renew {
                term: 1,
                focus: "c".into(),
            },
        );
        assert!(!c.accept(&renewal, 3_004));
        assert!(c.request_lease(&renewal, "b", "boot-1", "fresh-challenge".into(), 3_004));
    }

    fn answer(c: &Control, seq: u64, nonce: &str) -> Packet {
        let mut presence = c.members["a"].presence.clone();
        presence.lease_answers = vec![LeaseAnswer {
            recipient: "b".into(),
            recipient_boot: "boot-1".into(),
            nonce: nonce.into(),
            term: 1,
            focus: "c".into(),
        }];
        packet("a", seq, Event::Presence { presence })
    }

    #[test]
    fn expired_lease_resync_needs_a_fresh_correlated_answer() {
        let mut c = expired_remote_lease();
        query(&mut c);
        assert!(c.accept(&answer(&c, 7, "previous-challenge"), 3_005));
        assert!(c.lease.is_none());
        assert!(c.accept(&answer(&c, 9, "fresh-challenge"), 3_006));
        let lease = c.lease.as_ref().unwrap();
        assert_eq!(
            (lease.owner.as_str(), lease.focus.as_str(), lease.term),
            ("a", "c", 1)
        );
        assert!(c.lease_query().is_none());
        // A reordered input queued before the answer cannot revive an old key.
        assert!(!c.accept(
            &packet(
                "a",
                8,
                Event::Input {
                    term: 1,
                    target: "c".into(),
                    kind: InputKind::Key {
                        keycode: 38,
                        pressed: true
                    },
                }
            ),
            3_007
        ));
        assert!(c.accept(
            &packet(
                "a",
                10,
                Event::Input {
                    term: 1,
                    target: "c".into(),
                    kind: InputKind::Key {
                        keycode: 38,
                        pressed: false
                    },
                }
            ),
            3_008
        ));
    }

    #[test]
    fn unsolicited_answer_cannot_resurrect_a_lease() {
        let mut c = expired_remote_lease();
        assert!(c.accept(&answer(&c, 4, "fresh-challenge"), 3_004));
        assert!(c.lease.is_none());
    }

    #[test]
    fn a_timed_out_challenge_cannot_restore_a_lease() {
        let mut c = expired_remote_lease();
        query(&mut c);
        let presences: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|node| (node, c.members[node].presence.clone()))
            .collect();
        for (node, presence) in presences {
            assert!(c.accept(
                &packet(
                    node,
                    if node == "a" { 5 } else { 2 },
                    Event::Presence { presence }
                ),
                5_000
            ));
        }
        assert!(c.accept(&answer(&c, 6, "fresh-challenge"), 6_005));
        assert!(c.lease.is_none());
    }

    #[test]
    fn restarted_recipient_and_newer_claim_fence_old_answers() {
        let mut c = expired_remote_lease();
        query(&mut c);
        let mut restart = packet(
            "b",
            1,
            Event::Presence {
                presence: c.members["b"].presence.clone(),
            },
        );
        restart.boot = "boot-2".into();
        assert!(c.accept(&restart, 3_005));
        assert!(c.accept(&answer(&c, 5, "fresh-challenge"), 3_006));
        assert!(c.lease.is_none());

        let mut c = expired_remote_lease();
        query(&mut c);
        assert!(c.accept(&packet("b", 2, Event::Claim { term: 2 }), 3_005));
        assert!(c.accept(&answer(&c, 5, "fresh-challenge"), 3_006));
        assert_eq!(c.lease.unwrap().owner, "b");
    }

    #[test]
    fn restarted_owner_cannot_answer_a_previous_process_challenge() {
        let mut c = expired_remote_lease();
        query(&mut c);
        let mut restart = packet(
            "a",
            1,
            Event::Presence {
                presence: c.members["a"].presence.clone(),
            },
        );
        restart.boot = "boot-2".into();
        assert!(c.accept(&restart, 3_005));
        let old = answer(&c, 5, "fresh-challenge");
        assert!(!c.accept(&old, 3_006));
        let mut wrong_process = answer(&c, 2, "fresh-challenge");
        wrong_process.boot = "boot-2".into();
        assert!(c.accept(&wrong_process, 3_007));
        assert!(c.lease.is_none());
    }

    #[test]
    fn answers_do_not_reset_an_uninterrupted_controller_session() {
        let mut c = expired_remote_lease();
        assert!(c.accept(&packet("a", 4, Event::Claim { term: 2 }), 3_004));
        assert!(c.accept(
            &packet(
                "a",
                5,
                Event::Switch {
                    term: 2,
                    target: "c".into(),
                    x: 20,
                    y: 20
                }
            ),
            3_005
        ));
        let before = c.lease.as_ref().unwrap().clone();
        assert!(c.accept(&answer(&c, 6, "someone-else"), 3_006));
        let after = c.lease.unwrap();
        assert_eq!(
            (
                after.owner,
                after.focus,
                after.term,
                after.switch_seq,
                after.renewed_at
            ),
            (
                before.owner,
                before.focus,
                before.term,
                before.switch_seq,
                before.renewed_at
            )
        );
    }

    #[test]
    fn resync_metadata_is_optional_for_legacy_presence() {
        let mut c = Control::default();
        join(&mut c, "a", true);
        let legacy = serde_json::to_value(&c.members["a"].presence).unwrap();
        assert!(legacy.get("lease_query").is_none() && legacy.get("lease_answers").is_none());
        let decoded: Presence = serde_json::from_value(legacy).unwrap();
        assert!(decoded.lease_query.is_none() && decoded.lease_answers.is_empty());
    }
    #[test]
    fn clipboard_only_peers_cannot_claim_or_receive_input() {
        let mut c = Control::default();
        join(&mut c, "a", true);
        join(&mut c, "c", false);
        assert!(!c.accept(&packet("c", 2, Event::Claim { term: 99 }), 1));
        assert!(c.accept(&packet("a", 2, Event::Claim { term: 1 }), 1));
        assert!(!c.accept(
            &packet(
                "a",
                3,
                Event::Switch {
                    term: 1,
                    target: "c".into(),
                    x: 1,
                    y: 1
                }
            ),
            2
        ));
    }
    #[test]
    fn old_process_and_wrong_lease_cannot_inject() {
        let mut c = Control::default();
        join(&mut c, "a", true);
        join(&mut c, "b", true);
        c.accept(&packet("a", 2, Event::Claim { term: 1 }), 1);
        c.accept(
            &packet(
                "a",
                3,
                Event::Switch {
                    term: 1,
                    target: "b".into(),
                    x: 20,
                    y: 20,
                },
            ),
            2,
        );
        let bad = packet(
            "b",
            2,
            Event::Input {
                term: 1,
                target: "b".into(),
                kind: InputKind::Key {
                    keycode: 38,
                    pressed: true,
                },
            },
        );
        assert!(!c.accept(&bad, 3));
        let mut restart = packet("a", 1, c.members["a"].presence.clone().into());
        restart.boot = "boot-2".into();
        assert!(c.accept(&restart, 4));
        assert!(c.lease.is_none());
        assert!(!c.accept(&packet("a", 5, Event::Claim { term: 2 }), 5));
    }
    impl From<Presence> for Event {
        fn from(presence: Presence) -> Self {
            Event::Presence { presence }
        }
    }
    #[test]
    fn unrelated_motion_reordering_does_not_drop_a_key_release() {
        let mut c = Control::default();
        join(&mut c, "a", true);
        join(&mut c, "b", true);
        c.accept(&packet("a", 2, Event::Claim { term: 1 }), 1);
        c.accept(
            &packet(
                "a",
                3,
                Event::Switch {
                    term: 1,
                    target: "b".into(),
                    x: 20,
                    y: 20,
                },
            ),
            2,
        );
        for (seq, kind) in [
            (
                4,
                InputKind::Key {
                    keycode: 38,
                    pressed: true,
                },
            ),
            (6, InputKind::MouseMove { x: 40, y: 40 }),
            (
                5,
                InputKind::Key {
                    keycode: 38,
                    pressed: false,
                },
            ),
        ] {
            assert!(c.accept(
                &packet(
                    "a",
                    seq,
                    Event::Input {
                        term: 1,
                        target: "b".into(),
                        kind
                    }
                ),
                3
            ));
        }
    }
    #[test]
    fn peer_encryption_hides_events_and_detects_modification() {
        let key = B64.encode([7; 32]);
        let wrong = B64.encode([8; 32]);
        let p = packet("private-machine", 1, Event::Claim { term: 1 });
        let wire = encrypt(&p, &key).unwrap();
        assert!(!wire.contains("private-machine"));
        assert!(!wire.contains("claim"));
        assert_eq!(decrypt(&wire, &key).unwrap().origin, p.origin);
        assert!(decrypt(&wire, &wrong).is_err());
        let mut env: Envelope = serde_json::from_str(&wire).unwrap();
        let mut bytes = B64.decode(&env.ciphertext).unwrap();
        bytes[0] ^= 1;
        env.ciphertext = B64.encode(bytes);
        assert!(decrypt(&serde_json::to_string(&env).unwrap(), &key).is_err());
    }
}
