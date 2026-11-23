//! Printer enumeration, paper sizes and raw submission through the Windows
//! spooler.
//!
//! `EnumPrintersW`/`GetDefaultPrinterW` list queues, `DeviceCapabilitiesW` reads
//! the driver's paper list, and raw jobs go through
//! `StartDocPrinterW`/`WritePrinter` with `pDatatype = "RAW"`, returning the
//! real spooler job id. A printer whose print processor does not advertise RAW
//! is refused instead of receiving device commands as a rendered document.
//!
//! Graphic submission still needs WebView2 `Print` plus a per-instance binding
//! between the render host and this backend, so `submit` stays an explicit
//! `Unsupported`; there is no PDF-to-spooler route on Windows.

use std::ffi::c_void;

use printkit_platform::{
    HtmlPrintJob, PaperInfo, PlatformError, PrintJob, PrinterBackend, PrinterInfo, RawJob,
    SubmittedJob,
};
use windows::Win32::Foundation::{
    ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_INVALID_PRINTER_NAME,
    ERROR_PRINTER_NOT_FOUND, POINT,
};
use windows::Win32::Graphics::Printing::{
    ClosePrinter, DATATYPES_INFO_1W, DOC_INFO_1W, EndDocPrinter, EndPagePrinter,
    EnumPrintProcessorDatatypesW, EnumPrintersW, GetDefaultPrinterW, GetPrinterW,
    JOB_CONTROL_DELETE, OpenPrinterW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_HANDLE,
    PRINTER_INFO_2W, PRINTER_INFO_4W, SetJobW, StartDocPrinterW, StartPagePrinter, WritePrinter,
};
use windows::Win32::Storage::Xps::{
    DC_PAPERNAMES, DC_PAPERS, DC_PAPERSIZE, DeviceCapabilitiesW, PRINTER_DEVICE_CAPABILITIES,
};
use windows::core::{Error, HRESULT, HSTRING, PCWSTR, PWSTR};

use crate::host::{self, PRINT_TIMEOUT};

/// `DeviceCapabilitiesW` returns paper names as 64-wide UTF-16 fields.
const PAPER_NAME_CHARS: usize = 64;

/// The Windows spooler backend.
///
/// Raw jobs use the spooler API and get a real job id. PDF and HTML jobs go
/// through the WebView2 print path, which produces no spooler id, so graphic
/// cancellation is unsupported there while raw cancellation is not.
#[derive(Debug, Default)]
pub struct WinPrinterBackend {}

impl WinPrinterBackend {
    /// A backend with no configuration; every call resolves printers and job
    /// ids on the fly.
    pub fn new() -> Self {
        Self {}
    }
}

