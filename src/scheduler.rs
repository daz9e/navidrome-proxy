//! The download queue: a priority lane and a fixed pool of background workers.
//!
//! * Priority lane (P0 play, P1 like / next-track prefetch): every job starts
//!   immediately, it never waits behind background work.
//! * Background lane (P2 album likes, imports, upgrades): exactly
//!   `background_workers` jobs run at a time (5 by default).
//!
//! One track has at most one job of each kind; asking again only raises its
//! priority. A play request for a track that a background worker is already
//! downloading simply waits for that download. Jobs survive restarts (SQLite).

use crate::db::{Field, Job, Store, Track};
use crate::sources::SourceError;
use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

pub const PLAY: i64 = 0;
pub const LIKE: i64 = 1;
pub const PREFETCH: i64 = 1;
pub const BACKGROUND: i64 = 2;

pub const BACKOFF: [f64; 5] = [15.0, 60.0, 300.0, 1200.0, 3600.0];

pub type JobResult = Result<Option<Track>, SourceError>;
pub type Handler = Arc<dyn Fn(Job, CancellationToken) -> BoxFuture<'static, JobResult> + Send + Sync>;
pub type DoneHook = Arc<dyn Fn(Job, Track) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct Scheduler {
    store: Arc<Store>,
    background_workers: usize,
    priority_slots: Arc<Semaphore>,
    /// Retry delays; the job fails for good after `backoff.len()` attempts.
    pub backoff: Vec<f64>,
    handlers: RwLock<HashMap<String, Handler>>,
    on_done: RwLock<Vec<DoneHook>>,
    waiters: Mutex<HashMap<String, Vec<oneshot::Sender<Option<Track>>>>>,
    priority_wake: Notify,
    background_wake: Notify,
    lanes: Mutex<Vec<JoinHandle<()>>>,
    running: Mutex<HashMap<(String, String), (AbortHandle, CancellationToken)>>,
}

