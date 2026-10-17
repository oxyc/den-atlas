//! Small, separate admission classes for work whose cost is local to Atlas.
//!
//! Catalog refresh has its own single-flight and upstream permits. It deliberately does not pass through
//! these gates: a cold catalogue wave must still finish and fill its cache, while local ranking and request-body
//! aggregation stay bounded independently.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A short queue is enough to absorb an ordinary burst without turning waiting requests into unbounded memory.
pub const WAIT: Duration = Duration::from_millis(100);
/// `Retry-After` is whole seconds. One second is long compared with the local queue and short for an app retry.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);
const DEFAULT_BODY_JOBS: usize = 4;
const MAX_JOBS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Class {
    Cpu,
    Body,
}

impl Class {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Body => "body",
        }
    }
}

#[derive(Clone)]
pub struct Admission {
    cpu: Arc<Gate>,
    body: Arc<Gate>,
}

impl Admission {
    /// Defaults leave one visible CPU for Tokio, cheap routes and the kernel, and cap ranking at four jobs: the
    /// full-corpus scans compete for memory bandwidth well before a large host's core count. The body class is
    /// deliberately separate: its largest legal member is an 8 MiB playground import. Each queue is twice its
    /// active class.
    pub fn from_env() -> Self {
        let visible = std::thread::available_parallelism().map_or(2, usize::from);
        let cpu_default = visible.saturating_sub(1).clamp(1, 4);
        let cpu = jobs_from_env("ATLAS_CPU_JOBS", cpu_default);
        let body = jobs_from_env("ATLAS_BODY_JOBS", DEFAULT_BODY_JOBS);
        Self::new(cpu, cpu.saturating_mul(2), body, body.saturating_mul(2), WAIT)
    }

    pub fn new(cpu: usize, cpu_queue: usize, body: usize, body_queue: usize, wait: Duration) -> Self {
        Self {
            cpu: Arc::new(Gate::new(Class::Cpu, cpu, cpu_queue, wait)),
            body: Arc::new(Gate::new(Class::Body, body, body_queue, wait)),
        }
    }

    pub async fn body(&self) -> Result<Permit, Busy> {
        Gate::acquire(&self.body).await
    }

    /// Reserve one local-CPU job before synchronous preparation starts. Recommendation uses this form because
    /// building its embedding plan and materialising kept catalogue lists are local work too, even though an
    /// asynchronous catalogue lookup is interleaved between them. The catalogue routes themselves never enter
    /// this gate.
    pub async fn cpu_permit(&self) -> Result<Permit, Busy> {
        Gate::acquire(&self.cpu).await
    }

    /// Run synchronous CPU work with the permit owned by the blocking task itself. If its request future is
    /// cancelled, Tokio cannot stop a blocking closure already in flight; keeping the permit inside that closure
    /// is what continues to bound the abandoned work until it really returns.
    pub async fn cpu<T, F>(&self, work: F) -> Result<T, RunError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let permit = self.cpu_permit().await.map_err(|_| RunError::Busy)?;
        Self::run_cpu(permit, work).await
    }

    /// Finish a CPU job whose permit was reserved before its synchronous preparation. Moving the permit into
    /// the closure keeps the same cancellation guarantee as [`Self::cpu`].
    pub async fn run_cpu<T, F>(permit: Permit, work: F) -> Result<T, RunError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|error| RunError::Failed(error.to_string()))
    }

    pub fn snapshots(&self) -> [Snapshot; 2] {
        [self.cpu.snapshot(), self.body.snapshot()]
    }

    pub fn description(&self) -> String {
        format!(
            "cpu={}/{} body={}/{} wait={}ms",
            self.cpu.capacity,
            self.cpu.queue_max,
            self.body.capacity,
            self.body.queue_max,
            self.cpu.wait.as_millis()
        )
    }

    #[cfg(test)]
    pub fn gate(&self, class: Class) -> Arc<Gate> {
        match class {
            Class::Cpu => Arc::clone(&self.cpu),
            Class::Body => Arc::clone(&self.body),
        }
    }
}

fn jobs_from_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|jobs: &usize| *jobs > 0)
        .unwrap_or(default)
        .min(MAX_JOBS)
}

pub struct Gate {
    class: Class,
    permits: Arc<Semaphore>,
    capacity: usize,
    queue_max: usize,
    wait: Duration,
    active: AtomicUsize,
    queued: AtomicUsize,
    refused: AtomicU64,
    completed: AtomicU64,
    held_micros: AtomicU64,
}

impl Gate {
    fn new(class: Class, capacity: usize, queue_max: usize, wait: Duration) -> Self {
        let capacity = capacity.max(1);
        Self {
            class,
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
            queue_max,
            wait,
            active: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            refused: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            held_micros: AtomicU64::new(0),
        }
    }

