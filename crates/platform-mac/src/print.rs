//! Print submission and printer enumeration through CUPS and PrintCore.
//!
//! `cupsPrintFile` is the only path that returns a spooler job number, which
//! is what makes `job.cancel` of a submitted job real instead of best effort.
//! It takes a file path rather than bytes, so the PDF is written to a temporary
//! file that is removed on every exit path.
//!
//! Enumeration goes through `PMServerCreatePrinterList`, because CUPS dests do
//! not expose the driver's paper list. Driver options that CUPS does expose
//! (duplex, raw, media source) come from `cupsGetDests` and are merged in.
//!
//! Every call here blocks in FFI and talks to `cupsd`; callers run it on a
//! render worker or a blocking task, never on the WebKit main thread.

use std::ffi::{CStr, CString, c_char, c_double, c_int, c_void};
use std::io::Write as _;
use std::ptr;

use objc2_core_foundation::{CFArray, CFRetained, CFString};
use printkit_platform::{
    PaperInfo, PlatformError, PrintJob, PrinterBackend, PrinterInfo, RawJob, SubmittedJob,
};

type PmServer = *mut c_void;
type PmPrinter = *mut c_void;
type PmPaper = *mut c_void;
type OsStatus = i32;

/// `cups_option_t`.
#[repr(C)]
struct CupsOption {
    name: *mut c_char,
    value: *mut c_char,
}

/// `cups_dest_t`.
#[repr(C)]
struct CupsDest {
    name: *mut c_char,
    instance: *mut c_char,
    is_default: c_int,
    num_options: c_int,
    options: *mut CupsOption,
}

/// `cups_job_t`. `time_t` is 64-bit on every supported Mac.
#[repr(C)]
struct CupsJob {
    id: c_int,
    dest: *mut c_char,
    title: *mut c_char,
    user: *mut c_char,
    format: *mut c_char,
    state: c_int,
    size: c_int,
    priority: c_int,
    completed_time: i64,
    creation_time: i64,
    processing_time: i64,
}

#[link(name = "cups")]
unsafe extern "C" {
    fn cupsPrintFile(
        name: *const c_char,
        filename: *const c_char,
        title: *const c_char,
        num_options: c_int,
        options: *mut CupsOption,
    ) -> c_int;
    fn cupsCancelJob(name: *const c_char, job_id: c_int) -> c_int;
    fn cupsGetJobs(
        jobs: *mut *mut CupsJob,
        name: *const c_char,
        myjobs: c_int,
        whichjobs: c_int,
    ) -> c_int;
    fn cupsFreeJobs(num_jobs: c_int, jobs: *mut CupsJob);
    fn cupsLastErrorString() -> *const c_char;
    fn cupsLastError() -> c_int;
    fn cupsGetDests(dests: *mut *mut CupsDest) -> c_int;
    fn cupsFreeDests(num_dests: c_int, dests: *mut CupsDest);
    fn cupsGetDefault() -> *const c_char;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn PMServerCreatePrinterList(server: PmServer, list: *mut *const CFArray) -> OsStatus;
    fn PMPrinterGetName(printer: PmPrinter) -> *const CFString;
    fn PMPrinterGetMimeTypes(
        printer: PmPrinter,
        settings: *mut c_void,
        types: *mut *const CFArray,
    ) -> OsStatus;
    fn PMPrinterGetPaperList(printer: PmPrinter, list: *mut *const CFArray) -> OsStatus;
    fn PMPaperGetID(paper: PmPaper, id: *mut *const CFString) -> OsStatus;
    fn PMPaperGetWidth(paper: PmPaper, width: *mut c_double) -> OsStatus;
    fn PMPaperGetHeight(paper: PmPaper, height: *mut c_double) -> OsStatus;
}

/// The CUPS whichjobs selector for jobs still in the queue.
const CUPS_WHICHJOBS_ACTIVE: c_int = 0;

// IPP status values from `cups/ipp.h`. Only the ones that change what a failed
// cancel means are listed; everything else keeps the spooler's own text.
/// `client-error-not-possible`: the job is already finished.
const IPP_STATUS_ERROR_NOT_POSSIBLE: c_int = 0x0404;
/// `client-error-not-found`: no such job or queue.
const IPP_STATUS_ERROR_NOT_FOUND: c_int = 0x0406;
/// `client-error-gone`: the job was removed.
const IPP_STATUS_ERROR_GONE: c_int = 0x0407;
/// `server-error-operation-not-supported`: the spooler cannot cancel at all.
const IPP_STATUS_ERROR_OPERATION_NOT_SUPPORTED: c_int = 0x0501;
/// `server-error-service-unavailable`: cupsd itself is unreachable.
const IPP_STATUS_ERROR_SERVICE_UNAVAILABLE: c_int = 0x0502;

