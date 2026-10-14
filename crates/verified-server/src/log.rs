//! The append-only record log: framing, group commit, fsync and replay.
//!
//! One file per session, one framed line per record:
//!
//! ```text
//! <crc32c:8 hex> <seq:16 digits> <group:16 digits> <record JSON>\n
//! ```
//!
//! The CRC covers everything after it up to the newline. `group` is the seq
//! of the first record of the group commit the line was written in: every
//! record of one `Decision` goes in one write and one fsync.
//!
//! On replay a bad line (bad CRC, wrong seq, or no newline) is a
//! torn tail when nothing valid from a *later* group follows it: that group
//! was never acknowledged, any prefix of a decision recovers, so the file is
//! truncated there. A valid line from a later group after a bad one means a
//! durable record was damaged: that is corruption, and recovery stops. A
//! line whose CRC is good is a record that was written, never a torn one:
//! if this build cannot take it (it does not parse, or it is longer than
//! `max_line`, as an older build's bound allowed) recovery stops too.
//!
//! Appends enforce the same bounds replay does (`max_line` per line,
//! `max_file` per file), before writing a byte: a record replay would cut as
//! a torn tail, or a file recovery would refuse, is never acknowledged.

use core::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use verified_domain::Record;
use verified_domain::id::ActorId;

use crate::payload::Parsed;
use crate::shim::fs;

/// Why the log failed. A refused append ([`LogError::refused`]) wrote
/// nothing and leaves the log as it was; every other variant stops the
/// session: crash-only.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// A record's line would be longer than replay accepts: nothing was
    /// written, but no input makes one. `ServerLimits::check` fits every
    /// record the daemon makes in a line, so only a shell defect does.
    #[error("log {path}: record {seq} would be a {length}-byte line, more than {limit}")]
    LineTooLong {
        path: String,
        seq: u64,
        length: usize,
        limit: usize,
    },
    /// The group would grow the file past what recovery reads: nothing was
    /// written.
    #[error("log {path} would grow to {length} bytes, more than {limit}")]
    Full {
        path: String,
        length: u64,
        limit: u64,
    },
    /// The disk is full, or the quota spent: whatever of the group was
    /// written was cut off again, so the log holds what was acknowledged.
    #[error("log {path}: no space left: {source}")]
    NoSpace {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("log {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("log {path} is {length} bytes, more than {limit}")]
    TooLarge {
        path: String,
        length: u64,
        limit: u64,
    },
    #[error(
        "log {path} is corrupt at byte {offset} (seq {seq}): a valid record from a later group follows a damaged one"
    )]
    Corrupt { path: String, offset: u64, seq: u64 },
    #[error("log {path} record {seq} does not parse: {reason}")]
    Unparseable {
        path: String,
        seq: u64,
        reason: String,
    },
    #[error("log {path} has a file name that is not a session id: {name}")]
    BadName { path: String, name: String },
}

impl LogError {
    /// Whether the append was refused because the log is full (its own
    /// file cap, or the disk): nothing was written, the log still holds
    /// exactly what was acknowledged, and the session fails stop.
    #[must_use]
    pub const fn refused(&self) -> bool {
        match self {
            Self::Full { .. } | Self::NoSpace { .. } => true,
            Self::LineTooLong { .. }
            | Self::Io { .. }
            | Self::TooLarge { .. }
            | Self::Corrupt { .. }
            | Self::Unparseable { .. }
            | Self::BadName { .. } => false,
        }
    }
}

/// Whether `error` says the disk has no room: it is full, or the quota is
/// spent.
fn no_space(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
    )
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> LogError + use<> {
    let path = path.display().to_string();
    move |source| LogError::Io { path, source }
}

/// CRC-32C (Castagnoli), table-driven.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "const table construction: index < 256 fits in u32 and indexes the table; bit < 8"
)]
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut index = 0usize;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0u32;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1u32) ^ 0x82F6_3B78
            } else {
                crc >> 1u32
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

/// The CRC-32C of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        let slot = usize::from((crc.to_le_bytes()[0]) ^ byte);
        let entry = CRC_TABLE.get(slot).copied().unwrap_or_default();
        crc = (crc >> 8u32) ^ entry;
    }
    !crc
}

