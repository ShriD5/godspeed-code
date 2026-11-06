//! Engine-owned discovery/proxy lifecycle, independent of headed UI lifetimes.
use crate::{
    catalog::Catalog,
    discovery,
    login::{self, CallbackForwarder, CallbackRoutes, CallbackTunnels},
    mux::{self, BoxIo, Connector},
    peer::Peers,
    proxy, signaling,
};
use futures::{StreamExt, stream};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Discovery must not turn into HTTP traffic against the user's dev servers.
/// A listening socket is probed until it answers HTTP once; that verdict then
/// holds for the socket's lifetime, because the process enumeration each
/// cycle already proves the same process still owns the same port. Sockets
/// that did not answer HTTP (a server bound but not yet serving) are retried
/// with a capped exponential backoff instead of on every cycle.
#[derive(Default)]
pub(crate) struct ProbeMemory {
    verdicts: HashMap<(u32, u64, SocketAddr), Verdict>,
}
enum Verdict {
    Http,
    NotHttp {
        retry_at: Instant,
        backoff: Duration,
    },
}
/// Discovery cadence while something may start or stop a server, or someone
/// is looking: a run is live, a terminal is open, a client watches previews
/// (an open browser pane), or a person acted in the last [`ACTIVE_FOR`].
const ACTIVE_SCAN: Duration = Duration::from_secs(2);
/// Cadence otherwise. Each scan runs `lsof` twice and `ps` once (about
/// 70 ms of CPU); every 2 s that was 6% of a core for the engine's life.
const QUIET_SCAN: Duration = Duration::from_secs(60);
/// How long a person's action keeps discovery at [`ACTIVE_SCAN`].
const ACTIVE_FOR: Duration = Duration::from_secs(2 * 60);

/// Whether something may be starting servers now: see [`ACTIVE_SCAN`].
pub type LiveProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The pause after a scan: short while active, long while quiet. Without a
/// live probe (embedders that never set one) discovery stays active.
pub(crate) fn scan_delay(
    live: Option<bool>,
    watched: bool,
    last_activity: Option<Instant>,
    now: Instant,
) -> Duration {
    let acted = last_activity.is_some_and(|at| now.saturating_duration_since(at) < ACTIVE_FOR);
    if live.unwrap_or(true) || watched || acted {
        ACTIVE_SCAN
    } else {
        QUIET_SCAN
    }
}

const NOT_HTTP_INITIAL_BACKOFF: Duration = Duration::from_secs(4);
const NOT_HTTP_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// (servers already confirmed HTTP, listeners to probe this cycle)
type Planned<T> = (Vec<(T, discovery::Listener)>, Vec<(T, discovery::Listener)>);

fn probe_key(listener: &discovery::Listener) -> (u32, u64, SocketAddr) {
    (listener.pid, listener.started_at, listener.address)
}

impl ProbeMemory {
    /// Splits this cycle's candidates into servers already known to speak
    /// HTTP and listeners that need a probe now. Verdicts for sockets that
    /// are no longer listed are forgotten so a reused port is probed afresh.
    pub(crate) fn plan<T>(
        &mut self,
        candidates: Vec<(T, discovery::Listener)>,
        now: Instant,
    ) -> Planned<T> {
        let present: std::collections::HashSet<_> =
            candidates.iter().map(|(_, l)| probe_key(l)).collect();
        self.verdicts.retain(|key, _| present.contains(key));
        let mut known = Vec::new();
        let mut probe = Vec::new();
        for candidate in candidates {
            match self.verdicts.get(&probe_key(&candidate.1)) {
                Some(Verdict::Http) => known.push(candidate),
                Some(Verdict::NotHttp { retry_at, .. }) if *retry_at > now => {}
                _ => probe.push(candidate),
            }
        }
        (known, probe)
    }
    pub(crate) fn record(&mut self, listener: &discovery::Listener, http: bool, now: Instant) {
        let key = probe_key(listener);
        let verdict = if http {
            Verdict::Http
        } else {
            let backoff = match self.verdicts.get(&key) {
                Some(Verdict::NotHttp { backoff, .. }) => (*backoff * 2).min(NOT_HTTP_MAX_BACKOFF),
                _ => NOT_HTTP_INITIAL_BACKOFF,
            };
            Verdict::NotHttp {
                retry_at: now + backoff,
                backoff,
            }
        };
        self.verdicts.insert(key, verdict);
    }
}
#[derive(Clone)]
pub struct PreviewService(Arc<Inner>);
struct Inner {
    catalog: Catalog,
    stop: CancellationToken,
    started: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Sign-in callbacks peers may reach on this device (see [`login`]).
    callback_routes: CallbackRoutes,
    /// Sign-in callbacks this device forwards to the device running them.
    callback_tunnels: CallbackTunnels,
    peers: std::sync::OnceLock<Peers>,
    /// Engine-provided stream services peers may open, by id prefix.
    streams: StreamServices,
    /// Runs live or terminals open: discovery at full cadence.
    live: Mutex<Option<LiveProbe>>,
    /// The last time a person acted in the app.
    last_activity: Mutex<Option<Instant>>,
    /// Wakes a quiet scanner for an immediate scan.
    scan_now: tokio::sync::Notify,
    /// Clients watching the previews list ([`PreviewService::watch`]).
    watchers: std::sync::atomic::AtomicUsize,
}

