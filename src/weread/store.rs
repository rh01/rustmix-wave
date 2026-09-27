//! Chapter parse and SD commit off the firmware main task.
//!
//! Opening a WeRead chapter used to flatten HTML and rename `CHAP.TMP` on the
//! 16 KB main stack. A smashed frame there shows up as `InstrFetchProhibited`
//! with a return address of 1 and `.TMP` left in a register. This thread does
//! that work instead. Its stack is internal RAM: the jobs call FATFS, and a
//! PSRAM stack must not. `JoinHandle::join` is never used, so a dead worker
//! is a failed save rather than a main-task panic.
//!
//! There is only ever one store thread. A caller that stops waiting does not
//! start a second one; later jobs queue behind the late job, so `CHAP.TMP`,
//! `TEXT.TMP`, and `IMG.TMP` have one writer. [`busy`] stays true until that
//! queue drains, and the HTTP poller does not open a new download temp file on
//! the main task while it is. Download commits and chapter saves are polled or
//! queued; only the reads the page render needs (load, paginate, image refs)
//! wait, bounded by [`STORE_TIMEOUT`].

use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex},
    time::{Duration, Instant},
};

#[cfg(test)]
use std::thread::ThreadId;

use crate::{
    reader::ReaderLayout,
    runtime_worker::LongLivedWorker,
    weread::{
        offline::{self, CachedChapter, CardDownload},
        text::{self, ImageRef},
    },
};

const STORE_WORKER_STACK_BYTES: usize = 32 * 1024;
const STORE_TIMEOUT: Duration = Duration::from_secs(20);
const STORE_IDLE_EXIT: Duration = Duration::from_secs(10);

enum StoreJob {
    Pages {
        text: String,
        layout: ReaderLayout,
        measures: Vec<Option<text::ImageMeasure>>,
    },
    Save {
        root: PathBuf,
        book_id: String,
        chapter: CachedChapter,
    },
    Commit {
        download: CardDownload,
        succeeded: bool,
        write_error: Option<String>,
    },
    Load {
        root: PathBuf,
        book_id: String,
        index: u32,
    },
    ImageRefs {
        root: PathBuf,
        book_id: String,
        index: u32,
    },
    #[cfg(test)]
    ThreadId,
    #[cfg(test)]
    Hold(mpsc::Receiver<()>),
}

enum StoreReply {
    Pages(Vec<Vec<text::FlowItem>>),
    Save(Result<(), String>),
    Commit(Result<(), String>),
    Load(Option<CachedChapter>),
    ImageRefs(Vec<ImageRef>),
    #[cfg(test)]
    ThreadId(ThreadId),
}

type StoreWorker = LongLivedWorker<StoreJob, StoreReply>;

fn slot() -> &'static Mutex<Option<StoreWorker>> {
    static SLOT: Mutex<Option<StoreWorker>> = Mutex::new(None);
    &SLOT
}

fn worker() -> Result<StoreWorker, String> {
    let mut guard = slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(worker) = guard.as_ref() {
        if worker.is_alive() {
            return Ok(worker.clone());
        }
    }
    log::info!("rustmix-wave=weread-store status=starting stack-bytes={STORE_WORKER_STACK_BYTES}");
    let spawned = LongLivedWorker::spawn_with_idle_exit(
        "weread-store",
        STORE_WORKER_STACK_BYTES,
        Some(STORE_IDLE_EXIT),
        handle_job,
    )
    .map_err(|error| format!("WeRead store worker start failed: {error}"))?;
    *guard = Some(spawned.clone());
    Ok(spawned)
}

/// True while any store job is queued or running, including one whose caller
/// already gave up. New download temp files must not be opened until then.
#[must_use]
pub fn busy() -> bool {
    slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_some_and(|worker| worker.is_alive() && worker.outstanding() > 0)
}