const CRC_WIDTH: usize = 8;
const SEQ_WIDTH: usize = 16;
/// `crc seq group ` before the JSON.
const HEADER: usize = CRC_WIDTH + 1 + SEQ_WIDTH + 1 + SEQ_WIDTH + 1;

/// One framed line, newline included.
///
/// # Panics
/// Never: records serialize infallibly.
#[must_use]
pub fn encode_frame(seq: u64, group: u64, record: &Record<Parsed>) -> Vec<u8> {
    let json = serde_json::to_vec(record).expect("records have string keys and serialize");
    let mut body = format!("{seq:016} {group:016} ").into_bytes();
    body.extend_from_slice(&json);
    let mut line = format!("{:08x} ", crc32c(&body)).into_bytes();
    line.extend_from_slice(&body);
    line.push(b'\n');
    line
}

/// A line that frames correctly: its seq, group and JSON.
struct Frame<'line> {
    seq: u64,
    group: u64,
    json: &'line [u8],
}

fn parse_digits(bytes: &[u8]) -> Option<u64> {
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    core::str::from_utf8(bytes).ok()?.parse().ok()
}

/// The frame in `line` (without its newline) if its CRC and layout are good.
fn frame(line: &[u8]) -> Option<Frame<'_>> {
    let crc_text = line.get(..CRC_WIDTH)?;
    let body = line.get(CRC_WIDTH.checked_add(1)?..)?;
    if line.get(CRC_WIDTH) != Some(&b' ') {
        return None;
    }
    // Only the spelling the writer makes (lowercase): `from_str_radix` alone
    // would also read `A`..`F`, so a flipped case bit would go unnoticed.
    if !crc_text
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return None;
    }
    let crc = u32::from_str_radix(core::str::from_utf8(crc_text).ok()?, 16).ok()?;
    if crc != crc32c(body) {
        return None;
    }
    let seq = parse_digits(body.get(..SEQ_WIDTH)?)?;
    let group_start = SEQ_WIDTH.checked_add(1)?;
    let group = parse_digits(body.get(group_start..group_start.checked_add(SEQ_WIDTH)?)?)?;
    let json = line.get(HEADER..)?;
    Some(Frame { seq, group, json })
}

/// What a scan of a log's bytes found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The records, in seq order from 0.
    pub records: Vec<Record<Parsed>>,
    /// Bytes holding exactly those records: any after them are an
    /// unacknowledged group commit cut short.
    pub valid_len: u64,
}

/// Why bytes are not a log, with the position of the problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanError {
    Corrupt { offset: u64, seq: u64 },
    Unparseable { seq: u64, reason: String },
}

impl ScanError {
    /// This error, in the log at `path`.
    #[must_use]
    pub fn at(self, path: String) -> LogError {
        match self {
            Self::Corrupt { offset, seq } => LogError::Corrupt { path, offset, seq },
            Self::Unparseable { seq, reason } => LogError::Unparseable { path, seq, reason },
        }
    }
}

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).expect("a log is shorter than 2^64 bytes")
}

/// Replays framed bytes. Pure: the property tests drive it directly.
///
/// # Errors
/// Corruption before the tail, or a well-framed record that does not parse.
///
/// # Panics
/// Never for inputs shorter than 2^64 bytes.
pub fn scan(bytes: &[u8], max_line: usize) -> Result<Scan, ScanError> {
    let mut records = Vec::new();
    let mut offset: usize = 0;
    let mut seq: u64 = 0;
    while let Some(rest) = bytes.get(offset..) {
        if rest.is_empty() {
            break;
        }
        let newline = rest.iter().position(|byte| *byte == b'\n');
        let good = newline.and_then(|end| {
            let found = frame(rest.get(..end)?)?;
            (found.seq == seq && found.group <= seq).then_some((found, end))
        });
        let Some((found, end)) = good else {
            return torn_or_corrupt(bytes, offset, seq, records);
        };
        if end > max_line {
            return Err(ScanError::Unparseable {
                seq,
                reason: format!("a {end}-byte line, more than {max_line}"),
            });
        }
        // Parsing is the whole check: every id and every payload slot has
        // its own type, so a line that deserializes is a well-formed record.
        let record: Record<Parsed> =
            serde_json::from_slice(found.json).map_err(|error| ScanError::Unparseable {
                seq,
                reason: error.to_string(),
            })?;
        records.push(record);
        offset = offset
            .checked_add(end)
            .and_then(|at| at.checked_add(1))
            .expect("offsets stay within the buffer");
        seq = seq.checked_add(1).expect("fewer than 2^64 records");
    }
    Ok(Scan {
        records,
        valid_len: to_u64(offset),
    })
}

