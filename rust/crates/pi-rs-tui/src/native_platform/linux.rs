use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use x11rb::connection::Connection;
use x11rb::protocol::{Event, xproto::*};
use x11rb::rust_connection::RustConnection;

use super::{ClipboardError, ClipboardRead};

const MAX_BYTES: usize = 50 * 1024 * 1024;
const TEXT_TARGETS: [&[u8]; 5] = [
    b"text/plain;charset=utf-8",
    b"text/plain;charset=UTF-8",
    b"UTF8_STRING",
    b"text/plain",
    b"STRING",
];
const IMAGE_TARGETS: [&[u8]; 6] = [
    b"image/png",
    b"image/jpeg",
    b"image/webp",
    b"image/gif",
    b"image/bmp",
    b"image/tiff",
];

static BUSY: AtomicBool = AtomicBool::new(false);

struct Worker;

impl Drop for Worker {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

pub(super) fn read(image: bool) -> Result<ClipboardRead<(Vec<u8>, bool)>, ClipboardError> {
    if BUSY.swap(true, Ordering::AcqRel) {
        return Ok(ClipboardRead::Unavailable);
    }
    let worker = Worker;
    let deadline = Instant::now() + Duration::from_secs(2);
    let (send, receive) = mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("tui-x11-clipboard".into())
        .spawn(move || {
            let result = match X11Clipboard::open(deadline) {
                Some(clipboard) => clipboard.read(image),
                None => Ok(ClipboardRead::Unavailable),
            };
            drop(worker);
            let _ = send.send(result);
        })
        .is_err()
    {
        return Ok(ClipboardRead::Unavailable);
    }
    receive
        .recv_timeout(Duration::from_secs(3))
        .unwrap_or(Ok(ClipboardRead::Unavailable))
}

struct TransferProperty {
    atom: Atom,
    format: u8,
    bytes: Vec<u8>,
}

struct X11Clipboard {
    connection: RustConnection,
    window: Window,
    clipboard: Atom,
    property: Atom,
    targets: Atom,
    incr: Atom,
    deadline: Instant,
}

impl X11Clipboard {
    fn open(deadline: Instant) -> Option<Self> {
        let (connection, screen) = RustConnection::connect(None).ok()?;
        let root = connection.setup().roots.get(screen)?.root;
        let window = connection.generate_id().ok()?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .ok()?
            .check()
            .ok()?;
        let atom = |name: &[u8]| {
            connection
                .intern_atom(false, name)
                .ok()?
                .reply()
                .ok()
                .map(|reply| reply.atom)
        };
        let (clipboard, property, targets, incr) = (
            atom(b"CLIPBOARD")?,
            atom(b"PI_CLIPBOARD")?,
            atom(b"TARGETS")?,
            atom(b"INCR")?,
        );
        Some(Self {
            connection,
            window,
            clipboard,
            property,
            targets,
            incr,
            deadline,
        })
    }

    fn atom(&self, name: &[u8]) -> Result<Atom, ClipboardError> {
        self.connection
            .intern_atom(false, name)
            .map_err(|_| ClipboardError)?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|_| ClipboardError)
    }

    fn wait_for_event(
        &self,
        mut matches: impl FnMut(&Event) -> bool,
    ) -> Result<Event, ClipboardError> {
        self.connection.flush().map_err(|_| ClipboardError)?;
        loop {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .ok_or(ClipboardError)?;
            if let Some(event) = self
                .connection
                .poll_for_event()
                .map_err(|_| ClipboardError)?
            {
                if matches(&event) {
                    return Ok(event);
                }
                continue;
            }
            let timeout = Timespec {
                tv_sec: remaining.as_secs() as i64,
                tv_nsec: remaining.subsec_nanos().into(),
            };
            let mut socket = [PollFd::new(self.connection.stream(), PollFlags::IN)];
            match poll(&mut socket, Some(&timeout)) {
                Ok(1) if socket[0].revents().contains(PollFlags::IN) => {}
                Err(rustix::io::Errno::INTR) => {}
                _ => return Err(ClipboardError),
            }
        }
    }

