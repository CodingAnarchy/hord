//! Request activity: calls in flight and when the last one started, so a
//! per-repo daemon can exit when idle (ADR 0021).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tower::{Layer, Service};

/// Calls in flight and the time of the last one.
#[derive(Debug)]
pub struct Activity {
    started: Instant,
    last_ms: AtomicU64,
    in_flight: AtomicUsize,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            last_ms: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
        }
    }
}

impl Activity {
    fn touch(&self) {
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// How long since the last call started or finished; zero while one is
    /// in flight. (A call ends when its response starts: a long event
    /// stream does not keep a daemon busy, and resumes from its cursor.)
    #[must_use]
    pub fn idle(&self) -> Duration {
        if self.in_flight.load(Ordering::Relaxed) > 0 {
            return Duration::ZERO;
        }
        let last = Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }
}

/// Tower layer that records [`Activity`].
#[derive(Clone, Debug)]
pub struct ActivityLayer(pub Arc<Activity>);

impl<S> Layer<S> for ActivityLayer {
    type Service = Tracked<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Tracked {
            inner,
            activity: Arc::clone(&self.0),
        }
    }
}

/// Service made by [`ActivityLayer`].
#[derive(Clone, Debug)]
pub struct Tracked<S> {
    inner: S,
    activity: Arc<Activity>,
}

impl<S, R> Service<R> for Tracked<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        let activity = Arc::clone(&self.activity);
        activity.in_flight.fetch_add(1, Ordering::Relaxed);
        activity.touch();
        let future = self.inner.call(request);
        Box::pin(async move {
            let out = future.await;
            activity.touch();
            activity.in_flight.fetch_sub(1, Ordering::Relaxed);
            out
        })
    }
}
