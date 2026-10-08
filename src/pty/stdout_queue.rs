//! Console output on its own thread, so the Windows reader keeps tracking the
//! screen and delivery state while the console isn't draining (QuickEdit
//! selection, a paused or slow terminal). The child is still backpressured:
//! once the bounded queue fills, [`StdoutQueue::write`] blocks the reader,
//! which stops reading the ConPTY.
//!
//! Everything for the console goes through one queue — frames, title updates —
//! so it reaches the console in the order the reader produced it.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

/// Queued chunks before `write` blocks. The reader's chunks are at most 8 KiB,
/// so this bounds the backlog at about 2 MiB, matching the ConPTY read channel.
const QUEUE_CHUNKS: usize = 256;

enum Item {
    Bytes(Vec<u8>),
    /// Acknowledged once everything queued before it is written and flushed.
    Barrier(SyncSender<()>),
}

pub(super) struct StdoutQueue {
    tx: Option<SyncSender<Item>>,
    /// Disconnects when the writer thread exits.
    done: Receiver<()>,
    /// Set when [`finish`](Self::finish) gives up: the writer discards what's
    /// left instead of writing it after the console modes are restored.
    abandon: Arc<AtomicBool>,
}

impl StdoutQueue {
    /// Start the writer thread. `on_write` sees each chunk just before it's
    /// written (the relay trace logs the `stdout` hop there).
    pub(super) fn spawn<W, F>(out: W, on_write: F) -> Self
    where
        W: Write + Send + 'static,
        F: Fn(&[u8]) + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CHUNKS);
        let (done_tx, done) = mpsc::channel();
        let abandon = Arc::new(AtomicBool::new(false));
        let stop = abandon.clone();
        thread::spawn(move || {
            let _done = done_tx;
            drain(rx, out, on_write, &stop);
        });
        Self {
            tx: Some(tx),
            done,
            abandon,
        }
    }

    /// Queue `bytes` for the console. Blocks while the queue is full.
    pub(super) fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(Item::Bytes(bytes.to_vec()));
        }
    }

    /// Block until everything queued so far is on the console.
    pub(super) fn barrier(&self) {
        let Some(tx) = &self.tx else { return };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if tx.send(Item::Barrier(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// Write out whatever is still queued, waiting up to `timeout` for it.
    /// Returns whether everything reached the console. On timeout the rest is
    /// discarded: a stalled console can't hold up shutdown, and nothing is
    /// written after the caller restores the console modes (beyond a chunk the
    /// writer is already blocked in).
    pub(super) fn finish(mut self, timeout: Duration) -> bool {
        self.tx.take();
        match self.done.recv_timeout(timeout) {
            Err(RecvTimeoutError::Timeout) => {
                self.abandon.store(true, Ordering::Relaxed);
                false
            }
            _ => true,
        }
    }
}

impl Drop for StdoutQueue {
    /// Without [`finish`](Self::finish) the writer drains what's queued and
    /// exits on its own; don't wait for it, as the console may never drain.
    fn drop(&mut self) {
        self.tx.take();
    }
}

fn drain<W: Write, F: Fn(&[u8])>(
    rx: Receiver<Item>,
    mut out: W,
    on_write: F,
    abandon: &AtomicBool,
) {
    let handle = |item: Item, out: &mut W| match item {
        Item::Bytes(_) if abandon.load(Ordering::Relaxed) => {}
        Item::Bytes(bytes) => {
            on_write(&bytes);
            let _ = out.write_all(&bytes);
        }
        Item::Barrier(ack) => {
            let _ = out.flush();
            let _ = ack.send(());
        }
    };
    while let Ok(item) = rx.recv() {
        handle(item, &mut out);
        // Write whatever else is already queued before flushing once.
        while let Ok(item) = rx.try_recv() {
            handle(item, &mut out);
        }
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const LONG: Duration = Duration::from_secs(5);

    /// A writer that records what reaches it and can be held shut.
    #[derive(Clone, Default)]
    struct Console {
        written: Arc<Mutex<Vec<u8>>>,
        gate: Arc<Mutex<()>>,
    }

    impl Write for Console {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _open = self.gate.lock().unwrap();
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_arrive_in_order_and_finish_drains_them() {
        let console = Console::default();
        let queue = StdoutQueue::spawn(console.clone(), |_| {});
        for i in 0..1000u32 {
            queue.write(format!("{i},").as_bytes());
        }
        assert!(queue.finish(LONG));
        let expected: String = (0..1000u32).map(|i| format!("{i},")).collect();
        assert_eq!(*console.written.lock().unwrap(), expected.into_bytes());
    }

    #[test]
    fn barrier_returns_once_earlier_writes_are_on_the_console() {
        let console = Console::default();
        let queue = StdoutQueue::spawn(console.clone(), |_| {});
        queue.write(b"before");
        queue.barrier();
        assert_eq!(*console.written.lock().unwrap(), b"before");
        assert!(queue.finish(LONG));
    }

    #[test]
    fn a_stalled_console_does_not_block_writes_until_the_queue_fills() {
        let console = Console::default();
        let stalled = console.gate.lock().unwrap();
        let queue = StdoutQueue::spawn(console.clone(), |_| {});
        // The writer takes one chunk and blocks in it; the rest queue up.
        let (done_tx, done_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            for _ in 0..QUEUE_CHUNKS {
                queue.write(b"x");
            }
            done_tx.send(()).unwrap();
            queue
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writes blocked although the queue had room");
        drop(stalled);
        let queue = writer.join().unwrap();
        assert!(queue.finish(LONG));
        assert_eq!(console.written.lock().unwrap().len(), QUEUE_CHUNKS);
    }

    #[test]
    fn finish_gives_up_on_a_stalled_console_and_discards_the_rest() {
        let console = Console::default();
        let stalled = console.gate.lock().unwrap();
        let queue = StdoutQueue::spawn(console.clone(), |_| {});
        queue.write(b"first");
        // Let the writer take "first" and block in it before more is queued.
        thread::sleep(Duration::from_millis(50));
        queue.write(b"rest");
        assert!(!queue.finish(Duration::from_millis(50)));
        drop(stalled);
        let deadline = std::time::Instant::now() + LONG;
        while console.written.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(*console.written.lock().unwrap(), b"first");
    }

    #[test]
    fn on_write_sees_each_chunk() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let queue = StdoutQueue::spawn(std::io::sink(), move |b| {
            log.lock().unwrap().push(b.to_vec());
        });
        queue.write(b"a");
        queue.write(b"");
        queue.write(b"bc");
        assert!(queue.finish(LONG));
        assert_eq!(*seen.lock().unwrap(), vec![b"a".to_vec(), b"bc".to_vec()]);
    }
}
