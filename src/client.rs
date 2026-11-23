// SPDX-License-Identifier: AGPL-3.0-or-later
//! Background client for the earbuds' control channel, reporting state through
//! a callback. Fully event-driven: one worker thread blocks on a channel, one
//! short-lived reader thread per session blocks on the socket.
//!
//! Two modes:
//! - [`Mode::OnDemand`]: connect only to refresh or apply a change, then close
//!   after a short linger, so the phone app can still use the (single-client)
//!   control channel. Refreshes periodically while the earbuds are connected.
//! - [`Mode::Hold`]: keep the session open and reconnect with backoff.

use crate::frame::Message;
use crate::protocol::{AncMode, Event, Session, TapType, config};
use crate::windows_bt::{ConnectError, Connection, find_device};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothDevice};
use windows::Foundation::TypedEventHandler;

const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const WSAEHOSTDOWN_HR: u32 = 0x8007_2750;
/// Pause after Windows reports a fresh Bluetooth connection before we connect.
const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Hold,
    OnDemand {
        /// Periodic refresh while the earbuds are connected to this PC.
        refresh_every: Duration,
        /// Close the session after this long without traffic.
        linger: Duration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SetAnc(AncMode),
    SetWearDetection(bool),
    SetGesture(TapType, Option<u8>, Option<u8>),
    /// Re-read device info (battery), run info (ANC, wear detection) and gestures.
    Refresh,
    /// Hold mode: retry the connection now.
    Reconnect,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connecting,
    /// Authenticated session is open.
    Connected,
    /// On-demand mode: session closed normally; values are the last read.
    Idle,
    /// Earbuds are connected to Windows but refuse the control channel
    /// (typically held by a phone).
    Busy,
    /// Earbuds not connected to this PC.
    Disconnected,
    NotPaired,
}

#[derive(Debug, Clone)]
pub enum Update {
    Link(Link),
    Event(Event),
    /// Raw traffic for debugging: (sent_by_us, bytes).
    Raw(bool, Vec<u8>),
    /// Diagnostic message (errors, session end reasons).
    Log(String),
    /// Progress of user setting changes (on-demand mode).
    Apply(Apply),
}

/// State of applying setting changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    Applying,
    /// All changes confirmed by the earbuds.
    Done,
    /// Not confirmed; will retry. `busy` = the control channel was refused.
    Retrying { attempt: u32, of: u32, busy: bool },
    /// Gave up after all retries.
    Failed { busy: bool },
    /// The earbuds refused the change (non-zero status, e.g. 0x09 for ANC with one earbud worn).
    Rejected { status: u8 },
}

/// Retries for unconfirmed setting changes, and the delay before each.
const APPLY_RETRY_DELAYS: [Duration; 4] =
    [Duration::from_secs(2), Duration::from_secs(4), Duration::from_secs(8), Duration::from_secs(15)];

/// Is `cmd` confirmed by `ev` (the earbuds report the requested state)?
fn confirms(cmd: &Command, ev: &Event) -> bool {
    match (cmd, ev) {
        (Command::SetAnc(m), Event::Anc(e)) => m == e,
        (Command::SetWearDetection(on), Event::WearDetection(e)) => on == e,
        (Command::SetGesture(t, l, r), Event::Gesture { tap, left, right }) => {
            t == tap && l.is_none_or(|l| l == *left) && r.is_none_or(|r| r == *right)
        }
        _ => false,
    }
}

/// Same setting (a newer command replaces an older one).
fn same_setting(a: &Command, b: &Command) -> bool {
    match (a, b) {
        (Command::SetAnc(_), Command::SetAnc(_)) => true,
        (Command::SetWearDetection(_), Command::SetWearDetection(_)) => true,
        (Command::SetGesture(t1, l1, r1), Command::SetGesture(t2, l2, r2)) => {
            t1 == t2 && l1.is_some() == l2.is_some() && r1.is_some() == r2.is_some()
        }
        _ => false,
    }
}

fn merge_commands(queue: &mut Vec<Command>, c: Command) {
    queue.retain(|q| !same_setting(q, &c));
    queue.push(c);
}

