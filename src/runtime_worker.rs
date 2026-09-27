//! Reusable short-lived worker boundary.
//!
//! Heavy operations receive named stack budgets and return compact heap-owned
//! results to the main hardware-orchestration loop. Panel SPI ownership stays
//! on the main task.

use core::fmt::{self, Display};
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Debug)]
pub enum NamedWorkerError<E> {
    Start(std::io::Error),
    Panicked,
    Operation(E),
}

impl<E: Display> Display for NamedWorkerError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start(error) => write!(formatter, "worker start failed: {error}"),
            Self::Panicked => formatter.write_str("worker panicked"),
            Self::Operation(error) => Display::fmt(error, formatter),
        }
    }
}

impl<E: Display + fmt::Debug> std::error::Error for NamedWorkerError<E> {}

/// Longest a blocking caller waits for a named worker. The jobs behind it
/// (weather HTTP, lexicon open, Lua load, image decode) carry shorter limits.
const NAMED_WORKER_TIMEOUT: Duration = Duration::from_secs(120);

/// Start `task` on a detached thread that reports through a channel.
///
/// `JoinHandle::join` is never called. On ESP-IDF a thread that dies without
/// storing its result makes `std` run `expect("threads should not terminate
/// unexpectedly")` in the joining task, which aborts the main loop. A dropped
/// sender is reported as [`NamedWorkerError::Panicked`] instead.
fn spawn_reporting<T, F>(
    name: &'static str,
    stack_bytes: usize,
    task: F,
) -> io::Result<mpsc::Receiver<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (reply, inbox) = mpsc::sync_channel(1);
    let thread = spawn_thread(name, stack_bytes, move || {
        let _ = reply.send(task());
    })?;
    detach(thread);
    Ok(inbox)
}

pub fn run_named_worker<T, E, F>(
    name: &'static str,
    stack_bytes: usize,
    task: F,
) -> Result<T, NamedWorkerError<E>>
where
    T: Send + 'static,
    E: Display + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    NamedWorkerHandle::spawn(name, stack_bytes, task)
        .map_err(|error| {
            log::warn!(
                "rustmix-wave=worker-boundary name={name} status=start-failed error={error}"
            );
            NamedWorkerError::Start(error)
        })?
        .join()
}

/// A named worker the caller can poll without blocking for the whole task.
///
/// `try_join` returns `None` until the thread has reported, so the main loop
/// can keep reading buttons and the idle-sleep timer. Nothing here calls
/// `JoinHandle::join`.
pub struct NamedWorkerHandle<T, E> {
    name: &'static str,
    inbox: Option<mpsc::Receiver<Result<T, E>>>,
    ready: Option<Result<T, NamedWorkerError<E>>>,
}

impl<T, E> NamedWorkerHandle<T, E>
where
    T: Send + 'static,
    E: Display + Send + 'static,
{
    pub fn spawn<F>(name: &'static str, stack_bytes: usize, task: F) -> Result<Self, std::io::Error>
    where
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        log::info!(
            "rustmix-wave=worker-boundary name={name} status=starting stack-bytes={stack_bytes}"
        );
        crate::runtime_memory::log_runtime_memory(&format!("before-worker-{name}"));
        let inbox = spawn_reporting(name, stack_bytes, task)?;
        Ok(Self {
            name,
            inbox: Some(inbox),
            ready: None,
        })
    }

    fn settle(&mut self, received: Result<Result<T, E>, NamedWorkerError<E>>) {
        let name = self.name;
        self.inbox = None;
        crate::runtime_memory::log_runtime_memory(&format!("after-worker-{name}"));
        self.ready = Some(match received {
            Ok(result) => finish_worker(name, result),
            Err(error) => {
                log::warn!("rustmix-wave=worker-boundary name={name} status=panicked");
                Err(error)
            }
        });
    }

    fn poll(&mut self) {
        if self.ready.is_some() {
            return;
        }
        let Some(inbox) = self.inbox.as_ref() else {
            return;
        };
        match inbox.try_recv() {
            Ok(result) => self.settle(Ok(result)),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.settle(Err(NamedWorkerError::Panicked)),
        }
    }

    #[must_use]
    pub fn is_finished(&mut self) -> bool {
        self.poll();
        self.inbox.is_none()
    }

    /// `None` while the worker is still running.
    pub fn try_join(&mut self) -> Option<Result<T, NamedWorkerError<E>>> {
        self.poll();
        self.ready.take()
    }

    /// Block until the worker reports, it dies, or the wait times out.
    pub fn join(mut self) -> Result<T, NamedWorkerError<E>> {
        if self.ready.is_none() {
            let received = match self.inbox.as_ref() {
                Some(inbox) => inbox
                    .recv_timeout(NAMED_WORKER_TIMEOUT)
                    .map_err(|_| NamedWorkerError::Panicked),
                None => Err(NamedWorkerError::Panicked),
            };
            self.settle(received);
        }
        self.ready.take().unwrap_or(Err(NamedWorkerError::Panicked))
    }
}

