//! Terminals on other devices: their I/O takes a direct peer-to-peer link
//! ([`crate::terminals::link`]) whenever the two devices can reach each other,
//! and the device-room relay only when they cannot.
//!
//! - Subscribe opens the direct link when one is already up; otherwise it
//!   starts on the relay and keeps trying to pair, then moves the live stream
//!   onto the direct link mid-session by re-attaching after the last seq it
//!   delivered (no gap, no repeat). A direct link that dies falls back to the
//!   relay the same way, without ending the viewer's stream.
//! - Writes and resizes return at once and stay in typed order: an attached
//!   direct link carries them; otherwise a per-terminal queue forwards them over
//!   the relay one at a time. Writes only move onto a fresh direct link once the
//!   relay queue has drained.
//! - Input is numbered and held until the host acknowledges it. Whichever path
//!   takes over after another dies replays what is unacknowledged, and the host
//!   applies each number once, so a network change neither drops nor repeats
//!   a keystroke.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::mpsc;
use zeron_preview::mux::BoxIo;
use zeron_proto::TerminalEvent;
use zeron_rpc::{RpcError, methods};

use crate::terminals::link::{self, Link, Outgoing};

/// How long a subscribe waits on an already-open direct connection before it
/// settles for the relay.
const DIRECT_ATTACH_BUDGET: Duration = Duration::from_secs(3);
/// Retry pacing for upgrading a relayed terminal to a direct link.
const UPGRADE_RETRY_MIN: Duration = Duration::from_secs(15);
const UPGRADE_RETRY_MAX: Duration = Duration::from_secs(300);
/// A relay write queue with nothing to send retires after this long.
const RELAY_QUEUE_IDLE: Duration = Duration::from_secs(60);
/// Recently closed or exited terminals remembered, so writes to them fail
/// like a local write would instead of vanishing into a queue.
const ENDED_MEMORY: usize = 256;

/// The relay path: ordinary device-addressed RPC forwarding.
#[async_trait]
pub trait Relay: Send + Sync + 'static {
    async fn call(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError>;
    async fn subscribe(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<BoxStream<'static, serde_json::Value>, RpcError>;
}

/// The direct path: peer-to-peer streams to another device.
#[async_trait]
pub trait Direct: Send + Sync + 'static {
    /// A direct connection to `device` is open right now.
    async fn ready(&self, device: &str) -> bool;
    /// Open `service` on `device`, pairing first when needed.
    async fn open(&self, device: &str, service: &str) -> anyhow::Result<BoxIo>;
}

#[async_trait]
impl Direct for zeron_preview::PreviewService {
    async fn ready(&self, device: &str) -> bool {
        self.peer_ready(device).await
    }
    async fn open(&self, device: &str, service: &str) -> anyhow::Result<BoxIo> {
        Ok(Box::new(self.open_peer_stream(device, service).await?))
    }
}

type Key = (String, String);

/// What a viewer asks of a remote terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Unacknowledged input kept for replay after a path change. Past this the
/// oldest is dropped: a path down that long has bigger problems, and the
/// bound keeps a dead link from holding a paste forever.
const MAX_UNACKED_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct Entry {
    /// Writers of attached direct links, each tagged with the subscription
    /// that owns it (two views of one terminal each attach their own).
    direct: Vec<(u64, mpsc::UnboundedSender<Outgoing>)>,
    /// Ordered relay writer, spawned on first use.
    relay: Option<mpsc::UnboundedSender<Outgoing>>,
    /// Relay writes queued or in flight.
    relay_pending: Arc<AtomicUsize>,
    /// Last input seq handed out. Never reset while this engine runs: the
    /// host drops any seq it has already applied from this client.
    next_input: u64,
    /// Input the host hasn't acknowledged yet, oldest first.
    unacked: std::collections::VecDeque<(u64, Vec<u8>)>,
    unacked_bytes: usize,
}

impl Entry {
    fn live_direct(&mut self) -> Option<&mpsc::UnboundedSender<Outgoing>> {
        self.direct.retain(|(_, tx)| !tx.is_closed());
        self.direct.first().map(|(_, tx)| tx)
    }

