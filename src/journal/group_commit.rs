//! Group-commit journal worker.
//!
//! `WalWriter` owns the file format and append/truncate mechanics.
//! This module owns concurrency: foreground writers enqueue fully
//! encoded WAL records, then wait outside the tree's commit-publish
//! critical section. Durable callers share one `sync_data` when they
//! arrive inside the short group window.

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::api::errors::{Error, Result};

use super::writer::WalWriter;

const GROUP_COMMIT_WINDOW: Duration = Duration::from_micros(200);
const GROUP_COMMIT_MAX_BYTES: usize = 256 * 1024;

type AckTx = Sender<Result<()>>;
type AckRx = Receiver<Result<()>>;

enum JournalCommand {
    Append {
        bytes: Vec<u8>,
        durable: bool,
        ack: Option<AckTx>,
    },
    Flush {
        ack: AckTx,
    },
    Truncate {
        ack: AckTx,
    },
    Stop,
}

struct AppendRequest {
    bytes: Vec<u8>,
    durable: bool,
    ack: Option<AckTx>,
}

/// Completion handle for one durable journal append.
///
/// Non-durable appends return after the in-process queue accepts
/// the record; durable appends return this handle and wait until
/// the worker has included the record in a `sync_data` batch.
pub(crate) struct JournalAck {
    rx: AckRx,
}

impl JournalAck {
    pub(crate) fn wait(self) -> Result<()> {
        self.rx
            .recv()
            .map_err(|_| Error::Internal("journal worker dropped append acknowledgement"))?
    }
}

/// Background WAL append worker with durable group commit.
pub(crate) struct Journal {
    tx: Sender<JournalCommand>,
    handle: Mutex<Option<JoinHandle<()>>>,
    appends: Arc<AtomicU64>,
    batches: Arc<AtomicU64>,
    syncs: Arc<AtomicU64>,
}

impl Journal {
    pub(crate) fn open_or_create(path: &std::path::Path, tree_id: u64) -> Result<Self> {
        let writer = WalWriter::open_or_create(path, tree_id)?;
        let (tx, rx) = bounded::<JournalCommand>(1024);
        let batches = Arc::new(AtomicU64::new(0));
        let syncs = Arc::new(AtomicU64::new(0));
        let worker_batches = Arc::clone(&batches);
        let worker_syncs = Arc::clone(&syncs);
        let handle = thread::Builder::new()
            .name("holt-journal".to_owned())
            .spawn(move || run_worker(writer, rx, worker_batches, worker_syncs))
            .map_err(|_| Error::Internal("OS rejected thread spawn for holt-journal"))?;
        Ok(Self {
            tx,
            handle: Mutex::new(Some(handle)),
            appends: Arc::new(AtomicU64::new(0)),
            batches,
            syncs,
        })
    }

    /// Submit one fully encoded WAL record.
    pub(crate) fn submit(&self, bytes: Vec<u8>, durable: bool) -> Result<Option<JournalAck>> {
        let (ack, rx) = if durable {
            let (ack, rx) = bounded(1);
            (Some(ack), Some(rx))
        } else {
            (None, None)
        };
        self.tx
            .send(JournalCommand::Append {
                bytes,
                durable,
                ack,
            })
            .map_err(|_| Error::Internal("journal worker stopped before append"))?;
        self.appends.fetch_add(1, Ordering::Relaxed);
        Ok(rx.map(|rx| JournalAck { rx }))
    }

    /// Drain every append submitted before this call and force the
    /// WAL file durable.
    pub(crate) fn flush(&self) -> Result<()> {
        let (ack, rx) = bounded(1);
        self.tx
            .send(JournalCommand::Flush { ack })
            .map_err(|_| Error::Internal("journal worker stopped before flush"))?;
        recv_control_ack(rx, "journal worker dropped flush acknowledgement")
    }

