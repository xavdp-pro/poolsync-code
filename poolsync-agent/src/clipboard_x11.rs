//! Correlated, bounded X11 selection reads on one persistent client connection.
//!
//! Killing short-lived xclip readers lets X11 reuse their client/window IDs.
//! A slow owner's old reply can then arrive at the next reader as TARGETS in
//! place of PNG or text. Each request here uses a fresh window ID on a retained
//! connection, checks its notification/type, and supports cancellable INCR.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, GetPropertyReply, Property,
    SelectionNotifyEvent, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct UnsupportedTarget;
impl std::fmt::Display for UnsupportedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native selection target unsupported")
    }
}
impl std::error::Error for UnsupportedTarget {}

struct Request {
    selection: String,
    target: String,
    deadline: Instant,
    response: oneshot::Sender<Result<Vec<u8>>>,
}

struct Pending {
    selection: Atom,
    target: Atom,
    limit: usize,
    request: Request,
    incremental: bool,
    bytes: Vec<u8>,
}

static READER: OnceLock<mpsc::SyncSender<Request>> = OnceLock::new();

fn submit(
    selection: &str,
    target: &str,
    limit: Duration,
) -> Result<oneshot::Receiver<Result<Vec<u8>>>> {
    let sender = READER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(64);
        std::thread::Builder::new()
            .name("clipboard-reader".into())
            .spawn(move || worker(receiver))
            .expect("start native clipboard reader");
        sender
    });
    let (response, receiver) = oneshot::channel();
    sender
        .try_send(Request {
            selection: selection.into(),
            target: target.into(),
            deadline: Instant::now() + limit,
            response,
        })
        .map_err(|_| anyhow::anyhow!("native selection reader busy or unavailable"))?;
    Ok(receiver)
}

pub async fn read(selection: &str, target: &str, limit: Duration) -> Result<Vec<u8>> {
    let receiver = submit(selection, target, limit)?;
    tokio::time::timeout(limit, receiver)
        .await
        .context("native selection read timeout")?
        .context("native selection reader stopped")?
}

/// Used only by the dedicated SAVE_TARGETS manager thread, never a Tokio task.
pub fn read_blocking(selection: &str, target: &str, limit: Duration) -> Result<Vec<u8>> {
    read_blocking_while(selection, target, limit, || true)
}

pub fn read_blocking_while(
    selection: &str,
    target: &str,
    limit: Duration,
    still_current: impl Fn() -> bool,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + limit;
    let mut receiver = submit(selection, target, limit)?;
    loop {
        if !still_current() {
            bail!("native selection read superseded");
        }
        match receiver.try_recv() {
            Ok(result) => return result,
            Err(oneshot::error::TryRecvError::Closed) => bail!("native selection reader stopped"),
            Err(oneshot::error::TryRecvError::Empty) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => bail!("native selection read timeout"),
        }
    }
}

struct Reader {
    conn: RustConnection,
    root: Window,
    property: Atom,
    incr: Atom,
    atoms: HashMap<String, Atom>,
    names: HashMap<Atom, String>,
    pending: HashMap<Window, Pending>,
}