/// A bad line at `offset` where `seq` was expected: torn unless a valid line
/// from a later group follows.
fn torn_or_corrupt(
    bytes: &[u8],
    offset: usize,
    seq: u64,
    records: Vec<Record<Parsed>>,
) -> Result<Scan, ScanError> {
    let rest = bytes.get(offset..).unwrap_or_default();
    for line in rest.split(|byte| *byte == b'\n').skip(1) {
        if let Some(later) = frame(line)
            && later.group > seq
        {
            return Err(ScanError::Corrupt {
                offset: to_u64(offset),
                seq,
            });
        }
    }
    Ok(Scan {
        records,
        valid_len: to_u64(offset),
    })
}

/// The Log seam: one session's durable records.
#[async_trait]
pub trait Log: Send + fmt::Debug {
    /// The seq the next appended record gets.
    fn next_seq(&self) -> u64;

    /// Appends `records` as one group commit: one write, one fsync, within
    /// the file's bound. Returns once they are durable. After an error the
    /// log is unusable, except after a [`LogError::refused`] one, which
    /// wrote nothing.
    ///
    /// # Errors
    /// Any IO error; the caller crashes.
    async fn append(&mut self, records: &[Record<Parsed>]) -> Result<(), LogError>;
}

/// The framed lines of a group commit from `first`, refused when a line
/// would be longer than `max_line` (as replay measures it: without its
/// newline) or the file, now `length` bytes, would grow past `limit`.
fn frames(
    path: &str,
    first: u64,
    records: &[Record<Parsed>],
    length: u64,
    max_line: usize,
    limit: u64,
) -> Result<Vec<u8>, LogError> {
    let mut bytes = Vec::new();
    let mut seq = first;
    for record in records {
        let line = encode_frame(seq, first, record);
        let content = line.len().saturating_sub(1);
        if content > max_line {
            return Err(LogError::LineTooLong {
                path: path.to_owned(),
                seq,
                length: content,
                limit: max_line,
            });
        }
        bytes.extend_from_slice(&line);
        seq = seq.checked_add(1).expect("fewer than 2^64 records");
    }
    let grown = length.saturating_add(to_u64(bytes.len()));
    if grown > limit {
        return Err(LogError::Full {
            path: path.to_owned(),
            length: grown,
            limit,
        });
    }
    Ok(bytes)
}

fn advance(next: u64, records: &[Record<Parsed>]) -> u64 {
    next.checked_add(to_u64(records.len()))
        .expect("fewer than 2^64 records")
}

/// A log file, through the fs shim (turmoil's filesystem under `sim`).
#[derive(Debug)]
pub struct FileLog {
    path: PathBuf,
    file: fs::File,
    next: u64,
    /// The file's length: every acknowledged byte.
    length: u64,
    max_line: usize,
    max_file: u64,
    /// The planted defect (`Planted::SkipFsync`): appends skip their
    /// fsync. Only under `sim`.
    #[cfg(feature = "sim")]
    skip_sync: bool,
}

impl FileLog {
    /// Creates a new, empty log durably: create, sync it, sync its directory.
    ///
    /// # Errors
    /// The file exists, or any IO error.
    pub async fn create(path: &Path, max_line: usize, max_file: u64) -> Result<Self, LogError> {
        let file = fs::create_new(path).await.map_err(io_error(path))?;
        file.sync_all().await.map_err(io_error(path))?;
        if let Some(dir) = path.parent() {
            fs::sync_dir(dir).await.map_err(io_error(path))?;
        }
        Ok(Self {
            path: path.to_owned(),
            file,
            next: 0,
            length: 0,
            max_line,
            max_file,
            #[cfg(feature = "sim")]
            skip_sync: false,
        })
    }