impl PrinterBackend for WinPrinterBackend {
    fn printers(&self) -> Result<Vec<PrinterInfo>, PlatformError> {
        let default = default_printer();
        let mut listed: Vec<PrinterInfo> = printer_names()?
            .into_iter()
            .map(|name| {
                // Probing opens the queue and reads its print processor; a
                // failure is "raw not proven", never a reason to hide the
                // printer from the list.
                let raw = raw_supported(&name);
                PrinterInfo {
                    is_default: default.as_deref() == Some(name.as_str()),
                    name,
                    // The spooler does not publish MIME types; the driver
                    // format is a datatype (`RAW`, `XPS_PASS`), not a MIME type.
                    mime_types: Vec::new(),
                    // Duplex and paper source stay false until a real device
                    // proves them; `PrintSettings2.Duplex` exists but is
                    // unverified.
                    duplex: false,
                    raw,
                    paper_source: false,
                }
            })
            .collect();
        listed.sort_by(|left, right| {
            right
                .is_default
                .cmp(&left.is_default)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(listed)
    }

    fn papers(&self, printer: &str) -> Result<Vec<PaperInfo>, PlatformError> {
        let handle = open_printer(printer)?;
        let details = printer_details(&handle, printer)?;
        let Some(port) = details.port else {
            return Err(PlatformError::Print(format!(
                "cannot read paper sizes for {printer}: the driver reported no port"
            )));
        };
        paper_list(printer, &port)
    }

    fn submit(&self, job: &PrintJob) -> Result<SubmittedJob, PlatformError> {
        // `doc.printPdf`/`doc.printBatch` hand a merged PDF to the WebView2
        // built-in viewer, which prints it through the same driver path as
        // `Print`. No spooler job id exists on this path.
        let printer = match &job.printer {
            Some(name) => name.clone(),
            None => default_printer().ok_or(PlatformError::NoDefaultPrinter)?,
        };
        host::print_pdf(
            job.pdf.clone(),
            host::PrintRequest {
                printer: Some(printer.clone()),
                copies: job.copies,
                collate: job.collate,
                duplex: job.duplex,
            },
            job.paper_mm,
            &job.title,
            PRINT_TIMEOUT,
        )?;
        Ok(SubmittedJob {
            job_id: None,
            printer,
        })
    }

    fn prints_pdf(&self) -> bool {
        true
    }

    fn prints_html(&self) -> bool {
        true
    }

    fn submit_html(&self, job: &HtmlPrintJob) -> Result<SubmittedJob, PlatformError> {
        let printer = match &job.printer {
            Some(name) => name.clone(),
            None => default_printer().ok_or(PlatformError::NoDefaultPrinter)?,
        };
        // WebView2 `Print` hands the page to the driver without a spooler job
        // id. The HTML and spec are copied because the STA thread outlives this
        // borrow; the engine rendered the same pair once already.
        host::print_html(
            host::PrintRequest {
                printer: Some(printer.clone()),
                copies: job.copies,
                collate: job.collate,
                duplex: job.duplex,
            },
            job.html.clone(),
            job.spec.clone(),
            PRINT_TIMEOUT,
        )?;
        Ok(SubmittedJob {
            job_id: None,
            printer,
        })
    }

    fn submit_raw(&self, job: &RawJob) -> Result<SubmittedJob, PlatformError> {
        let printer = match &job.printer {
            Some(name) => name.clone(),
            None => default_printer().ok_or(PlatformError::NoDefaultPrinter)?,
        };
        let handle = open_printer(&printer)?;
        let details = printer_details(&handle, &printer)?;
        if details.print_processor.is_empty() {
            return Err(PlatformError::RawUnsupported(format!(
                "{printer}: the driver reports no print processor, so RAW cannot be verified"
            )));
        }
        let advertised = datatypes(&printer, &details.print_processor)?;
        if !advertised
            .iter()
            .any(|name| name.eq_ignore_ascii_case("RAW"))
        {
            // Device bytes printed as a document come out as control codes.
            return Err(PlatformError::RawUnsupported(format!(
                "{printer}: the print processor does not advertise the RAW datatype"
            )));
        }

        let title = HSTRING::from(job.title.as_str());
        let datatype = HSTRING::from("RAW");
        let info = DOC_INFO_1W {
            pDocName: PWSTR(title.as_ptr() as *mut u16),
            pOutputFile: PWSTR::null(),
            pDatatype: PWSTR(datatype.as_ptr() as *mut u16),
        };
        // SAFETY: the handle is live, both strings outlive the call, and level
        // 1 is the DOC_INFO_1W layout.
        let job_id = unsafe { StartDocPrinterW(handle.0, 1, &info) };
        if job_id == 0 {
            return Err(PlatformError::Print(format!(
                "cannot start a raw job on {printer}: {}",
                Error::from_thread()
            )));
        }
        // `StartDocPrinterW` succeeded, so both ends must be closed even when a
        // later call fails.
        let mut document = RawDocument {
            handle: &handle,
            page_open: false,
            document_open: true,
        };

        // SAFETY: the document is open on this handle.
        if !unsafe { StartPagePrinter(handle.0) }.as_bool() {
            return Err(PlatformError::Print(format!(
                "cannot start a raw page on {printer}: {}",
                Error::from_thread()
            )));
        }
        document.page_open = true;

        let mut offset = 0usize;
        while offset < job.bytes.len() {
            let remaining = &job.bytes[offset..];
            let length = remaining.len().min(u32::MAX as usize);
            let chunk = &remaining[..length];
            let mut written = 0u32;
            // SAFETY: the chunk is readable for `length` bytes and `written` is
            // a writable out-parameter; bytes are sent unchanged.
            let written_ok = unsafe {
                WritePrinter(
                    handle.0,
                    chunk.as_ptr() as *const c_void,
                    length as u32,
                    &mut written,
                )
            };
            if !written_ok.as_bool() {
                return Err(PlatformError::Print(format!(
                    "cannot write raw bytes to {printer}: {}",
                    Error::from_thread()
                )));
            }
            if written == 0 {
                return Err(PlatformError::Print(format!(
                    "the spooler accepted no raw bytes for {printer}"
                )));
            }
            offset += written as usize;
        }

        // SAFETY: a page is open on this handle.
        if !unsafe { EndPagePrinter(handle.0) }.as_bool() {
            return Err(PlatformError::Print(format!(
                "cannot end a raw page on {printer}: {}",
                Error::from_thread()
            )));
        }
        document.page_open = false;
        // SAFETY: the document is open on this handle.
        if !unsafe { EndDocPrinter(handle.0) }.as_bool() {
            return Err(PlatformError::Print(format!(
                "cannot end a raw job on {printer}: {}",
                Error::from_thread()
            )));
        }
        document.document_open = false;

        Ok(SubmittedJob {
            job_id: Some(job_id),
            printer,
        })
    }

    fn cancel_submitted(&self, job: &SubmittedJob) -> Result<(), PlatformError> {
        // A WebView2 `Print` job never had a spooler id; refusing before any
        // spooler call keeps the "no fake id" rule honest.
        let Some(job_id) = job.job_id else {
            return Err(PlatformError::CancelUnsupported);
        };
        let handle = match open_printer(&job.printer) {
            Ok(handle) => handle,
            Err(PlatformError::PrinterNotFound(_)) => {
                return Err(PlatformError::JobNotFound {
                    printer: job.printer.clone(),
                    job: job_id,
                    detail: "the printer no longer exists".to_owned(),
                });
            }
            Err(error) => return Err(error),
        };
        // Microsoft documents JOB_CONTROL_CANCEL as "do not use"; deleting the
        // job is the supported way to retract it.
        // SAFETY: the handle is live and level 0 takes no job data.
        let deleted = unsafe { SetJobW(handle.0, job_id, 0, None, JOB_CONTROL_DELETE) };
        if deleted.as_bool() {
            return Ok(());
        }
        let error = Error::from_thread();
        if is_missing_job(&error) {
            return Err(PlatformError::JobNotFound {
                printer: job.printer.clone(),
                job: job_id,
                detail: error.to_string(),
            });
        }
        Err(PlatformError::Print(format!(
            "cannot cancel job {job_id} on {}: {error}",
            job.printer
        )))
    }

    fn cancel_submitted_supported(&self) -> bool {
        // Graphic/PDF jobs never get a spooler id, but raw jobs do and
        // `cancel_submitted` can retract them; `cancel_raw_supported` reports
        // that separate capability.
        false
    }

    fn cancel_raw_supported(&self) -> bool {
        // Raw jobs always return the real `StartDocPrinterW` id, so
        // `SetJob(JOB_CONTROL_DELETE)` can retract them.
        true
    }
}

/// An open spooler handle; `ClosePrinter` runs on every exit path.
struct PrinterHandle(PRINTER_HANDLE);

impl Drop for PrinterHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by OpenPrinterW and is closed once.
        unsafe {
            let _ = ClosePrinter(self.0);
        }
    }
}