enum Msg {
    Cmd(Command),
    /// Bytes from the reader of session `id`.
    Rx(u32, Vec<u8>),
    RxEnd(u32),
    BtConnected(bool),
}

#[derive(Clone)]
pub struct Client {
    tx: Sender<Msg>,
}

impl Client {
    /// Starts the worker thread. `on_update` runs on the worker thread.
    pub fn spawn(name: String, mode: Mode, on_update: impl FnMut(Update) + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let tx2 = tx.clone();
        std::thread::Builder::new()
            .name("redmibuds-client".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut cx = Ctx { name, tx: tx2, rx, on: on_update, device: None, session_id: 0 };
                match mode {
                    Mode::Hold => hold_loop(&mut cx),
                    Mode::OnDemand { refresh_every, linger } => on_demand_loop(&mut cx, refresh_every, linger),
                }
            })
            .expect("spawn client thread");
        Self { tx }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(Msg::Cmd(cmd));
    }
}

struct Ctx<F: FnMut(Update)> {
    name: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    on: F,
    device: Option<BluetoothDevice>,
    session_id: u32,
}

impl<F: FnMut(Update)> Ctx<F> {
    /// Gets the BluetoothDevice once and subscribes to its connection changes.
    fn ensure_device(&mut self) {
        if self.device.is_some() {
            return;
        }
        if let Ok((_, dev)) = find_device(&self.name) {
            let tx = self.tx.clone();
            let _ = dev.ConnectionStatusChanged(&TypedEventHandler::new(move |d: &Option<BluetoothDevice>, _| {
                if let Some(d) = d {
                    let up = d.ConnectionStatus().map(|s| s == BluetoothConnectionStatus::Connected).unwrap_or(false);
                    let _ = tx.send(Msg::BtConnected(up));
                }
                Ok(())
            }));
            self.device = Some(dev);
        }
    }

    fn bt_connected(&self) -> bool {
        self.device
            .as_ref()
            .and_then(|d| d.ConnectionStatus().ok())
            .map(|s| s == BluetoothConnectionStatus::Connected)
            .unwrap_or(false)
    }

    fn classify(&self, e: &ConnectError) -> Link {
        match e {
            ConnectError::NotPaired(_) => Link::NotPaired,
            ConnectError::Win(e) if e.code().0 as u32 == WSAEHOSTDOWN_HR && self.bt_connected() => Link::Busy,
            _ => Link::Disconnected,
        }
    }

    /// Connects and runs a session with `pending` commands. Returns the final
    /// link state, the commands the earbuds did not confirm and the ones they refused,
    /// or Err(()) on shutdown.
    #[allow(clippy::type_complexity)]
    fn connect_and_run(&mut self, pending: Vec<Command>, linger: Option<Duration>) -> Result<(Link, Vec<Command>, Vec<(Command, u8)>), ()> {
        self.ensure_device();
        (self.on)(Update::Link(Link::Connecting));
        let mut expect: Vec<Command> =
            pending.iter().filter(|c| matches!(c, Command::SetAnc(_) | Command::SetWearDetection(_) | Command::SetGesture(..))).cloned().collect();
        let mut rejected = Vec::new();
        let t0 = Instant::now();
        match Connection::connect(&self.name) {
            Ok(conn) => {
                (self.on)(Update::Log(format!("connected in {} ms", t0.elapsed().as_millis())));
                let end = run_session(self, conn, pending, linger, &mut expect, &mut rejected);
                let link = match end {
                    SessionEnd::Shutdown => return Err(()),
                    SessionEnd::Linger => Link::Idle,
                    SessionEnd::Lost => {
                        if self.bt_connected() {
                            Link::Busy
                        } else {
                            Link::Disconnected
                        }
                    }
                };
                (self.on)(Update::Log(format!("session ended: {end:?}; unconfirmed: {expect:?}; rejected: {rejected:?}")));
                Ok((link, expect, rejected))
            }
            Err(e) => {
                let link = self.classify(&e);
                let msg = format!(
                    "connect failed after {} ms: {e} (bt_connected={}) -> {link:?}",
                    t0.elapsed().as_millis(),
                    self.bt_connected()
                );
                (self.on)(Update::Log(msg));
                Ok((link, expect, rejected))
            }
        }
    }
}