    /// Opens a log, replays it and cuts a torn tail (durably) so appends
    /// continue after the last whole record.
    ///
    /// # Errors
    /// IO errors, corruption, or a record that does not parse.
    pub async fn open(
        path: &Path,
        max_line: usize,
        max_file: u64,
    ) -> Result<(Self, Vec<Record<Parsed>>), LogError> {
        let bytes = read(path, max_file).await?;
        let scan = scan(&bytes, max_line).map_err(|error| error.at(path.display().to_string()))?;
        let file = fs::open_append(path).await.map_err(io_error(path))?;
        if scan.valid_len < to_u64(bytes.len()) {
            fs::truncate(&file, scan.valid_len)
                .await
                .map_err(io_error(path))?;
        }
        let log = Self {
            path: path.to_owned(),
            file,
            next: to_u64(scan.records.len()),
            length: scan.valid_len,
            max_line,
            max_file,
            #[cfg(feature = "sim")]
            skip_sync: false,
        };
        Ok((log, scan.records))
    }
}

/// The log's bytes, refused when the file is longer than recovery reads.
async fn read(path: &Path, max_file: u64) -> Result<Vec<u8>, LogError> {
    if let Some((_, length)) = fs::stat(path).await.map_err(io_error(path))?
        && length > max_file
    {
        return Err(LogError::TooLarge {
            path: path.display().to_string(),
            length,
            limit: max_file,
        });
    }
    fs::read(path).await.map_err(io_error(path))
}

#[async_trait]
impl Log for FileLog {
    fn next_seq(&self) -> u64 {
        self.next
    }

    async fn append(&mut self, records: &[Record<Parsed>]) -> Result<(), LogError> {
        let bytes = frames(
            &self.path.display().to_string(),
            self.next,
            records,
            self.length,
            self.max_line,
            self.max_file,
        )?;
        #[cfg(feature = "sim")]
        let appended = if self.skip_sync {
            fs::append_unsynced(&mut self.file, &bytes).await
        } else {
            fs::append_synced(&mut self.file, &bytes).await
        };
        #[cfg(not(feature = "sim"))]
        let appended = fs::append_synced(&mut self.file, &bytes).await;
        if let Err(error) = appended {
            if !no_space(&error) {
                return Err(io_error(&self.path)(error));
            }
            // Storage is the world: a full disk refuses the group. Cut off
            // what of it was written, or, if that fails too, crash: the
            // log's tail is then unknown, and recovery reads it.
            fs::truncate(&self.file, self.length)
                .await
                .map_err(io_error(&self.path))?;
            return Err(LogError::NoSpace {
                path: self.path.display().to_string(),
                source: error,
            });
        }
        self.next = advance(self.next, records);
        self.length = self.length.saturating_add(to_u64(bytes.len()));
        Ok(())
    }
}

/// Shared bytes standing in for a disk: what survives a simulated restart.
#[derive(Debug, Clone, Default)]
pub struct MemDisk {
    #[expect(
        clippy::disallowed_types,
        reason = "test disk: the one MemLog using it writes; readers are tests between restarts; dropped with the test"
    )]
    files: Arc<std::sync::Mutex<std::collections::BTreeMap<ActorId, Vec<u8>>>>,
}

impl MemDisk {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of one session's bytes.
    ///
    /// # Panics
    /// When a test panicked while holding the disk.
    #[must_use]
    pub fn bytes(&self, session: ActorId) -> Option<Vec<u8>> {
        self.files
            .lock()
            .expect("the disk lock is never poisoned")
            .get(&session)
            .cloned()
    }

    /// Replaces one session's bytes (to tear or corrupt them in a test).
    ///
    /// # Panics
    /// When a test panicked while holding the disk.
    pub fn set_bytes(&self, session: ActorId, bytes: Vec<u8>) {
        self.files
            .lock()
            .expect("the disk lock is never poisoned")
            .insert(session, bytes);
    }

    fn sessions(&self) -> Vec<ActorId> {
        self.files
            .lock()
            .expect("the disk lock is never poisoned")
            .keys()
            .copied()
            .collect()
    }

    fn append(&self, session: ActorId, bytes: &[u8]) {
        self.files
            .lock()
            .expect("the disk lock is never poisoned")
            .entry(session)
            .or_default()
            .extend_from_slice(bytes);
    }
}

/// A log in memory with the file log's framing and bounds, for tests.
#[derive(Debug)]
pub struct MemLog {
    disk: MemDisk,
    session: ActorId,
    next: u64,
    max_line: usize,
    max_file: u64,
}