/// One job currently in the spooler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolerJob {
    /// CUPS job number, the value `cancel_submitted` accepts.
    pub id: u32,
    /// Destination queue the job is filed under.
    pub printer: String,
    /// Job title; a PDF submission shows the `PrintJob::title` it arrived with.
    pub title: String,
}

/// macOS print backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct MacPrinterBackend;

impl MacPrinterBackend {
    /// A backend with no configuration; every call takes its printer from the
    /// job it receives.
    pub fn new() -> Self {
        Self
    }

    /// Jobs in the spooler queue right now, for diagnostics and acceptance
    /// scripts that assert how many jobs one call created.
    pub fn spooler_jobs(&self, printer: &str) -> Result<Vec<SpoolerJob>, PlatformError> {
        let name = CString::new(printer)
            .map_err(|_| PlatformError::PrinterNotFound(printer.to_owned()))?;
        let mut jobs: *mut CupsJob = ptr::null_mut();
        // SAFETY: the name outlives the call and cupsd owns the returned array
        // until `cupsFreeJobs`.
        let count = unsafe { cupsGetJobs(&mut jobs, name.as_ptr(), 1, CUPS_WHICHJOBS_ACTIVE) };
        if count < 0 {
            return Err(spooler_error("cannot list spooler jobs"));
        }
        let mut listed = Vec::new();
        if !jobs.is_null() {
            // SAFETY: cupsd returned `count` initialised entries.
            for index in 0..count as usize {
                let job = unsafe { &*jobs.add(index) };
                listed.push(SpoolerJob {
                    id: u32::try_from(job.id).unwrap_or_default(),
                    printer: c_string(job.dest),
                    title: c_string(job.title),
                });
            }
            // SAFETY: the same array returned by `cupsGetJobs`.
            unsafe { cupsFreeJobs(count, jobs) };
        }
        Ok(listed)
    }
}