/// Held while a client watches the previews list: discovery stays at full
/// cadence until the last one drops.
pub struct PreviewWatch(PreviewService);

impl Drop for PreviewWatch {
    fn drop(&mut self) {
        self.0.0.watchers.fetch_sub(1, Ordering::SeqCst);
    }
}
/// Stream services beyond previews (terminals), keyed by service id prefix.
/// Each handler sees the authenticated peer that opened the stream and
/// decides itself whether to serve it.
type StreamServices = Arc<Mutex<Vec<(String, Arc<dyn Connector>)>>>;
pub type Projects = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;
impl PreviewService {
    pub fn new(file: PathBuf, device_id: String, device_name: String) -> anyhow::Result<Self> {
        Ok(Self(Arc::new(Inner {
            catalog: Catalog::open(file, device_id, device_name)?,
            stop: CancellationToken::new(),
            started: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            callback_routes: CallbackRoutes::default(),
            callback_tunnels: CallbackTunnels::default(),
            peers: std::sync::OnceLock::new(),
            streams: StreamServices::default(),
            live: Mutex::new(None),
            last_activity: Mutex::new(None),
            scan_now: tokio::sync::Notify::new(),
            watchers: std::sync::atomic::AtomicUsize::new(0),
        })))
    }

    /// Watch the previews list. Keep the guard as long as the subscription:
    /// a list someone is looking at is refreshed at full cadence, starting
    /// with a scan now.
    pub fn watch(
        &self,
    ) -> (
        tokio::sync::watch::Receiver<zeron_proto::PreviewSnapshot>,
        PreviewWatch,
    ) {
        self.0.watchers.fetch_add(1, Ordering::SeqCst);
        self.0.scan_now.notify_one();
        (self.0.catalog.subscribe(), PreviewWatch(self.clone()))
    }

    /// What keeps discovery at full cadence besides a person acting: a live
    /// run or an open terminal (either may start a dev server).
    pub fn set_live_probe(&self, probe: LiveProbe) {
        *self.0.live.lock().unwrap() = Some(probe);
    }

    /// A person acted, or a run started: scan now, then at full cadence for
    /// a while, so the list is current whenever someone may look at it.
    pub fn note_activity(&self) {
        *self.0.last_activity.lock().unwrap() = Some(Instant::now());
        self.0.scan_now.notify_one();
    }