fn hold_loop<F: FnMut(Update)>(cx: &mut Ctx<F>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        let Ok((link, _, _)) = cx.connect_and_run(Vec::new(), None) else { return };
        (cx.on)(Update::Link(link));

        // Wait for the backoff, a Bluetooth (re)connect, or an explicit command.
        let deadline = Instant::now() + backoff;
        let mut woke = false;
        loop {
            match cx.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) | Ok(Msg::Cmd(Command::Shutdown)) => return,
                Ok(Msg::Cmd(Command::Reconnect)) | Ok(Msg::BtConnected(true)) => {
                    woke = true;
                    break;
                }
                Ok(_) => {}
            }
        }
        backoff = if woke { BACKOFF_MIN } else { (backoff * 2).min(BACKOFF_MAX) };
        if woke {
            std::thread::sleep(SETTLE);
        }
    }
}

fn on_demand_loop<F: FnMut(Update)>(cx: &mut Ctx<F>, refresh_every: Duration, linger: Duration) {
    // Initial read at startup.
    let mut pending: Option<Vec<Command>> = Some(Vec::new());
    // Unconfirmed settings waiting for a retry: (commands, attempts so far, due).
    let mut retry: Option<(Vec<Command>, Instant)> = None;
    let mut attempt = 0u32;
    let total = APPLY_RETRY_DELAYS.len() as u32;
    loop {
        if let Some(cmds) = pending.take() {
            let has_settings = cmds.iter().any(is_setting);
            if has_settings {
                (cx.on)(Update::Apply(Apply::Applying));
            }
            match cx.connect_and_run(cmds, Some(linger)) {
                Ok((link, unconfirmed, rejected)) => {
                    (cx.on)(Update::Link(link));
                    let busy = link == Link::Busy;
                    if let Some((_, status)) = rejected.first() {
                        // Refused by the earbuds: retrying does not help.
                        (cx.on)(Update::Apply(Apply::Rejected { status: *status }));
                    }
                    if !unconfirmed.is_empty() {
                        if attempt < total {
                            let due = Instant::now() + APPLY_RETRY_DELAYS[attempt as usize];
                            attempt += 1;
                            (cx.on)(Update::Apply(Apply::Retrying { attempt, of: total, busy }));
                            retry = Some((unconfirmed, due));
                        } else {
                            (cx.on)(Update::Apply(Apply::Failed { busy }));
                            (cx.on)(Update::Log(format!("giving up on {unconfirmed:?}")));
                            attempt = 0;
                            // Re-read so the UI shows the real state again.
                            pending = Some(Vec::new());
                            continue;
                        }
                    } else if has_settings && rejected.is_empty() {
                        attempt = 0;
                        (cx.on)(Update::Apply(Apply::Done));
                    } else {
                        attempt = 0;
                    }
                }
                Err(()) => return,
            }
        }
        let timeout = match &retry {
            Some((_, due)) => due.saturating_duration_since(Instant::now()),
            None => refresh_every,
        };
        match cx.rx.recv_timeout(timeout) {
            Err(RecvTimeoutError::Timeout) => {
                if let Some((cmds, _)) = retry.take() {
                    pending = Some(cmds);
                    continue;
                }
                cx.ensure_device();
                if cx.bt_connected() {
                    pending = Some(Vec::new());
                }
            }
            Err(RecvTimeoutError::Disconnected) | Ok(Msg::Cmd(Command::Shutdown)) => return,
            Ok(Msg::Cmd(Command::Refresh | Command::Reconnect)) => {
                // Keep unconfirmed settings in the same connection.
                pending = Some(retry.take().map(|(c, _)| c).unwrap_or_default());
            }
            Ok(Msg::Cmd(c)) => {
                // New user change: merge with retries and queued clicks, restart the retry count.
                let mut cmds = retry.take().map(|(c, _)| c).unwrap_or_default();
                attempt = 0;
                merge_commands(&mut cmds, c);
                while let Ok(Msg::Cmd(c)) = cx.rx.try_recv() {
                    if matches!(c, Command::Shutdown) {
                        return;
                    }
                    merge_commands(&mut cmds, c);
                }
                pending = Some(cmds);
            }
            Ok(Msg::BtConnected(true)) => {
                std::thread::sleep(SETTLE);
                pending = Some(Vec::new());
            }
            Ok(Msg::BtConnected(false)) => (cx.on)(Update::Link(Link::Disconnected)),
            Ok(Msg::Rx(..) | Msg::RxEnd(_)) => {} // stale, from a closed session
        }
    }
}