impl PrinterBackend for MacPrinterBackend {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PlatformError> {
        let dests = dests()?;
        let core = printer_entries();
        let mut listed: Vec<PrinterInfo> = dests
            .iter()
            .map(|dest| {
                let mime_types = core
                    .iter()
                    .find(|(name, _)| same_printer(&dest.name, name))
                    .map(|(_, types)| types.clone())
                    .unwrap_or_default();
                dest_to_info(dest, &mime_types)
            })
            .collect();
        // Do not fall back to PrintCore-only names: cupsPrintFile cannot
        // submit to those display names, so listing them would look usable
        // while every print would fail with PrinterNotFound.
        listed.sort_by(|left, right| {
            right
                .is_default
                .cmp(&left.is_default)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(listed)
    }

    fn papers(&self, printer: &str) -> Result<Vec<PaperInfo>, PlatformError> {
        if let Some((array, printers)) = printer_list() {
            let mut found = None;
            for handle in printers {
                // SAFETY: the handle belongs to the live array; the name is
                // borrowed for the same lifetime.
                let name = unsafe {
                    let name = PMPrinterGetName(handle);
                    if name.is_null() {
                        continue;
                    }
                    cf_string(name)
                };
                if !same_printer(printer, &name) {
                    continue;
                }
                found = Some(paper_list(handle));
                break;
            }
            drop(array);
            if let Some(papers) = found
                && !papers.is_empty()
            {
                return Ok(papers);
            }
        }
        // PrintCore reported no papers, or the printer is not visible to it.
        // CUPS still knows the media keywords the driver advertises.
        Ok(dest_media(printer))
    }

    fn submit(&self, job: &PrintJob) -> Result<SubmittedJob, PlatformError> {
        let mut values: Vec<(&str, String)> =
            vec![("document-format", "application/pdf".to_owned())];
        values.push((
            "sides",
            if job.duplex {
                "two-sided-long-edge".to_owned()
            } else {
                "one-sided".to_owned()
            },
        ));
        if job.copies > 1 {
            values.push(("copies", job.copies.to_string()));
            values.push((
                "collate",
                if job.collate { "true" } else { "false" }.to_owned(),
            ));
        }
        if let Some((width_mm, height_mm)) = job.paper_mm
            && let Some(media) = media_keyword(width_mm, height_mm)
        {
            values.push(("media", media));
        }
        submit_cups(job.printer.clone(), &job.pdf, "pdf", &values, &job.title)
    }

    fn prints_pdf(&self) -> bool {
        true
    }

    fn submit_raw(&self, job: &RawJob) -> Result<SubmittedJob, PlatformError> {
        let printer = resolve_printer(job.printer.clone())?;
        let dests = dests()?;
        let Some(dest) = dests.iter().find(|dest| dest.name == printer) else {
            // Not a CUPS queue: no spooler job id exists, so refusing is the
            // only honest answer. Printing the bytes as a document would make
            // the label language come out as garbage.
            return Err(PlatformError::RawUnsupported(printer));
        };
        let mime_types = printer_entries()
            .into_iter()
            .find(|(name, _)| same_printer(&printer, name))
            .map(|(_, types)| types)
            .unwrap_or_default();
        if !dest_supports_raw(dest, &mime_types) {
            return Err(PlatformError::RawUnsupported(printer));
        }
        submit_cups(
            Some(printer),
            &job.bytes,
            "raw",
            &[("document-format", "application/vnd.cups-raw".to_owned())],
            &job.title,
        )
    }

    fn cancel_submitted(&self, job: &SubmittedJob) -> Result<(), PlatformError> {
        let Some(job_id) = job.job_id else {
            return Err(PlatformError::CancelUnsupported);
        };
        let name = CString::new(job.printer.as_str())
            .map_err(|_| PlatformError::PrinterNotFound(job.printer.clone()))?;
        // SAFETY: the name outlives the call.
        let cancelled =
            unsafe { cupsCancelJob(name.as_ptr(), c_int::try_from(job_id).unwrap_or(c_int::MAX)) };
        if cancelled == 0 {
            return Err(cancel_failure(
                job,
                job_id,
                unsafe { cupsLastError() },
                &last_error_string(),
            ));
        }
        Ok(())
    }

    fn cancel_submitted_supported(&self) -> bool {
        true
    }

    fn cancel_raw_supported(&self) -> bool {
        // Raw jobs use the same cupsCancelJob path and always carry an id.
        true
    }
}

/// Copy the spooler's last error text; empty when libcups recorded none.
fn last_error_string() -> String {
    // SAFETY: the error string is owned by libcups and copied immediately.
    unsafe {
        let pointer = cupsLastErrorString();
        if pointer.is_null() {
            String::new()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    }
}

fn spooler_error(context: &str) -> PlatformError {
    let detail = last_error_string();
    if detail.is_empty() {
        PlatformError::Print(context.to_owned())
    } else {
        PlatformError::Print(format!("{context}: {detail}"))
    }
}

/// What a failed spooler call means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpoolerFailure {
    /// The job or queue is no longer there; cancelling cannot succeed but the
    /// platform still supports cancellation.
    JobGone,
    /// cupsd could not be reached: a later retry may work.
    SpoolerDown,
    /// The spooler refuses the operation itself.
    Unsupported,
    /// Any other refusal (permissions, busy spooler, internal error); the
    /// spooler's own text carries the detail.
    Refused,
}

/// Classify a failure from the IPP status and the spooler's text.
///
/// The numeric status is authoritative while it is set; libcups leaves a stale
/// status behind for local transport failures, so the text is matched too.
/// Unknown text stays [`SpoolerFailure::Refused`] rather than guessing that the
/// platform cannot cancel.
fn classify_spooler_failure(status: c_int, detail: &str) -> SpoolerFailure {
    match status {
        IPP_STATUS_ERROR_NOT_POSSIBLE | IPP_STATUS_ERROR_NOT_FOUND | IPP_STATUS_ERROR_GONE => {
            return SpoolerFailure::JobGone;
        }
        IPP_STATUS_ERROR_OPERATION_NOT_SUPPORTED => return SpoolerFailure::Unsupported,
        IPP_STATUS_ERROR_SERVICE_UNAVAILABLE => return SpoolerFailure::SpoolerDown,
        _ => {}
    }
    let text = detail.to_ascii_lowercase();
    const DOWN: [&str; 5] = ["connect", "refused", "unavailable", "no route", "timed out"];
    const GONE: [&str; 6] = [
        "not found",
        "does not exist",
        "no longer exists",
        "already completed",
        "already finished",
        "not possible",
    ];
    const UNSUPPORTED: [&str; 2] = ["not supported", "operation-not-supported"];
    if DOWN.iter().any(|needle| text.contains(needle)) {
        SpoolerFailure::SpoolerDown
    } else if GONE.iter().any(|needle| text.contains(needle)) {
        SpoolerFailure::JobGone
    } else if UNSUPPORTED.iter().any(|needle| text.contains(needle)) {
        SpoolerFailure::Unsupported
    } else {
        SpoolerFailure::Refused
    }
}

/// The error a failed `cupsCancelJob` reports.
fn cancel_failure(job: &SubmittedJob, job_id: u32, status: c_int, detail: &str) -> PlatformError {
    let detail = if detail.is_empty() {
        "the spooler gave no detail"
    } else {
        detail
    };
    match classify_spooler_failure(status, detail) {
        SpoolerFailure::JobGone => PlatformError::JobNotFound {
            printer: job.printer.clone(),
            job: job_id,
            detail: detail.to_owned(),
        },
        SpoolerFailure::SpoolerDown => PlatformError::Print(format!(
            "cannot reach the spooler to cancel job {job_id} on {}: {detail}",
            job.printer
        )),
        SpoolerFailure::Unsupported => PlatformError::CancelUnsupported,
        SpoolerFailure::Refused => PlatformError::Print(format!(
            "the spooler refused to cancel job {job_id} on {}: {detail}",
            job.printer
        )),
    }
}

fn c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: CUPS strings are NUL-terminated and valid while the dest or job
    // array that owns them is alive.
    unsafe { CStr::from_ptr(pointer).to_string_lossy().into_owned() }
}

fn push_option(values: &mut Vec<CString>, name: &str, value: &str) {
    // Option names and values are generated here, so a NUL cannot occur; a
    // malformed pair would otherwise shift every following option.
    if let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) {
        values.push(name);
        values.push(value);
    }
}