#[async_trait]
impl Log for MemLog {
    fn next_seq(&self) -> u64 {
        self.next
    }

    async fn append(&mut self, records: &[Record<Parsed>]) -> Result<(), LogError> {
        let length = self
            .disk
            .bytes(self.session)
            .map_or(0, |bytes| to_u64(bytes.len()));
        let bytes = frames(
            &format!("mem:{}", self.session),
            self.next,
            records,
            length,
            self.max_line,
            self.max_file,
        )?;
        self.disk.append(self.session, &bytes);
        self.next = advance(self.next, records);
        Ok(())
    }
}

/// Where sessions' logs live.
#[async_trait]
pub trait LogStore: Send + Sync + fmt::Debug {
    /// Every session with a log.
    ///
    /// # Errors
    /// IO errors, a log name that is not a session id, or more logs than
    /// the store's bound.
    async fn list(&self) -> Result<Vec<ActorId>, LogError>;

    /// Creates `session`'s empty log durably.
    ///
    /// # Errors
    /// It exists, or IO errors.
    async fn create(&self, session: ActorId) -> Result<Box<dyn Log>, LogError>;

    /// Opens `session`'s log, cutting a torn tail, with its records.
    ///
    /// # Errors
    /// IO errors, corruption, or a record that does not parse.
    async fn open(&self, session: ActorId)
    -> Result<(Box<dyn Log>, Vec<Record<Parsed>>), LogError>;
}

/// `<dir>/<session id>.log`, through the fs shim.
#[derive(Debug, Clone)]
pub struct FileLogStore {
    dir: PathBuf,
    max_line: usize,
    max_file: u64,
    max_sessions: usize,
    /// The planted defect (`Planted::SkipFsync`), for every log it opens.
    #[cfg(feature = "sim")]
    skip_sync: bool,
}

impl FileLogStore {
    #[must_use]
    pub const fn new(dir: PathBuf, max_line: usize, max_file: u64, max_sessions: usize) -> Self {
        Self {
            dir,
            max_line,
            max_file,
            max_sessions,
            #[cfg(feature = "sim")]
            skip_sync: false,
        }
    }

    /// This store, its logs acknowledging appends without their fsync: the
    /// planted defect `Planted::SkipFsync` (breaks L3), for the
    /// simulation's mutation tests.
    #[cfg(feature = "sim")]
    #[must_use]
    pub fn skipping_sync(self) -> Self {
        Self {
            skip_sync: true,
            ..self
        }
    }

    fn path(&self, session: ActorId) -> PathBuf {
        self.dir.join(format!("{session}.log"))
    }
}

#[async_trait]
impl LogStore for FileLogStore {
    async fn list(&self) -> Result<Vec<ActorId>, LogError> {
        let entries = fs::list(&self.dir, self.max_sessions.saturating_mul(2))
            .await
            .map_err(io_error(&self.dir))?;
        let mut sessions = Vec::new();
        for entry in entries {
            let name = entry.name.to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".log") else {
                continue;
            };
            let session = ActorId::parse(stem).map_err(|_bad| LogError::BadName {
                path: self.dir.display().to_string(),
                name: name.clone(),
            })?;
            if sessions.len() >= self.max_sessions {
                return Err(LogError::Io {
                    path: self.dir.display().to_string(),
                    source: std::io::Error::other(format!(
                        "more than {} session logs",
                        self.max_sessions
                    )),
                });
            }
            sessions.push(session);
        }
        Ok(sessions)
    }

    async fn create(&self, session: ActorId) -> Result<Box<dyn Log>, LogError> {
        #[cfg_attr(not(feature = "sim"), expect(unused_mut, reason = "planted under sim"))]
        let mut log = FileLog::create(&self.path(session), self.max_line, self.max_file).await?;
        #[cfg(feature = "sim")]
        {
            log.skip_sync = self.skip_sync;
        }
        Ok(Box::new(log))
    }

    async fn open(
        &self,
        session: ActorId,
    ) -> Result<(Box<dyn Log>, Vec<Record<Parsed>>), LogError> {
        #[cfg_attr(not(feature = "sim"), expect(unused_mut, reason = "planted under sim"))]
        let (mut log, records) =
            FileLog::open(&self.path(session), self.max_line, self.max_file).await?;
        #[cfg(feature = "sim")]
        {
            log.skip_sync = self.skip_sync;
        }
        Ok((Box::new(log), records))
    }
}