impl Reader {
    fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("native selection connection")?;
        let root = conn.setup().roots[screen].root;
        let property = conn.intern_atom(false, b"POOLSYNC_READ")?.reply()?.atom;
        let incr = conn.intern_atom(false, b"INCR")?.reply()?.atom;
        Ok(Self {
            conn,
            root,
            property,
            incr,
            atoms: HashMap::new(),
            names: HashMap::new(),
            pending: HashMap::new(),
        })
    }

    fn atom(&mut self, name: &str) -> Result<Atom> {
        if let Some(atom) = self.atoms.get(name) {
            return Ok(*atom);
        }
        let atom = self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
        self.atoms.insert(name.into(), atom);
        self.names.insert(atom, name.into());
        Ok(atom)
    }

    fn enqueue(&mut self, request: Request) {
        if request.response.is_closed() || Instant::now() >= request.deadline {
            let _ = request
                .response
                .send(Err(anyhow::anyhow!("native selection read expired")));
            return;
        }
        let setup = (|| -> Result<(Window, Atom, Atom)> {
            let selection = self.atom(if request.selection == "primary" {
                "PRIMARY"
            } else {
                "CLIPBOARD"
            })?;
            let target = self.atom(&request.target)?;
            let window = self.conn.generate_id()?;
            self.conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                self.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?;
            self.conn.convert_selection(
                window,
                selection,
                target,
                self.property,
                x11rb::CURRENT_TIME,
            )?;
            self.conn.flush()?;
            Ok((window, selection, target))
        })();
        match setup {
            Ok((window, selection, target)) => {
                let limit = if request.target == "TARGETS" || request.target == "TIMESTAMP" {
                    MAX_METADATA_BYTES
                } else {
                    MAX_IMAGE_BYTES
                };
                self.pending.insert(
                    window,
                    Pending {
                        selection,
                        target,
                        limit,
                        request,
                        incremental: false,
                        bytes: Vec::new(),
                    },
                );
            }
            Err(error) => {
                let _ = request.response.send(Err(error));
            }
        }
    }

    fn finish(&mut self, window: Window, result: Result<Vec<u8>>) {
        if let Some(pending) = self.pending.remove(&window) {
            let _ = self.conn.destroy_window(window);
            let _ = self.conn.flush();
            let _ = pending.request.response.send(result);
        }
    }

    fn property(&self, window: Window) -> Result<GetPropertyReply> {
        let limit = self
            .pending
            .get(&window)
            .context("unknown selection request")?
            .limit;
        Ok(self
            .conn
            .get_property(
                true,
                window,
                self.property,
                AtomEnum::ANY,
                0,
                limit.div_ceil(4) as u32,
            )?
            .reply()?)
    }

    fn selection(&mut self, event: SelectionNotifyEvent) -> Result<()> {
        let Some(pending) = self.pending.get(&event.requestor) else {
            return Ok(());
        };
        if !notification_matches(pending.selection, pending.target, self.property, &event) {
            return Ok(());
        }
        if event.property == x11rb::NONE {
            self.finish(event.requestor, Err(UnsupportedTarget.into()));
            return Ok(());
        }
        let reply = self.property(event.requestor)?;
        if reply.type_ == self.incr {
            let pending = self
                .pending
                .get_mut(&event.requestor)
                .expect("live request");
            let advertised = reply.value32().and_then(|mut words| words.next());
            if reply.format != 32 || advertised.is_none_or(|size| size as usize > pending.limit) {
                self.finish(
                    event.requestor,
                    Err(anyhow::anyhow!("invalid or oversized INCR selection")),
                );
            } else {
                pending.incremental = true;
                // get_property(delete=true) acknowledges the INCR header.
                self.conn.flush()?;
            }
            return Ok(());
        }
        let bytes = self.decode(event.requestor, reply);
        self.finish(event.requestor, bytes);
        Ok(())
    }

    fn decode(&mut self, window: Window, reply: GetPropertyReply) -> Result<Vec<u8>> {
        let pending = self
            .pending
            .get(&window)
            .context("unknown selection request")?;
        if reply.bytes_after != 0 || reply.value.len() > pending.limit {
            bail!("native selection exceeds its byte limit");
        }
        if pending.request.target == "TARGETS" {
            if reply.type_ != u32::from(AtomEnum::ATOM) || reply.format != 32 {
                bail!("native TARGETS response has the wrong type");
            }
            let mut result = String::new();
            for atom in reply.value32().context("invalid TARGETS atom list")? {
                if let std::collections::hash_map::Entry::Vacant(entry) = self.names.entry(atom) {
                    let name = self.conn.get_atom_name(atom)?.reply()?.name;
                    entry.insert(String::from_utf8_lossy(&name).into_owned());
                }
                result.push_str(&self.names[&atom]);
                result.push('\n');
            }
            return Ok(result.into_bytes());
        }
        if pending.request.target == "TIMESTAMP" {
            if reply.type_ != u32::from(AtomEnum::INTEGER) || reply.format != 32 {
                bail!("native TIMESTAMP response has the wrong type");
            }
            return Ok(reply
                .value32()
                .and_then(|mut words| words.next())
                .context("empty TIMESTAMP")?
                .to_string()
                .into_bytes());
        }
        if !self.names.contains_key(&reply.type_) {
            let name = self.conn.get_atom_name(reply.type_)?.reply()?.name;
            self.names
                .insert(reply.type_, String::from_utf8_lossy(&name).into_owned());
        }
        let text_type = self.names.get(&reply.type_).is_some_and(|name| {
            matches!(
                name.as_str(),
                "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT"
            ) || name.starts_with("text/")
        });
        if !content_type_matches(
            pending.target,
            reply.type_,
            reply.format,
            pending.request.target.starts_with("text/")
                || matches!(
                    pending.request.target.as_str(),
                    "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT"
                ),
            text_type,
        ) {
            bail!("native selection response has the wrong content type");
        }
        Ok(reply.value)
    }

    fn events(&mut self) -> Result<()> {
        for _ in 0..256 {
            let Some(event) = self.conn.poll_for_event()? else {
                break;
            };
            match event {
                Event::SelectionNotify(event) => self.selection(event)?,
                Event::PropertyNotify(event)
                    if event.atom == self.property && event.state == Property::NEW_VALUE =>
                {
                    if !self
                        .pending
                        .get(&event.window)
                        .is_some_and(|p| p.incremental)
                    {
                        continue;
                    }
                    let reply = self.property(event.window)?;
                    match self.decode(event.window, reply) {
                        Ok(bytes) => {
                            let pending = self.pending.get_mut(&event.window).expect("live INCR");
                            if bytes.is_empty() {
                                let bytes = std::mem::take(&mut pending.bytes);
                                self.finish(event.window, Ok(bytes));
                            } else if pending.bytes.len() + bytes.len() > pending.limit {
                                self.finish(
                                    event.window,
                                    Err(anyhow::anyhow!("INCR exceeds its byte limit")),
                                );
                            } else {
                                pending.bytes.extend_from_slice(&bytes);
                            }
                        }
                        Err(error) => self.finish(event.window, Err(error)),
                    }
                    self.conn.flush()?;
                }
                _ => {}
            }
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.request.response.is_closed() || Instant::now() >= p.request.deadline)
            .map(|(window, _)| *window)
            .collect();
        for window in expired {
            self.finish(
                window,
                Err(anyhow::anyhow!("native selection read timeout")),
            );
        }
        Ok(())
    }
}

