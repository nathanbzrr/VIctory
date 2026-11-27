use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

enum Msg {
    Added,
    Flush(Sender<()>),
}

pub(super) struct Writer {
    requests: Sender<Msg>,
    dirty: Arc<AtomicBool>,
}

impl Writer {
    pub(super) fn start(flush: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let (requests, pending) = mpsc::channel::<Msg>();
        let dirty = Arc::new(AtomicBool::new(false));
        let worker_dirty = dirty.clone();
        std::thread::Builder::new().name("victory-known".into()).spawn(move || loop {
            // Park indefinitely while nothing has been added (zero periodic
            // wakeups during steady-state gameplay).
            let done = match pending.recv() {
                Ok(Msg::Flush(done)) => Some(done),
                Ok(Msg::Added) => {
                    // Coalesce bursts of newly created pipelines for up to 2 s
                    // before flushing, unless an explicit flush arrives sooner.
                    let mut immediate = None;
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    loop {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        match pending.recv_timeout(left) {
                            Ok(Msg::Added) => continue,
                            Ok(Msg::Flush(done)) => {
                                immediate = Some(done);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => {
                                worker_dirty.store(false, Ordering::Release);
                                flush();
                                return;
                            }
                        }
                    }
                    immediate
                }
                Err(_) => break,
            };
            worker_dirty.store(false, Ordering::Release);
            flush();
            if let Some(done) = done {
                let _ = done.send(());
            }
        })?;
        Ok(Self { requests, dirty })
    }

    /// Signals that at least one new pipeline was buffered for writing.
    /// Coalesced lock-free so bursts only wake the writer once.
    pub(super) fn notify_added(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            let _ = self.requests.send(Msg::Added);
        }
    }

    /// The failing thread may already hold a lock the writer needs. Only the
    /// writer touches the book and the disk; the caller can leave on timeout.
    pub(super) fn request_flush(&self, timeout: Duration) {
        let (done, finished) = mpsc::channel();
        if self.requests.send(Msg::Flush(done)).is_ok() {
            let _ = finished.recv_timeout(timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_returns_after_the_writer_finishes() {
        let (saved, observed) = mpsc::channel();
        let writer = Writer::start(move || {
            let _ = saved.send(());
        })
        .unwrap();
        writer.request_flush(Duration::from_secs(1));
        observed.try_recv().unwrap();
    }

    #[test]
    fn a_stalled_save_does_not_hold_the_failing_thread() {
        let (started, entered) = mpsc::channel();
        let (release, stalled) = mpsc::channel();
        let writer = Writer::start(move || {
            let _ = started.send(());
            let _ = stalled.recv();
        })
        .unwrap();
        let (returned, finished) = mpsc::channel();
        std::thread::spawn(move || {
            writer.request_flush(Duration::from_millis(25));
            let _ = returned.send(());
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        let _ = release.send(());
    }

    #[test]
    fn idle_writer_does_not_flush_until_notified() {
        let flushes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = flushes.clone();
        let writer = Writer::start(move || {
            count.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(flushes.load(Ordering::Relaxed), 0);
        writer.notify_added();
        writer.notify_added();
        writer.request_flush(Duration::from_secs(1));
        assert!(flushes.load(Ordering::Relaxed) >= 1);
    }
}
