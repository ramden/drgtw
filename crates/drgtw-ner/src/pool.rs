//! WP 4.2 — bounded worker pool over [`NerModel`].
//!
//! Design (std-only, no crossbeam):
//! - One bounded job channel (`SyncSender`/`Receiver`) with capacity =
//!   `queue_capacity`. The `Receiver` is wrapped in `Arc<Mutex<_>>` and
//!   shared by every worker; workers compete for jobs by locking the mutex
//!   just long enough to `recv()` one job.
//! - Each job carries its own one-shot reply channel (`std::sync::mpsc`).
//!   The caller blocks on `recv_timeout`; if it times out, the eventual
//!   worker reply is dropped harmlessly (the receiver is gone).
//! - `try_send` on the bounded sender returns `Unavailable` when the queue
//!   is full.
//! - On `Drop`, the sender is dropped, which closes the channel; workers
//!   observe `recv` errors and exit, then we join them (no hang).

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::model::NerModel;
use crate::{NerError, NerPoolConfig, NerSpan};

type DetectResult = Result<Vec<NerSpan>, NerError>;

/// A unit of work: the input text and a one-shot reply channel.
struct Job {
    text: String,
    reply: SyncSender<DetectResult>,
}

pub struct NerPool {
    /// Bounded job queue. `Option` so `Drop` can take it and close the channel
    /// before joining workers. Always `Some` until drop.
    sender: Option<SyncSender<Job>>,
    timeout: std::time::Duration,
    workers: Vec<JoinHandle<()>>,
}

impl NerPool {
    pub fn new(model: NerModel, config: NerPoolConfig) -> Self {
        let model = Arc::new(model);
        let (tx, rx) = mpsc::sync_channel::<Job>(config.queue_capacity);
        let rx = Arc::new(Mutex::new(rx));

        let n = config.workers.max(1);
        let mut workers = Vec::with_capacity(n);
        for i in 0..n {
            let rx = Arc::clone(&rx);
            let model = Arc::clone(&model);
            let handle = std::thread::Builder::new()
                .name(format!("ner-worker-{i}"))
                .spawn(move || worker_loop(rx, model))
                .expect("spawning NER worker thread");
            workers.push(handle);
        }

        Self {
            sender: Some(tx),
            timeout: config.timeout,
            workers,
        }
    }

    pub fn detect(&self, text: &str) -> Result<Vec<NerSpan>, NerError> {
        // One-shot reply channel (capacity 1: worker never blocks on send).
        let (reply_tx, reply_rx) = mpsc::sync_channel::<DetectResult>(1);
        let job = Job {
            text: text.to_string(),
            reply: reply_tx,
        };

        let sender = self.sender.as_ref().ok_or(NerError::Unavailable)?;
        sender.try_send(job).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => NerError::Unavailable,
            // Workers gone / channel disconnected.
            mpsc::TrySendError::Disconnected(_) => NerError::Unavailable,
        })?;

        match reply_rx.recv_timeout(self.timeout) {
            Ok(result) => result,
            // Timed out waiting for a worker; the late reply is dropped.
            Err(mpsc::RecvTimeoutError::Timeout) => Err(NerError::Timeout(self.timeout)),
            // All senders dropped without replying => workers gone.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(NerError::Unavailable),
        }
    }
}

impl Drop for NerPool {
    fn drop(&mut self) {
        // Close the job channel so worker `recv()` calls error out and loops exit.
        self.sender.take();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Worker thread body: pull jobs until the channel closes, run the model,
/// reply (best-effort — caller may have timed out and dropped the receiver).
fn worker_loop(rx: Arc<Mutex<Receiver<Job>>>, model: Arc<NerModel>) {
    loop {
        // Lock only to dequeue, then release before running inference so other
        // workers can grab the next job concurrently.
        let job = {
            let guard = match rx.lock() {
                Ok(g) => g,
                Err(_) => return, // poisoned: bail out
            };
            match guard.recv() {
                Ok(job) => job,
                Err(_) => return, // channel closed: pool dropped
            }
        };

        let result = model.detect(&job.text);
        // Ignore send errors: the caller may have timed out and gone away.
        let _ = job.reply.try_send(result);
    }
}