impl Scheduler {
    pub fn new(store: Arc<Store>, background_workers: usize, priority_limit: usize) -> Self {
        Self {
            store,
            background_workers,
            priority_slots: Arc::new(Semaphore::new(priority_limit.max(1))),
            backoff: BACKOFF.to_vec(),
            handlers: RwLock::new(HashMap::new()),
            on_done: RwLock::new(Vec::new()),
            waiters: Mutex::new(HashMap::new()),
            priority_wake: Notify::new(),
            background_wake: Notify::new(),
            lanes: Mutex::new(Vec::new()),
            running: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_handler(&self, kind: &str, handler: Handler) {
        self.handlers.write().insert(kind.into(), handler);
    }

    pub fn has_handler(&self, kind: &str) -> bool {
        self.handlers.read().contains_key(kind)
    }

    pub fn add_done_hook(&self, hook: DoneHook) {
        self.on_done.write().push(hook);
    }

    // ---------------------------------------------------------------- control
    pub fn start(self: &Arc<Self>) {
        let requeued = self.store.requeue_running_jobs();
        if requeued > 0 {
            tracing::info!("requeued {requeued} jobs interrupted by a restart");
        }
        // Nothing downloads yet: a crash must not leave tracks "downloading" forever.
        self.store.reset_downloading_tracks();
        let mut lanes = self.lanes.lock();
        let me = self.clone();
        lanes.push(tokio::spawn(async move { me.priority_dispatcher().await }));
        for _ in 0..self.background_workers {
            let me = self.clone();
            lanes.push(tokio::spawn(async move { me.background_worker().await }));
        }
    }

    pub async fn stop(&self) {
        let lanes: Vec<_> = self.lanes.lock().drain(..).collect();
        let running: Vec<_> = self.running.lock().drain().map(|(_, v)| v).collect();
        for (task, cancel) in &running {
            cancel.cancel();
            task.abort();
        }
        for lane in lanes {
            lane.abort();
            let _ = lane.await;
        }
        self.store.requeue_running_jobs();
    }

    /// Queue work for a track (idempotent). `fresh` gives a failed job a new attempt budget.
    pub fn request(&self, track_id: &str, priority: i64, reason: &str, kind: &str, fresh: bool) -> bool {
        let queued = self.store.enqueue(track_id, kind, priority, Some(reason), fresh);
        if queued || fresh {
            if kind == "download"
                && let Some(track) = self.store.get_track(track_id)
                && track.status != "ready"
                && track.status != "downloading"
            {
                self.store.update_track(track_id, &[Field::status("queued"), Field::Error(None)]);
            }
            self.wake(priority);
        }
        queued
    }

    pub fn download(&self, track_id: &str, priority: i64, reason: &str, fresh: bool) -> bool {
        self.request(track_id, priority, reason, "download", fresh)
    }

    fn wake(&self, priority: i64) {
        if priority <= PREFETCH { &self.priority_wake } else { &self.background_wake }.notify_waiters();
    }

    /// Wait until the download job of `track_id` finishes; the backing track or None.
    pub async fn wait(&self, track_id: &str, timeout: Duration) -> Option<Track> {
        // Register first, then look: a job that finishes in between still reaches us.
        let (tx, rx) = oneshot::channel();
        {
            let mut waiters = self.waiters.lock();
            let list = waiters.entry(track_id.to_string()).or_default();
            list.retain(|w| !w.is_closed());
            list.push(tx);
        }
        let track = self.store.get_track(track_id);
        if let Some(t) = track.as_ref().filter(|t| t.is_ready()) {
            return Some(t.clone());
        }
        match self.store.get_job(track_id, "download") {
            Some(job) if job.state != "done" && job.state != "failed" => {}
            _ => return track.filter(|t| t.is_ready()),
        }
        tokio::time::timeout(timeout, rx).await.ok().and_then(Result::ok).flatten()
    }

    fn resolve(&self, track_id: &str, result: Option<Track>) {
        for waiter in self.waiters.lock().remove(track_id).unwrap_or_default() {
            let _ = waiter.send(result.clone());
        }
    }

    pub fn running(&self) -> Vec<(String, String)> {
        self.running.lock().keys().cloned().collect()
    }

    // ------------------------------------------------------------------ lanes
    fn next_wakeup(&self) -> Duration {
        let wait = match self.store.next_retry_time() {
            None => 30.0,
            Some(at) => (at - crate::util::now()).clamp(0.05, 30.0),
        };
        Duration::from_secs_f64(wait)
    }

    async fn priority_dispatcher(self: Arc<Self>) {
        loop {
            let woken = self.priority_wake.notified();
            let Some(job) = self.store.claim_next_job(Some(PREFETCH), None, None) else {
                let _ = tokio::time::timeout(self.next_wakeup(), woken).await;
                continue;
            };
            let Ok(permit) = self.priority_slots.clone().acquire_owned().await else { return };
            let me = self.clone();
            tokio::spawn(async move {
                me.execute(job).await;
                drop(permit);
            });
        }
    }

    async fn background_worker(self: Arc<Self>) {
        loop {
            let woken = self.background_wake.notified();
            let Some(job) = self.store.claim_next_job(None, Some(BACKGROUND), None) else {
                let _ = tokio::time::timeout(self.next_wakeup(), woken).await;
                continue;
            };
            self.execute(job).await;
        }
    }

    // -------------------------------------------------------------------- run
    async fn execute(&self, job: Job) {
        let key = (job.track_id.clone(), job.kind.clone());
        let handler = self.handlers.read().get(&job.kind).cloned();
        if job.kind == "download" {
            self.store.update_track(&job.track_id, &[Field::status("downloading")]);
        }
        let started = Instant::now();
        let cancel = CancellationToken::new();
        let background = job.priority >= BACKGROUND;
        let inner = {
            // Holding the lock across spawn: the entry exists before the task can finish.
            let mut running = self.running.lock();
            let (job, token) = (job.clone(), cancel.clone());
            let task = tokio::spawn(crate::deezer::BACKGROUND.scope(background, async move {
                match handler {
                    None => Err(SourceError::new(format!("no handler for {}", job.kind), false)),
                    Some(h) => h(job, token).await,
                }
            }));
            running.insert(key.clone(), (task.abort_handle(), cancel));
            task
        };
        let outcome = inner.await;
        self.running.lock().remove(&key);
        let result = match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(SourceError::Cancelled)) => {
                self.store.finish_job(&job.track_id, &job.kind, "queued", None, Some(crate::util::now()));
                return;
            }
            Ok(Err(e)) => return self.failed(&job, e.transient(), &e.to_string()),
            Err(e) if e.is_cancelled() => {
                self.store.finish_job(&job.track_id, &job.kind, "queued", None, Some(crate::util::now()));
                return;
            }
            Err(e) => return self.failed(&job, true, &format!("handler panicked: {e}")),
        };
        self.store.finish_job(&job.track_id, &job.kind, "done", None, None);
        tracing::info!(
            "{} {} done in {:.1}s (P{}, {})",
            job.kind,
            job.track_id,
            started.elapsed().as_secs_f64(),
            job.priority,
            job.reason.as_deref().unwrap_or("")
        );
        if job.kind == "download" {
            self.resolve(&job.track_id, result.clone());
        }
        if let Some(track) = result {
            let hooks = self.on_done.read().clone();
            for hook in hooks {
                hook(job.clone(), track.clone()).await;
            }
        }
    }