#[derive(Debug)]
enum SessionEnd {
    Shutdown,
    /// On-demand session closed after the linger time.
    Linger,
    /// Socket closed / failed.
    Lost,
}

fn command_frames(session: &mut Session, c: Command) -> Vec<Vec<u8>> {
    match c {
        Command::SetAnc(m) => vec![session.encode_set_anc(m)],
        Command::SetWearDetection(on) => vec![session.encode_set_wear_detection(on), session.encode_get_run_info()],
        Command::SetGesture(t, l, r) => vec![session.encode_set_gesture(t, l, r), session.encode_get_config(config::GESTURES)],
        Command::Refresh => vec![
            session.encode_get_device_info(),
            session.encode_get_run_info(),
            session.encode_get_config(config::GESTURES),
        ],
        Command::Reconnect | Command::Shutdown => Vec::new(),
    }
}

/// Delay before re-sending a setting the earbuds rejected (non-zero status).
const NACK_RESEND: Duration = Duration::from_millis(600);
/// A setting rejected this many times in one session is reported as refused.
const NACK_LIMIT: u32 = 2;

fn is_setting(c: &Command) -> bool {
    matches!(c, Command::SetAnc(_) | Command::SetWearDetection(_) | Command::SetGesture(..))
}

/// Per-session bookkeeping of sent settings, for matching rejections by seq.
#[derive(Default)]
struct Tracker {
    /// (seq of the SET frame, command)
    sent: Vec<(u8, Command)>,
    /// (command, times rejected)
    nacks: Vec<(Command, u32)>,
    resend: Vec<Command>,
}

impl Tracker {
    fn frames(&mut self, session: &mut Session, c: Command) -> Vec<Vec<u8>> {
        let frames = command_frames(session, c.clone());
        if is_setting(&c) {
            if let Some(m) = frames.first().and_then(|f| Message::decode(f).ok()) {
                self.sent.push((m.seq, c));
            }
        }
        frames
    }
}

