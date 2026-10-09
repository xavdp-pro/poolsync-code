//! Bounded application-level clipboard fragments. Control messages keep their
//! existing format and can pass between complete binary WebSocket messages.
//! The reassembled payload still needs the existing authenticated decryption.
use anyhow::{ensure, Context, Result};
use std::time::{Duration, Instant};

pub const CAPABILITY_HEADER: &str = "x-poolsync-clipboard-fragments";
pub const CHUNK_BYTES: usize = 16 * 1024;
const HEADER_BYTES: usize = 29;
const MAGIC: &[u8; 5] = b"PSCF1";
const ACK_MAGIC: &[u8; 5] = b"PSCA1";
const ACK_BYTES: usize = 25;
const WINDOW_BYTES: usize = 256 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const STALL_LIMIT: Duration = Duration::from_secs(30);

/// Bound unsent bulk bytes without shrinking TCP flight/receive windows.
pub fn pace_socket(fd: std::os::fd::RawFd) -> Result<()> {
    let threshold = CHUNK_BYTES as libc::c_uint;
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NOTSENT_LOWAT,
            (&threshold as *const libc::c_uint).cast(),
            std::mem::size_of_val(&threshold) as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("pace peer TCP unsent queue");
    }
    Ok(())
}

pub struct Outgoing {
    id: [u8; 16],
    bytes: Vec<u8>,
    offset: usize,
    acknowledged: usize,
    progressed: Instant,
}

impl Outgoing {
    pub fn new(payload: String) -> Result<Self> {
        ensure!(
            !payload.is_empty() && payload.len() <= MAX_BYTES,
            "clipboard transfer size out of bounds"
        );
        Ok(Self {
            id: *uuid::Uuid::new_v4().as_bytes(),
            bytes: payload.into_bytes(),
            offset: 0,
            acknowledged: 0,
            progressed: Instant::now(),
        })
    }
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if !self.ready() {
            return None;
        }
        let end = (self.offset + CHUNK_BYTES).min(self.bytes.len());
        let mut frame = Vec::with_capacity(HEADER_BYTES + end - self.offset);
        frame.extend_from_slice(MAGIC);
        frame.extend_from_slice(&self.id);
        frame.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&(self.offset as u32).to_be_bytes());
        frame.extend_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Some(frame)
    }
    pub fn finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
    pub fn ready(&self) -> bool {
        !self.finished() && self.offset - self.acknowledged < WINDOW_BYTES
    }
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.progressed) >= STALL_LIMIT
    }
    pub fn acknowledge(&mut self, frame: &[u8], now: Instant) -> Result<()> {
        ensure!(is_ack(frame), "invalid clipboard acknowledgement");
        if frame[5..21] != self.id {
            return Ok(());
        }
        let offset = u32::from_be_bytes(frame[21..25].try_into()?) as usize;
        ensure!(
            offset <= self.offset,
            "clipboard acknowledgement exceeds sent bytes"
        );
        if offset > self.acknowledged {
            self.acknowledged = offset;
            self.progressed = now;
        }
        Ok(())
    }
}

pub fn is_ack(frame: &[u8]) -> bool {
    frame.len() == ACK_BYTES && &frame[..5] == ACK_MAGIC
}

/// Acknowledgements bound bulk already accepted by the remote application,
/// including buffering proxies. TCP's unsent limit alone cannot bound that.
pub fn acknowledgement(frame: &[u8]) -> Result<Vec<u8>> {
    let (id, _, offset, data) = parse_fragment(frame)?;
    let mut ack = Vec::with_capacity(ACK_BYTES);
    ack.extend_from_slice(ACK_MAGIC);
    ack.extend_from_slice(&id);
    ack.extend_from_slice(&((offset + data.len()) as u32).to_be_bytes());
    Ok(ack)
}

