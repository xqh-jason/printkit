//! `printkit logs`: read the local JSONL trace, with or without a service.

use std::str::FromStr;

use clap::Args;
use printkit_facade as facade;
use time::{Duration, OffsetDateTime};

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Only events of one trace id.
    #[arg(long, value_name = "ID")]
    pub trace: Option<String>,

    /// Only one stage: recv, auth, validate, queue, render, submit or result.
    #[arg(long, value_name = "STAGE")]
    pub stage: Option<String>,

    /// Keep only events carrying this field, for example html_ms.
    #[arg(long, value_name = "FIELD")]
    pub field: Option<String>,

    /// Only events newer than a window (1h, 30m, 7d) or a date (2026-11-01).
    #[arg(long, value_name = "WINDOW")]
    pub since: Option<String>,

    /// Poll for new events and print them as they arrive.
    #[arg(long)]
    pub follow: bool,

    /// Output a JSON array of events; with --follow, one object per line.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: LogsArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    if !super::plugin_active(&config, "observability") {
        return Err(Failure::environment(
            "observability is disabled; run `printkit plugin enable observability` first",
        ));
    }
    let stage = match args.stage.as_deref() {
        Some(raw) => Some(
            facade::Stage::from_str(raw)
                .map_err(|_| Failure::usage(format!("unknown stage: {raw}")))?,
        ),
        None => None,
    };
    let field = match args.field.as_deref() {
        Some(raw) => Some(
            raw.parse::<facade::FieldKey>()
                .map_err(|_| Failure::usage(format!("unknown field: {raw}")))?,
        ),
        None => None,
    };
    let cutoff = match args.since.as_deref() {
        Some(raw) => Some(parse_since(raw)?),
        None => None,
    };
    let filter = Filter {
        trace: args.trace.clone(),
        stage,
        field,
        cutoff,
    };
    let dir = facade::logs_dir(&config.data_dir.value);
    let events = facade::read_events(&dir)
        .map_err(|error| Failure::io(format!("cannot read trace log: {error}")))?;
    let mut filtered: Vec<_> = events
        .into_iter()
        .filter(|event| filter.matches(event))
        .collect();
    filtered.sort_by_key(|event| event.seq);

    if !args.follow {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&filtered)
                    .map_err(|error| Failure::io(format!("cannot encode events: {error}")))?
            );
            return Ok(());
        }
        if filtered.is_empty() {
            println!("no events");
            return Ok(());
        }
        for event in &filtered {
            print_text(event);
        }
        return Ok(());
    }

    for event in &filtered {
        print_event(event, args.json)?;
    }
    let mut last = filtered.last().map(|event| event.seq).unwrap_or(0);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let events = facade::events_since(&dir, last)
            .map_err(|error| Failure::io(format!("cannot read trace log: {error}")))?;
        for event in events {
            last = last.max(event.seq);
            if filter.matches(&event) {
                print_event(&event, args.json)?;
            }
        }
    }
}

struct Filter {
    trace: Option<String>,
    stage: Option<facade::Stage>,
    field: Option<facade::FieldKey>,
    cutoff: Option<OffsetDateTime>,
}

impl Filter {
    fn matches(&self, event: &facade::TraceEvent) -> bool {
        self.trace
            .as_deref()
            .is_none_or(|trace| event.trace_id.as_str() == trace)
            && self.stage.is_none_or(|stage| event.stage == stage)
            && self
                .field
                .is_none_or(|field| event.fields.contains_key(&field))
            && self.cutoff.is_none_or(|cutoff| event.at >= cutoff)
    }
}

fn print_event(event: &facade::TraceEvent, json: bool) -> Result<(), Failure> {
    if json {
        println!(
            "{}",
            serde_json::to_string(event)
                .map_err(|error| Failure::io(format!("cannot encode event: {error}")))?
        );
    } else {
        print_text(event);
    }
    Ok(())
}

fn print_text(event: &facade::TraceEvent) {
    let duration = event
        .dur_ms
        .map(|milliseconds| format!("{milliseconds}ms"))
        .unwrap_or_else(|| "-".to_owned());
    let code = event.code.as_deref().unwrap_or("-");
    let detail = event.detail.as_deref().unwrap_or("");
    println!(
        "{} {:>8} seq={:<5} ok={:<5} dur={:<8} code={} {}",
        event.at,
        event.stage.as_str(),
        event.seq,
        event.ok,
        duration,
        code,
        detail
    );
}

/// `YYYY-MM-DD`, without pulling in the `time` macro feature.
fn parse_date(raw: &str) -> Option<time::Date> {
    let mut parts = raw.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || raw.len() != 10 {
        return None;
    }
    time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

/// `30s`, `10m`, `1h`, `7d`, an RFC 3339 timestamp, or `YYYY-MM-DD`.
fn parse_since(raw: &str) -> Result<OffsetDateTime, Failure> {
    if let Some(date) = parse_date(raw) {
        return Ok(date.midnight().assume_utc());
    }
    if let Ok(value) = OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339) {
        return Ok(value);
    }
    Ok(OffsetDateTime::now_utc() - parse_window(raw)?)
}

/// `30s`, `10m`, `1h`, `7d`.
fn parse_window(raw: &str) -> Result<Duration, Failure> {
    let digits = raw.chars().take_while(char::is_ascii_digit).count();
    let (amount, unit) = raw.split_at(digits);
    let amount: i64 = amount
        .parse()
        .map_err(|_| Failure::usage(format!("invalid --since window: {raw}")))?;
    let seconds = match unit {
        "s" => amount,
        "m" => amount.saturating_mul(60),
        "h" => amount.saturating_mul(60 * 60),
        "d" => amount.saturating_mul(24 * 60 * 60),
        _ => return Err(Failure::usage(format!("invalid --since window: {raw}"))),
    };
    if seconds <= 0 {
        return Err(Failure::usage(format!("invalid --since window: {raw}")));
    }
    Ok(Duration::seconds(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_parse_to_seconds() {
        assert_eq!(parse_window("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_window("10m").unwrap(), Duration::seconds(600));
        assert_eq!(parse_window("1h").unwrap(), Duration::seconds(3600));
        assert_eq!(parse_window("7d").unwrap(), Duration::seconds(604_800));
    }

    #[test]
    fn malformed_windows_are_usage_errors() {
        for raw in ["", "h", "1w", "-1h", "1.5h"] {
            assert!(parse_window(raw).is_err(), "{raw} should be rejected");
        }
    }
}