    /// Reset the WAL to a fresh header-only file after checkpoint.
    pub(crate) fn truncate(&self) -> Result<()> {
        let (ack, rx) = bounded(1);
        self.tx
            .send(JournalCommand::Truncate { ack })
            .map_err(|_| Error::Internal("journal worker stopped before truncate"))?;
        recv_control_ack(rx, "journal worker dropped truncate acknowledgement")
    }

    pub(crate) fn stats(&self) -> JournalStats {
        JournalStats {
            appends: self.appends.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.tx.send(JournalCommand::Stop);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

/// Production journal counters surfaced through `Tree::stats`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct JournalStats {
    pub(crate) appends: u64,
    pub(crate) batches: u64,
    pub(crate) syncs: u64,
}

fn recv_control_ack(rx: AckRx, closed_msg: &'static str) -> Result<()> {
    rx.recv().map_err(|_| Error::Internal(closed_msg))?
}

fn run_worker(
    mut writer: WalWriter,
    rx: Receiver<JournalCommand>,
    batches: Arc<AtomicU64>,
    syncs: Arc<AtomicU64>,
) {
    let mut backlog = VecDeque::new();

    loop {
        let cmd = match backlog.pop_front() {
            Some(cmd) => cmd,
            None => match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            },
        };

        match cmd {
            JournalCommand::Append {
                bytes,
                durable,
                ack,
            } => {
                process_append_batch(
                    AppendRequest {
                        bytes,
                        durable,
                        ack,
                    },
                    &rx,
                    &mut backlog,
                    &mut writer,
                    &batches,
                    &syncs,
                );
            }
            JournalCommand::Flush { ack } => {
                let res = writer.flush();
                if res.is_ok() {
                    syncs.fetch_add(1, Ordering::Relaxed);
                }
                let _ = ack.send(res);
            }
            JournalCommand::Truncate { ack } => {
                let _ = ack.send(writer.truncate());
            }
            JournalCommand::Stop => break,
        }
    }
}

fn process_append_batch(
    first: AppendRequest,
    rx: &Receiver<JournalCommand>,
    backlog: &mut VecDeque<JournalCommand>,
    writer: &mut WalWriter,
    batches: &AtomicU64,
    syncs: &AtomicU64,
) {
    let mut batch = vec![first];
    let mut durable = batch[0].durable;
    let mut bytes = batch[0].bytes.len();
    let mut deadline = durable.then(|| Instant::now() + GROUP_COMMIT_WINDOW);

    loop {
        if bytes >= GROUP_COMMIT_MAX_BYTES {
            break;
        }

        let next = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.try_recv() {
                Ok(cmd) => cmd,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            },
        };

        match next {
            JournalCommand::Append {
                bytes: record,
                durable: record_durable,
                ack,
            } => {
                bytes += record.len();
                durable |= record_durable;
                if record_durable && deadline.is_none() {
                    deadline = Some(Instant::now() + GROUP_COMMIT_WINDOW);
                }
                batch.push(AppendRequest {
                    bytes: record,
                    durable: record_durable,
                    ack,
                });
            }
            other => {
                backlog.push_back(other);
                break;
            }
        }
    }

    let mut ok = true;
    for req in &batch {
        if writer.append_encoded(&req.bytes).is_err() {
            ok = false;
            break;
        }
    }

    if ok && durable {
        ok = writer.flush().is_ok();
        if ok {
            syncs.fetch_add(1, Ordering::Relaxed);
        }
    }

    batches.fetch_add(1, Ordering::Relaxed);
    let result = if ok {
        Ok(())
    } else {
        Err(Error::Internal(
            "journal worker append or durable flush failed",
        ))
    };
    for req in batch {
        if let Some(ack) = req.ack {
            let _ = ack.send(match &result {
                Ok(()) => Ok(()),
                Err(Error::Internal(msg)) => Err(Error::Internal(msg)),
                Err(_) => Err(Error::Internal("journal worker failed")),
            });
        }
    }
}
