//! Combine one-page PDF captures into one multi-page document.
//!
//! `WKPDFConfiguration.rect` captures exactly one page rectangle per call, so
//! the host captures each page separately. CoreGraphics is used only to copy
//! each captured vector page into a page of the final document; no page is
//! rasterised.

use std::ffi::c_void;
use std::ptr;

use objc2_core_foundation::{CFData, CFMutableData, CGPoint, CGRect, CGSize};
use printkit_platform::{MM_TO_PT, PlatformError, RenderSpec};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDataProviderCreateWithCFData(data: *const CFData) -> *mut c_void;
    fn CGDataProviderRelease(provider: *mut c_void);
    fn CGPDFDocumentCreateWithProvider(provider: *mut c_void) -> *mut c_void;
    fn CGPDFDocumentRelease(document: *mut c_void);
    fn CGPDFDocumentGetPage(document: *mut c_void, page_number: usize) -> *mut c_void;
    fn CGPDFDocumentGetNumberOfPages(document: *mut c_void) -> usize;
    fn CGPDFPageGetBoxRect(page: *mut c_void, box_type: u32) -> CGRect;
    fn CGDataConsumerCreateWithCFData(data: *mut CFMutableData) -> *mut c_void;
    fn CGDataConsumerRelease(consumer: *mut c_void);
    fn CGPDFContextCreate(
        consumer: *mut c_void,
        media_box: *const CGRect,
        auxiliary_info: *mut c_void,
    ) -> *mut c_void;
    fn CGContextBeginPage(context: *mut c_void, media_box: *const CGRect);
    fn CGContextEndPage(context: *mut c_void);
    fn CGContextSaveGState(context: *mut c_void);
    fn CGContextRestoreGState(context: *mut c_void);
    fn CGContextScaleCTM(context: *mut c_void, sx: f64, sy: f64);
    fn CGContextDrawPDFPage(context: *mut c_void, page: *mut c_void);
    fn CGPDFContextClose(context: *mut c_void);
    fn CGContextRelease(context: *mut c_void);
}

/// `kCGPDFMediaBox` from `CGPDFBox`.
const MEDIA_BOX: u32 = 0;

/// Merge captures into one PDF. Each capture is redrawn into the exact paper
/// box from `spec`, so a mixed-size document keeps every page's real size and
/// WebKit's whole-point capture rounding does not shave the sheet.
///
/// Every capture must be a readable one-page PDF, and the result must carry
/// exactly `spec.pages` pages. A capture that cannot be read is an error, never
/// a silently dropped page: a short PDF looks like a successful print.
pub(super) fn merge_pages(parts: &[Vec<u8>], spec: &RenderSpec) -> Result<Vec<u8>, PlatformError> {
    let expected = spec.pages.max(1);
    if parts.len() != expected {
        return Err(PlatformError::Render(format!(
            "page capture count mismatch: expected {expected}, got {}",
            parts.len()
        )));
    }
    let (default_width, default_height) = spec.size_mm(0);
    let data = CFMutableData::new(None, 0)
        .ok_or_else(|| PlatformError::Render("cannot allocate the merged PDF".to_owned()))?;
    let media_box = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(default_width * MM_TO_PT, default_height * MM_TO_PT),
    );

    // SAFETY: the mutable data and rectangle live until the context is closed
    // below; the consumer retains the data object.
    let consumer = unsafe {
        CGDataConsumerCreateWithCFData(&*data as *const CFMutableData as *mut CFMutableData)
    };
    if consumer.is_null() {
        return Err(PlatformError::Render(
            "cannot create the merged PDF data consumer".to_owned(),
        ));
    }
    // SAFETY: the consumer is live and the media box pointer is valid for the
    // duration of the call; the context copies the box.
    let context = unsafe { CGPDFContextCreate(consumer, &media_box, ptr::null_mut()) };
    if context.is_null() {
        // SAFETY: the consumer was created above and is not null.
        unsafe { CGDataConsumerRelease(consumer) };
        return Err(PlatformError::Render(
            "cannot create the merged PDF context".to_owned(),
        ));
    }

    let mut failure = None;
    for (index, bytes) in parts.iter().enumerate() {
        match draw_capture(context, index, bytes, spec) {
            Ok(()) => {}
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }

    // SAFETY: the context and consumer were created above and are live.
    unsafe {
        CGPDFContextClose(context);
        CGContextRelease(context);
        CGDataConsumerRelease(consumer);
    }
    if let Some(error) = failure {
        return Err(error);
    }

    let length = usize::try_from(data.length())
        .map_err(|_| PlatformError::Render("the merged PDF is too large".to_owned()))?;
    // SAFETY: the closed PDF context left the data object with `length` bytes.
    let pointer = data.byte_ptr();
    if length == 0 || pointer.is_null() {
        return Err(PlatformError::Render(
            "the merged PDF context produced no bytes".to_owned(),
        ));
    }
    // SAFETY: the pointer addresses `length` bytes owned by the live data object.
    let output = unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec();
    let written = pdf_page_count(&output);
    if written != expected {
        return Err(PlatformError::Render(format!(
            "the merged PDF has {written} pages, expected {expected}"
        )));
    }
    Ok(output)
}