fn handle_job(job: StoreJob) -> StoreReply {
    let kind = match &job {
        StoreJob::Pages { .. } => 0,
        StoreJob::Save { .. } => 1,
        StoreJob::Commit { .. } => 2,
        StoreJob::Load { .. } => 3,
        StoreJob::ImageRefs { .. } => 4,
        #[cfg(test)]
        StoreJob::ThreadId => 5,
        #[cfg(test)]
        StoreJob::Hold(_) => 5,
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| match job {
        StoreJob::Pages {
            text,
            layout,
            measures,
        } => {
            let blocks = text::blocks_from_markup(&text);
            StoreReply::Pages(text::paginate_blocks(&blocks, layout, &measures))
        }
        StoreJob::Save {
            root,
            book_id,
            chapter,
        } => {
            let result = offline::save_chapter(&root, &book_id, &chapter);
            if let Err(error) = &result {
                log::warn!("rustmix-wave=weread-store status=save-failed error={error}");
            }
            StoreReply::Save(result)
        }
        StoreJob::Commit {
            download,
            succeeded,
            write_error,
        } => StoreReply::Commit(offline::complete_download(download, succeeded, write_error)),
        StoreJob::Load {
            root,
            book_id,
            index,
        } => StoreReply::Load(offline::load_chapter(&root, &book_id, index)),
        StoreJob::ImageRefs {
            root,
            book_id,
            index,
        } => StoreReply::ImageRefs(offline::chapter_image_refs(&root, &book_id, index)),
        #[cfg(test)]
        StoreJob::ThreadId => StoreReply::ThreadId(std::thread::current().id()),
        #[cfg(test)]
        StoreJob::Hold(release) => {
            let _ = release.recv();
            StoreReply::ThreadId(std::thread::current().id())
        }
    }));
    match result {
        Ok(reply) => reply,
        Err(_) => {
            log::warn!("rustmix-wave=weread-store status=panicked");
            match kind {
                1 => StoreReply::Save(Err("WeRead store worker panicked".into())),
                2 => StoreReply::Commit(Err("WeRead store worker panicked".into())),
                3 => StoreReply::Load(None),
                4 => StoreReply::ImageRefs(Vec::new()),
                _ => StoreReply::Pages(Vec::new()),
            }
        }
    }
}

/// Queue a job on the one store thread. A thread that exited while idle is
/// replaced; a live one is always reused.
fn queue(mut job: StoreJob) -> Result<mpsc::Receiver<StoreReply>, String> {
    for _ in 0..2 {
        match worker()?.submit(job) {
            Ok(inbox) => return Ok(inbox),
            Err(returned) => job = returned,
        }
    }
    Err("WeRead store worker stopped".into())
}

fn submit(job: StoreJob) -> Result<StoreReply, String> {
    match queue(job)?.recv_timeout(STORE_TIMEOUT) {
        Ok(reply) => Ok(reply),
        Err(mpsc::RecvTimeoutError::Timeout) => Err("WeRead store worker timed out".into()),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("WeRead store worker stopped".into()),
    }
}

/// A download commit running on the store thread. The HTTP poller checks it
/// once per main-loop pass instead of waiting.
pub struct CommitJob {
    inbox: mpsc::Receiver<StoreReply>,
    started: Instant,
}

impl CommitJob {
    pub fn start(
        download: impl Into<CardDownload>,
        succeeded: bool,
        write_error: Option<String>,
    ) -> Result<Self, String> {
        Ok(Self {
            inbox: queue(StoreJob::Commit {
                download: download.into(),
                succeeded,
                write_error,
            })?,
            started: Instant::now(),
        })
    }

