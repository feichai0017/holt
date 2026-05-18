//! `WalWriter` — append-only WAL file writer.
//!
//! Lifecycle:
//!
//! 1. [`WalWriter::create`] for a fresh file or
//!    [`WalWriter::open_existing`] to resume an existing one.
//! 2. [`WalWriter::append`] for each `TxnOp` — bytes land in an
//!    in-memory buffer and are not durable yet.
//! 3. [`WalWriter::flush`] writes the buffered bytes through to
//!    the OS and runs `sync_data` so the records are durable past
//!    a power failure.
//! 4. Drop is a no-op — callers are responsible for the final
//!    `flush` (the WAL semantic is "what's flushed is durable;
//!    what's not is not").
//!
//! `Tree::checkpoint` will eventually call
//! [`WalWriter::truncate`] (Stage 5c) to trim records past the
//! last durable blob commit. v0.1 starts the file fresh and grows
//! it forever; the bound is the host filesystem.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::errors::{Error, Result};

use super::codec::{
    decode_file_header, encode_file_header, encode_record, FileHeader, FILE_HEADER_SIZE,
};
use super::txn_op::TxnOp;

/// Append-only WAL writer with explicit `flush`-for-durability.
#[derive(Debug)]
pub struct WalWriter {
    /// Path of the underlying file — needed by `truncate` to
    /// atomically replace the live log on checkpoint.
    path: PathBuf,
    /// Underlying file handle, opened in append mode.
    file: File,
    /// Buffered bytes not yet handed to the OS.
    pending: Vec<u8>,
    /// Sum of `pending.len()` over the lifetime of this writer
    /// (durable + in-flight). Useful for stats / tests.
    bytes_written: u64,
    /// File-header info recovered on open.
    header: FileHeader,
}

impl WalWriter {
    /// Create a fresh WAL file at `path` and write the file header.
    ///
    /// Returns an error if `path` already exists — use
    /// [`WalWriter::open_existing`] to append to an existing log.
    pub fn create(path: &Path, tree_id: u64) -> Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let header = FileHeader::now(tree_id);
        let mut buf = Vec::with_capacity(FILE_HEADER_SIZE);
        encode_file_header(&header, &mut buf);
        file.write_all(&buf)?;
        file.sync_data()?;
        // Reopen in append mode so subsequent writes go to the end
        // even if other code seeks around.
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            pending: Vec::with_capacity(4096),
            bytes_written: FILE_HEADER_SIZE as u64,
            header,
        })
    }

    /// Open an existing WAL file for append. Validates the file
    /// header and returns the parsed `FileHeader` via
    /// [`WalWriter::header`].
    pub fn open_existing(path: &Path) -> Result<Self> {
        let mut header_bytes = [0u8; FILE_HEADER_SIZE];
        {
            let mut f = File::open(path)?;
            use std::io::Read;
            f.read_exact(&mut header_bytes)?;
        }
        let header = decode_file_header(&header_bytes)?;
        let file = OpenOptions::new().append(true).open(path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            pending: Vec::with_capacity(4096),
            bytes_written,
            header,
        })
    }

    /// Open existing or create fresh. The file's recorded
    /// `tree_id` is **not** rewritten when opening — pass the
    /// expected `tree_id` so a mismatch (a wrong tree's WAL) can
    /// surface as an error.
    pub fn open_or_create(path: &Path, tree_id: u64) -> Result<Self> {
        if path.exists() {
            let w = Self::open_existing(path)?;
            if w.header.tree_id != tree_id {
                return Err(Error::ReplaySanityFailed {
                    context: "WAL file tree_id mismatch on open",
                    record_offset: 0,
                });
            }
            Ok(w)
        } else {
            Self::create(path, tree_id)
        }
    }

    /// Header recovered on open, including the embedded tree id.
    #[must_use]
    pub fn header(&self) -> FileHeader {
        self.header
    }

    /// Bytes written (durable + buffered) since the file was
    /// created — useful as a stand-in offset for telemetry.
    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written + self.pending.len() as u64
    }

    /// Stage a single `TxnOp` for the next flush. The record is
    /// encoded into the pending buffer in memory — no I/O.
    pub fn append(&mut self, op: &TxnOp, seq: u64) -> Result<()> {
        let before = self.pending.len();
        encode_record(op, seq, &mut self.pending)?;
        // The encoder grew the buffer by RECORD_OVERHEAD + body.
        // (No-op if `before` is what we expect; the assert keeps
        // the bytes-written counter honest under panics.)
        debug_assert!(self.pending.len() >= before);
        Ok(())
    }

    /// Write every staged record to the OS and `sync_data` so the
    /// records persist across a power loss.
    ///
    /// On platforms where `sync_data` is a no-op (memory-only
    /// filesystems used in CI / tests), durability falls back to
    /// whatever the OS provides — the bytes still land in the
    /// page cache.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.pending)?;
        self.file.sync_data()?;
        self.bytes_written += self.pending.len() as u64;
        self.pending.clear();
        Ok(())
    }

    /// Drop pending records without writing them. Useful when a
    /// caller decides mid-batch to bail out (e.g. precondition
    /// check failed). Records already `flush`ed are unaffected.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
    }

    /// Atomically reset the log to a fresh, header-only file.
    ///
    /// Used by `Tree::checkpoint` once every record up through the
    /// last `flush` is reflected in a durable blob commit: the WAL
    /// has nothing the blob doesn't already have, so we drop the
    /// records and start growing the log again from zero.
    ///
    /// Implementation: write a fresh header to a sibling temp file
    /// (`<path>.tmp`), `sync_data` it, atomically `rename` over
    /// the live log, and reopen the file handle. The temp file's
    /// pre-rename header has the **same** tree_id; `created_at`
    /// gets refreshed to the current wall clock.
    ///
    /// Any `pending` records buffered since the last `flush` are
    /// dropped — call `flush()` first if they matter.
    pub fn truncate(&mut self) -> Result<()> {
        self.pending.clear();

        let tmp_path = self.path.with_extension("wal.tmp");
        {
            let mut tmp = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            let header = FileHeader::now(self.header.tree_id);
            let mut buf = Vec::with_capacity(FILE_HEADER_SIZE);
            encode_file_header(&header, &mut buf);
            tmp.write_all(&buf)?;
            tmp.sync_data()?;
            self.header = header;
        }

        // Atomic replace — on POSIX, `rename` is atomic, and the
        // new fd inherits the durable state of the temp file.
        std::fs::rename(&tmp_path, &self.path)?;

        // The previous append-mode handle is now pointing at the
        // unlinked old inode; reopen against the new file.
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        self.bytes_written = FILE_HEADER_SIZE as u64;
        Ok(())
    }
}