    pub async fn acquire(gate: &Arc<Self>) -> Result<Permit, Busy> {
        if let Ok(permit) = Arc::clone(&gate.permits).try_acquire_owned() {
            return Ok(Permit::new(Arc::clone(gate), permit));
        }
        let Some(queued) = Queued::enter(Arc::clone(gate)) else {
            gate.refused.fetch_add(1, Ordering::Relaxed);
            return Err(Busy { class: gate.class });
        };
        let permit = tokio::time::timeout(gate.wait, Arc::clone(&gate.permits).acquire_owned()).await;
        drop(queued);
        match permit {
            Ok(Ok(permit)) => Ok(Permit::new(Arc::clone(gate), permit)),
            Ok(Err(_)) | Err(_) => {
                gate.refused.fetch_add(1, Ordering::Relaxed);
                Err(Busy { class: gate.class })
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            class: self.class,
            capacity: self.capacity,
            queue_max: self.queue_max,
            active: self.active.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            held_micros: self.held_micros.load(Ordering::Relaxed),
        }
    }
}

struct Queued {
    gate: Arc<Gate>,
}

impl Queued {
    fn enter(gate: Arc<Gate>) -> Option<Self> {
        gate.queued
            .try_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < gate.queue_max).then_some(queued + 1)
            })
            .ok()?;
        Some(Self { gate })
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.gate.queued.fetch_sub(1, Ordering::Release);
    }
}

pub struct Permit {
    gate: Arc<Gate>,
    _permit: OwnedSemaphorePermit,
    started: Instant,
}

impl Permit {
    fn new(gate: Arc<Gate>, permit: OwnedSemaphorePermit) -> Self {
        gate.active.fetch_add(1, Ordering::Relaxed);
        Self { gate, _permit: permit, started: Instant::now() }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::Relaxed);
        self.gate.completed.fetch_add(1, Ordering::Relaxed);
        let micros = self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.gate.held_micros.fetch_add(micros, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Busy {
    pub class: Class,
}

#[derive(Debug)]
pub enum RunError {
    Busy,
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub class: Class,
    pub capacity: usize,
    pub queue_max: usize,
    pub active: usize,
    pub queued: usize,
    pub refused: u64,
    pub completed: u64,
    pub held_micros: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(wait: Duration) -> Admission {
        Admission::new(1, 2, 1, 2, wait)
    }

    async fn until_queued(gate: &Arc<Gate>, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while gate.snapshot().queued != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_queue_is_bounded_and_refusal_is_counted() {
        let admission = one(Duration::from_secs(5));
        let gate = admission.gate(Class::Body);
        let held = Gate::acquire(&gate).await.unwrap();
        let one = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { Gate::acquire(&gate).await }
        });
        until_queued(&gate, 1).await;
        let two = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { Gate::acquire(&gate).await }
        });
        until_queued(&gate, 2).await;

        assert!(Gate::acquire(&gate).await.is_err());
        assert_eq!(gate.snapshot().refused, 1);
        drop(held);
        drop(one.await.unwrap().unwrap());
        drop(two.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn semaphore_fairness_is_kept() {
        let admission = one(Duration::from_secs(5));
        let gate = admission.gate(Class::Cpu);
        let held = Gate::acquire(&gate).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn({
            let gate = Arc::clone(&gate);
            let tx = tx.clone();
            async move {
                let permit = Gate::acquire(&gate).await.unwrap();
                tx.send(1).unwrap();
                drop(permit);
            }
        });
        until_queued(&gate, 1).await;
        let second = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                let permit = Gate::acquire(&gate).await.unwrap();
                tx.send(2).unwrap();
                drop(permit);
            }
        });
        until_queued(&gate, 2).await;
        drop(held);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        first.await.unwrap();
        second.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_waiter_releases_its_queue_slot() {
        let admission = one(Duration::from_secs(5));
        let gate = admission.gate(Class::Body);
        let _held = Gate::acquire(&gate).await.unwrap();
        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { Gate::acquire(&gate).await }
        });
        until_queued(&gate, 1).await;
        waiter.abort();
        let _ = waiter.await;
        until_queued(&gate, 0).await;
    }

    #[tokio::test]
    async fn cancelling_a_request_does_not_release_running_cpu_work_early() {
        let admission = one(Duration::from_secs(5));
        let gate = admission.gate(Class::Cpu);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let request = tokio::spawn({
            let admission = admission.clone();
            async move {
                admission
                    .cpu(move || {
                        started_tx.send(()).unwrap();
                        finish_rx.recv().unwrap();
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap()).await.unwrap();
        assert_eq!(gate.snapshot().active, 1);
        request.abort();
        let _ = request.await;
        assert_eq!(gate.snapshot().active, 1, "the abandoned blocking closure still consumes its slot");
        finish_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while gate.snapshot().active != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_completed_answer_does_not_hold_cpu_admission_while_its_reader_is_slow() {
        let admission = one(Duration::from_secs(1));
        let gate = admission.gate(Class::Cpu);
        let answer = admission.cpu(|| vec![0_u8; 1024 * 1024]).await.unwrap();

        assert_eq!(gate.snapshot().active, 0, "the fully built answer still held a CPU slot");
        assert_eq!(answer.len(), 1024 * 1024, "the unread answer remains owned by the caller");
    }
}