    /// `None` while the commit is still running.
    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.inbox.try_recv() {
            Ok(StoreReply::Commit(result)) => Some(result),
            Ok(_) => Some(Err(
                "WeRead store worker returned an unexpected result".into()
            )),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("WeRead store worker stopped".into()))
            }
            Err(mpsc::TryRecvError::Empty) if self.started.elapsed() >= STORE_TIMEOUT => {
                Some(Err("SD card write timed out".into()))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

pub fn paginate_chapter(
    text: &str,
    layout: ReaderLayout,
    measures: Vec<Option<text::ImageMeasure>>,
) -> Result<Vec<Vec<text::FlowItem>>, String> {
    match submit(StoreJob::Pages {
        text: text.to_string(),
        layout,
        measures,
    })? {
        StoreReply::Pages(pages) => Ok(pages),
        _ => Err("WeRead store worker returned an unexpected result".into()),
    }
}

pub fn save_chapter(root: &Path, book_id: &str, chapter: &CachedChapter) -> Result<(), String> {
    match submit(StoreJob::Save {
        root: root.to_path_buf(),
        book_id: book_id.to_string(),
        chapter: chapter.clone(),
    })? {
        StoreReply::Save(result) => result,
        _ => Err("WeRead store worker returned an unexpected result".into()),
    }
}

/// Queue a chapter save and return without waiting. The store thread logs a
/// failed write; the chapter is fetched again the next time it is opened.
pub fn queue_save_chapter(
    root: &Path,
    book_id: &str,
    chapter: CachedChapter,
) -> Result<(), String> {
    queue(StoreJob::Save {
        root: root.to_path_buf(),
        book_id: book_id.to_string(),
        chapter,
    })
    .map(drop)
}

/// Commit a finished download and wait. The device polls [`CommitJob`].
pub fn complete_download(
    download: impl Into<CardDownload>,
    succeeded: bool,
    write_error: Option<String>,
) -> Result<(), String> {
    let job = CommitJob::start(download, succeeded, write_error)?;
    loop {
        if let Some(result) = job.poll() {
            return result;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn load_chapter(root: &Path, book_id: &str, index: u32) -> Option<CachedChapter> {
    match submit(StoreJob::Load {
        root: root.to_path_buf(),
        book_id: book_id.to_string(),
        index,
    }) {
        Ok(StoreReply::Load(chapter)) => chapter,
        _ => None,
    }
}

pub fn chapter_image_refs(root: &Path, book_id: &str, index: u32) -> Vec<ImageRef> {
    match submit(StoreJob::ImageRefs {
        root: root.to_path_buf(),
        book_id: book_id.to_string(),
        index,
    }) {
        Ok(StoreReply::ImageRefs(refs)) => refs,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread};

    use super::{queue, submit, StoreJob, StoreReply};
    use crate::{
        reader::ReaderPreferences,
        weread::{
            offline::{self, CachedChapter, ChapterDownload, DownloadEvent},
            store,
        },
    };

    #[test]
    fn pagination_and_chapter_commit_run_off_the_caller() {
        let caller = thread::current().id();
        let reply = submit(StoreJob::ThreadId).unwrap();
        let StoreReply::ThreadId(worker) = reply else {
            panic!("store worker did not report its thread");
        };
        assert_ne!(caller, worker);

        let pages = store::paginate_chapter(
            "<p>Hello</p><p>Second paragraph for the page breaker.</p>",
            ReaderPreferences::default().layout(),
            Vec::new(),
        )
        .unwrap();
        assert!(!pages.is_empty());
        assert!(pages
            .iter()
            .flatten()
            .any(|item| item.line_text().contains("Hello")));

        let dir = std::env::temp_dir().join(format!("weread-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        store::save_chapter(
            &dir,
            "43208843",
            &CachedChapter {
                uid: "2".into(),
                index: 2,
                title: "Two".into(),
                text: "saved off the caller".into(),
            },
        )
        .unwrap();
        let book = offline::book_dir(&dir, "43208843");
        assert!(!book.join("TEXT.TMP").exists());
        let loaded = store::load_chapter(&dir, "43208843", 2).unwrap();
        assert!(loaded.text.contains("saved off the caller"));

        let mut download = ChapterDownload::begin(&dir, "43208843", "uid", 3).unwrap();
        download.apply(DownloadEvent::BeginPart("e0")).unwrap();
        download
            .apply(DownloadEvent::Chunk(b"streamed".to_vec()))
            .unwrap();
        download.apply(DownloadEvent::EndPart).unwrap();
        store::complete_download(download, true, None).unwrap();
        assert!(offline::chapter_cached(&dir, "43208843", 3));
        assert!(!book.join("CHAP.TMP").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_late_job_keeps_the_one_store_thread_and_holds_busy() {
        let (release, hold) = std::sync::mpsc::channel();
        let held = queue(StoreJob::Hold(hold)).unwrap();
        assert!(store::busy());
        let late = queue(StoreJob::ThreadId).unwrap();
        assert!(late
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        assert!(
            store::busy(),
            "an abandoned job must keep new temp files closed"
        );
        release.send(()).unwrap();
        let StoreReply::ThreadId(first) = held.recv().unwrap() else {
            panic!("hold job did not report its thread");
        };
        let StoreReply::ThreadId(second) = late.recv().unwrap() else {
            panic!("queued job did not report its thread");
        };
        assert_eq!(
            first, second,
            "a timed-out caller must not start a second writer"
        );
        let started = std::time::Instant::now();
        while store::busy() {
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn missing_store_reply_is_a_failure() {
        let (sender, inbox) = std::sync::mpsc::channel::<Result<(), String>>();
        drop(sender);
        assert!(inbox
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err());
    }
}