/// Draw one one-page capture into the next page of `context`.
fn draw_capture(
    context: *mut c_void,
    index: usize,
    bytes: &[u8],
    spec: &RenderSpec,
) -> Result<(), PlatformError> {
    let page = index.saturating_add(1);
    if bytes.is_empty() {
        return Err(PlatformError::Render(format!(
            "page capture {page} is empty"
        )));
    }
    // SAFETY: the byte slice is valid for the call; CFData copies the bytes.
    let cf_data =
        unsafe { CFData::new(None, bytes.as_ptr(), bytes.len() as isize) }.ok_or_else(|| {
            PlatformError::Render(format!("page capture {page} cannot be read as PDF data"))
        })?;
    // SAFETY: the data object is live and non-null.
    let provider = unsafe { CGDataProviderCreateWithCFData(&*cf_data) };
    if provider.is_null() {
        return Err(PlatformError::Render(format!(
            "page capture {page} has no data provider"
        )));
    }
    // SAFETY: the provider is live.
    let document = unsafe { CGPDFDocumentCreateWithProvider(provider) };
    // SAFETY: the document retains what it needs from the provider.
    unsafe { CGDataProviderRelease(provider) };
    if document.is_null() {
        return Err(PlatformError::Render(format!(
            "page capture {page} is not a readable PDF"
        )));
    }

    // SAFETY: the document is live; the capture must carry exactly one page.
    let page_count = unsafe { CGPDFDocumentGetNumberOfPages(document) };
    let capture_page = unsafe { CGPDFDocumentGetPage(document, 1) };
    if page_count != 1 || capture_page.is_null() {
        // SAFETY: the document is live and no longer needed.
        unsafe { CGPDFDocumentRelease(document) };
        return Err(PlatformError::Render(format!(
            "page capture {page} has {page_count} pages, expected exactly one"
        )));
    }
    // SAFETY: `CGPDFDocumentGetPage` returns a borrowed page owned by the live
    // document; it must not be released separately.
    let captured = unsafe { CGPDFPageGetBoxRect(capture_page, MEDIA_BOX) };
    if captured.size.width <= 0.0 || captured.size.height <= 0.0 {
        // SAFETY: the document is live and no longer needed.
        unsafe { CGPDFDocumentRelease(document) };
        return Err(PlatformError::Render(format!(
            "page capture {page} has an empty media box"
        )));
    }

    // SAFETY: the spec index is bounded by the capture count.
    let (width_mm, height_mm) = spec.size_mm(index);
    let page_box = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width_mm * MM_TO_PT, height_mm * MM_TO_PT),
    );
    let scale_x = page_box.size.width / captured.size.width;
    let scale_y = page_box.size.height / captured.size.height;
    // SAFETY: the context is live; every value is local to this page.
    unsafe {
        CGContextBeginPage(context, &page_box);
        CGContextSaveGState(context);
        CGContextScaleCTM(context, scale_x, scale_y);
        CGContextDrawPDFPage(context, capture_page);
        CGContextRestoreGState(context);
        CGContextEndPage(context);
    }
    // SAFETY: the document is live and no longer needed.
    unsafe { CGPDFDocumentRelease(document) };
    Ok(())
}

