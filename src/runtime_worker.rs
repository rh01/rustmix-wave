//! Reusable short-lived worker boundary.
//!
//! Heavy operations receive named stack budgets and return compact heap-owned
//! results to the main hardware-orchestration loop. Panel SPI ownership stays
//! on the main task.

use core::fmt::{self, Display};
use std::{
    io,
    sync::mpsc,
    thread::{self, JoinHandle},
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
    log::info!(
        "rustmix-wave=worker-boundary name={name} status=starting stack-bytes={stack_bytes}"
    );
    crate::runtime_memory::log_runtime_memory(&format!("before-worker-{name}"));
    let worker = std::thread::Builder::new()
        .name(name.into())
        .stack_size(stack_bytes)
        .spawn(task)
        .map_err(|error| {
            log::warn!(
                "rustmix-wave=worker-boundary name={name} status=start-failed error={error}"
            );
            NamedWorkerError::Start(error)
        })?;
    let result = worker.join().map_err(|_| {
        log::warn!("rustmix-wave=worker-boundary name={name} status=panicked");
        NamedWorkerError::Panicked
    })?;
    crate::runtime_memory::log_runtime_memory(&format!("after-worker-{name}"));
    finish_worker(name, result)
}

/// A named worker the caller can poll without joining for the whole task.
///
/// `try_join` returns `None` until the thread has finished, so the main loop
/// can keep reading buttons and the idle-sleep timer.
pub struct NamedWorkerHandle<T, E> {
    name: &'static str,
    handle: Option<std::thread::JoinHandle<Result<T, E>>>,
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
        let handle = std::thread::Builder::new()
            .name(name.into())
            .stack_size(stack_bytes)
            .spawn(task)?;
        Ok(Self {
            name,
            handle: Some(handle),
        })
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }

    /// `None` while the worker is still running.
    pub fn try_join(&mut self) -> Option<Result<T, NamedWorkerError<E>>> {
        let handle = self.handle.as_ref()?;
        if !handle.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        let name = self.name;
        crate::runtime_memory::log_runtime_memory(&format!("after-worker-{name}"));
        Some(match handle.join() {
            Ok(result) => finish_worker(name, result),
            Err(_) => {
                log::warn!("rustmix-wave=worker-boundary name={name} status=panicked");
                Err(NamedWorkerError::Panicked)
            }
        })
    }

    /// Block until the worker finishes. The caller is not the 16 KiB main task.
    pub fn join(mut self) -> Result<T, NamedWorkerError<E>> {
        let Some(handle) = self.handle.take() else {
            return Err(NamedWorkerError::Panicked);
        };
        let name = self.name;
        crate::runtime_memory::log_runtime_memory(&format!("after-worker-{name}"));
        match handle.join() {
            Ok(result) => finish_worker(name, result),
            Err(_) => {
                log::warn!("rustmix-wave=worker-boundary name={name} status=panicked");
                Err(NamedWorkerError::Panicked)
            }
        }
    }
}

struct JobEnvelope<J, R> {
    job: J,
    reply: mpsc::Sender<R>,
}

/// One thread that accepts jobs until the last sender is dropped.
///
/// WeRead HTTPS uses this so a chapter download does not allocate a new
/// pthread stack for every request. The stack is whatever the caller
/// configured on the thread that calls [`LongLivedWorker::spawn`].
pub struct LongLivedWorker<J, R> {
    tx: mpsc::Sender<JobEnvelope<J, R>>,
}

impl<J, R> Clone for LongLivedWorker<J, R> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl<J, R> LongLivedWorker<J, R>
where
    J: Send + 'static,
    R: Send + 'static,
{
    pub fn spawn<F>(name: &'static str, stack_bytes: usize, mut handler: F) -> io::Result<Self>
    where
        F: FnMut(J) -> R + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<JobEnvelope<J, R>>();
        let thread = spawn_thread(name, stack_bytes, move || {
            while let Ok(envelope) = rx.recv() {
                let result = handler(envelope.job);
                let _ = envelope.reply.send(result);
            }
        })?;
        // The firmware keeps this thread for the process lifetime. Detach so
        // cloning the sender does not require the join handle.
        detach(thread);
        Ok(Self { tx })
    }

    /// Queue one job. The receiver yields the result, or disconnects if the
    /// worker thread has stopped.
    pub fn submit(&self, job: J) -> Result<mpsc::Receiver<R>, J> {
        let (reply, inbox) = mpsc::channel();
        match self.tx.send(JobEnvelope { job, reply }) {
            Ok(()) => Ok(inbox),
            Err(mpsc::SendError(envelope)) => Err(envelope.job),
        }
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

fn detach(handle: JoinHandle<()>) {
    std::mem::forget(handle);
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