fn open_printer(printer: &str) -> Result<PrinterHandle, PlatformError> {
    let name = HSTRING::from(printer);
    let mut handle = PRINTER_HANDLE::default();
    // SAFETY: `name` outlives the call; `handle` is a valid out-parameter and
    // the default (NULL) `PRINTER_DEFAULTS` only requests use access.
    match unsafe { OpenPrinterW(&name, &mut handle, None) } {
        Ok(()) => Ok(PrinterHandle(handle)),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_INVALID_PRINTER_NAME.0) => {
            Err(PlatformError::PrinterNotFound(printer.to_owned()))
        }
        Err(error) => Err(PlatformError::Print(format!(
            "cannot open printer {printer}: {error}"
        ))),
    }
}

/// Every local and connection queue, by name.
fn printer_names() -> Result<Vec<String>, PlatformError> {
    const FLAGS: u32 = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    const LEVEL: u32 = 4;
    let mut needed = 0u32;
    let mut returned = 0u32;
    // SAFETY: a null buffer with cbBuf 0 asks only for the required size; the
    // call is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe {
        EnumPrintersW(
            FLAGS,
            PCWSTR::null(),
            LEVEL,
            None,
            &mut needed,
            &mut returned,
        )
    };
    if needed == 0 {
        return Ok(Vec::new());
    }
    for _ in 0..3 {
        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: the buffer is `needed` bytes, which is exactly what the
        // spooler reported for this level and flag set.
        let result = unsafe {
            EnumPrintersW(
                FLAGS,
                PCWSTR::null(),
                LEVEL,
                Some(buffer.as_mut_slice()),
                &mut needed,
                &mut returned,
            )
        };
        match result {
            Ok(()) => return Ok(read_printer_names(&buffer, returned)),
            Err(error) if error.code() == HRESULT::from_win32(122) => continue,
            Err(error) => {
                return Err(PlatformError::Print(format!(
                    "cannot enumerate printers: {error}"
                )));
            }
        }
    }
    Err(PlatformError::Print(
        "the printer list kept growing while it was being read".to_owned(),
    ))
}

