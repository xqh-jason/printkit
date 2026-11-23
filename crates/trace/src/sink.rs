//! Event sinks: JSONL on disk, an in-memory sink for tests, and a null sink.
//!
//! [`JsonlSink`] writes through a bounded channel and one writer thread: a
//! slow disk must never stall a render, and a full channel drops events rather
//! than blocking the producer. Dropped events are counted and reported on the
//! next event that is written, so a gap in `seq` can be explained from the log
//! itself instead of looking like a lost job.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
#[cfg(test)]
use std::time::Duration;

#[cfg(feature = "gzip")]
use flate2::Compression;
#[cfg(feature = "gzip")]
use flate2::read::GzDecoder;
#[cfg(feature = "gzip")]
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use time::{Date, Duration as TimeDuration, OffsetDateTime};

use crate::event::{FieldKey, Scalar, TraceEvent};

/// Queue depth between producers and the writer thread.
const DEFAULT_CAPACITY: usize = 1024;
/// Days of JSONL history kept on disk.
pub const DEFAULT_RETENTION_DAYS: u16 = 90;

/// How much is written. `off` writes nothing; `usage-only` writes statistics
/// rows only (no JSONL), so the sink stays silent in that mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    /// `off`: no JSONL events and no usage rows are written.
    Off,
    /// `debug`: every event and the usage rows are written.
    Debug,
    /// `info`: every event and the usage rows are written; currently identical to
    /// `debug`.
    Info,
    /// `usage-only`: usage rows are written but no JSONL events are.
    UsageOnly,
}

impl LogLevel {
    /// Whether this level writes JSONL events: true for `debug` and `info` only.
    pub fn writes_events(self) -> bool {
        matches!(self, Self::Debug | Self::Info)
    }

    /// Whether the usage table is written. Only `off` suppresses it.
    pub fn writes_usage(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl FromStr for LogLevel {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "usage-only" => Ok(Self::UsageOnly),
            _ => Err(()),
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::UsageOnly => "usage-only",
        })
    }
}
/// Whether the sink archives finished days when it opens.
///
/// The sequence floor is always restored from the files on disk, even with
/// [`RotateOnOpen::No`]; only compression and retention are skipped. A disabled
/// observability plugin therefore starts from the existing `seq` without
/// touching the log directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateOnOpen {
    /// Compress finished days and delete files past the retention window.
    Yes,
    /// Leave every existing file untouched; the writer still rotates when it
    /// crosses midnight after writes resume.
    No,
}

/// Anything that accepts trace events.
///
/// Synchronous on purpose: the producer only hands the event to a channel (or
/// to memory), so the call never waits for a disk.
pub trait Sink: Send + Sync {
    /// Posts one event to the sink.
    ///
    /// The trait is `Send + Sync`, so implementations must accept calls from
    /// several threads at once; a call must stay cheap for the producer and must
    /// not report a disk error back to it.
    fn emit(&self, event: &TraceEvent);

    /// Count one event discarded by a gate in front of this sink. The next
    /// event that is written carries the count in `dropped`.
    fn note_dropped(&self) {}

    /// Count one field whose name is not in the whitelist. The event itself is
    /// still written, minus the field; the next event carries the counter.
    fn note_dropped_field(&self) {}
}

/// Drops every event. Used by `--log=off` and `--log=usage-only`.
#[derive(Debug, Default)]
pub struct NullSink;

impl Sink for NullSink {
    fn emit(&self, _event: &TraceEvent) {}
}

/// Collects events for assertions. Tests only; it has no size limit.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<TraceEvent>>,
}

impl MemorySink {
    /// A cloned snapshot of every collected event; a poisoned lock yields an empty
    /// vector.
    pub fn events(&self) -> Vec<TraceEvent> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

impl Sink for MemorySink {
    fn emit(&self, event: &TraceEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event.clone());
        }
    }
}