struct JobEnvelope<J, R> {
    job: J,
    reply: mpsc::Sender<R>,
}

/// Liveness and queue depth shared by a [`LongLivedWorker`] and its thread.
struct WorkerShared {
    /// False once the thread has stopped taking jobs. Guarded by the same
    /// lock `submit` holds while sending, so a job is never queued to a thread
    /// that is about to exit.
    alive: Mutex<bool>,
    /// Jobs queued or running. The owner reads this to refuse work that would
    /// touch the same files while an older job is still on the thread.
    outstanding: AtomicUsize,
}

struct AliveGuard(Arc<WorkerShared>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        *self
            .0
            .alive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
    }
}

/// One thread that accepts jobs in order.
///
/// There is never more than one thread per worker. A caller that stops
/// waiting for a reply does not start another thread: the old job keeps
/// running and later jobs queue behind it, so file writes stay serial. With
/// `idle_exit` the thread exits after that long with nothing queued; its
/// stack is freed and the owner spawns a new one on the next job.
///
/// WeRead HTTPS uses this so a chapter download does not allocate a new
/// pthread stack for every request. The stack is whatever the caller
/// configured on the thread that calls [`LongLivedWorker::spawn`].
pub struct LongLivedWorker<J, R> {
    tx: mpsc::Sender<JobEnvelope<J, R>>,
    shared: Arc<WorkerShared>,
}