fn read_printer_names(buffer: &[u8], count: u32) -> Vec<String> {
    let stride = std::mem::size_of::<PRINTER_INFO_4W>();
    let mut names = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let Some(offset) = index.checked_mul(stride) else {
            break;
        };
        if offset + stride > buffer.len() {
            break;
        }
        // SAFETY: the record lies inside the buffer and `read_unaligned` does
        // not require the alignment a byte buffer cannot promise.
        let info =
            unsafe { (buffer.as_ptr().add(offset) as *const PRINTER_INFO_4W).read_unaligned() };
        if info.pPrinterName.is_null() {
            continue;
        }
        // SAFETY: the name points into the same buffer the spooler filled with
        // null-terminated UTF-16 strings.
        if let Ok(name) = unsafe { info.pPrinterName.to_string() } {
            names.push(name);
        }
    }
    names
}

/// The default queue, if the user configured one. No default is not an error.
fn default_printer() -> Option<String> {
    let mut length = 0u32;
    // SAFETY: a null buffer asks for the required character count.
    let _ = unsafe { GetDefaultPrinterW(None, &mut length) };
    if length == 0 {
        return None;
    }
    for _ in 0..2 {
        let mut buffer = vec![0u16; length as usize];
        // SAFETY: the buffer holds `length` UTF-16 units, which is what the
        // previous call asked for; the call null-terminates in place.
        let ok = unsafe { GetDefaultPrinterW(Some(PWSTR(buffer.as_mut_ptr())), &mut length) };
        if ok.as_bool() {
            // SAFETY: a successful call wrote a null-terminated string.
            return unsafe { PWSTR(buffer.as_mut_ptr()).to_string().ok() };
        }
    }
    None
}

/// Port and print processor for one queue. `DeviceCapabilitiesW` needs the
/// port, and `EnumPrintProcessorDatatypesW` needs the processor.
struct PrinterDetails {
    /// `None` when the driver reports no port (some virtual queues do not).
    port: Option<String>,
    print_processor: String,
}

