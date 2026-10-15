//! Task ownership shared by asynchronous and native request adapters.
//! Business IDs, retries and commit/rollback policy stay with their owners.
use futures_util::{FutureExt, future::BoxFuture};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::{AbortHandle, JoinError};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub(crate) struct TaskScope {
    pub shutdown: CancellationToken,
    tracker: TaskTracker,
    runtime: tokio::runtime::Handle,
    admission: Arc<Mutex<()>>,
    accepting: Arc<AtomicBool>,
    ancestors: Vec<(TaskTracker, Arc<AtomicBool>)>,
    name: &'static str,
}
impl TaskScope {
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
            runtime: tokio::runtime::Handle::current(),
            admission: Arc::new(Mutex::new(())),
            accepting: Arc::new(AtomicBool::new(true)),
            ancestors: Vec::new(),
            name,
        })
    }
    pub fn child(self: &Arc<Self>, name: &'static str) -> Arc<Self> {
        let mut ancestors = self.ancestors.clone();
        ancestors.push((self.tracker.clone(), self.accepting.clone()));
        Arc::new(Self {
            shutdown: self.shutdown.child_token(),
            tracker: TaskTracker::new(),
            runtime: self.runtime.clone(),
            admission: self.admission.clone(),
            accepting: Arc::new(AtomicBool::new(true)),
            ancestors,
            name,
        })
    }
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let _ = self.spawn_in(self.shutdown.clone(), future);
    }
    pub fn spawn_in(
        &self,
        stop: CancellationToken,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Option<AbortHandle> {
        self.admit(stop, future.boxed())
    }
    fn admit(
        &self,
        stop: CancellationToken,
        future: BoxFuture<'static, ()>,
    ) -> Option<AbortHandle> {
        let admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        if !self.accepting.load(Ordering::Acquire)
            || self
                .ancestors
                .iter()
                .any(|(_, accepting)| !accepting.load(Ordering::Acquire))
            || self.shutdown.is_cancelled()
            || stop.is_cancelled()
        {
            return None;
        }
        let shutdown = self.shutdown.clone();
        let name = self.name;
        let tracked = self.tracker.track_future(async move {
            let work = async move {
                tokio::select! { biased;
                    _=shutdown.cancelled()=>{},
                    _=stop.cancelled()=>{},
                    _=future=>{},
                }
            };
            if let Err(panic) = AssertUnwindSafe(work).catch_unwind().await {
                let reason = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string panic");
                tracing::error!(scope = name, reason, "owned asynchronous task panicked");
            }
        });
        // Parent admission and tracking are used as well: closing a peer waits
        // for its view's tasks, but closing a view never stops the shared peer.
        let mut tracked: BoxFuture<'static, ()> = Box::pin(tracked);
        for (parent, _) in &self.ancestors {
            tracked = Box::pin(parent.track_future(tracked));
        }
        drop(admission);
        Some(self.runtime.spawn(tracked).abort_handle())
    }
    pub fn cancel(&self) {
        {
            let _admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
            self.accepting.store(false, Ordering::Release);
        }
        self.shutdown.cancel();
        self.tracker.close();
    }
    pub async fn close(&self) {
        self.cancel();
        // No JoinHandles are removed while waiting: a cancelled close cannot
        // make a subsequent closer miss unfinished tasks.
        self.tracker.wait().await;
    }
}
impl Drop for TaskScope {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Cleanup on early return, panic or dropped future.
// Its users are the Windows service workers and the multi-window screen
// playback, which the Linux viewer does not drive yet.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct OnDrop<F: FnOnce()>(Option<F>);
#[cfg_attr(not(windows), allow(dead_code))]
impl<F: FnOnce()> OnDrop<F> {
    pub fn new(action: F) -> Self {
        Self(Some(action))
    }
}
impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}

/// Read-only native work observes cancellation when its awaiter disappears.
/// This is not a way to interrupt an already committed write or native syscall.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce(CancellationToken) -> T + Send + 'static,
) -> Result<T, JoinError> {
    let stop = CancellationToken::new();
    let _cancel = stop.clone().drop_guard();
    tokio::task::spawn_blocking(move || work(stop)).await
}

/// Recheck policy while awaiting cancellable admission, using the owning
/// business's existing cadence and deadline rather than a universal timeout.
pub(crate) async fn while_current<F: Future>(
    stop: &CancellationToken,
    cadence: Duration,
    current: impl Fn() -> bool,
    work: F,
) -> Option<F::Output> {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + cadence, cadence);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(work);
    loop {
        if stop.is_cancelled() || !current() {
            return None;
        }
        tokio::select! { biased;
            _=stop.cancelled()=>return None,
            _=tick.tick()=>{},
            result=&mut work=>return Some(result),
        }
    }
}
