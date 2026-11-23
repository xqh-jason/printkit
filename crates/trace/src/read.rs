//! Reading back JSONL files, including gzipped archives.
//!
//! The CLI and the admin page query logs without a running service, and a
//! reconnecting `trace.subscribe` replays from the same files. Events are
//! returned in `seq` order: one process writes increasing numbers, and sorting
//! by `seq` puts a multi-process stream in the order the subscribers expect.

use std::path::Path;
use std::{fs, io};

use crate::event::TraceEvent;
use crate::sink::read_file;

/// Read every retained file under `dir`, oldest file first, then sort by
/// `seq`.
///
/// Malformed lines are skipped: a torn line from a killed process must not
/// make the whole log unreadable. A missing directory is an empty result, not
/// an error - a CLI user who never printed anything should see an empty list.
pub fn read_events(dir: &Path) -> io::Result<Vec<TraceEvent>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("printkit-")
            && (name.ends_with(".jsonl") || name.ends_with(".jsonl.gz"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();

    let mut events = Vec::new();
    for path in paths {
        events.extend(read_file(&path)?);
    }
    events.sort_by_key(|event| event.seq);
    Ok(events)
}

/// Events newer than `since`, in `seq` order. `since` is the last sequence
/// number a subscriber received; the boundary itself is not replayed.
pub fn events_since(dir: &Path, since: u64) -> io::Result<Vec<TraceEvent>> {
    Ok(read_events(dir)?
        .into_iter()
        .filter(|event| event.seq > since)
        .collect())
}

/// Lowest sequence number still on disk. `None` when nothing was ever logged.
pub fn earliest_seq(dir: &Path) -> io::Result<Option<u64>> {
    Ok(read_events(dir)?.first().map(|event| event.seq))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::event::Stage;

    fn event(seq: u64) -> TraceEvent {
        TraceEvent {
            seq,
            trace_id: crate::id::TraceId::new(),
            stage: Stage::Recv,
            at: time::OffsetDateTime::now_utc(),
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

    fn write_plain(path: &Path, events: &[TraceEvent]) {
        let mut text = String::new();
        for event in events {
            text.push_str(&serde_json::to_string(event).unwrap());
            text.push('\n');
        }
        fs::write(path, text).unwrap();
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn archived_and_current_files_are_read_in_sequence_order() {
        let temp = tempfile::tempdir().unwrap();
        write_plain(
            &temp.path().join("printkit-2026-11-07.jsonl"),
            &[event(5), event(6)],
        );
        {
            let file = fs::File::create(temp.path().join("printkit-2026-11-06.jsonl.gz")).unwrap();
            let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            for event in [event(3), event(4)] {
                writeln!(encoder, "{}", serde_json::to_string(&event).unwrap()).unwrap();
            }
            encoder.finish().unwrap();
        }
        let events = read_events(temp.path()).unwrap();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );
        let replayed = events_since(temp.path(), 4).unwrap();
        assert_eq!(
            replayed.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(earliest_seq(temp.path()).unwrap(), Some(3));
    }
}