    fn scan_delay(&self) -> Duration {
        let live = self.0.live.lock().unwrap().clone().map(|probe| probe());
        let watched = self.0.watchers.load(Ordering::SeqCst) > 0;
        let last_activity = *self.0.last_activity.lock().unwrap();
        scan_delay(live, watched, last_activity, Instant::now())
    }
    /// Serve stream ids starting with `prefix` from `handler` — to peers and
    /// to this device alike. Registering a prefix again replaces its handler.
    pub fn serve_streams(&self, prefix: &str, handler: Arc<dyn Connector>) {
        let mut streams = self.0.streams.lock().unwrap();
        streams.retain(|(existing, _)| existing != prefix);
        streams.push((prefix.to_owned(), handler));
    }
    /// The open direct connection to `device`, if there is one right now.
    pub async fn peer_ready(&self, device: &str) -> bool {
        match self.0.peers.get() {
            Some(peers) => peers.ready(device).await.is_some(),
            None => false,
        }
    }
    /// Start pairing with `device` in the background, so a stream opened
    /// shortly after finds the direct connection already up.
    pub fn warm_peer(&self, device: &str) {
        let Some(peers) = self.0.peers.get().cloned() else {
            return;
        };
        let device = device.to_owned();
        tokio::spawn(async move {
            if let Err(error) = peers.connect(&device).await {
                tracing::debug!(%error, "direct connection warm-up failed");
            }
        });
    }
    /// Open stream `service` on `device` directly, pairing first if needed
    /// (bounded: pairing gives up after 30s).
    pub async fn open_peer_stream(
        &self,
        device: &str,
        service: &str,
    ) -> anyhow::Result<mux::Stream> {
        let peers = self.0.peers.get().cloned().ok_or_else(|| {
            anyhow::anyhow!("Direct connections to other devices aren't available")
        })?;
        peers.open(device, service, false).await
    }
    pub fn catalog(&self) -> &Catalog {
        &self.0.catalog
    }
    /// The registry the agent-login flows publish their callback ports to.
    pub fn callback_routes(&self) -> CallbackRoutes {
        self.0.callback_routes.clone()
    }
    /// Forward loopback `port` on this device to login `login_id`'s callback
    /// on `device` until [`Self::close_login_tunnel`] or `lifetime` passes.
    /// Idempotent per login; a port taken here is an error.
    pub async fn open_login_tunnel(
        &self,
        login_id: &str,
        device: &str,
        port: u16,
        lifetime: Duration,
    ) -> anyhow::Result<()> {
        if self.0.callback_tunnels.contains(login_id) {
            return Ok(());
        }
        let peers = self.0.peers.get().cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "Direct connections to other devices aren't available yet — try again in a moment."
            )
        })?;
        let device = device.to_owned();
        let service = login::service_id(login_id);
        let open: login::Opener = Arc::new(move || {
            let (peers, device, service) = (peers.clone(), device.clone(), service.clone());
            Box::pin(
                async move { Ok(Box::new(peers.open(&device, &service, false).await?) as BoxIo) },
            )
        });
        let forwarder = CallbackForwarder::bind(port, lifetime, open).await?;
        self.0.callback_tunnels.insert(login_id, forwarder);
        Ok(())
    }
    pub fn close_login_tunnel(&self, login_id: &str) {
        self.0.callback_tunnels.close(login_id);
    }
    pub async fn start(&self, projects: Projects, signaling: Option<signaling::Config>) {
        if self.0.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let connector = Arc::new(LocalConnector(
            self.0.catalog.clone(),
            self.0.callback_routes.clone(),
            self.0.streams.clone(),
        ));
        let local = mux::local(connector.clone(), self.0.stop.child_token());
        let (peers, output) = Peers::new(
            self.0.catalog.device_id().into(),
            connector,
            self.0.stop.child_token(),
        );
        let _ = self.0.peers.set(peers.clone());
        let router = proxy::Router {
            catalog: self.0.catalog.clone(),
            local,
            peers: peers.clone(),
        };
        let proxy_catalog = self.0.catalog.clone();
        let proxy_stop = self.0.stop.clone();
        let listener_task = tokio::spawn(async move {
            loop {
                let binding = tokio::select! {
                    _ = proxy_stop.cancelled() => break,
                    binding = proxy::serve(router.clone(), zeron_proto::PREVIEW_PROXY_PORT, proxy_stop.child_token()) => binding,
                };
                match binding {
                    Ok((port, tasks)) => {
                        proxy_catalog.set_proxy_status(port, None);
                        for task in tasks {
                            let _ = task.await;
                        }
                        break;
                    }
                    Err(error) => proxy_catalog.set_proxy_status(
                        zeron_proto::PREVIEW_PROXY_PORT,
                        Some(format!(
                            "Local previews could not listen on port 7331: {error}"
                        )),
                    ),
                }
                tokio::select! { _ = proxy_stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
            }
        });
        self.0.tasks.lock().unwrap().push(listener_task);
        let catalog = self.0.catalog.clone();
        let stop = self.0.stop.clone();
        let service = self.clone();
        let scanner = tokio::spawn(async move {
            let mut memory = ProbeMemory::default();
            loop {
                let memory = &mut memory;
                let scan = async {
                    let projects = projects.clone();
                    let candidates = tokio::task::spawn_blocking(move || {
                        let mut roots: Vec<_> = projects()
                            .into_iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .filter_map(|p| p.canonicalize().ok())
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        if roots.is_empty() {
                            return Vec::new();
                        }
                        roots.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
                        discovery::listeners()
                            .into_iter()
                            .filter(|l| {
                                l.address.port() != zeron_proto::PREVIEW_PROXY_PORT
                                    && l.pid != std::process::id()
                                    && !l.is_authentication_command()
                            })
                            .filter_map(|listener| {
                                roots
                                    .iter()
                                    .find(|root| listener.belongs_to(root))
                                    .cloned()
                                    .map(|root| (root, listener))
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
                    .unwrap_or_default();
                    let (mut servers, probe) = memory.plan(candidates, Instant::now());
                    let probed: Vec<_> = stream::iter(probe)
                        .map(|(root, listener)| async move {
                            let http = discovery::is_http(listener.address).await;
                            (root, listener, http)
                        })
                        .buffer_unordered(12)
                        .collect()
                        .await;
                    let now = Instant::now();
                    for (root, listener, http) in probed {
                        memory.record(&listener, http, now);
                        if http {
                            servers.push((root, listener));
                        }
                    }
                    if let Err(error) = catalog.replace_local(servers) {
                        tracing::warn!(%error, "could not update preview services");
                    }
                };
                tokio::select! { _ = stop.cancelled() => break, _ = scan => {} }
                let delay = service.scan_delay();
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                    _ = service.0.scan_now.notified() => {}
                }
            }
        });
        let mut tasks = self.0.tasks.lock().unwrap();
        tasks.push(scanner);
        if let Some(config) = signaling {
            tasks.push(tokio::spawn(signaling::run(
                config,
                self.0.catalog.clone(),
                peers,
                output,
                self.0.stop.child_token(),
            )));
        }
    }
    pub fn stop(&self) {
        self.0.stop.cancel();
        self.0.catalog.clear_remote();
    }
    pub async fn shutdown(&self) {
        self.stop();
        let tasks = std::mem::take(&mut *self.0.tasks.lock().unwrap());
        for task in tasks {
            let _ = task.await;
        }
    }
}
struct LocalConnector(Catalog, CallbackRoutes, StreamServices);
#[async_trait::async_trait]
impl Connector for LocalConnector {
    async fn connect(&self, id: &str) -> anyhow::Result<BoxIo> {
        self.connect_from(None, id).await
    }
    async fn connect_from(&self, peer: Option<&str>, id: &str) -> anyhow::Result<BoxIo> {
        let handler = self
            .2
            .lock()
            .unwrap()
            .iter()
            .find(|(prefix, _)| id.starts_with(prefix.as_str()))
            .map(|(_, handler)| handler.clone());
        if let Some(handler) = handler {
            return handler.connect_from(peer, id).await;
        }
        // A sign-in callback: only the registered port, only for the device
        // that started the login, only while it runs.
        if let Some(port) = self.1.target(peer, id) {
            return CallbackRoutes::connect(port?).await;
        }
        let route = self
            .0
            .local_route(id)
            .ok_or_else(|| anyhow::anyhow!("preview service stopped"))?;
        // Recheck process identity before dialing: a stopped dev server's port
        // may have been reused since the last scan. Checking the one pid is
        // cheap; enumerating every listener here spawned lsof and ps for each
        // asset a remote page requested, which showed up as memory and CPU
        // churn on the hosting Mac.
        let (pid, started_at) = (route.listener.pid, route.listener.started_at);
        let valid =
            tokio::task::spawn_blocking(move || discovery::same_process(pid, started_at)).await?;
        anyhow::ensure!(valid, "preview process changed; waiting for rediscovery");
        Ok(Box::new(
            tokio::net::TcpStream::connect(route.listener.address).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn listener(pid: u32, port: u16) -> discovery::Listener {
        discovery::Listener {
            pid,
            parent: 1,
            cwd: "/work/app".into(),
            args: vec!["node".into()],
            started_at: 1_000,
            address: ([127, 0, 0, 1], port).into(),
            zeron_owned: false,
        }
    }
    #[test]
    fn discovery_slows_down_when_nothing_can_start_a_server() {
        let now = Instant::now();
        // Embedders without a probe keep the full cadence.
        assert_eq!(scan_delay(None, false, None, now), ACTIVE_SCAN);
        // A live run or open terminal.
        assert_eq!(scan_delay(Some(true), false, None, now), ACTIVE_SCAN);
        // A client watches the list (an open browser pane).
        assert_eq!(scan_delay(Some(false), true, None, now), ACTIVE_SCAN);
        // A person acted recently.
        let recent = Some(now - Duration::from_secs(30));
        assert_eq!(scan_delay(Some(false), false, recent, now), ACTIVE_SCAN);
        // Nobody here and nothing running: once a minute.
        assert_eq!(scan_delay(Some(false), false, None, now), QUIET_SCAN);
        let stale = Some(now - ACTIVE_FOR);
        assert_eq!(scan_delay(Some(false), false, stale, now), QUIET_SCAN);
    }

    #[test]
    fn a_watcher_holds_full_cadence_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let service =
            PreviewService::new(dir.path().join("previews.json"), "dev".into(), "Dev".into())
                .unwrap();
        service.set_live_probe(Arc::new(|| false));
        assert_eq!(service.scan_delay(), QUIET_SCAN);
        let (_changes, watching) = service.watch();
        assert_eq!(service.scan_delay(), ACTIVE_SCAN);
        drop(watching);
        assert_eq!(service.scan_delay(), QUIET_SCAN);
    }

    #[test]
    fn a_confirmed_http_server_is_never_probed_again() {
        let mut memory = ProbeMemory::default();
        let t0 = Instant::now();
        let (known, probe) = memory.plan(vec![((), listener(7, 8081))], t0);
        assert!(known.is_empty());
        assert_eq!(probe.len(), 1);
        memory.record(&probe[0].1, true, t0);
        for cycle in 1..1_000u64 {
            let now = t0 + Duration::from_secs(2 * cycle);
            let (known, probe) = memory.plan(vec![((), listener(7, 8081))], now);
            assert_eq!(known.len(), 1, "cycle {cycle} must reuse the verdict");
            assert!(probe.is_empty(), "cycle {cycle} must not send a request");
        }
    }
    #[test]
    fn non_http_listeners_back_off_and_reused_ports_are_probed_afresh() {
        let mut memory = ProbeMemory::default();
        let t0 = Instant::now();
        let (_, probe) = memory.plan(vec![((), listener(7, 8081))], t0);
        memory.record(&probe[0].1, false, t0);
        let (_, probe) = memory.plan(vec![((), listener(7, 8081))], t0 + Duration::from_secs(2));
        assert!(probe.is_empty(), "the very next cycle is skipped");
        let (_, probe) = memory.plan(vec![((), listener(7, 8081))], t0 + Duration::from_secs(5));
        assert_eq!(probe.len(), 1, "retried after the initial backoff");
        memory.record(&probe[0].1, false, t0 + Duration::from_secs(5));
        let (_, probe) = memory.plan(vec![((), listener(7, 8081))], t0 + Duration::from_secs(10));
        assert!(probe.is_empty(), "backoff doubled");
        // A different process on the same port carries no verdict over.
        let (_, probe) = memory.plan(vec![((), listener(8, 8081))], t0 + Duration::from_secs(10));
        assert_eq!(probe.len(), 1);
        // The original socket, once absent, is forgotten and re-probed on return.
        memory.record(&probe[0].1, true, t0);
        let (known, probe) =
            memory.plan(vec![((), listener(7, 8081))], t0 + Duration::from_secs(11));
        assert!(known.is_empty());
        assert_eq!(probe.len(), 1);
    }
    #[test]
    fn backoff_is_capped() {
        let mut memory = ProbeMemory::default();
        let mut now = Instant::now();
        for _ in 0..20 {
            memory.record(&listener(7, 8081), false, now);
            now += NOT_HTTP_MAX_BACKOFF;
        }
        let (_, probe) = memory.plan(vec![((), listener(7, 8081))], now);
        assert_eq!(probe.len(), 1, "still retried once per max backoff");
    }
}