impl<J, R> Clone for LongLivedWorker<J, R> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<J, R> LongLivedWorker<J, R>
where
    J: Send + 'static,
    R: Send + 'static,
{
    pub fn spawn<F>(name: &'static str, stack_bytes: usize, handler: F) -> io::Result<Self>
    where
        F: FnMut(J) -> R + Send + 'static,
    {
        Self::spawn_with_idle_exit(name, stack_bytes, None, handler)
    }

    pub fn spawn_with_idle_exit<F>(
        name: &'static str,
        stack_bytes: usize,
        idle_exit: Option<Duration>,
        mut handler: F,
    ) -> io::Result<Self>
    where
        F: FnMut(J) -> R + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<JobEnvelope<J, R>>();
        let shared = Arc::new(WorkerShared {
            alive: Mutex::new(true),
            outstanding: AtomicUsize::new(0),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = spawn_thread(name, stack_bytes, move || {
            let _alive = AliveGuard(Arc::clone(&thread_shared));
            let mut run = |envelope: JobEnvelope<J, R>| {
                let result = handler(envelope.job);
                thread_shared.outstanding.fetch_sub(1, Ordering::AcqRel);
                let _ = envelope.reply.send(result);
            };
            loop {
                let received = match idle_exit {
                    Some(idle) => rx.recv_timeout(idle),
                    None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                };
                match received {
                    Ok(envelope) => run(envelope),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let mut alive = thread_shared
                            .alive
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        match rx.try_recv() {
                            Ok(envelope) => {
                                drop(alive);
                                run(envelope);
                            }
                            Err(_) => {
                                *alive = false;
                                break;
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
        detach(thread);
        Ok(Self { tx, shared })
    }

    /// Queue one job. The receiver yields the result, or disconnects if the
    /// worker thread has stopped. `Err` returns the job when the thread is no
    /// longer running, which is the only case where the owner may start a
    /// replacement.
    pub fn submit(&self, job: J) -> Result<mpsc::Receiver<R>, J> {
        let (reply, inbox) = mpsc::channel();
        let alive = self
            .shared
            .alive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*alive {
            return Err(job);
        }
        self.shared.outstanding.fetch_add(1, Ordering::AcqRel);
        match self.tx.send(JobEnvelope { job, reply }) {
            Ok(()) => Ok(inbox),
            Err(mpsc::SendError(envelope)) => {
                self.shared.outstanding.fetch_sub(1, Ordering::AcqRel);
                Err(envelope.job)
            }
        }
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        *self
            .shared
            .alive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Jobs queued or still running on the thread.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.shared.outstanding.load(Ordering::Acquire)
    }
}

fn spawn_thread<F>(name: &str, stack_bytes: usize, task: F) -> io::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack_bytes)
        .spawn(task)
}

/// Dropping a `JoinHandle` calls `pthread_detach`, so the pthread stack is
/// released when the thread returns. `mem::forget` would leave it joinable
/// and leak the stack.
fn detach(handle: JoinHandle<()>) {
    drop(handle);
}

fn finish_worker<T, E: Display>(
    name: &str,
    result: Result<T, E>,
) -> Result<T, NamedWorkerError<E>> {
    match result {
        Ok(value) => {
            log::info!("rustmix-wave=worker-boundary name={name} status=completed");
            Ok(value)
        }
        Err(error) => {
            log::warn!("rustmix-wave=worker-boundary name={name} status=failed error={error}");
            Err(NamedWorkerError::Operation(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run_named_worker, NamedWorkerHandle};
    use std::{thread, time::Duration};

    #[test]
    fn returns_compact_result_from_named_short_lived_worker() {
        let result = run_named_worker("unit-worker", 16 * 1024, || Ok::<_, String>(42)).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn try_join_stays_pending_until_the_worker_finishes() {
        let mut worker = NamedWorkerHandle::spawn("unit-poll", 16 * 1024, || {
            thread::sleep(Duration::from_millis(40));
            Ok::<_, String>(7)
        })
        .unwrap();
        assert!(worker.try_join().is_none());
        let started = std::time::Instant::now();
        let value = loop {
            if let Some(result) = worker.try_join() {
                break result.unwrap();
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(value, 7);
        assert!(worker.try_join().is_none());
    }

    #[test]
    fn a_dead_named_worker_is_an_error_not_a_caller_panic() {
        let blocking = run_named_worker::<u32, String, _>("unit-dead", 16 * 1024, || {
            panic!("worker died before storing a result")
        });
        assert!(matches!(blocking, Err(super::NamedWorkerError::Panicked)));

        let mut polled =
            NamedWorkerHandle::<u32, String>::spawn("unit-dead-poll", 16 * 1024, || {
                panic!("worker died before storing a result")
            })
            .unwrap();
        let started = std::time::Instant::now();
        let outcome = loop {
            if let Some(outcome) = polled.try_join() {
                break outcome;
            }
            assert!(started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(outcome, Err(super::NamedWorkerError::Panicked)));
    }

    #[test]
    fn idle_worker_exits_and_refuses_jobs_until_replaced() {
        let worker = super::LongLivedWorker::spawn_with_idle_exit(
            "unit-idle",
            32 * 1024,
            Some(Duration::from_millis(30)),
            |value: u32| value * 2,
        )
        .unwrap();
        assert_eq!(worker.submit(3).unwrap().recv().unwrap(), 6);
        assert_eq!(worker.outstanding(), 0);
        let started = std::time::Instant::now();
        while worker.is_alive() {
            assert!(started.elapsed() < Duration::from_secs(2));
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(worker.submit(4).unwrap_err(), 4);
    }

    #[test]
    fn long_lived_worker_runs_jobs_on_one_thread() {
        let worker = super::LongLivedWorker::spawn("unit-live", 32 * 1024, |value: u32| {
            (std::thread::current().id(), value + 1)
        })
        .unwrap();
        let first = worker.submit(1).unwrap().recv().unwrap();
        let second = worker.submit(4).unwrap().recv().unwrap();
        assert_eq!(first.0, second.0);
        assert_eq!(first.1, 2);
        assert_eq!(second.1, 5);
    }
}
