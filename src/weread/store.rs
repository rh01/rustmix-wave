//! Chapter parse and SD commit off the firmware main task.
//!
//! Opening a WeRead chapter used to flatten HTML and rename `CHAP.TMP` on the
//! 16 KB main stack. A smashed frame there shows up as `InstrFetchProhibited`
//! with a return address of 1 and `.TMP` left in a register. This thread does
//! that work instead. Its stack is internal RAM: the jobs call FATFS, and a
//! PSRAM stack must not. The caller waits on a channel. `JoinHandle::join` is
//! never used, so a dead worker is a failed save rather than a main-task panic.

use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{mpsc, Mutex},
    time::Duration,
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

fn slot() -> &'static Mutex<Option<LongLivedWorker<StoreJob, StoreReply>>> {
    static SLOT: Mutex<Option<LongLivedWorker<StoreJob, StoreReply>>> = Mutex::new(None);
    &SLOT
}

fn reset_worker() {
    let mut guard = slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

fn worker() -> Result<LongLivedWorker<StoreJob, StoreReply>, String> {
    let mut guard = slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        log::info!(
            "rustmix-wave=weread-store status=starting stack-bytes={STORE_WORKER_STACK_BYTES}"
        );
        let spawned = LongLivedWorker::spawn("weread-store", STORE_WORKER_STACK_BYTES, handle_job)
            .map_err(|error| format!("WeRead store worker start failed: {error}"))?;
        *guard = Some(spawned);
    }
    Ok(guard.as_ref().unwrap().clone())
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
        } => StoreReply::Save(offline::save_chapter(&root, &book_id, &chapter)),
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

fn submit(job: StoreJob) -> Result<StoreReply, String> {
    let worker = worker()?;
    let inbox = match worker.submit(job) {
        Ok(inbox) => inbox,
        Err(_) => {
            reset_worker();
            return Err("WeRead store worker stopped".into());
        }
    };
    match inbox.recv_timeout(STORE_TIMEOUT) {
        Ok(reply) => Ok(reply),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            reset_worker();
            Err("WeRead store worker timed out".into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            reset_worker();
            Err("WeRead store worker stopped".into())
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

pub fn complete_download(
    download: impl Into<CardDownload>,
    succeeded: bool,
    write_error: Option<String>,
) -> Result<(), String> {
    match submit(StoreJob::Commit {
        download: download.into(),
        succeeded,
        write_error,
    })? {
        StoreReply::Commit(result) => result,
        _ => Err("WeRead store worker returned an unexpected result".into()),
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

    use super::{submit, StoreJob, StoreReply};
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
        assert!(!book.join("CHAP.TMP").exists());
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
    fn missing_store_reply_is_a_failure() {
        let (sender, inbox) = std::sync::mpsc::channel::<Result<(), String>>();
        drop(sender);
        assert!(inbox
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err());
    }
}