fn run_session<F: FnMut(Update)>(
    cx: &mut Ctx<F>,
    conn: Connection,
    mut pending: Vec<Command>,
    linger: Option<Duration>,
    expect: &mut Vec<Command>,
    rejected: &mut Vec<(Command, u8)>,
) -> SessionEnd {
    cx.session_id = cx.session_id.wrapping_add(1);
    let id = cx.session_id;
    let conn = Arc::new(conn);
    let closer = conn.closer();
    {
        let conn = conn.clone();
        let tx = cx.tx.clone();
        let _ = std::thread::Builder::new().name("redmibuds-rx".into()).stack_size(64 * 1024).spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match conn.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if tx.send(Msg::Rx(id, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    _ => {
                        let _ = tx.send(Msg::RxEnd(id));
                        break;
                    }
                }
            }
        });
    }

    let mut session = Session::new();
    let mut tr = Tracker::default();
    let send_all = |cx: &mut Ctx<F>, frames: Vec<Vec<u8>>| -> bool {
        for f in frames {
            let ok = conn.write(&f).is_ok();
            (cx.on)(Update::Raw(true, f));
            if !ok {
                return false;
            }
        }
        true
    };
    let start = session.start();
    if !send_all(cx, vec![start]) {
        closer.close();
        return SessionEnd::Lost;
    }

    // Before authentication, wait at most 10 s; afterwards use the linger time.
    let mut wait = Duration::from_secs(10);
    let mut resend_at: Option<Instant> = None;
    let mut last_activity = Instant::now();
    let end = loop {
        let now = Instant::now();
        let linger_left = linger.map(|_| (last_activity + wait).saturating_duration_since(now));
        // A pending re-send keeps the session open past the linger time.
        let timeout = resend_at.map(|t| t.saturating_duration_since(now)).or(linger_left);
        let msg = match timeout {
            Some(t) => match cx.rx.recv_timeout(t) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    if resend_at.take().is_some() {
                        let cmds: Vec<Command> = std::mem::take(&mut tr.resend);
                        (cx.on)(Update::Log(format!("re-sending rejected settings: {cmds:?}")));
                        let frames: Vec<Vec<u8>> = cmds.into_iter().flat_map(|c| tr.frames(&mut session, c)).collect();
                        last_activity = Instant::now();
                        if !send_all(cx, frames) {
                            break SessionEnd::Lost;
                        }
                        continue;
                    }
                    break SessionEnd::Linger;
                }
                Err(RecvTimeoutError::Disconnected) => break SessionEnd::Shutdown,
            },
            None => match cx.rx.recv() {
                Ok(m) => m,
                Err(_) => break SessionEnd::Shutdown,
            },
        };
        let frames = match msg {
            Msg::Rx(sid, _) | Msg::RxEnd(sid) if sid != id => continue,
            Msg::Rx(_, bytes) => {
                last_activity = Instant::now();
                (cx.on)(Update::Raw(false, bytes.clone()));
                let (events, mut replies) = session.on_bytes(&bytes);
                for ev in events {
                    if ev == Event::Authenticated {
                        (cx.on)(Update::Link(Link::Connected));
                        for c in std::mem::take(&mut pending) {
                            replies.extend(tr.frames(&mut session, c));
                        }
                        if let Some(l) = linger {
                            wait = l;
                        }
                    }
                    if let Event::Nack { opcode, seq, status } = &ev {
                        let cmd = tr.sent.iter().find(|(s, _)| s == seq).map(|(_, c)| c.clone());
                        (cx.on)(Update::Log(format!("earbuds rejected {opcode:?} seq {seq} ({cmd:?}) with status 0x{status:02X}")));
                        if let Some(cmd) = cmd.filter(|c| expect.contains(c)) {
                            let n = match tr.nacks.iter_mut().find(|(c, _)| *c == cmd) {
                                Some((_, n)) => {
                                    *n += 1;
                                    *n
                                }
                                None => {
                                    tr.nacks.push((cmd.clone(), 1));
                                    1
                                }
                            };
                            if n >= NACK_LIMIT {
                                expect.retain(|c| *c != cmd);
                                rejected.push((cmd, *status));
                            } else {
                                merge_commands(&mut tr.resend, cmd);
                                resend_at.get_or_insert(Instant::now() + NACK_RESEND);
                            }
                        }
                    }
                    expect.retain(|c| !confirms(c, &ev));
                    (cx.on)(Update::Event(ev));
                }
                replies
            }
            Msg::RxEnd(_) => break SessionEnd::Lost,
            Msg::BtConnected(false) => break SessionEnd::Lost,
            Msg::BtConnected(true) => continue,
            Msg::Cmd(Command::Shutdown) => break SessionEnd::Shutdown,
            Msg::Cmd(Command::Reconnect) => break SessionEnd::Lost,
            Msg::Cmd(c) if !session.is_authenticated() => {
                if is_setting(&c) {
                    merge_commands(expect, c.clone());
                }
                merge_commands(&mut pending, c);
                continue;
            }
            Msg::Cmd(c) => {
                last_activity = Instant::now();
                if is_setting(&c) {
                    merge_commands(expect, c.clone());
                    rejected.retain(|(r, _)| !same_setting(r, &c));
                }
                tr.frames(&mut session, c)
            }
        };
        if !send_all(cx, frames) {
            break SessionEnd::Lost;
        }
    };
    closer.close();
    end
}