fn printer_details(handle: &PrinterHandle, printer: &str) -> Result<PrinterDetails, PlatformError> {
    const LEVEL: u32 = 2;
    let mut needed = 0u32;
    // SAFETY: a null buffer asks for the required size.
    let _ = unsafe { GetPrinterW(handle.0, LEVEL, None, &mut needed) };
    if needed == 0 {
        return Err(PlatformError::Print(format!(
            "cannot read the port for {printer}: the spooler returned no data"
        )));
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: the buffer is the size the spooler requested for level 2.
    unsafe { GetPrinterW(handle.0, LEVEL, Some(buffer.as_mut_slice()), &mut needed) }.map_err(
        |error| PlatformError::Print(format!("cannot read the port for {printer}: {error}")),
    )?;
    if buffer.len() < std::mem::size_of::<PRINTER_INFO_2W>() {
        return Err(PlatformError::Print(format!(
            "cannot read the port for {printer}: short PRINTER_INFO_2W"
        )));
    }
    // SAFETY: the record lies inside the buffer; unaligned reads keep a byte
    // buffer valid regardless of its alignment.
    let info = unsafe { (buffer.as_ptr() as *const PRINTER_INFO_2W).read_unaligned() };
    // SAFETY: both names point into the same buffer and are null-terminated.
    let port = if info.pPortName.is_null() {
        None
    } else {
        Some(unsafe { info.pPortName.to_string() }.map_err(|error| {
            PlatformError::Print(format!("cannot decode the port for {printer}: {error}"))
        })?)
    };
    let print_processor = if info.pPrintProcessor.is_null() {
        String::new()
    } else {
        // SAFETY: same buffer and lifetime as the port.
        unsafe { info.pPrintProcessor.to_string() }.unwrap_or_default()
    };
    Ok(PrinterDetails {
        port,
        print_processor,
    })
}

fn device_capability(
    printer: &str,
    port: &str,
    capability: PRINTER_DEVICE_CAPABILITIES,
    output: Option<PWSTR>,
) -> i32 {
    let device = HSTRING::from(printer);
    let port = HSTRING::from(port);
    // SAFETY: both strings outlive the call; `output` is either null or a
    // caller buffer large enough for `capability` (see callers). A null
    // DEVMODE asks for the driver's defaults, which is what listing needs.
    unsafe { DeviceCapabilitiesW(&device, &port, capability, output, None) }
}

fn paper_list(printer: &str, port: &str) -> Result<Vec<PaperInfo>, PlatformError> {
    let count = device_capability(printer, port, DC_PAPERS, None);
    if count <= 0 {
        return Ok(Vec::new());
    }
    let count = count as usize;

    let mut ids = vec![0i16; count];
    // SAFETY: `ids` holds exactly `count` WORDs, the number the driver
    // reported for DC_PAPERS on this port.
    let id_count = device_capability(
        printer,
        port,
        DC_PAPERS,
        Some(PWSTR(ids.as_mut_ptr() as *mut u16)),
    );

    let mut sizes = vec![POINT::default(); count];
    // SAFETY: `sizes` holds `count` POINTs, the layout DC_PAPERSIZE writes.
    let size_count = device_capability(
        printer,
        port,
        DC_PAPERSIZE,
        Some(PWSTR(sizes.as_mut_ptr() as *mut u16)),
    );

    let mut names = vec![0u16; count * PAPER_NAME_CHARS];
    // SAFETY: `names` holds 64 UTF-16 units per paper, the documented layout
    // of DC_PAPERNAMES.
    let name_count = device_capability(
        printer,
        port,
        DC_PAPERNAMES,
        Some(PWSTR(names.as_mut_ptr())),
    );

    let listed = id_count.max(0) as usize;
    let measured = size_count.max(0) as usize;
    let named = name_count.max(0) as usize;
    let usable = count.min(listed).min(measured);
    let mut papers = Vec::with_capacity(usable);
    for (index, size) in sizes.iter().enumerate().take(usable) {
        // `DC_PAPERSIZE` is in tenths of a millimetre.
        let width_mm = size.x as f64 / 10.0;
        let height_mm = size.y as f64 / 10.0;
        if width_mm <= 0.0 || height_mm <= 0.0 {
            continue;
        }
        let driver_name = if index < named {
            paper_name(&names, index)
        } else {
            String::new()
        };
        let name = if driver_name.is_empty() {
            format!("{width_mm:.1}x{height_mm:.1}mm")
        } else {
            driver_name
        };
        papers.push(PaperInfo {
            name,
            width_mm,
            height_mm,
            named: standard_name(width_mm, height_mm).is_some(),
        });
    }
    papers.sort_by(|left, right| {
        (right.width_mm * right.height_mm)
            .total_cmp(&(left.width_mm * left.height_mm))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(papers)
}

fn paper_name(names: &[u16], index: usize) -> String {
    let start = index * PAPER_NAME_CHARS;
    let end = (start + PAPER_NAME_CHARS).min(names.len());
    let field = &names[start..end];
    let length = field
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(field.len());
    String::from_utf16_lossy(&field[..length]).trim().to_owned()
}

/// A4/A3/A5/Letter/Legal when the dimensions match within 1 mm.
///
/// The spooler reports dimensions but not which ones are named standards;
/// this mirrors the rule `platform-mac` uses so both platforms agree on what
/// `named` means.
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

/// Whether the queue's print processor advertises the RAW datatype.
///
/// A queue that cannot be opened, or whose processor cannot be queried, reports
/// `false`: raw support is never assumed.
fn raw_supported(printer: &str) -> bool {
    let Ok(handle) = open_printer(printer) else {
        return false;
    };
    let Ok(details) = printer_details(&handle, printer) else {
        return false;
    };
    datatypes(printer, &details.print_processor)
        .map(|names| names.iter().any(|name| name.eq_ignore_ascii_case("RAW")))
        .unwrap_or(false)
}

/// Datatypes the print processor accepts, such as `RAW` and `XPS_PASS`.
fn datatypes(printer: &str, print_processor: &str) -> Result<Vec<String>, PlatformError> {
    const LEVEL: u32 = 1;
    let name = HSTRING::from(printer);
    let processor = HSTRING::from(print_processor);
    let mut needed = 0u32;
    let mut returned = 0u32;
    // SAFETY: a null buffer asks for the required size; the strings outlive
    // both calls.
    let _ = unsafe {
        EnumPrintProcessorDatatypesW(&name, &processor, LEVEL, None, &mut needed, &mut returned)
    };
    if needed == 0 {
        return Ok(Vec::new());
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: the buffer is exactly the size the spooler requested.
    let listed = unsafe {
        EnumPrintProcessorDatatypesW(
            &name,
            &processor,
            LEVEL,
            Some(buffer.as_mut_slice()),
            &mut needed,
            &mut returned,
        )
    };
    if !listed.as_bool() {
        return Err(PlatformError::Print(format!(
            "cannot list datatypes for {printer}: {}",
            Error::from_thread()
        )));
    }

    let stride = std::mem::size_of::<DATATYPES_INFO_1W>();
    let mut names = Vec::with_capacity(returned as usize);
    for index in 0..returned as usize {
        let Some(offset) = index.checked_mul(stride) else {
            break;
        };
        if offset + stride > buffer.len() {
            break;
        }
        // SAFETY: the record lies inside the buffer; `read_unaligned` does not
        // require the alignment a byte buffer cannot promise.
        let info =
            unsafe { (buffer.as_ptr().add(offset) as *const DATATYPES_INFO_1W).read_unaligned() };
        if info.pName.is_null() {
            continue;
        }
        // SAFETY: the name points into the same buffer and is null-terminated.
        if let Ok(name) = unsafe { info.pName.to_string() } {
            names.push(name);
        }
    }
    Ok(names)
}

/// End the page and document when a raw write fails halfway through.
struct RawDocument<'a> {
    handle: &'a PrinterHandle,
    page_open: bool,
    document_open: bool,
}

impl Drop for RawDocument<'_> {
    fn drop(&mut self) {
        // SAFETY: each flag tracks a successful start call on this handle.
        unsafe {
            if self.page_open {
                let _ = EndPagePrinter(self.handle.0);
            }
            if self.document_open {
                let _ = EndDocPrinter(self.handle.0);
            }
        }
    }
}

/// Spooler errors that mean the job is no longer there, not that deleting it
/// was refused.
fn is_missing_job(error: &Error) -> bool {
    let code = error.code();
    [
        ERROR_INVALID_HANDLE,
        ERROR_INVALID_PARAMETER,
        ERROR_INVALID_PRINTER_NAME,
        ERROR_PRINTER_NOT_FOUND,
    ]
    .into_iter()
    .any(|candidate| code == HRESULT::from_win32(candidate.0))
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    use super::*;

    #[test]
    fn raw_cancel_is_reported_separately_from_graphic_cancel() {
        let backend = WinPrinterBackend::new();
        assert!(!backend.cancel_submitted_supported());
        assert!(backend.cancel_raw_supported());
    }

    #[test]
    fn the_windows_backend_prints_both_pdf_and_html() {
        let backend = WinPrinterBackend::new();
        assert!(backend.prints_pdf());
        assert!(backend.prints_html());
    }

    #[test]
    fn a_graphic_job_has_no_spooler_id_to_cancel() {
        let backend = WinPrinterBackend::new();
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
    fn a_raw_job_with_an_id_reaches_the_spooler() {
        let backend = WinPrinterBackend::new();
        let job = SubmittedJob {
            job_id: Some(7),
            printer: "printkit-no-such-printer".to_owned(),
        };
        let result = backend.cancel_submitted(&job);
        // Not a blanket refusal: the id goes to `SetJob` (the queue is missing
        // here, so the refusal is "job/queue gone", not "unsupported").
        assert!(
            !matches!(result, Err(PlatformError::CancelUnsupported)),
            "{result:?}"
        );
        assert!(
            matches!(
                result,
                Err(PlatformError::JobNotFound { .. })
                    | Err(PlatformError::PrinterNotFound(_))
                    | Err(PlatformError::Print(_))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn gone_job_codes_are_not_cancel_refusals() {
        for code in [
            ERROR_INVALID_HANDLE,
            ERROR_INVALID_PARAMETER,
            ERROR_INVALID_PRINTER_NAME,
            ERROR_PRINTER_NOT_FOUND,
        ] {
            let error = Error::from_hresult(HRESULT::from_win32(code.0));
            assert!(
                is_missing_job(&error),
                "0x{:08x} should mean the job is gone",
                code.0
            );
        }
        let denied = Error::from_hresult(HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
        assert!(!is_missing_job(&denied));
    }

    #[test]
    fn standard_paper_is_named_within_a_millimetre() {
        assert_eq!(standard_name(210.0, 297.0), Some("A4"));
        assert_eq!(standard_name(297.0, 210.0), Some("A4"));
        assert_eq!(standard_name(215.9, 279.4), Some("Letter"));
        assert_eq!(standard_name(100.0, 150.0), None);
    }
}