    fn read_property(&self, remove: bool) -> Result<TransferProperty, ClipboardError> {
        let reply = self
            .connection
            .get_property(
                remove,
                self.window,
                self.property,
                AtomEnum::ANY,
                0,
                (MAX_BYTES / 4) as u32,
            )
            .map_err(|_| ClipboardError)?
            .reply()
            .map_err(|_| ClipboardError)?;
        if reply.bytes_after != 0
            || reply.type_ == x11rb::NONE
            || !matches!(reply.format, 8 | 16 | 32)
            || reply.value.len() > MAX_BYTES
            || (reply.value_len as usize).checked_mul(usize::from(reply.format / 8))
                != Some(reply.value.len())
        {
            return Err(ClipboardError);
        }
        Ok(TransferProperty {
            atom: reply.type_,
            format: reply.format,
            bytes: reply.value,
        })
    }

    fn selection(&self, target: Atom) -> Result<Option<TransferProperty>, ClipboardError> {
        let conn = &self.connection;
        conn.delete_property(self.window, self.property)
            .map_err(|_| ClipboardError)?;
        conn.convert_selection(
            self.window,
            self.clipboard,
            target,
            self.property,
            x11rb::CURRENT_TIME,
        )
        .map_err(|_| ClipboardError)?;
        let Event::SelectionNotify(notification) = self.wait_for_event(|event| {
            matches!(event,
                Event::SelectionNotify(e) if e.selection == self.clipboard && e.target == target
            )
        })?
        else {
            return Err(ClipboardError);
        };
        if notification.property == x11rb::NONE {
            return Ok(None);
        }
        let initial = self.read_property(false)?;
        if initial.atom != self.incr {
            return Ok(Some(initial));
        }
        conn.delete_property(self.window, self.property)
            .map_err(|_| ClipboardError)?;
        let mut result: Option<TransferProperty> = None;
        loop {
            self.wait_for_event(|event| matches!(event,
                Event::PropertyNotify(e) if e.atom == self.property && e.state == Property::NEW_VALUE
            ))?;
            let chunk = self.read_property(true)?;
            let finished = chunk.bytes.is_empty();
            if let Some(result) = result.as_mut() {
                if (result.atom, result.format) != (chunk.atom, chunk.format)
                    || chunk.bytes.len() > MAX_BYTES - result.bytes.len()
                {
                    return Err(ClipboardError);
                }
                result.bytes.extend(chunk.bytes);
            } else {
                result = Some(chunk);
            }
            if finished {
                return Ok(result);
            }
        }
    }

    fn read(self, image: bool) -> Result<ClipboardRead<(Vec<u8>, bool)>, ClipboardError> {
        let requested = if image {
            IMAGE_TARGETS.as_slice()
        } else {
            TEXT_TARGETS.as_slice()
        };
        let mut wanted = [0; 6];
        for (name, atom) in requested.iter().zip(&mut wanted) {
            *atom = self.atom(name)?;
        }
        let target = match self.selection(self.targets)? {
            None if !image => Some(wanted[2]),
            None => None,
            Some(property)
                if property.atom == u32::from(AtomEnum::ATOM) && property.format == 32 =>
            {
                wanted[..requested.len()].iter().copied().find(|wanted| {
                    property
                        .bytes
                        .as_chunks::<4>()
                        .0
                        .contains(&wanted.to_ne_bytes())
                })
            }
            Some(_) => return Err(ClipboardError),
        };
        match target
            .map(|target| self.selection(target))
            .transpose()?
            .flatten()
        {
            Some(property) => Ok(ClipboardRead::Data((
                property.bytes,
                property.atom == u32::from(AtomEnum::STRING),
            ))),
            None => Ok(ClipboardRead::Empty),
        }
    }
}