/// One JSON object per line, split by UTC day: `printkit-YYYY-MM-DD.jsonl`.
///
/// Finished days are gzipped and kept for `retention_days` days (default
/// [`DEFAULT_RETENTION_DAYS`]); older files are removed. File writes are best
/// effort: a failing disk is reported once on stderr and never fails a print.
/// Dropping the sink closes the channel and joins the writer, so a one-shot CLI
/// flushes everything before it exits.
///
/// The sink is `Send + Sync`: `emit` clones the event and tries a non-blocking
/// channel send, so several producer threads may share one sink.
pub struct JsonlSink {
    sender: Option<SyncSender<TraceEvent>>,
    dropped: Arc<AtomicU64>,
    dropped_fields: Arc<AtomicU64>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl JsonlSink {
    /// A sink with the default queue depth and retention.
    pub fn new(dir: impl Into<PathBuf>, level: LogLevel) -> Self {
        Self::with_capacity(dir, level, DEFAULT_RETENTION_DAYS, DEFAULT_CAPACITY)
    }

    /// A sink with the configured retention; the queue depth stays default.
    pub fn with_retention(dir: impl Into<PathBuf>, level: LogLevel, retention_days: u16) -> Self {
        Self::with_retention_opts(dir, level, retention_days, RotateOnOpen::Yes)
    }

    /// A sink with the configured retention and an explicit open-time rotation
    /// choice.
    pub fn with_retention_opts(
        dir: impl Into<PathBuf>,
        level: LogLevel,
        retention_days: u16,
        rotate: RotateOnOpen,
    ) -> Self {
        Self::with_capacity_opts(dir, level, retention_days, DEFAULT_CAPACITY, rotate)
    }

    /// A sink with an explicit queue depth, used by tests that need to
    /// observe a full channel.
    pub fn with_capacity(
        dir: impl Into<PathBuf>,
        level: LogLevel,
        retention_days: u16,
        capacity: usize,
    ) -> Self {
        Self::with_capacity_opts(dir, level, retention_days, capacity, RotateOnOpen::Yes)
    }

    fn with_capacity_opts(
        dir: impl Into<PathBuf>,
        level: LogLevel,
        retention_days: u16,
        capacity: usize,
        rotate: RotateOnOpen,
    ) -> Self {
        let dir = dir.into();
        #[cfg(unix)]
        if level.writes_events() && fs::create_dir_all(&dir).is_ok() {
            restrict_path(&dir, 0o700);
        }
        if level.writes_events() {
            // Continue the sequence where the previous process stopped, even
            // when only old, gzipped files carry the highest number. This is a
            // read-only scan and is unrelated to rotation.
            if let Some(max) = max_seq_in_dir(&dir) {
                TraceEvent::seed_seq(max);
            }
            if rotate == RotateOnOpen::Yes {
                let _ = rotate_dir(&dir, retention_days, OffsetDateTime::now_utc().date());
            }
        }

        let (sender, receiver) = sync_channel(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_fields = Arc::new(AtomicU64::new(0));
        let writer = if level.writes_events() {
            let writer = Writer {
                dir,
                retention_days,
                dropped: Arc::clone(&dropped),
                dropped_fields: Arc::clone(&dropped_fields),
                warned: false,
                #[cfg(test)]
                delay: None,
            };
            thread::Builder::new()
                .name("printkit-trace".to_owned())
                .spawn(move || writer.run(receiver))
                .ok()
        } else {
            None
        };

        Self {
            sender: writer.as_ref().map(|_| sender),
            dropped,
            dropped_fields,
            writer: Mutex::new(writer),
        }
    }

    /// Events dropped because the channel was full since the last written
    /// event. For tests and diagnostics.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// A sink whose writer pauses before every event, so a test can fill the
    /// channel deterministically.
    #[cfg(test)]
    fn with_test_delay(dir: impl Into<PathBuf>, capacity: usize, delay: Duration) -> Self {
        let dir = dir.into();
        let (sender, receiver) = sync_channel(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_fields = Arc::new(AtomicU64::new(0));
        let writer = Writer {
            dir,
            retention_days: DEFAULT_RETENTION_DAYS,
            dropped: Arc::clone(&dropped),
            dropped_fields: Arc::clone(&dropped_fields),
            warned: false,
            delay: Some(delay),
        };
        let handle = thread::Builder::new()
            .name("printkit-trace-test".to_owned())
            .spawn(move || writer.run(receiver))
            .ok();
        Self {
            sender: handle.as_ref().map(|_| sender),
            dropped,
            dropped_fields,
            writer: Mutex::new(handle),
        }
    }
}

impl Sink for JsonlSink {
    fn note_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn note_dropped_field(&self) {
        self.dropped_fields.fetch_add(1, Ordering::Relaxed);
    }

    fn emit(&self, event: &TraceEvent) {
        let Some(sender) = &self.sender else {
            return;
        };
        let mut event = event.clone();
        redact(&mut event);
        match sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for JsonlSink {
    fn drop(&mut self) {
        // Closing the channel is what stops the writer; it drains every queued
        // event before it returns.
        drop(self.sender.take());
        if let Ok(mut writer) = self.writer.lock()
            && let Some(handle) = writer.take()
        {
            let _ = handle.join();
        }
    }
}

struct Writer {
    dir: PathBuf,
    retention_days: u16,
    dropped: Arc<AtomicU64>,
    dropped_fields: Arc<AtomicU64>,
    warned: bool,
    #[cfg(test)]
    delay: Option<Duration>,
}

impl Writer {
    fn run(mut self, receiver: Receiver<TraceEvent>) {
        let mut file: Option<(String, BufWriter<File>)> = None;
        while let Ok(mut event) = receiver.recv() {
            #[cfg(test)]
            if let Some(delay) = self.delay {
                thread::sleep(delay);
            }
            let dropped = self.dropped.swap(0, Ordering::Relaxed);
            let dropped_fields = self.dropped_fields.swap(0, Ordering::Relaxed);
            if dropped > 0 {
                event
                    .fields
                    .insert(FieldKey::Dropped, Scalar::Int(saturating_i64(dropped)));
            }
            if dropped_fields > 0 {
                event.fields.insert(
                    FieldKey::DroppedFields,
                    Scalar::Int(saturating_i64(dropped_fields)),
                );
            }
            if let Err(error) = self.write_event(&mut file, &event)
                && !self.warned
            {
                self.warned = true;
                eprintln!("printkit: trace log write failed: {error}");
            }
        }
        if let Some((_, mut buffered)) = file {
            let _ = buffered.flush();
        }
    }

    fn write_event(
        &self,
        file: &mut Option<(String, BufWriter<File>)>,
        event: &TraceEvent,
    ) -> io::Result<()> {
        let date = date_string(event.at);
        let stale = file
            .as_ref()
            .map(|(current, _)| current != &date)
            .unwrap_or(true);
        if stale {
            if let Some((_, mut buffered)) = file.take() {
                buffered.flush()?;
            }
            fs::create_dir_all(&self.dir)?;
            let today = OffsetDateTime::now_utc().date();
            // A new day starts: archive whatever was finished.
            let _ = rotate_dir(&self.dir, self.retention_days, today);
            let name = format!("printkit-{date}.jsonl");
            let path = self.dir.join(&name);
            let handle = OpenOptions::new().create(true).append(true).open(&path)?;
            #[cfg(unix)]
            restrict_path(&path, 0o600);
            *file = Some((date, BufWriter::new(handle)));
        }
        let line = serde_json::to_string(event).map_err(io::Error::other)?;
        let Some((_, buffered)) = file.as_mut() else {
            return Err(io::Error::other("trace log handle missing"));
        };
        buffered.write_all(line.as_bytes())?;
        buffered.write_all(b"\n")?;
        buffered.flush()
    }
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Redact values that can carry a document body or a credential.
///
/// The whitelist is structural: `fields` is keyed by [`FieldKey`], so an
/// unknown key cannot reach the writer. The key-name rule catches a whitelisted
/// key whose name itself names a credential; the data-URI rule is the only
/// reliable way to spot a base64 body by value.
fn redact(event: &mut TraceEvent) -> bool {
    let mut changed = false;
    for (key, value) in &mut event.fields {
        if is_sensitive_name(key.as_str()) {
            *value = Scalar::Str("<redacted>".to_owned());
            changed = true;
            continue;
        }
        if let Scalar::Str(text) = value {
            if is_data_uri(text) {
                let length = text.chars().count();
                *value = Scalar::string(format!("<redacted len={length}>"));
                changed = true;
            } else if let Some(redacted) = redact_urls(text) {
                *value = Scalar::string(redacted);
                changed = true;
            }
        }
    }
    if let Some(detail) = &event.detail {
        if is_data_uri(detail) {
            let length = detail.chars().count();
            event.detail = Some(format!("<redacted len={length}>"));
            changed = true;
        } else if let Some(redacted) = redact_urls(detail) {
            event.detail = Some(redacted);
            changed = true;
        }
    }
    changed
}

fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "token",
        "authorization",
        "secret",
        "password",
        "key",
        "credential",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Drop the query and fragment from any URL embedded in free-form text.
fn redact_urls(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    let mut changed = false;
    loop {
        let start = rest
            .find("http://")
            .into_iter()
            .chain(rest.find("https://"))
            .min();
        let Some(start) = start else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '<' | '>' | ']'))
            .map(|offset| start + offset)
            .unwrap_or(rest.len());
        let url = &rest[start..end];
        if let Some(query) = url.find(['?', '#']) {
            out.push_str(&url[..query]);
            changed = true;
        } else {
            out.push_str(url);
        }
        rest = &rest[end..];
    }
    changed.then_some(out)
}

fn is_data_uri(value: &str) -> bool {
    value.starts_with("data:") && value[5..].contains(";base64,")
}

fn date_string(at: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        at.year(),
        u8::from(at.month()),
        at.day()
    )
}

/// Archive finished days and delete expired files.
///
/// Runs at sink creation and whenever the writer crosses midnight. A file that
/// cannot be compressed stays as plain JSONL and is retried on the next day;
/// losing a log line is worse than keeping a larger file.
fn rotate_dir(dir: &Path, retention_days: u16, today: Date) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let today_text = date_text(today);
    let cutoff = retention_cutoff(today, retention_days);
    let mut day_files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stamp) = name.strip_prefix("printkit-").and_then(|rest| {
            rest.strip_suffix(".jsonl.gz")
                .or_else(|| rest.strip_suffix(".jsonl"))
        }) else {
            continue;
        };
        let Some(date) = parse_date(stamp) else {
            continue;
        };
        let path = entry.path();
        if !name.ends_with(".jsonl.gz") && stamp < today_text.as_str() {
            day_files.push((date, path));
        }
    }