fn resolve_printer(printer: Option<String>) -> Result<String, PlatformError> {
    match printer {
        Some(name) => Ok(name),
        None => default_printer(),
    }
}

/// Submit one file through `cupsPrintFile`.
///
/// The spooler reads the document format from the `document-format` option, so
/// PDF jobs and raw device bytes share the path and the same spool-file
/// lifetime: the temporary file is removed when this function returns, on
/// success and on failure.
fn submit_cups(
    printer: Option<String>,
    bytes: &[u8],
    suffix: &str,
    values: &[(&str, String)],
    title: &str,
) -> Result<SubmittedJob, PlatformError> {
    let printer = resolve_printer(printer)?;
    let dests = dests()?;
    if !dests.iter().any(|dest| dest.name == printer) {
        return Err(PlatformError::PrinterNotFound(printer));
    }
    let name = CString::new(printer.as_str())
        .map_err(|_| PlatformError::PrinterNotFound(printer.clone()))?;
    let title = CString::new(title)
        .map_err(|_| PlatformError::Print("the job title contains a NUL byte".to_owned()))?;
    let mut file = tempfile::Builder::new()
        .prefix("printkit-")
        .suffix(&format!(".{suffix}"))
        .tempfile()
        .map_err(|error| PlatformError::Print(format!("cannot create a spool file: {error}")))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|error| PlatformError::Print(format!("cannot write the spool file: {error}")))?;
    let filename = CString::new(file.path().as_os_str().as_encoded_bytes())
        .map_err(|_| PlatformError::Print("the spool path contains a NUL byte".to_owned()))?;

    let mut owned: Vec<CString> = Vec::new();
    for (key, value) in values {
        push_option(&mut owned, key, value);
    }
    let mut options: Vec<CupsOption> = owned
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| CupsOption {
            name: pair[0].as_ptr().cast_mut(),
            value: pair[1].as_ptr().cast_mut(),
        })
        .collect();

    // SAFETY: every string pointer is owned by `owned`, which outlives the
    // call; `options` has exactly one entry per name/value pair.
    let id = unsafe {
        cupsPrintFile(
            name.as_ptr(),
            filename.as_ptr(),
            title.as_ptr(),
            c_int::try_from(options.len()).unwrap_or(0),
            options.as_mut_ptr(),
        )
    };
    if id == 0 {
        return Err(spooler_error("the spooler refused the job"));
    }
    Ok(SubmittedJob {
        job_id: Some(
            u32::try_from(id)
                .map_err(|_| spooler_error("the spooler returned a job id out of range"))?,
        ),
        printer,
    })
}