    fn ack(&mut self, seq: u64) {
        while self.unacked.front().is_some_and(|(s, _)| *s <= seq) {
            if let Some((_, bytes)) = self.unacked.pop_front() {
                self.unacked_bytes -= bytes.len();
            }
        }
    }

    fn replay(&self) -> Vec<Outgoing> {
        self.unacked
            .iter()
            .map(|(seq, bytes)| Outgoing::Input {
                seq: *seq,
                bytes: bytes.clone(),
            })
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct RemoteTerminals {
    /// This engine's identity as a remote viewer; input seqs are per client.
    client: Arc<str>,
    entries: Arc<Mutex<HashMap<Key, Entry>>>,
    ended: Arc<Mutex<std::collections::VecDeque<Key>>>,
    pumps: Arc<std::sync::atomic::AtomicU64>,
}

impl Default for RemoteTerminals {
    fn default() -> Self {
        Self {
            client: crate::new_id().into(),
            entries: Default::default(),
            ended: Default::default(),
            pumps: Default::default(),
        }
    }
}

impl RemoteTerminals {
    /// Queue `write` for the remote terminal; never waits on the network.
    /// Fails only for a terminal this device saw close or exit.
    pub fn send(
        &self,
        relay: &Arc<dyn Relay>,
        target: &str,
        terminal_id: &str,
        write: Write,
    ) -> Result<(), RpcError> {
        let key = (target.to_owned(), terminal_id.to_owned());
        if lock(&self.ended).contains(&key) {
            return Err(RpcError::Failed("Terminal has exited".into()));
        }
        let mut entries = lock(&self.entries);
        let entry = entries.entry(key.clone()).or_default();
        let out = match write {
            Write::Input(bytes) => {
                entry.next_input += 1;
                let seq = entry.next_input;
                entry.unacked_bytes += bytes.len();
                entry.unacked.push_back((seq, bytes.clone()));
                while entry.unacked_bytes > MAX_UNACKED_BYTES && entry.unacked.len() > 1 {
                    if let Some((_, dropped)) = entry.unacked.pop_front() {
                        entry.unacked_bytes -= dropped.len();
                    }
                }
                Outgoing::Input { seq, bytes }
            }
            Write::Resize { cols, rows } => Outgoing::Resize { cols, rows },
        };
        self.route(entry, relay, &key, vec![out]);
        Ok(())
    }

    /// Hand `outs` to the attached direct link, or to the relay queue while
    /// there is none or the relay still has earlier writes to deliver.
    fn route(&self, entry: &mut Entry, relay: &Arc<dyn Relay>, key: &Key, outs: Vec<Outgoing>) {
        if entry.relay_pending.load(Ordering::Acquire) == 0
            && let Some(direct) = entry.live_direct()
            && outs.iter().all(|out| direct.send(out.clone()).is_ok())
        {
            return;
        }
        let pending = entry.relay_pending.clone();
        let queue = entry.relay.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(relay_writer(
                self.clone(),
                key.clone(),
                relay.clone(),
                pending.clone(),
                rx,
            ));
            tx
        });
        for out in outs {
            pending.fetch_add(1, Ordering::AcqRel);
            if queue.send(out).is_err() {
                pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn ack(&self, key: &Key, seq: u64) {
        if let Some(entry) = lock(&self.entries).get_mut(key) {
            entry.ack(seq);
        }
    }

    /// A path just took over: send it whatever the host hasn't acknowledged
    /// (input a dying link may have swallowed). The host applies each seq
    /// once, so a replay of input that did land is harmless.
    fn replay(&self, key: &Key, relay: &Arc<dyn Relay>) {
        let mut entries = lock(&self.entries);
        if let Some(entry) = entries.get_mut(key) {
            let outs = entry.replay();
            if !outs.is_empty() {
                self.route(entry, relay, key, outs);
            }
        }
    }

    /// The terminal closed or exited: detach its link, refuse further writes.
    pub fn ended(&self, target: &str, terminal_id: &str) {
        let key = (target.to_owned(), terminal_id.to_owned());
        lock(&self.entries).remove(&key);
        let mut ended = lock(&self.ended);
        if !ended.contains(&key) {
            if ended.len() == ENDED_MEMORY {
                ended.pop_front();
            }
            ended.push_back(key);
        }
    }

    fn attach_writer(&self, key: &Key, pump: u64, writer: mpsc::UnboundedSender<Outgoing>) {
        let mut entries = lock(&self.entries);
        if lock(&self.ended).contains(key) {
            return;
        }
        let entry = entries.entry(key.clone()).or_default();
        if entry
            .direct
            .iter()
            .any(|(owner, tx)| *owner == pump && tx.same_channel(&writer))
        {
            return; // already attached
        }
        // Replay first, on the new link itself: it carries them ahead of
        // anything typed from now on.
        for out in entry.replay() {
            let _ = writer.send(out);
        }
        entry.direct.push((pump, writer));
    }

    fn detach_writer(&self, key: &Key, pump: u64) {
        if let Some(entry) = lock(&self.entries).get_mut(key) {
            entry.direct.retain(|(owner, _)| *owner != pump);
        }
    }

    /// The terminal's events after `after_seq`, over the best path available
    /// now and the better one once it opens. Errors only when no path reaches
    /// the terminal at all.
    pub async fn subscribe(
        &self,
        relay: Arc<dyn Relay>,
        direct: Option<Arc<dyn Direct>>,
        target: &str,
        terminal_id: &str,
        after_seq: u64,
        host_replies: bool,
    ) -> Result<BoxStream<'static, serde_json::Value>, RpcError> {
        let key = (target.to_owned(), terminal_id.to_owned());
        let mut first = None;
        if let Some(direct) = &direct
            && direct.ready(target).await
        {
            match tokio::time::timeout(
                DIRECT_ATTACH_BUDGET,
                attach_direct(
                    direct.as_ref(),
                    target,
                    terminal_id,
                    after_seq,
                    &self.client,
                ),
            )
            .await
            {
                Ok(Ok(link)) => first = Some(Source::Direct(link)),
                Ok(Err(error)) => tracing::debug!(%error, "direct terminal attach failed"),
                Err(_) => tracing::debug!("direct terminal attach timed out"),
            }
        }
        let first = match first {
            Some(source) => source,
            None => Source::Relay(
                relay_subscribe(relay.as_ref(), target, terminal_id, after_seq, host_replies)
                    .await?,
            ),
        };
        let (out, events) = mpsc::unbounded_channel();
        let id = self.pumps.fetch_add(1, Ordering::Relaxed);
        // Writes issued right after this returns must already take the link.
        if let Source::Direct(link) = &first {
            self.attach_writer(&key, id, link.outgoing.clone());
        }
        tokio::spawn(
            Pump {
                id,
                hub: self.clone(),
                key,
                relay,
                direct,
                out,
                last_seq: after_seq,
                host_replies,
            }
            .run(first),
        );
        Ok(futures::stream::unfold(events, |mut events| async move {
            events.recv().await.map(|event| (event, events))
        })
        .boxed())
    }
}

async fn attach_direct(
    direct: &dyn Direct,
    target: &str,
    terminal_id: &str,
    after_seq: u64,
    client: &str,
) -> anyhow::Result<Link> {
    let io = direct.open(target, &link::service_id(terminal_id)).await?;
    link::attach(io, after_seq, client).await
}

/// A direct link always leaves queries to the host (only engines with host
/// replies open one); over the relay the viewer's choice travels along.
async fn relay_subscribe(
    relay: &dyn Relay,
    target: &str,
    terminal_id: &str,
    after_seq: u64,
    host_replies: bool,
) -> Result<BoxStream<'static, serde_json::Value>, RpcError> {
    relay
        .subscribe(
            target,
            methods::SUBSCRIBE_TERMINAL,
            serde_json::json!({
                "terminalId": terminal_id,
                "afterSeq": after_seq,
                "targetDeviceId": target,
                "hostReplies": host_replies,
            }),
        )
        .await
}

/// Forwards relayed writes one at a time, in order.
async fn relay_writer(
    hub: RemoteTerminals,
    key: Key,
    relay: Arc<dyn Relay>,
    pending: Arc<AtomicUsize>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
) {
    loop {
        let out = match tokio::time::timeout(RELAY_QUEUE_IDLE, rx.recv()).await {
            Ok(Some(out)) => out,
            Ok(None) => return,
            Err(_) => {
                // Retire under the lock so no write lands in a dead queue.
                let mut entries = lock(&hub.entries);
                if let Ok(out) = rx.try_recv() {
                    drop(entries);
                    forward_one(&hub, &key, relay.as_ref(), out).await;
                    pending.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                if let Some(entry) = entries.get_mut(&key) {
                    entry.relay = None;
                }
                return;
            }
        };
        forward_one(&hub, &key, relay.as_ref(), out).await;
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn forward_one(hub: &RemoteTerminals, key: &Key, relay: &dyn Relay, out: Outgoing) {
    use base64::Engine as _;
    let (target, terminal_id) = key;
    let (method, params, seq) = match out {
        Outgoing::Input { seq, bytes } => (
            methods::WRITE_TERMINAL,
            serde_json::json!({
                "terminalId": terminal_id,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                "targetDeviceId": target,
                "clientId": &*hub.client,
                "inputSeq": seq,
            }),
            Some(seq),
        ),
        Outgoing::Resize { cols, rows } => (
            methods::RESIZE_TERMINAL,
            serde_json::json!({
                "terminalId": terminal_id,
                "cols": cols,
                "rows": rows,
                "targetDeviceId": target,
            }),
            None,
        ),
    };
    match relay.call(target, method, params).await {
        Ok(_) => {
            if let Some(seq) = seq {
                hub.ack(key, seq);
            }
        }
        // Unacknowledged input stays queued for the next path to replay.
        Err(error) => tracing::debug!(%error, method, "relayed terminal write failed"),
    }
}

enum Source {
    Direct(Link),
    Relay(BoxStream<'static, serde_json::Value>),
}

/// Why a source stopped delivering.
enum Ended {
    /// The terminal exited, the viewer went away, or the relay itself
    /// dropped (the viewer resubscribes with its own backoff, so a relay
    /// that keeps failing is never redialed in a tight loop).
    Done,
    /// The direct link died; resume on the relay at once.
    Lost,
    /// A direct link opened while relaying: switch to it.
    Upgrade(Link),
}

struct Pump {
    /// Owner tag for this subscription's direct writer.
    id: u64,
    hub: RemoteTerminals,
    key: Key,
    relay: Arc<dyn Relay>,
    direct: Option<Arc<dyn Direct>>,
    out: mpsc::UnboundedSender<serde_json::Value>,
    last_seq: u64,
    /// Carried to relay subscriptions (a direct link implies it).
    host_replies: bool,
}

impl Pump {
    async fn run(mut self, mut source: Source) {
        loop {
            let ended = match source {
                Source::Direct(link) => self.pump_direct(link).await,
                Source::Relay(stream) => self.pump_relay(stream).await,
            };
            source = match ended {
                Ended::Done => break,
                Ended::Upgrade(link) => Source::Direct(link),
                Ended::Lost => {
                    // Ending here would make the viewer resubscribe after a
                    // pause; resuming on the relay right away keeps typing
                    // alive through a network change.
                    let (target, terminal_id) = &self.key;
                    match relay_subscribe(
                        self.relay.as_ref(),
                        target,
                        terminal_id,
                        self.last_seq,
                        self.host_replies,
                    )
                    .await
                    {
                        Ok(stream) => {
                            self.hub.replay(&self.key, &self.relay);
                            Source::Relay(stream)
                        }
                        Err(error) => {
                            tracing::debug!(%error, "terminal relay resume failed");
                            break;
                        }
                    }
                }
            };
        }
        self.hub.detach_writer(&self.key, self.id);
    }

    /// Deliver one event; `false` once the stream is over.
    fn deliver(&mut self, event: &TerminalEvent, value: serde_json::Value) -> bool {
        let (seq, exit) = match event {
            TerminalEvent::Data { seq, .. } => (*seq, false),
            TerminalEvent::Exit { seq, .. } => (*seq, true),
        };
        if seq <= self.last_seq {
            return true; // already delivered over the previous path
        }
        self.last_seq = seq;
        if exit {
            self.hub.ended(&self.key.0, &self.key.1);
        }
        self.out.send(value).is_ok() && !exit
    }

    async fn pump_direct(&mut self, link: Link) -> Ended {
        let Link {
            outgoing,
            mut events,
            mut acks,
        } = link;
        self.hub.attach_writer(&self.key, self.id, outgoing);
        let ended = loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break Ended::Lost };
                    let Ok(value) = serde_json::to_value(&event) else { continue };
                    if !self.deliver(&event, value) {
                        break Ended::Done;
                    }
                }
                Some(seq) = acks.recv() => self.hub.ack(&self.key, seq),
                _ = self.out.closed() => break Ended::Done,
            }
        };
        self.hub.detach_writer(&self.key, self.id);
        ended
    }

    async fn pump_relay(&mut self, mut stream: BoxStream<'static, serde_json::Value>) -> Ended {
        let (target, terminal_id) = (self.key.0.clone(), self.key.1.clone());
        let direct = self.direct.clone();
        // Pairing runs beside the relay; a resolved direct stream is attached
        // only between relay events, so the attach point is exact.
        let mut upgrade: futures::future::BoxFuture<'static, BoxIo> = Box::pin(async move {
            let Some(direct) = direct else {
                return futures::future::pending::<BoxIo>().await;
            };
            let mut retry = UPGRADE_RETRY_MIN;
            loop {
                match direct.open(&target, &link::service_id(&terminal_id)).await {
                    Ok(io) => return io,
                    Err(error) => {
                        tracing::debug!(%error, "direct terminal link unavailable; staying on the relay")
                    }
                }
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(UPGRADE_RETRY_MAX);
            }
        });
        loop {
            tokio::select! {
                item = stream.next() => {
                    let Some(value) = item else { return Ended::Done };
                    let Ok(event) = serde_json::from_value::<TerminalEvent>(value.clone()) else {
                        continue;
                    };
                    if !self.deliver(&event, value) {
                        return Ended::Done;
                    }
                }
                io = &mut upgrade => {
                    match link::attach(io, self.last_seq, &self.hub.client).await {
                        Ok(link) => return Ended::Upgrade(link),
                        Err(error) => {
                            tracing::debug!(%error, "direct terminal attach failed; staying on the relay");
                            // Leave the relay in charge; no further upgrades
                            // this session (the host refused the link).
                            upgrade = Box::pin(futures::future::pending());
                        }
                    }
                }
                _ = self.out.closed() => return Ended::Done,
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::sync::atomic::AtomicBool;

    use crate::terminals::Terminals;

    /// One remote device: real PTYs behind a switchable direct path and an
    /// always-up relay, counting which path carried each write.
    struct Host {
        terminals: Terminals,
        direct_up: AtomicBool,
        changed: tokio::sync::Notify,
        links: Mutex<Vec<tokio::task::JoinHandle<()>>>,
        relay_writes: AtomicUsize,
        /// The next direct link accepts the attach, then goes dark: input
        /// and pings vanish, as on a network that just died.
        black_hole: AtomicBool,
    }

    impl Host {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                terminals: Terminals::new(),
                direct_up: AtomicBool::new(false),
                changed: tokio::sync::Notify::new(),
                links: Mutex::new(Vec::new()),
                relay_writes: AtomicUsize::new(0),
                black_hole: AtomicBool::new(false),
            })
        }
        fn set_direct(&self, up: bool) {
            self.direct_up.store(up, Ordering::SeqCst);
            self.changed.notify_waiters();
        }
        /// Sever every live direct link (a network change mid-session).
        fn cut_links(&self) {
            for link in lock(&self.links).drain(..) {
                link.abort();
            }
        }
    }

    #[async_trait]
    impl Direct for Host {
        async fn ready(&self, _device: &str) -> bool {
            self.direct_up.load(Ordering::SeqCst)
        }
        async fn open(&self, _device: &str, service: &str) -> anyhow::Result<BoxIo> {
            // Pairing: wait until the path exists.
            loop {
                let changed = self.changed.notified();
                if self.direct_up.load(Ordering::SeqCst) {
                    break;
                }
                changed.await;
            }
            let id = service
                .strip_prefix(link::SERVICE_PREFIX)
                .unwrap()
                .to_owned();
            let (viewer, host) = tokio::io::duplex(64 * 1024);
            if self.black_hole.swap(false, Ordering::SeqCst) {
                lock(&self.links).push(tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let (mut read, mut write) = tokio::io::split(host);
                    // Answer the attach's ping (kind 19 = PONG), then nothing.
                    let _ = write
                        .write_all(&[19, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0])
                        .await;
                    let mut sink = vec![0; 4096];
                    while matches!(read.read(&mut sink).await, Ok(n) if n > 0) {}
                }));
                return Ok(Box::new(viewer));
            }
            lock(&self.links).push(tokio::spawn(link::serve(self.terminals.clone(), id, host)));
            Ok(Box::new(viewer))
        }
    }

    #[async_trait]
    impl Relay for Host {
        async fn call(
            &self,
            _target: &str,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, RpcError> {
            let id = params["terminalId"].as_str().unwrap();
            let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
            match method {
                methods::WRITE_TERMINAL => {
                    self.relay_writes.fetch_add(1, Ordering::SeqCst);
                    self.terminals
                        .write(id, params["data"].as_str().unwrap())
                        .map_err(failed)?
                }
                methods::RESIZE_TERMINAL => self
                    .terminals
                    .resize(
                        id,
                        params["cols"].as_u64().unwrap() as u16,
                        params["rows"].as_u64().unwrap() as u16,
                    )
                    .map_err(failed)?,
                _ => unreachable!("{method}"),
            }
            Ok(serde_json::json!({ "ok": true }))
        }
        async fn subscribe(
            &self,
            _target: &str,
            _method: &str,
            params: serde_json::Value,
        ) -> Result<BoxStream<'static, serde_json::Value>, RpcError> {
            let rx = self
                .terminals
                .subscribe(
                    params["terminalId"].as_str().unwrap(),
                    params["afterSeq"].as_u64(),
                )
                .map_err(|e| RpcError::Failed(e.to_string()))?;
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                let event = rx.recv().await?;
                Some((serde_json::to_value(event).unwrap(), rx))
            })
            .boxed())
        }
    }

    struct Viewer {
        hub: RemoteTerminals,
        host: Arc<Host>,
        id: String,
        stream: BoxStream<'static, serde_json::Value>,
        seqs: Vec<u64>,
        text: String,
    }

    impl Viewer {
        async fn open(host: &Arc<Host>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let session = host
                .terminals
                .open_with_shell(dir.path().to_str().unwrap(), 80, 24, Some("/bin/sh"))
                .unwrap();
            let hub = RemoteTerminals::default();
            let stream = hub
                .subscribe(
                    host.clone(),
                    Some(host.clone() as Arc<dyn Direct>),
                    "host",
                    &session.id,
                    0,
                    true,
                )
                .await
                .unwrap();
            Self {
                hub,
                host: host.clone(),
                id: session.id,
                stream,
                seqs: Vec::new(),
                text: String::new(),
            }
        }
        fn type_line(&self, line: &str) {
            let relay: Arc<dyn Relay> = self.host.clone();
            self.hub
                .send(
                    &relay,
                    "host",
                    &self.id,
                    Write::Input(line.as_bytes().to_vec()),
                )
                .unwrap();
        }
        async fn until(&mut self, needle: &str) {
            tokio::time::timeout(Duration::from_secs(15), async {
                while !self.text.contains(needle) {
                    let value = self.stream.next().await.expect("stream ended");
                    match serde_json::from_value::<TerminalEvent>(value).unwrap() {
                        TerminalEvent::Data { seq, data } => {
                            self.seqs.push(seq);
                            self.text.push_str(&String::from_utf8_lossy(
                                &base64::engine::general_purpose::STANDARD
                                    .decode(data)
                                    .unwrap(),
                            ));
                        }
                        TerminalEvent::Exit { seq, .. } => self.seqs.push(seq),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("never saw {needle:?}; saw {:?}", self.text));
        }
        fn assert_contiguous(&self) {
            for pair in self.seqs.windows(2) {
                assert_eq!(pair[1], pair[0] + 1, "gap or repeat in {:?}", self.seqs);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_open_direct_path_carries_everything() {
        let host = Host::new();
        host.set_direct(true);
        let mut viewer = Viewer::open(&host).await;
        viewer.type_line("echo d''irect-$((1+1))\n");
        viewer.until("direct-2").await;
        assert_eq!(host.relay_writes.load(Ordering::SeqCst), 0);
        viewer.assert_contiguous();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relayed_terminal_moves_to_the_direct_path_without_a_gap() {
        let host = Host::new();
        let mut viewer = Viewer::open(&host).await;
        viewer.type_line("echo re''lay-$((2+2))\n");
        viewer.until("relay-4").await;
        assert!(host.relay_writes.load(Ordering::SeqCst) > 0);

        host.set_direct(true);
        // The upgrade lands between relay events; wait until writes take it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while lock(&viewer.hub.entries)
            .get(&("host".to_owned(), viewer.id.clone()))
            .is_none_or(|entry| entry.direct.is_empty())
        {
            assert!(tokio::time::Instant::now() < deadline, "never upgraded");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let relayed = host.relay_writes.load(Ordering::SeqCst);
        viewer.type_line("echo up''graded-$((3+3))\n");
        viewer.until("upgraded-6").await;
        assert_eq!(
            host.relay_writes.load(Ordering::SeqCst),
            relayed,
            "writes after the upgrade must take the direct path"
        );
        viewer.assert_contiguous();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lost_direct_path_falls_back_to_the_relay_mid_stream() {
        let host = Host::new();
        host.set_direct(true);
        let mut viewer = Viewer::open(&host).await;
        viewer.type_line("echo be''fore\n");
        viewer.until("before").await;

        host.set_direct(false);
        host.cut_links();
        // Writes queue on the relay once the link is gone; the stream itself
        // carries on without the viewer resubscribing.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while lock(&viewer.hub.entries)
            .get(&("host".to_owned(), viewer.id.clone()))
            .is_some_and(|entry| !entry.direct.is_empty())
        {
            assert!(tokio::time::Instant::now() < deadline, "loss not noticed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        viewer.type_line("echo af''ter-$((4+4))\n");
        viewer.until("after-8").await;
        assert!(host.relay_writes.load(Ordering::SeqCst) > 0);
        viewer.assert_contiguous();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keys_typed_into_a_dying_link_arrive_once_over_the_relay() {
        let host = Host::new();
        host.set_direct(true);
        host.black_hole.store(true, Ordering::SeqCst);
        let mut viewer = Viewer::open(&host).await;
        host.set_direct(false);
        // Typed into the dead link: nothing acknowledges it.
        viewer.type_line("echo sw''allowed-$((5+5))\n");
        // Silence detection drops the link, the relay takes over and
        // replays the unacknowledged input.
        viewer.until("swallowed-10").await;
        viewer.type_line("echo ne''xt\n");
        viewer.until("next").await;
        assert_eq!(
            viewer.text.matches("swallowed-10").count(),
            1,
            "replayed input must run exactly once: {:?}",
            viewer.text
        );
        viewer.assert_contiguous();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_to_an_ended_terminal_fail() {
        let host = Host::new();
        host.set_direct(true);
        let mut viewer = Viewer::open(&host).await;
        viewer.type_line("exit 0\n");
        tokio::time::timeout(Duration::from_secs(10), async {
            while viewer.stream.next().await.is_some() {}
        })
        .await
        .expect("stream ends after exit");
        let relay: Arc<dyn Relay> = host.clone();
        assert!(
            viewer
                .hub
                .send(&relay, "host", &viewer.id, Write::Input(b"x".to_vec()))
                .is_err()
        );
    }
}
