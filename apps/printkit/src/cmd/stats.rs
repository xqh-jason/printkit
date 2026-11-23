//! `printkit stats`: aggregate the local usage table without the service.
//!
//! The SQL lives in `printkit-store` and is the same code the WebSocket method
//! and the records page call, so the three numbers are one implementation.

use clap::Args;
use printkit_store::{RecordFilter, StatsBucket, StatsGroup, StatsQuery, Store};
use serde::Serialize;
use time::Date;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Aggregation dimension.
    #[arg(long, value_name = "GROUP", default_value = "tag")]
    pub group: String,

    /// Inclusive lower bound, `YYYY-MM-DD` or RFC 3339.
    #[arg(long, value_name = "DATE")]
    pub from: Option<String>,

    /// Exclusive upper bound; a date includes the whole day.
    #[arg(long, value_name = "DATE")]
    pub to: Option<String>,

    /// Output `{buckets: [{key, count, sheets, avgDurMs}]}`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    key: String,
    count: i64,
    sheets: i64,
    avg_dur_ms: f64,
}

pub fn run(args: StatsArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    if !super::plugin_active(&config, "records") {
        return Err(Failure::environment(
            "records is disabled; run `printkit plugin enable records` first",
        ));
    }
    let group = args
        .group
        .parse::<StatsGroup>()
        .map_err(|_| Failure::usage(format!("unknown group: {}", args.group)))?;
    let from = args
        .from
        .as_deref()
        .map(|value| parse_bound(value, false))
        .transpose()?;
    let to = args
        .to
        .as_deref()
        .map(|value| parse_bound(value, true))
        .transpose()?;
    let db = config.data_dir.value.join(printkit_store::DB_FILE);
    let buckets = match Store::open_existing(&db) {
        Ok(Some(store)) => store
            .stats(&StatsQuery {
                group,
                filter: RecordFilter {
                    from,
                    to,
                    ..RecordFilter::default()
                },
            })
            .map_err(|error| Failure::runtime("E_RECORDS", error.to_string(), 3))?,
        Ok(None) => Vec::new(),
        Err(error) => {
            return Err(Failure::runtime("E_RECORDS", error.to_string(), 3));
        }
    };
    let buckets: Vec<Bucket> = buckets.into_iter().map(bucket).collect();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "buckets": buckets }))
                .map_err(|error| Failure::io(format!("cannot encode stats: {error}")))?
        );
        return Ok(());
    }
    if buckets.is_empty() {
        println!("no records");
        return Ok(());
    }
    println!(
        "{:<24} {:>8} {:>8} {:>12}",
        "key", "count", "sheets", "avg ms"
    );
    for bucket in &buckets {
        println!(
            "{:<24} {:>8} {:>8} {:>12.1}",
            bucket.key, bucket.count, bucket.sheets, bucket.avg_dur_ms
        );
    }
    Ok(())
}

fn bucket(value: StatsBucket) -> Bucket {
    Bucket {
        key: value.key,
        count: value.count,
        sheets: value.sheets,
        avg_dur_ms: value.avg_ms,
    }
}

fn parse_date(raw: &str) -> Option<Date> {
    let mut parts = raw.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || raw.len() != 10 {
        return None;
    }
    Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

/// A date bound becomes midnight; an upper date bound becomes the next
/// midnight, so `--to 2026-11-08` includes that whole day.
fn parse_bound(value: &str, end_of_day: bool) -> Result<String, Failure> {
    if let Some(date) = parse_date(value) {
        let midnight = date.midnight().assume_utc();
        let bound = if end_of_day {
            midnight + time::Duration::days(1)
        } else {
            midnight
        };
        return bound
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| Failure::usage(format!("invalid date {value}: {error}")));
    }
    if time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).is_ok() {
        return Ok(value.to_owned());
    }
    Err(Failure::usage(format!(
        "expected YYYY-MM-DD or an RFC 3339 timestamp: {value}"
    )))
}