    day_files.sort_by_key(|(date, _)| *date);
    for (date, path) in day_files {
        #[cfg(feature = "gzip")]
        {
            let _ = date;
            gzip_file(&path)?;
        }
        #[cfg(not(feature = "gzip"))]
        {
            // Without the gzip feature finished days stay as plain JSONL, but
            // the retention limit still applies.
            if date < cutoff {
                fs::remove_file(&path)?;
            }
        }
    }
    // Re-scan: archiving may have just created files that are already older
    // than the retention window.
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stamp) = name
            .strip_prefix("printkit-")
            .and_then(|rest| rest.strip_suffix(".jsonl.gz"))
        else {
            continue;
        };
        if parse_date(stamp).is_some_and(|date| date < cutoff) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_path(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(feature = "gzip")]
fn gzip_file(path: &Path) -> io::Result<()> {
    let bytes = fs::read(path)?;

    let target = PathBuf::from(format!("{}.gz", path.display()));
    let temporary = PathBuf::from(format!("{}.tmp", target.display()));
    {
        let file = File::create(&temporary)?;
        #[cfg(unix)]
        restrict_path(&temporary, 0o600);
        let mut encoder = GzEncoder::new(file, Compression::default());
        encoder.write_all(&bytes)?;
        encoder.finish()?;
    }
    fs::rename(&temporary, &target)?;
    fs::remove_file(path)
}