/// Whether a destination accepts raw device bytes.
///
/// CUPS hands `application/vnd.cups-raw` to the backend unchanged for a queue,
/// so a queue that declares no formats is still a valid raw target. Only a
/// driver that publishes a format list without raw or octet-stream rules raw
/// out; treating silence as refusal would hide usable label printers.
fn dest_supports_raw(dest: &Dest, mime_types: &[String]) -> bool {
    let names_raw = |formats: &str| {
        formats.split([',', ' ']).any(|format| {
            matches!(
                format.trim(),
                "application/vnd.cups-raw" | "application/octet-stream"
            )
        })
    };
    if dest
        .options
        .iter()
        .any(|(key, value)| key == "document-format-supported" && names_raw(value))
    {
        return true;
    }
    if mime_types.is_empty() {
        return true;
    }
    mime_types.iter().any(|mime| {
        matches!(
            mime.as_str(),
            "application/vnd.cups-raw" | "application/octet-stream"
        )
    })
}

#[derive(Debug, Clone)]
struct Dest {
    name: String,
    is_default: bool,
    options: Vec<(String, String)>,
}

fn dests() -> Result<Vec<Dest>, PlatformError> {
    let mut raw: *mut CupsDest = ptr::null_mut();
    // SAFETY: cupsd owns the returned array until `cupsFreeDests`.
    let count = unsafe { cupsGetDests(&mut raw) };
    if dests_query_failed(count, raw.is_null()) {
        // An unreachable spooler is not an empty printer list: reporting it as
        // one turns every submission into `printer not found`.
        return Err(spooler_error("cannot enumerate printers"));
    }
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let mut listed = Vec::new();
    // SAFETY: cupsd returned `count` initialised dests.
    for index in 0..count as usize {
        let dest = unsafe { &*raw.add(index) };
        let mut options = Vec::new();
        for option in 0..dest.num_options.max(0) as usize {
            // SAFETY: `options` has `num_options` entries.
            let option = unsafe { &*dest.options.add(option) };
            options.push((c_string(option.name), c_string(option.value)));
        }
        listed.push(Dest {
            name: c_string(dest.name),
            is_default: dest.is_default != 0,
            options,
        });
    }
    // SAFETY: the same array returned by `cupsGetDests`.
    unsafe { cupsFreeDests(count, raw) };
    Ok(listed)
}

fn dest_to_info(dest: &Dest, mime_types: &[String]) -> PrinterInfo {
    let option = |name: &str| {
        dest.options
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let raw = dest_supports_raw(dest, mime_types);
    PrinterInfo {
        name: dest.name.clone(),
        is_default: dest.is_default,
        mime_types: mime_types.to_vec(),
        duplex: option("sides-supported").is_some_and(|sides| {
            sides
                .split_whitespace()
                .any(|side| side.starts_with("two-sided"))
        }),
        raw,
        paper_source: option("media-source-supported").is_some(),
    }
}

/// The printer list plus the array that owns the pointers.
fn printer_list() -> Option<(CFRetained<CFArray>, Vec<PmPrinter>)> {
    let mut raw: *const CFArray = ptr::null();
    // SAFETY: `raw` receives an owned array or stays null.
    let status = unsafe { PMServerCreatePrinterList(ptr::null_mut(), &mut raw) };
    if status != 0 || raw.is_null() {
        return None;
    }
    // SAFETY: the array came from a Create function, so this takes ownership.
    let array = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(raw as *mut CFArray)?) };
    let mut printers = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        // SAFETY: the index is in bounds and the array is not mutated.
        let printer = unsafe { array.value_at_index(index as isize) };
        printers.push(printer as PmPrinter);
    }
    Some((array, printers))
}