fn parse_fragment(frame: &[u8]) -> Result<([u8; 16], usize, usize, &[u8])> {
    ensure!(
        frame.len() > HEADER_BYTES
            && frame.len() <= HEADER_BYTES + CHUNK_BYTES
            && &frame[..5] == MAGIC,
        "invalid clipboard fragment header"
    );
    let id = frame[5..21].try_into()?;
    let total = u32::from_be_bytes(frame[21..25].try_into()?) as usize;
    let offset = u32::from_be_bytes(frame[25..29].try_into()?) as usize;
    let data = &frame[HEADER_BYTES..];
    ensure!(
        total > 0 && total <= MAX_BYTES && offset <= total && data.len() <= total - offset,
        "invalid clipboard fragment bounds"
    );
    Ok((id, total, offset, data))
}

struct Partial {
    id: [u8; 16],
    total: usize,
    bytes: Vec<u8>,
    progressed: Instant,
}

#[derive(Default)]
pub struct Incoming {
    partial: Option<Partial>,
}

impl Incoming {
    pub fn cancel(&mut self) {
        self.partial = None;
    }
    pub fn expire(&mut self, now: Instant) {
        if self
            .partial
            .as_ref()
            .is_some_and(|p| now.saturating_duration_since(p.progressed) >= STALL_LIMIT)
        {
            self.cancel();
        }
    }
    pub fn push(&mut self, frame: &[u8], now: Instant) -> Result<Option<String>> {
        self.expire(now);
        let (id, total, offset, data) = parse_fragment(frame)?;
        if offset == 0 {
            self.partial = Some(Partial {
                id,
                total,
                bytes: Vec::with_capacity(data.len()),
                progressed: now,
            });
        }
        // Tail fragments of a cancelled transfer are harmless. They cannot
        // allocate memory or revive it after departure, replacement or expiry.
        let Some(partial) = self.partial.as_mut().filter(|p| p.id == id) else {
            return Ok(None);
        };
        ensure!(
            partial.total == total && partial.bytes.len() == offset,
            "inconsistent clipboard fragment sequence"
        );
        partial.bytes.extend_from_slice(data);
        partial.progressed = now;
        if partial.bytes.len() != total {
            return Ok(None);
        }
        let completed = self.partial.take().expect("complete clipboard transfer");
        Ok(Some(
            String::from_utf8(completed.bytes).context("clipboard transfer is not UTF-8")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frames(payload: String) -> Vec<Vec<u8>> {
        let mut outgoing = Outgoing::new(payload).unwrap();
        let mut frames = vec![];
        while let Some(frame) = outgoing.next_frame() {
            outgoing
                .acknowledge(&acknowledgement(&frame).unwrap(), Instant::now())
                .unwrap();
            frames.push(frame);
        }
        frames
    }
    #[test]
    fn unacknowledged_bulk_is_bounded_and_fresh_credit_resumes_it() {
        let mut outgoing = Outgoing::new("a".repeat(WINDOW_BYTES * 2)).unwrap();
        let mut last = vec![];
        for _ in 0..WINDOW_BYTES / CHUNK_BYTES {
            last = outgoing.next_frame().unwrap();
        }
        assert!(!outgoing.ready());
        assert!(!outgoing.finished());
        assert!(outgoing.next_frame().is_none());
        let now = Instant::now();
        let ack = acknowledgement(&last).unwrap();
        outgoing.acknowledge(&ack, now).unwrap();
        assert!(outgoing.ready());
        assert!(!outgoing.expired(now + STALL_LIMIT / 2));
        assert!(outgoing.expired(now + STALL_LIMIT));
        assert!(outgoing.next_frame().is_some());
    }
    #[test]
    fn old_or_duplicate_acknowledgements_cannot_grant_extra_credit() {
        let mut outgoing = Outgoing::new("a".repeat(WINDOW_BYTES * 2)).unwrap();
        let first = outgoing.next_frame().unwrap();
        let ack = acknowledgement(&first).unwrap();
        outgoing.acknowledge(&ack, Instant::now()).unwrap();
        outgoing.acknowledge(&ack, Instant::now()).unwrap();
        assert_eq!(outgoing.acknowledged, CHUNK_BYTES);
        let mut stale = ack.clone();
        stale[5] ^= 1;
        outgoing.acknowledge(&stale, Instant::now()).unwrap();
        assert_eq!(outgoing.acknowledged, CHUNK_BYTES);
        let mut future = ack;
        future[21..25].copy_from_slice(&((CHUNK_BYTES * 2) as u32).to_be_bytes());
        assert!(outgoing.acknowledge(&future, Instant::now()).is_err());
        assert!(!is_ack(b"PSCA1"));
        assert!(acknowledgement(b"PSCF1").is_err());
    }
    #[test]
    fn reassembly_preserves_utf8_across_byte_boundaries() {
        let payload = "é🔐clipboard".repeat(CHUNK_BYTES);
        let mut incoming = Incoming::default();
        let mut complete = None;
        for frame in frames(payload.clone()) {
            complete = incoming.push(&frame, Instant::now()).unwrap();
        }
        assert_eq!(complete, Some(payload));
    }
    #[test]
    fn superseding_transfer_and_cancelled_tails_cannot_revive_old_contents() {
        let old = frames("old".repeat(CHUNK_BYTES));
        let new = frames("fresh".repeat(CHUNK_BYTES));
        let mut incoming = Incoming::default();
        incoming.push(&old[0], Instant::now()).unwrap();
        let mut complete = incoming.push(&new[0], Instant::now()).unwrap();
        assert!(incoming.push(&old[1], Instant::now()).unwrap().is_none());
        for frame in &new[1..] {
            complete = incoming.push(frame, Instant::now()).unwrap();
        }
        assert_eq!(complete, Some("fresh".repeat(CHUNK_BYTES)));
        for frame in &old[1..] {
            assert!(incoming.push(frame, Instant::now()).unwrap().is_none());
        }
    }
    #[test]
    fn participation_cancellation_and_stall_expiry_drop_incomplete_payloads() {
        let parts = frames("data".repeat(CHUNK_BYTES));
        let now = Instant::now();
        let mut incoming = Incoming::default();
        incoming.push(&parts[0], now).unwrap();
        incoming.cancel();
        for frame in &parts[1..] {
            assert!(incoming.push(frame, now).unwrap().is_none());
        }
        incoming.push(&parts[0], now).unwrap();
        incoming.expire(now + STALL_LIMIT);
        assert!(incoming
            .push(&parts[1], now + STALL_LIMIT)
            .unwrap()
            .is_none());
        assert!(incoming
            .push(&frames("fresh".into())[0], now)
            .unwrap()
            .is_some());
    }
    #[test]
    fn malformed_and_oversized_fragments_fail_before_allocation() {
        let mut incoming = Incoming::default();
        for bad in [
            vec![],
            vec![0; HEADER_BYTES],
            vec![0; HEADER_BYTES + CHUNK_BYTES + 1],
        ] {
            assert!(incoming.push(&bad, Instant::now()).is_err());
        }
        let mut frame = frames("valid".into()).remove(0);
        frame[21..25].copy_from_slice(&((MAX_BYTES + 1) as u32).to_be_bytes());
        assert!(incoming.push(&frame, Instant::now()).is_err());
        frame[21..25].copy_from_slice(&5_u32.to_be_bytes());
        frame[25..29].copy_from_slice(&6_u32.to_be_bytes());
        assert!(incoming.push(&frame, Instant::now()).is_err());
        assert!(Outgoing::new(String::new()).is_err());
    }
    #[test]
    fn gaps_duplicates_and_changed_totals_are_rejected() {
        let parts = frames("data".repeat(CHUNK_BYTES));
        let mut incoming = Incoming::default();
        incoming.push(&parts[0], Instant::now()).unwrap();
        assert!(incoming.push(&parts[2], Instant::now()).is_err());
        incoming.push(&parts[1], Instant::now()).unwrap();
        assert!(incoming.push(&parts[1], Instant::now()).is_err());
        let mut changed = parts[2].clone();
        changed[21..25].copy_from_slice(&1_u32.to_be_bytes());
        assert!(incoming.push(&changed, Instant::now()).is_err());
    }
}