/// Logs on a [`MemDisk`], for tests.
#[derive(Debug, Clone)]
pub struct MemLogStore {
    disk: MemDisk,
    max_line: usize,
    max_file: u64,
}

impl MemLogStore {
    #[must_use]
    pub const fn new(disk: MemDisk, max_line: usize, max_file: u64) -> Self {
        Self {
            disk,
            max_line,
            max_file,
        }
    }
}

#[async_trait]
impl LogStore for MemLogStore {
    async fn list(&self) -> Result<Vec<ActorId>, LogError> {
        Ok(self.disk.sessions())
    }

    async fn create(&self, session: ActorId) -> Result<Box<dyn Log>, LogError> {
        self.disk.append(session, &[]);
        Ok(Box::new(MemLog {
            disk: self.disk.clone(),
            session,
            next: 0,
            max_line: self.max_line,
            max_file: self.max_file,
        }))
    }

    async fn open(
        &self,
        session: ActorId,
    ) -> Result<(Box<dyn Log>, Vec<Record<Parsed>>), LogError> {
        let bytes = self.disk.bytes(session).unwrap_or_default();
        let scan =
            scan(&bytes, self.max_line).map_err(|error| error.at(format!("mem:{session}")))?;
        let kept = bytes
            .get(..usize::try_from(scan.valid_len).unwrap_or(usize::MAX))
            .unwrap_or_default()
            .to_vec();
        self.disk.set_bytes(session, kept);
        let log = MemLog {
            disk: self.disk.clone(),
            session,
            next: to_u64(scan.records.len()),
            max_line: self.max_line,
            max_file: self.max_file,
        };
        Ok((Box::new(log), scan.records))
    }
}

#[cfg(test)]
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "test arithmetic over small generated sizes; a panic fails the test"
)]
mod tests {
    use proptest::prelude::*;
    use verified_domain::id::{ActorId, UnixMillis};
    use verified_domain::{Epoch, Record};

    use super::{
        LogError, LogStore as _, MemDisk, MemLogStore, ScanError, crc32c, encode_frame, no_space,
        scan, to_u64,
    };
    use crate::payload::{Parsed, measure};

    const MAX_LINE: usize = 1 << 16;

    /// Storage is the world (L3): a full disk or a spent quota refuses an
    /// append, as the log's own cap does; any other IO error crashes.
    #[test]
    fn a_full_disk_refuses_and_other_errors_crash() {
        use std::io::{Error, ErrorKind};
        for (kind, full) in [
            (ErrorKind::StorageFull, true),
            (ErrorKind::QuotaExceeded, true),
            (ErrorKind::PermissionDenied, false),
            (ErrorKind::Other, false),
        ] {
            assert_eq!(no_space(&Error::from(kind)), full, "{kind:?}");
        }
        let refused = LogError::NoSpace {
            path: "log".to_owned(),
            source: Error::from(ErrorKind::StorageFull),
        };
        assert!(refused.refused(), "a full disk fails stop");
        let crashed = LogError::Io {
            path: "log".to_owned(),
            source: Error::from(ErrorKind::Other),
        };
        assert!(!crashed.refused(), "another error crashes");
    }

    fn epochs(count: u64) -> Vec<Record<Parsed>> {
        (0..count)
            .map(|epoch| Record::EpochBumped {
                epoch: Epoch(epoch),
            })
            .collect()
    }

    /// Frames `records` in groups of the given sizes.
    fn framed(records: &[Record<Parsed>], groups: &[usize]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut seq = 0u64;
        let mut index = 0usize;
        for size in groups {
            let first = seq;
            for record in records.iter().skip(index).take(*size) {
                bytes.extend_from_slice(&encode_frame(seq, first, record));
                seq += 1;
            }
            index += size;
        }
        bytes
    }

    /// A record whose line is about `size` bytes.
    fn sized(size: usize) -> Record<Parsed> {
        Record::Delivered {
            envelope: verified_domain::Envelope {
                id: verified_domain::EnvelopeId::User(verified_domain::MessageId::from_parts(
                    UnixMillis::new(1_700_000_000_000).expect("a time"),
                    &[u8::try_from(size % 251).expect("small"); 10],
                )),
                from: verified_domain::Address::User,
                to: session(),
                reply_to: None,
                body: verified_domain::Body::Text(measure("x".repeat(size))),
            },
        }
    }