/// PrintCore's display names and MIME types, detached from the owning array.
fn printer_entries() -> Vec<(String, Vec<String>)> {
    let Some((array, printers)) = printer_list() else {
        return Vec::new();
    };
    let mut entries = Vec::with_capacity(printers.len());
    for printer in printers {
        // SAFETY: the pointer belongs to the live array and the name is
        // borrowed for the same lifetime.
        let name = unsafe {
            let name = PMPrinterGetName(printer);
            if name.is_null() {
                continue;
            }
            cf_string(name)
        };
        entries.push((name, mime_types(printer)));
    }
    drop(array);
    entries
}

/// `cupsGetDests` returns queue names (`printkit-test`), PrintCore returns
/// display names (`printkit test`). Ignoring case and punctuation maps one to
/// the other without inventing an id.
fn same_printer(queue_name: &str, display_name: &str) -> bool {
    fn normalize(value: &str) -> String {
        value
            .chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
    normalize(queue_name) == normalize(display_name)
}

fn mime_types(printer: PmPrinter) -> Vec<String> {
    let mut raw: *const CFArray = ptr::null();
    // SAFETY: PrintCore fills an array that stays valid while the printer is
    // alive. Its contract forbids releasing that reference, so this borrows it
    // instead of taking ownership.
    let status = unsafe { PMPrinterGetMimeTypes(printer, ptr::null_mut(), &mut raw) };
    if status != 0 || raw.is_null() {
        return Vec::new();
    }
    // SAFETY: see above; the array outlives this call.
    let array = unsafe { &*raw };
    let mut types = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        // SAFETY: the index is in bounds; elements are CFStrings owned by the
        // live array.
        let pointer = unsafe { array.value_at_index(index as isize) };
        if pointer.is_null() {
            continue;
        }
        // SAFETY: the element is a CFString.
        types.push(cf_string(pointer.cast::<CFString>()));
    }
    types
}

fn paper_list(printer: PmPrinter) -> Vec<PaperInfo> {
    let mut raw: *const CFArray = ptr::null();
    // SAFETY: PrintCore returns an owned array or null.
    let status = unsafe { PMPrinterGetPaperList(printer, &mut raw) };
    if status != 0 || raw.is_null() {
        return Vec::new();
    }
    // SAFETY: the paper list belongs to the printer and must outlive this
    // call; it is read while the printer stays alive and is not released here.
    let array = unsafe { &*raw };
    let mut papers = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        // SAFETY: the index is in bounds; elements belong to the live array.
        let paper = unsafe { array.value_at_index(index as isize) };
        if paper.is_null() {
            continue;
        }
        let paper = paper as PmPaper;
        let mut width = 0.0;
        let mut height = 0.0;
        // SAFETY: `width`/`height` are writable doubles.
        let width_status = unsafe { PMPaperGetWidth(paper, &mut width) };
        // SAFETY: same as above.
        let height_status = unsafe { PMPaperGetHeight(paper, &mut height) };
        if width_status != 0 || height_status != 0 || width <= 0.0 || height <= 0.0 {
            continue;
        }
        let width_mm = width * 25.4 / 72.0;
        let height_mm = height * 25.4 / 72.0;
        let mut id: *const CFString = ptr::null();
        // SAFETY: `id` is writable and the paper is live.
        let id_status = unsafe { PMPaperGetID(paper, &mut id) };
        let id = if id_status == 0 && !id.is_null() {
            // SAFETY: PrintCore returned a borrowed CFString.
            cf_string(id)
        } else {
            String::new()
        };
        papers.push(paper_info(id, width_mm, height_mm));
    }
    papers
}

/// Read `media-supported` from the CUPS dest when PrintCore has no paper list.
fn dest_media(printer: &str) -> Vec<PaperInfo> {
    let Ok(dests) = dests() else {
        return Vec::new();
    };
    let Some(dest) = dests.iter().find(|dest| dest.name == printer) else {
        return Vec::new();
    };
    let Some(supported) = dest
        .options
        .iter()
        .find(|(name, _)| name == "media-supported")
        .map(|(_, value)| value)
    else {
        return Vec::new();
    };
    let mut papers = Vec::new();
    for keyword in supported.split_whitespace() {
        if let Some((width_mm, height_mm)) = pwg_dimensions(keyword) {
            papers.push(paper_info(keyword.to_owned(), width_mm, height_mm));
        }
    }
    papers
}