/// Page count of a PDF byte slice; 0 when CoreGraphics cannot open it.
fn pdf_page_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    // SAFETY: the byte slice is valid for the call; CFData copies the bytes.
    let Some(data) = (unsafe { CFData::new(None, bytes.as_ptr(), bytes.len() as isize) }) else {
        return 0;
    };
    // SAFETY: the data object is live.
    let provider = unsafe { CGDataProviderCreateWithCFData(&*data) };
    if provider.is_null() {
        return 0;
    }
    // SAFETY: the provider is live.
    let document = unsafe { CGPDFDocumentCreateWithProvider(provider) };
    // SAFETY: the document retains what it needs from the provider.
    unsafe { CGDataProviderRelease(provider) };
    if document.is_null() {
        return 0;
    }
    // SAFETY: the document is live.
    let pages = unsafe { CGPDFDocumentGetNumberOfPages(document) };
    // SAFETY: the document is no longer needed.
    unsafe { CGPDFDocumentRelease(document) };
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid one-page PDF made by CoreGraphics, so no external fixture or
    /// GUI is needed.
    fn sample_pdf(pages: usize, width: f64, height: f64) -> Vec<u8> {
        let data = CFMutableData::new(None, 0).expect("mutable data");
        // SAFETY: the data object is live for this scope.
        let consumer = unsafe {
            CGDataConsumerCreateWithCFData(&*data as *const CFMutableData as *mut CFMutableData)
        };
        assert!(!consumer.is_null());
        let media_box = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height));
        // SAFETY: the consumer and media box are live.
        let context = unsafe { CGPDFContextCreate(consumer, &media_box, ptr::null_mut()) };
        assert!(!context.is_null());
        for _ in 0..pages {
            // SAFETY: the context is live.
            unsafe {
                CGContextBeginPage(context, &media_box);
                CGContextEndPage(context);
            }
        }
        // SAFETY: the context and consumer are live and no longer needed.
        unsafe {
            CGPDFContextClose(context);
            CGContextRelease(context);
            CGDataConsumerRelease(consumer);
        }
        let length = usize::try_from(data.length()).expect("length");
        // SAFETY: the closed document owns `length` bytes.
        let pointer = data.byte_ptr();
        assert!(length > 0 && !pointer.is_null());
        // SAFETY: the pointer addresses `length` bytes owned by the data object.
        unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec()
    }

    fn spec(pages: usize) -> RenderSpec {
        let mut spec = RenderSpec::new(210.0, 297.0);
        spec.pages = pages;
        spec
    }

    #[test]
    fn merges_every_capture_into_one_page_per_spec_page() {
        let captures = vec![sample_pdf(1, 595.0, 842.0), sample_pdf(1, 595.0, 842.0)];
        let merged = merge_pages(&captures, &spec(2)).expect("merge");
        assert_eq!(pdf_page_count(&merged), 2);
    }

    #[test]
    fn an_empty_capture_is_an_error_even_between_good_pages() {
        let captures = vec![
            sample_pdf(1, 595.0, 842.0),
            Vec::new(),
            sample_pdf(1, 595.0, 842.0),
        ];
        let error = merge_pages(&captures, &spec(3)).unwrap_err();
        assert!(
            error.to_string().contains("page capture 2 is empty"),
            "{error}"
        );
    }

    #[test]
    fn an_unreadable_capture_is_an_error() {
        let captures = vec![sample_pdf(1, 595.0, 842.0), b"not a pdf".to_vec()];
        let error = merge_pages(&captures, &spec(2)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("page capture 2 is not a readable PDF"),
            "{error}"
        );
    }

    #[test]
    fn a_capture_with_several_pages_is_an_error() {
        let captures = vec![sample_pdf(2, 595.0, 842.0)];
        let error = merge_pages(&captures, &spec(1)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("has 2 pages, expected exactly one"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_capture_is_an_error_not_a_short_pdf() {
        let captures = vec![sample_pdf(1, 595.0, 842.0)];
        let error = merge_pages(&captures, &spec(2)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "render failed: page capture count mismatch: expected 2, got 1"
        );
    }
}