    fn session() -> ActorId {
        ActorId::from_parts(
            UnixMillis::new(1_700_000_000_000).expect("a time"),
            &[3; 10],
        )
    }

    /// Appends groups, keeping what each acknowledged: the acknowledged
    /// records, exactly, are what replay finds; a group a bound refused
    /// wrote nothing.
    async fn append_all(
        groups: &[Vec<Record<Parsed>>],
        max_line: usize,
        max_file: u64,
    ) -> (Vec<Record<Parsed>>, Vec<LogError>, Vec<Record<Parsed>>) {
        let disk = MemDisk::new();
        let store = MemLogStore::new(disk.clone(), max_line, max_file);
        let mut log = store.create(session()).await.expect("created");
        let mut acked = Vec::new();
        let mut refused = Vec::new();
        for group in groups {
            let before = disk.bytes(session());
            match log.append(group).await {
                Ok(()) => acked.extend(group.iter().cloned()),
                Err(error) => {
                    assert!(
                        matches!(error, LogError::LineTooLong { .. } | LogError::Full { .. }),
                        "only a bound refuses: {error}"
                    );
                    assert_eq!(disk.bytes(session()), before, "{error} wrote nothing");
                    refused.push(error);
                }
            }
        }
        let bytes = disk.bytes(session()).unwrap_or_default();
        let replayed = scan(&bytes, max_line).expect("the log replays");
        assert_eq!(replayed.valid_len, to_u64(bytes.len()), "nothing torn");
        assert!(
            u64::try_from(bytes.len()).expect("small") <= max_file,
            "the file stays within its bound"
        );
        (acked, refused, replayed.records)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(future)
    }

    #[test]
    fn an_oversized_record_is_refused_at_append() {
        let (acked, refused, replayed) = block_on(append_all(
            &[
                vec![sized(10)],
                vec![sized(10), sized(5_000)],
                vec![sized(20)],
            ],
            1_024,
            1 << 20,
        ));
        assert_eq!(
            acked,
            vec![sized(10), sized(20)],
            "the oversized group wrote nothing"
        );
        assert!(
            matches!(refused.as_slice(), [LogError::LineTooLong { seq: 2, .. }]),
            "{refused:?}"
        );
        assert_eq!(
            replayed, acked,
            "replay finds exactly what was acknowledged"
        );
        let (acked, refused, replayed) = block_on(append_all(
            &[vec![sized(100)], vec![sized(100)]],
            1_024,
            400,
        ));
        assert_eq!(
            acked,
            vec![sized(100)],
            "the second group would overfill the file"
        );
        assert!(
            matches!(refused.as_slice(), [LogError::Full { .. }]),
            "{refused:?}"
        );
        assert_eq!(replayed, acked);
    }