/// Turn a driver paper name and dimensions into the protocol shape.
fn paper_info(id: String, width_mm: f64, height_mm: f64) -> PaperInfo {
    match standard_name(width_mm, height_mm) {
        Some(name) => PaperInfo {
            name: name.to_owned(),
            width_mm,
            height_mm,
            named: true,
        },
        None => PaperInfo {
            name: if id.is_empty() {
                format!("{width_mm:.1}x{height_mm:.1}mm")
            } else {
                id
            },
            width_mm,
            height_mm,
            named: false,
        },
    }
}

/// A4/A3/A5/Letter/Legal when the dimensions match within 1 mm.
fn standard_name(width_mm: f64, height_mm: f64) -> Option<&'static str> {
    let (short, long) = if width_mm <= height_mm {
        (width_mm, height_mm)
    } else {
        (height_mm, width_mm)
    };
    [
        ("A4", 210.0, 297.0),
        ("A3", 297.0, 420.0),
        ("A5", 148.0, 210.0),
        ("Letter", 215.9, 279.4),
        ("Legal", 215.9, 355.6),
    ]
    .into_iter()
    .find(|(_, w, h)| (short - w).abs() < 1.0 && (long - h).abs() < 1.0)
    .map(|(name, _, _)| name)
}

/// CUPS media keyword for a size the driver is likely to know.
fn media_keyword(width_mm: f64, height_mm: f64) -> Option<String> {
    if let Some(name) = standard_name(width_mm, height_mm) {
        let (w, h) = if width_mm <= height_mm {
            (width_mm, height_mm)
        } else {
            (height_mm, width_mm)
        };
        return Some(match name {
            "A4" => "iso_a4_210x297mm".to_owned(),
            "A3" => "iso_a3_297x420mm".to_owned(),
            "A5" => "iso_a5_148x210mm".to_owned(),
            "Letter" => "na_letter_8.5x11in".to_owned(),
            "Legal" => "na_legal_8.5x14in".to_owned(),
            _ => format!("Custom.{w:.0}x{h:.0}mm"),
        });
    }
    (width_mm > 0.0 && height_mm > 0.0).then(|| {
        format!(
            "Custom.{:.0}x{:.0}mm",
            width_mm.max(1.0),
            height_mm.max(1.0)
        )
    })
}

/// Parse the size suffix of a PWG media keyword (`iso_a4_210x297mm`).
fn pwg_dimensions(keyword: &str) -> Option<(f64, f64)> {
    let size = keyword.rsplit('_').next()?;
    let (numbers, unit) = if let Some(numbers) = size.strip_suffix("mm") {
        (numbers, 1.0)
    } else {
        (size.strip_suffix("in")?, 25.4)
    };
    let (width, height) = numbers.split_once('x')?;
    let width: f64 = width.parse().ok()?;
    let height: f64 = height.parse().ok()?;
    (width > 0.0 && height > 0.0).then_some((width * unit, height * unit))
}

/// Whether `cupsGetDests` reported a failure rather than an empty list.
///
/// `0` with no array is a real empty list; a negative count, or a positive
/// count without its array, means the spooler could not be asked.
fn dests_query_failed(count: c_int, raw_is_null: bool) -> bool {
    count < 0 || (count > 0 && raw_is_null)
}

fn default_printer() -> Result<String, PlatformError> {
    // SAFETY: libcups returns an internal NUL-terminated string.
    let pointer = unsafe { cupsGetDefault() };
    if pointer.is_null() {
        let status = unsafe { cupsLastError() };
        let detail = last_error_string();
        if classify_spooler_failure(status, &detail) == SpoolerFailure::SpoolerDown {
            return Err(PlatformError::Print(format!(
                "cannot reach the spooler to read the default printer: {}",
                if detail.is_empty() {
                    "the spooler gave no detail"
                } else {
                    &detail
                }
            )));
        }
        return Err(PlatformError::NoDefaultPrinter);
    }
    Ok(c_string(pointer))
}

