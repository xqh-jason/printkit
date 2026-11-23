//! Hard limits shared by validation, the generated JSON Schema and the protocol.
//!
//! Every number lives here and nowhere else: `validate` rejects above these
//! values, and the schema attributes point at the same constants, so a client
//! written against the published schema cannot be rejected for a value the
//! schema allows.

/// Maximum pages in one document.
pub const MAX_PAGES: usize = 200;
/// Maximum finite size of one custom paper edge. Larger values exhaust the
/// renderer's layout surface before any useful page can be produced.
pub const MAX_PAPER_MM: f64 = 2000.0;
/// Maximum items on one page.
pub const MAX_ITEMS_PER_PAGE: usize = 500;
/// Maximum serialised document size. Exceeding it is a runtime `E_TOO_LARGE`
/// failure, not a schema violation.
pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
/// Maximum body of one administrator write. It carries one document plus a
/// name/id envelope, so it reuses the document cap with 1 MiB JSON overhead.
pub const MAX_ADMIN_BODY_BYTES: usize = MAX_DOCUMENT_BYTES + 1024 * 1024;
/// Maximum decoded size of one embedded resource (image, embedded PDF, font).
pub const MAX_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum characters in one text item.
pub const MAX_TEXT_CHARS: usize = 10_000;
/// Maximum entries in `meta.tags`.
pub const MAX_TAGS: usize = 8;
/// Maximum characters in one tag. Counted in Unicode scalar values, not bytes.
pub const MAX_TAG_CHARS: usize = 24;
/// Maximum table rows.
pub const MAX_TABLE_ROWS: usize = 10_000;
/// Maximum table columns.
pub const MAX_TABLE_COLUMNS: usize = 32;
/// Maximum bytes fetched for one `data.url`.
pub const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;
/// Maximum distinct `data.url` bodies one document may fetch. Every URL costs
/// a network round trip, so a document cannot turn into an unbounded scan.
pub const MAX_FETCH_URLS: usize = 32;
/// Maximum decoded size of an embedded font file.
pub const MAX_FONT_BYTES: usize = 5 * 1024 * 1024;
/// Maximum bytes encoded in one barcode value.
pub const MAX_BARCODE_BYTES: usize = 2_048;
/// Lowest accepted `copies`.
pub const MIN_COPIES: u32 = 1;
/// Highest accepted `copies`.
pub const MAX_COPIES: u32 = 999;
/// Lowest accepted preview DPI.
pub const MIN_DPI: u16 = 72;
/// Highest accepted preview DPI.
pub const MAX_DPI: u16 = 300;

/// Maximum documents in one batch call.
pub const MAX_BATCH_DOCS: usize = 100;
/// Maximum pages across one batch.
pub const MAX_BATCH_PAGES: usize = 2_000;
/// Maximum combined PDF size of one batch.
pub const MAX_BATCH_PDF_BYTES: usize = 128 * 1024 * 1024;
/// Maximum bytes of one WebSocket message.
pub const MAX_WS_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum decoded PDF bytes sent through the WebSocket channel.
pub const MAX_WS_PDF_BYTES: usize = 24 * 1024 * 1024;
/// Maximum base64 text carried by one WebSocket message.
pub const MAX_WS_BASE64_BYTES: usize = 32 * 1024 * 1024;
/// Maximum raw bytes in one `raw.send`.
pub const MAX_RAW_BYTES: usize = 16 * 1024 * 1024;
/// Maximum size of one multipart segment of `POST /print/batch`.
pub const MAX_BATCH_SEGMENT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum total size of `POST /print/batch`.
pub const MAX_BATCH_REQUEST_BYTES: usize = 256 * 1024 * 1024;

/// Maximum cells in one `tianGrid` (`rows * cols`).
pub const MAX_GRID_CELLS: u32 = 10_000;