    fn failed(&self, job: &Job, transient: bool, message: &str) {
        let message = match crate::util::tail(message, 800) {
            m if m.is_empty() => "error".to_string(),
            m => m,
        };
        if transient && (job.attempts as usize) < self.backoff.len() {
            let delay = self.backoff[(job.attempts.max(1) as usize - 1).min(self.backoff.len() - 1)];
            tracing::warn!("{} {} failed (attempt {}), retry in {delay}s: {message}", job.kind, job.track_id, job.attempts);
            self.store.finish_job(&job.track_id, &job.kind, "queued", Some(&message), Some(crate::util::now() + delay));
            self.wake(job.priority); // re-arm the lane's timer for the retry
            if job.kind == "download" {
                self.store.update_track(&job.track_id, &[Field::status("queued"), Field::Error(Some(message))]);
                // A waiting stream should not hang for minutes on a retry.
                self.resolve(&job.track_id, None);
            }
            return;
        }
        tracing::warn!("{} {} failed permanently: {message}", job.kind, job.track_id);
        self.store.finish_job(&job.track_id, &job.kind, "failed", Some(&message), None);
        if job.kind == "download" {
            if self.store.get_track(&job.track_id).is_some_and(|t| !t.is_ready()) {
                self.store.update_track(&job.track_id, &[Field::status("failed"), Field::Error(Some(message))]);
            }
            self.resolve(&job.track_id, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::m;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store() -> Arc<Store> {
        Arc::new(Store::memory())
    }

    fn ready_track(store: &Store, dir: &std::path::Path, id: &str) -> Option<Track> {
        let f = dir.join(format!("{}.m4a", id.replace(':', "_")));
        std::fs::write(&f, b"x").unwrap();
        store.update_track(id, &[Field::status("ready"), Field::File(Some(f.display().to_string()))]);
        store.get_track(id)
    }

    async fn until(mut predicate: impl FnMut() -> bool) {
        for _ in 0..300 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    fn handler<F, Fut>(f: F) -> Handler
    where
        F: Fn(Job, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = JobResult> + Send + 'static,
    {
        Arc::new(move |job, cancel| Box::pin(f(job, cancel)))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn priority_lane_never_waits_for_busy_background_workers() {
        let (store, dir) = (store(), tempfile::tempdir().unwrap());
        let gate = Arc::new(Notify::new());
        let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let running = Arc::new(Mutex::new(std::collections::HashSet::<String>::new()));
        let peak = Arc::new(AtomicUsize::new(0));
        let s = Arc::new(Scheduler::new(store.clone(), 5, 32));
        let (st, d, g, o, r, p) = (store.clone(), dir.path().to_path_buf(), gate.clone(), open.clone(), running.clone(), peak.clone());
        s.set_handler(
            "download",
            handler(move |job, _| {
                let (st, d, g, o, r, p) = (st.clone(), d.clone(), g.clone(), o.clone(), r.clone(), p.clone());
                async move {
                    let bg = {
                        let mut r = r.lock();
                        r.insert(job.track_id.clone());
                        r.iter().filter(|t| t.starts_with("deezer:bg")).count()
                    };
                    if job.priority >= BACKGROUND {
                        p.fetch_max(bg, Ordering::SeqCst);
                        while !o.load(Ordering::SeqCst) {
                            let _ = tokio::time::timeout(Duration::from_millis(20), g.notified()).await;
                        }
                    }
                    r.lock().remove(&job.track_id);
                    Ok(ready_track(&st, &d, &job.track_id))
                }
            }),
        );
        for i in 0..12 {
            store.remember_track(&format!("deezer:bg{i}"), &m(json!({"title": i.to_string()})));
            s.download(&format!("deezer:bg{i}"), BACKGROUND, "import", false);
        }
        s.start();
        until(|| running.lock().len() == 5).await;
        store.remember_track("deezer:play", &m(json!({"title": "now"})));
        s.download("deezer:play", PLAY, "play", false);
        let track = s.wait("deezer:play", Duration::from_secs(2)).await;
        assert_eq!(track.unwrap().id, "deezer:play");
        assert_eq!(peak.load(Ordering::SeqCst), 5);
        open.store(true, Ordering::SeqCst);
        gate.notify_waiters();
        until(|| store.job_counts().get("download:done") == Some(&13)).await;
        assert_eq!(peak.load(Ordering::SeqCst), 5);
        s.stop().await;
    }

    #[tokio::test]
    async fn duplicate_requests_run_once_and_play_joins_running_background_job() {
        let (store, dir) = (store(), tempfile::tempdir().unwrap());
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let s = Arc::new(Scheduler::new(store.clone(), 1, 32));
        let (st, d, c, g) = (store.clone(), dir.path().to_path_buf(), calls.clone(), gate.clone());
        s.set_handler(
            "download",
            handler(move |job, _| {
                let (st, d, c, g) = (st.clone(), d.clone(), c.clone(), g.clone());
                async move {
                    c.lock().push(job.track_id.clone());
                    let _ = g.acquire().await.unwrap();
                    Ok(ready_track(&st, &d, &job.track_id))
                }
            }),
        );
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        s.download("deezer:1", BACKGROUND, "album", false);
        s.start();
        until(|| *calls.lock() == ["deezer:1"]).await;
        assert!(!s.download("deezer:1", PLAY, "play", false), "already running: not queued twice");
        assert_eq!(s.running(), [("deezer:1".to_string(), "download".to_string())]);
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.wait("deezer:1", Duration::from_secs(2)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.add_permits(1);
        assert_eq!(waiter.await.unwrap().unwrap().id, "deezer:1");
        assert_eq!(*calls.lock(), ["deezer:1"]);
        s.stop().await;
    }

    #[tokio::test]
    async fn transient_failures_retry_then_fail_and_fresh_request_revives() {
        let store = store();
        let attempts = Arc::new(Mutex::new(Vec::<i64>::new()));
        let mut s = Scheduler::new(store.clone(), 1, 32);
        s.backoff = vec![0.0, 0.0, 0.0];
        let s = Arc::new(s);
        let a = attempts.clone();
        s.set_handler(
            "download",
            handler(move |job, _| {
                a.lock().push(job.attempts);
                async { Err(SourceError::new("HTTP 403", true)) }
            }),
        );
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        s.download("deezer:1", LIKE, "like", false);
        s.start();
        until(|| store.get_job("deezer:1", "download").unwrap().state == "failed").await;
        assert_eq!(*attempts.lock(), [1, 2, 3]);
        assert_eq!(store.get_track("deezer:1").unwrap().status, "failed");
        s.download("deezer:1", PLAY, "play", true);
        until(|| attempts.lock().len() == 6).await;
        s.stop().await;
    }

    #[tokio::test]
    async fn permanent_failure_is_not_retried_and_waiters_get_none() {
        let store = store();
        let s = Arc::new(Scheduler::new(store.clone(), 1, 32));
        s.set_handler("download", handler(|_, _| async { Err(SourceError::new("no matching audio", false)) }));
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        s.download("deezer:1", PLAY, "play", false);
        s.start();
        assert!(s.wait("deezer:1", Duration::from_secs(2)).await.is_none());
        until(|| store.get_job("deezer:1", "download").unwrap().state == "failed").await;
        assert_eq!(store.get_job("deezer:1", "download").unwrap().attempts, 1);
        assert!(store.get_track("deezer:1").unwrap().error.unwrap().contains("no matching audio"));
        s.stop().await;
    }

    #[tokio::test]
    async fn missing_handler_fails_and_panics_are_retried() {
        let store = store();
        let s = Arc::new(Scheduler::new(store.clone(), 1, 32));
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        s.request("deezer:1", BACKGROUND, "x", "upgrade", false);
        s.set_handler("download", handler(|_, _| async { panic!("boom") }));
        s.download("deezer:1", PLAY, "play", false);
        s.start();
        until(|| store.get_job("deezer:1", "upgrade").unwrap().state == "failed").await;
        until(|| store.get_job("deezer:1", "download").unwrap().error.is_some_and(|e| e.contains("panicked"))).await;
        assert_eq!(store.get_job("deezer:1", "download").unwrap().state, "queued", "a panic is retried");
        s.stop().await;
    }

    #[tokio::test]
    async fn wait_times_out_and_ready_tracks_return_immediately() {
        let (store, dir) = (store(), tempfile::tempdir().unwrap());
        let s = Scheduler::new(store.clone(), 1, 32);
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        ready_track(&store, dir.path(), "deezer:1");
        assert_eq!(s.wait("deezer:1", Duration::from_millis(100)).await.unwrap().id, "deezer:1");
        store.remember_track("deezer:2", &m(json!({"title": "y"})));
        store.enqueue("deezer:2", "download", 2, None, false);
        assert!(s.wait("deezer:2", Duration::from_millis(50)).await.is_none());
        assert!(s.wait("deezer:3", Duration::from_millis(50)).await.is_none(), "no job at all");
    }

    #[tokio::test]
    async fn done_hooks_receive_the_backing_track_and_background_scope_is_set() {
        let (store, dir) = (store(), tempfile::tempdir().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::new(Scheduler::new(store.clone(), 1, 32));
        let (st, d) = (store.clone(), dir.path().to_path_buf());
        s.set_handler(
            "download",
            handler(move |job, _| {
                let (st, d) = (st.clone(), d.clone());
                async move {
                    assert_eq!(crate::deezer::is_background(), job.priority >= BACKGROUND);
                    Ok(ready_track(&st, &d, &job.track_id))
                }
            }),
        );
        let sink = seen.clone();
        s.add_done_hook(Arc::new(move |job: Job, track: Track| {
            sink.lock().push((job.reason.unwrap_or_default(), track.id));
            Box::pin(async {})
        }));
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        store.remember_track("deezer:2", &m(json!({"title": "y"})));
        s.download("deezer:1", LIKE, "like:alice", false);
        s.download("deezer:2", BACKGROUND, "album", false);
        s.start();
        until(|| seen.lock().len() == 2).await;
        seen.lock().sort();
        assert_eq!(*seen.lock(), [("album".to_string(), "deezer:2".to_string()), ("like:alice".into(), "deezer:1".into())]);
        s.stop().await;
    }

    #[tokio::test]
    async fn stop_cancels_running_jobs_and_requeues_them() {
        let store = store();
        let s = Arc::new(Scheduler::new(store.clone(), 1, 32));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = cancelled.clone();
        s.set_handler(
            "download",
            handler(move |_, cancel| {
                let c = c.clone();
                async move {
                    cancel.cancelled().await;
                    c.store(true, Ordering::SeqCst);
                    Err(SourceError::Cancelled)
                }
            }),
        );
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        s.download("deezer:1", PLAY, "play", false);
        s.start();
        until(|| !s.running().is_empty()).await;
        assert_eq!(store.get_track("deezer:1").unwrap().status, "downloading");
        s.stop().await;
        let job = store.get_job("deezer:1", "download").unwrap();
        assert_eq!(job.state, "queued");
        // Restart: the interrupted track is no longer stuck in "downloading".
        let s = Arc::new(Scheduler::new(store.clone(), 1, 32));
        s.start();
        assert_eq!(store.get_track("deezer:1").unwrap().status, "queued");
        s.stop().await;
    }
}