fn cf_string(pointer: *const CFString) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: the caller guarantees a live borrowed CFString. Formatting
    // copies the characters and does not retain the pointer.
    format!("{}", unsafe { &*pointer })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> SubmittedJob {
        SubmittedJob {
            job_id: Some(42),
            printer: "printkit-test".to_owned(),
        }
    }

    #[test]
    fn raw_and_graphic_jobs_report_their_cancel_capabilities_separately() {
        let backend = MacPrinterBackend::new();
        assert!(backend.cancel_submitted_supported());
        assert!(backend.cancel_raw_supported());
    }

    #[test]
    fn a_job_without_a_spooler_id_cannot_be_cancelled() {
        let backend = MacPrinterBackend::new();
        let job = SubmittedJob {
            job_id: None,
            printer: "printkit-test".to_owned(),
        };
        assert!(matches!(
            backend.cancel_submitted(&job),
            Err(PlatformError::CancelUnsupported)
        ));
    }

    #[test]
    fn ipp_status_decides_what_a_failed_cancel_means() {
        assert_eq!(
            classify_spooler_failure(IPP_STATUS_ERROR_NOT_FOUND, ""),
            SpoolerFailure::JobGone
        );
        assert_eq!(
            classify_spooler_failure(IPP_STATUS_ERROR_NOT_POSSIBLE, ""),
            SpoolerFailure::JobGone
        );
        assert_eq!(
            classify_spooler_failure(IPP_STATUS_ERROR_GONE, ""),
            SpoolerFailure::JobGone
        );
        assert_eq!(
            classify_spooler_failure(IPP_STATUS_ERROR_SERVICE_UNAVAILABLE, ""),
            SpoolerFailure::SpoolerDown
        );
        assert_eq!(
            classify_spooler_failure(IPP_STATUS_ERROR_OPERATION_NOT_SUPPORTED, ""),
            SpoolerFailure::Unsupported
        );
        // Permissions, busy spooler and internal errors keep their own text.
        assert_eq!(
            classify_spooler_failure(0x0401, "Forbidden"),
            SpoolerFailure::Refused
        );
    }

    #[test]
    fn local_error_text_is_classified_when_the_status_is_stale() {
        // libcups leaves the previous IPP status behind for transport errors,
        // so the text has to carry the meaning here.
        assert_eq!(
            classify_spooler_failure(0, "Unable to connect to server."),
            SpoolerFailure::SpoolerDown
        );
        assert_eq!(
            classify_spooler_failure(0, "Connection refused"),
            SpoolerFailure::SpoolerDown
        );
        assert_eq!(
            classify_spooler_failure(0, "The job does not exist."),
            SpoolerFailure::JobGone
        );
        assert_eq!(
            classify_spooler_failure(0, "The job was already completed."),
            SpoolerFailure::JobGone
        );
        assert_eq!(
            classify_spooler_failure(0, "The operation is not supported."),
            SpoolerFailure::Unsupported
        );
        assert_eq!(
            classify_spooler_failure(0, "Something unexpected"),
            SpoolerFailure::Refused
        );
    }

    #[test]
    fn a_gone_job_is_a_not_found_error_not_an_unsupported_cancel() {
        let error = cancel_failure(&job(), 42, IPP_STATUS_ERROR_NOT_FOUND, "Job 42 not found");
        match error {
            PlatformError::JobNotFound {
                printer,
                job: id,
                detail,
            } => {
                assert_eq!(printer, "printkit-test");
                assert_eq!(id, 42);
                assert!(detail.contains("not found"), "{detail}");
            }
            other => panic!("expected JobNotFound, got {other:?}"),
        }
    }

    #[test]
    fn a_spooler_outage_keeps_the_connection_detail() {
        let error = cancel_failure(&job(), 42, 0, "Unable to connect to server.");
        match error {
            PlatformError::Print(message) => {
                assert!(message.contains("cannot reach the spooler"), "{message}");
                assert!(message.contains("Unable to connect"), "{message}");
            }
            other => panic!("expected a print failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unsupported_operation_stays_cancel_unsupported() {
        assert!(matches!(
            cancel_failure(&job(), 42, IPP_STATUS_ERROR_OPERATION_NOT_SUPPORTED, ""),
            PlatformError::CancelUnsupported
        ));
    }

    #[test]
    fn an_unknown_refusal_names_the_job_instead_of_blaming_the_platform() {
        let error = cancel_failure(&job(), 42, 0x0401, "Forbidden");
        match error {
            PlatformError::Print(message) => {
                assert!(message.contains("refused to cancel"), "{message}");
                assert!(message.contains("Forbidden"), "{message}");
            }
            other => panic!("expected a print failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unreachable_spooler_is_not_an_empty_printer_list() {
        assert!(dests_query_failed(-1, true));
        assert!(dests_query_failed(3, true));
        assert!(!dests_query_failed(0, true));
        assert!(!dests_query_failed(3, false));
    }
}