fn notification_matches(
    selection: Atom,
    target: Atom,
    property: Atom,
    event: &SelectionNotifyEvent,
) -> bool {
    event.selection == selection
        && event.target == target
        && (event.property == property || event.property == x11rb::NONE)
}

fn content_type_matches(
    target: Atom,
    actual: Atom,
    format: u8,
    text_request: bool,
    text_type: bool,
) -> bool {
    format == 8 && (target == actual || (text_request && text_type))
}

fn worker(receiver: mpsc::Receiver<Request>) {
    let mut reader: Option<Reader> = None;
    loop {
        let wait = if reader.as_ref().is_some_and(|r| !r.pending.is_empty()) {
            Duration::from_millis(5)
        } else {
            Duration::from_secs(60)
        };
        match receiver.recv_timeout(wait) {
            Ok(request) => {
                if reader.is_none() {
                    match Reader::new() {
                        Ok(new) => reader = Some(new),
                        Err(error) => {
                            let _ = request.response.send(Err(error));
                            continue;
                        }
                    }
                }
                let reader = reader.as_mut().expect("initialized reader");
                reader.enqueue(request);
                for request in receiver.try_iter() {
                    reader.enqueue(request);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(active) = reader.as_mut() {
            if let Err(error) = active.events() {
                tracing::debug!("native selection connection reset: {error:#}");
                for (_, pending) in active.pending.drain() {
                    let _ = pending
                        .request
                        .response
                        .send(Err(anyhow::anyhow!("native selection connection failed")));
                }
                reader = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_old_metadata_reply_cannot_complete_a_png_request() {
        let mut event = SelectionNotifyEvent {
            response_type: 31,
            sequence: 0,
            time: 0,
            requestor: 1,
            selection: 2,
            target: 3,
            property: 4,
        };
        assert!(!notification_matches(2, 5, 4, &event));
        event.target = 5;
        assert!(notification_matches(2, 5, 4, &event));
        event.property = 7;
        assert!(!notification_matches(2, 5, 4, &event));
        event.property = 0;
        assert!(notification_matches(2, 5, 4, &event));
    }
    #[test]
    fn image_and_text_reads_reject_metadata_properties() {
        assert!(!content_type_matches(
            10,
            u32::from(AtomEnum::ATOM),
            32,
            false,
            false
        ));
        assert!(!content_type_matches(
            10,
            u32::from(AtomEnum::ATOM),
            8,
            true,
            false
        ));
        assert!(content_type_matches(10, 10, 8, false, false));
        assert!(content_type_matches(10, 11, 8, true, true));
        assert!(!content_type_matches(10, 11, 8, false, true));
    }
}