fn retention_cutoff(today: Date, retention_days: u16) -> Date {
    let keep = i64::from(retention_days.max(1)) - 1;
    today - TimeDuration::days(keep)
}

fn date_text(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn parse_date(stamp: &str) -> Option<Date> {
    let mut parts = stamp.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

/// Highest `seq` in every retained file, including gzipped archives.
pub fn max_seq_in_dir(dir: &Path) -> Option<u64> {
    crate::read::read_events(dir)
        .ok()?
        .iter()
        .map(|event| event.seq)
        .max()
}

/// Read every event of one file. The caller owns the file list.
pub(crate) fn read_file(path: &Path) -> io::Result<Vec<TraceEvent>> {
    let gzipped = path.extension().is_some_and(|ext| ext == "gz");
    #[cfg(not(feature = "gzip"))]
    if gzipped {
        // A build without the feature cannot decompress archives left by an
        // earlier build; they are skipped rather than reported as corruption.
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let reader: Box<dyn BufRead> = if gzipped {
        #[cfg(feature = "gzip")]
        {
            Box::new(BufReader::new(GzDecoder::new(file)))
        }
        #[cfg(not(feature = "gzip"))]
        {
            Box::new(BufReader::new(file))
        }
    } else {
        Box::new(BufReader::new(file))
    };
    let mut events = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<TraceEvent>(&line) {
            events.push(event);
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::TraceId;

    fn event(seq: u64, at: OffsetDateTime) -> TraceEvent {
        TraceEvent {
            seq,
            trace_id: TraceId::new(),
            stage: crate::event::Stage::Recv,
            at,
            dur_ms: None,
            ok: true,
            code: None,
            detail: None,
            origin: None,
            printer: None,
            pages: None,
            queue_pos: None,
            fields: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn data_uris_are_redacted_in_fields_and_detail() {
        let mut record = event(1, OffsetDateTime::now_utc());
        record.fields.insert(
            FieldKey::DocumentSha256,
            Scalar::Str("data:image/png;base64,AAAA".to_owned()),
        );
        record.detail = Some("data:text/plain;base64,BBBB".to_owned());
        assert!(redact(&mut record));
        assert_eq!(
            record.fields.get(&FieldKey::DocumentSha256),
            Some(&Scalar::Str("<redacted len=26>".to_owned()))
        );
        assert_eq!(record.detail.as_deref(), Some("<redacted len=27>"));
    }

    #[test]
    fn url_queries_and_fragments_are_removed_from_logs() {
        let mut record = event(1, OffsetDateTime::now_utc());
        record.detail =
            Some("cannot fetch https://example.test/file?token=secret#frag: timed out".to_owned());
        assert!(redact(&mut record));
        assert_eq!(
            record.detail.as_deref(),
            Some("cannot fetch https://example.test/file timed out")
        );
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn finished_days_are_gzipped_and_old_ones_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let today = Date::from_calendar_date(2026, time::Month::November, 8).unwrap();
        let yesterday = date_text(today - TimeDuration::days(1));
        let old = date_text(today - TimeDuration::days(40));
        fs::write(
            temp.path().join(format!("printkit-{yesterday}.jsonl")),
            "{}\n",
        )
        .unwrap();
        fs::write(temp.path().join(format!("printkit-{old}.jsonl")), "{}\n").unwrap();
        rotate_dir(temp.path(), 14, today).unwrap();
        assert!(
            !temp
                .path()
                .join(format!("printkit-{yesterday}.jsonl"))
                .exists()
        );
        assert!(
            temp.path()
                .join(format!("printkit-{yesterday}.jsonl.gz"))
                .exists()
        );
        assert!(!temp.path().join(format!("printkit-{old}.jsonl")).exists());
        assert!(
            !temp
                .path()
                .join(format!("printkit-{old}.jsonl.gz"))
                .exists()
        );
    }

    #[test]
    fn an_unknown_field_name_is_dropped_and_counted() {
        let temp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::new(temp.path(), LogLevel::Info);
        let tracer = crate::event::Tracer::new(&sink, crate::id::TraceId::new());
        tracer
            .event(crate::event::Stage::Recv)
            .field_named("document_body", Scalar::string("CONFIDENTIAL-ALPHA-93"))
            .emit();
        drop(sink);
        let events = crate::read_events(temp.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(!events[0].fields.contains_key(&FieldKey::Bytes));
        assert!(matches!(
            events[0].fields.get(&FieldKey::DroppedFields),
            Some(Scalar::Int(count)) if *count == 1
        ));
        let json = serde_json::to_string(&events[0]).unwrap();
        assert!(!json.contains("CONFIDENTIAL-ALPHA-93"), "{json}");
    }

    #[test]
    fn a_full_channel_is_counted_and_reported_on_the_next_event() {
        let temp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::with_test_delay(temp.path(), 1, Duration::from_millis(20));
        for index in 0..8u64 {
            sink.emit(&event(index + 1, OffsetDateTime::now_utc()));
        }
        assert!(sink.dropped() > 0, "the bounded channel never dropped");
        drop(sink);
        let events = crate::read_events(temp.path()).unwrap();
        assert!(!events.is_empty());
        assert!(
            events
                .iter()
                .any(|event| event.fields.contains_key(&FieldKey::Dropped)),
            "no written event carried the dropped counter: {events:?}"
        );
    }

    #[test]
    fn rotation_disabled_still_restores_the_sequence_floor() {
        let temp = tempfile::tempdir().unwrap();
        let yesterday = OffsetDateTime::now_utc().date() - TimeDuration::days(1);
        let name = format!("printkit-{}.jsonl", date_text(yesterday));
        let path = temp.path().join(name);
        let old = event(100_000_000, OffsetDateTime::now_utc());
        let original = format!("{}\n", serde_json::to_string(&old).unwrap());
        fs::write(&path, &original).unwrap();

        let sink = JsonlSink::with_retention_opts(
            temp.path(),
            LogLevel::Info,
            DEFAULT_RETENTION_DAYS,
            RotateOnOpen::No,
        );
        let tracer = crate::event::Tracer::new(&sink, TraceId::new());
        let seq = tracer.event(crate::event::Stage::Recv).emit();
        assert!(
            seq > old.seq,
            "the new sequence {seq} did not continue above {}",
            old.seq
        );
        drop(sink);
    }

    #[test]
    fn rotation_disabled_does_not_touch_files_on_open() {
        let temp = tempfile::tempdir().unwrap();
        let yesterday = OffsetDateTime::now_utc().date() - TimeDuration::days(1);
        let name = format!("printkit-{}.jsonl", date_text(yesterday));
        let path = temp.path().join(name);
        let old = event(100, OffsetDateTime::now_utc());
        let original = format!("{}\n", serde_json::to_string(&old).unwrap());
        fs::write(&path, &original).unwrap();

        // Retention zero would delete the finished file if rotation ran at open.
        let sink = JsonlSink::with_retention_opts(temp.path(), LogLevel::Info, 0, RotateOnOpen::No);
        drop(sink);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "rotation-disabled open changed an existing log file"
        );
    }
}