    #[test]
    fn crc32c_matches_the_standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283, "RFC 3720 check value");
    }

    #[test]
    fn clean_log_replays() {
        let records = epochs(5);
        let bytes = framed(&records, &[2, 3]);
        let scanned = scan(&bytes, MAX_LINE).expect("replays");
        assert_eq!(scanned.records, records, "every record");
        assert_eq!(scanned.valid_len, to_u64(bytes.len()), "nothing torn");
    }

    #[test]
    fn damaged_acknowledged_group_is_corrupt() {
        let records = epochs(4);
        let mut bytes = framed(&records, &[1, 1, 2]);
        // Flip a byte inside the first record: a later group follows it.
        let at = 20;
        bytes[at] ^= 0x01;
        assert_eq!(
            scan(&bytes, MAX_LINE),
            Err(ScanError::Corrupt { offset: 0, seq: 0 }),
            "a later group proves the damaged one was acknowledged"
        );
    }

    #[test]
    fn well_framed_garbage_record_stops_recovery() {
        let mut body = b"0000000000000000 0000000000000000 {\"type\":\"Nope\"}".to_vec();
        let mut line = format!("{:08x} ", crc32c(&body)).into_bytes();
        line.append(&mut body);
        line.push(b'\n');
        assert!(
            matches!(
                scan(&line, MAX_LINE),
                Err(ScanError::Unparseable { seq: 0, .. })
            ),
            "a good CRC over a bad record is not a torn write"
        );
    }

    /// A written line longer than the bound (an older build's) is no torn
    /// tail: recovery stops on it rather than cut it, whether or not a later
    /// group follows.
    #[test]
    fn an_acknowledged_line_past_the_bound_stops_recovery() {
        let records = vec![sized(10), sized(2_000)];
        for groups in [&[1, 1][..], &[2]] {
            let bytes = framed(&records, groups);
            assert!(
                matches!(
                    scan(&bytes, 1_000),
                    Err(ScanError::Unparseable { seq: 1, .. })
                ),
                "{groups:?}: {:?}",
                scan(&bytes, 1_000).map(|scanned| scanned.valid_len)
            );
        }
    }

    proptest! {
        /// Any cut inside the last group, and any damage confined to it, is a
        /// torn tail: the records before the group survive, the rest go.
        #[test]
        fn torn_last_group_truncates(
            groups in prop::collection::vec(1usize..4, 1..6),
            cut in any::<prop::sample::Index>(),
            flip in any::<Option<(prop::sample::Index, u8)>>(),
        ) {
            let count: usize = groups.iter().sum();
            let records = epochs(u64::try_from(count).expect("small"));
            let last = *groups.last().expect("one group");
            let before: usize = count - last;
            let durable = framed(&records[..before], &groups[..groups.len() - 1]);
            let all = framed(&records, &groups);
            let tail_len = all.len() - durable.len();
            // Keep a prefix of the last group's bytes (never all of it)...
            let keep = cut.index(tail_len);
            let mut bytes = all[..durable.len() + keep].to_vec();
            // ...and maybe damage one byte of what was kept of it.
            if let Some((at, mask)) = flip && keep > 0 && mask != 0 {
                let at = durable.len() + at.index(keep);
                bytes[at] ^= mask;
            }
            let scanned = scan(&bytes, MAX_LINE).expect("a torn tail is not corruption");
            prop_assert!(scanned.records.len() >= before, "acknowledged records survive");
            prop_assert_eq!(&scanned.records[..], &records[..scanned.records.len()]);
            prop_assert!(scanned.records.len() < count, "the cut group is not whole");
            let valid = usize::try_from(scanned.valid_len).expect("small");
            prop_assert_eq!(scan(&bytes[..valid], MAX_LINE).map(|clean| clean.valid_len), Ok(scanned.valid_len));
        }

        /// Damage to any acknowledged group, followed by a later group, is
        /// detected as corruption rather than silently truncated.
        #[test]
        fn damage_before_a_later_group_is_corrupt(
            groups in prop::collection::vec(1usize..4, 2..6),
            at in any::<prop::sample::Index>(),
            mask in 1u8..=255,
        ) {
            let count: usize = groups.iter().sum();
            let records = epochs(u64::try_from(count).expect("small"));
            let last = *groups.last().expect("groups");
            let mut bytes = framed(&records, &groups);
            let earlier = framed(&records[..count - last], &groups[..groups.len() - 1]).len();
            let at = at.index(earlier);
            bytes[at] ^= mask;
            let scanned = scan(&bytes, MAX_LINE);
            let damaged_newline = bytes[at] == b'\n' || (bytes[at] ^ mask) == b'\n';
            // Damage that merges or splits lines still never truncates silently.
            prop_assert!(
                matches!(scanned, Err(ScanError::Corrupt { .. })) || damaged_newline,
                "damage at {} went unnoticed: {:?}", at, scanned
            );
        }

        /// Whatever the sizes and bounds, everything acknowledged replays,
        /// exactly, and nothing refused does.
        #[test]
        fn everything_acknowledged_replays(
            groups in prop::collection::vec(prop::collection::vec(0usize..3_000, 1..4), 1..8),
            max_line in 200usize..4_000,
            max_file in 500u64..20_000,
        ) {
            let groups: Vec<Vec<Record<Parsed>>> = groups
                .iter()
                .map(|sizes| sizes.iter().map(|size| sized(*size)).collect())
                .collect();
            let (acked, _, replayed) = block_on(append_all(&groups, max_line, max_file));
            prop_assert_eq!(replayed, acked);
        }
    }
}
