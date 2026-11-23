//! Merge embedded PDF pages into the renderer's PDF as Form XObjects.
//!
//! A page is copied as a vector object graph: its content streams, fonts, images
//! and any other resources are cloned into the target document with remapped
//! object ids, then painted with a `Do` operator at the item's frame. Nothing
//! is rasterised.
//!
//! Only the page's own resources and content are copied. Annotations, outlines
//! and the page tree are not part of the painted page, so they are ignored.
//! The page is clipped to the intersection of `/MediaBox` and `/CropBox`, and
//! its `/Rotate` is baked into the placement matrix.
//!
//! Embedded pages are painted by content streams appended after the
//! renderer's own content, so they sit above HTML items on the same page
//! regardless of item array order. The renderer emits one stream per paper
//! page, and only an operator inside that stream could interleave the two.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use bytes::Bytes;
use lopdf::{Dictionary, Document, Error as LopdfError, Object, ObjectId, Stream};

use crate::PdfError;

/// A rectangle in PDF points, with its origin at the page's bottom-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtRect {
    /// Left edge, in points from the page's left edge.
    pub x: f64,
    /// Bottom edge, in points from the page's bottom edge.
    pub y: f64,
    /// Width in points; the merge requires it to be finite and positive.
    pub width: f64,
    /// Height in points; the merge requires it to be finite and positive.
    pub height: f64,
}

impl PtRect {
    /// Build a frame from page coordinates and size, all in points.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One embedded page painted onto a page of the target document.
#[derive(Debug, Clone)]
pub struct EmbeddedPage {
    /// The whole embedded PDF, as decoded from its data URI.
    pub bytes: Bytes,
    /// One-based page inside the embedded PDF.
    pub page: u32,
    /// One-based page of the target document.
    pub target_page: u32,
    /// Where the page is painted, in points.
    pub frame: PtRect,
}

/// Largest decompressed page content this crate will build a Form XObject from.
/// Embedded inputs are capped at 8 MiB, so this only stops a decompression bomb.
const MAX_FORM_CONTENT_BYTES: usize = 64 * 1024 * 1024;
/// A page resource graph deeper than this is treated as unsupported instead of
/// exhausting the stack.
const MAX_OBJECT_DEPTH: usize = 64;

/// Merge every embedded page into `target`.
pub(crate) fn merge(target: &mut Document, pages: &[EmbeddedPage]) -> Result<(), PdfError> {
    let mut placements: BTreeMap<u32, Vec<(String, [f64; 6])>> = BTreeMap::new();
    let mut sources: BTreeMap<(usize, usize), Source> = BTreeMap::new();
    let mut next_name = 0_u32;

    for embedded in pages {
        if !embedded.frame.width.is_finite()
            || !embedded.frame.height.is_finite()
            || !embedded.frame.x.is_finite()
            || !embedded.frame.y.is_finite()
            || embedded.frame.width <= 0.0
            || embedded.frame.height <= 0.0
        {
            return Err(PdfError::InvalidFrame);
        }
        // Items that share a payload share one parse and one object-id map, so
        // a font used by several pages is copied into the target once.
        let key = (embedded.bytes.as_ptr() as usize, embedded.bytes.len());
        let source = match sources.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Source::load(&embedded.bytes)?),
        };
        let page_id = source.page_object(embedded.page)?;
        let geometry = PageGeometry::read(&source.document, page_id)?;
        let form = build_form(&source.document, &geometry, &mut source.copier, target)?;
        let form_id = allocate(target)?;
        target.objects.insert(form_id, Object::Stream(form));

        let name = unique_name(target, embedded.target_page, &mut next_name)?;
        insert_xobject(target, embedded.target_page, &name, form_id)?;
        placements.entry(embedded.target_page).or_default().push((
            name,
            placement_matrix(&embedded.frame, geometry.visible, geometry.rotate),
        ));
    }

    for (page, items) in placements {
        let page_id = target
            .get_pages()
            .get(&page)
            .copied()
            .ok_or(PdfError::MissingPage { page })?;
        let mut content = String::with_capacity(items.len() * 64);
        for (name, matrix) in items {
            let [a, b, c, d, e, f] = matrix;
            let _ = writeln!(
                content,
                "q {a:.6} {b:.6} {c:.6} {d:.6} {e:.6} {f:.6} cm /{name} Do Q"
            );
        }
        target
            .add_page_contents(page_id, content.into_bytes())
            .map_err(|error| PdfError::Parse(error.to_string()))?;
    }
    Ok(())
}

/// One parsed payload, kept for every item that references the same bytes.
struct Source {
    document: Document,
    pages: BTreeMap<u32, ObjectId>,
    /// Object ids already copied into the target. Shared by every page of this
    /// payload, so shared fonts and images are copied once.
    copier: Copier,
}

impl Source {
    fn load(bytes: &[u8]) -> Result<Self, PdfError> {
        if !crate::is_pdf(bytes) {
            return Err(PdfError::NotPdf);
        }
        let document = Document::load_mem(bytes).map_err(|error| match error {
            LopdfError::Decryption(_)
            | LopdfError::InvalidPassword
            | LopdfError::AlreadyEncrypted => PdfError::Encrypted,
            other => PdfError::Parse(other.to_string()),
        })?;
        // An inline `/Encrypt` dictionary is as binding as a reference; both
        // mean the payload's strings and streams are encrypted.
        if document.trailer.get(b"Encrypt").is_ok() {
            return Err(PdfError::Encrypted);
        }
        let pages = document.get_pages();
        Ok(Self {
            document,
            pages,
            copier: Copier::default(),
        })
    }

    fn page_object(&self, page: u32) -> Result<ObjectId, PdfError> {
        let pages = u32::try_from(self.pages.len()).unwrap_or(u32::MAX);
        self.pages
            .get(&page)
            .copied()
            .ok_or(PdfError::EmbeddedPageOutOfRange { page, pages })
    }
}

/// Content and resources of one source page, resolved through the page tree.
struct PageGeometry {
    /// Intersection of `/MediaBox` and `/CropBox`, the area painted.
    visible: [f64; 4],
    rotate: u16,
    resources: Dictionary,
    contents: Vec<ObjectId>,
}

impl PageGeometry {
    fn read(source: &Document, page_id: ObjectId) -> Result<Self, PdfError> {
        let media =
            inherited_box(source, page_id, b"MediaBox")?.ok_or(PdfError::UnsupportedObject {
                object: page_id,
                reason: "page has no MediaBox".to_owned(),
            })?;
        let visible = match inherited_box(source, page_id, b"CropBox")? {
            Some(crop) => intersect(media, crop).ok_or(PdfError::UnsupportedObject {
                object: page_id,
                reason: "CropBox does not intersect MediaBox".to_owned(),
            })?,
            None => media,
        };
        if visible[2] <= visible[0] || visible[3] <= visible[1] {
            return Err(PdfError::UnsupportedObject {
                object: page_id,
                reason: "page has an empty visible box".to_owned(),
            });
        }
        let rotate = inherited_number(source, page_id, b"Rotate")?.unwrap_or(0.0);
        let rotate = quarter_turns(rotate).ok_or(PdfError::UnsupportedObject {
            object: page_id,
            reason: "Rotate is not a multiple of 90 degrees".to_owned(),
        })?;

        let resources = effective_resources(source, page_id)?;
        Ok(Self {
            visible,
            rotate,
            resources,
            contents: source.get_page_contents(page_id),
        })
    }
}

fn build_form(
    source: &Document,
    geometry: &PageGeometry,
    copier: &mut Copier,
    target: &mut Document,
) -> Result<Stream, PdfError> {
    // A Form XObject has one content stream, so an array of page streams is
    // decoded and joined in order.
    let mut content = Vec::new();
    let mut remaining = MAX_FORM_CONTENT_BYTES;
    for id in &geometry.contents {
        let stream = source
            .get_object(*id)
            .and_then(Object::as_stream)
            .map_err(|error| map_source(error, *id))?;
        let decoded = stream
            .decompressed_content_with_limit(remaining)
            .map_err(|error| PdfError::UnsupportedObject {
                object: *id,
                reason: format!("page content stream cannot be decoded: {error}"),
            })?;
        remaining = remaining.saturating_sub(decoded.len());
        content.extend_from_slice(&decoded);
        content.push(b'\n');
    }
    let resources = copier.copy(
        source,
        target,
        &Object::Dictionary(geometry.resources.clone()),
        0,
    )?;
    let Object::Dictionary(resources) = resources else {
        return Err(PdfError::Parse(
            "page resources did not copy to a dictionary".to_owned(),
        ));
    };

    let [x0, y0, x1, y1] = geometry.visible;
    let mut dictionary = Dictionary::new();
    dictionary.set("Type", "XObject");
    dictionary.set("Subtype", "Form");
    dictionary.set("FormType", 1);
    dictionary.set("BBox", vec![real(x0), real(y0), real(x1), real(y1)]);
    dictionary.set("Resources", Object::Dictionary(resources));
    Ok(Stream::new(dictionary, content))
}

/// Copy an object graph from a source into the target, remapping references.
///
/// A reference is allocated in the target before its value is copied, so shared
/// objects and reference cycles stay shared instead of being duplicated or
/// followed forever.
#[derive(Default)]
struct Copier {
    ids: BTreeMap<ObjectId, ObjectId>,
}

impl Copier {
    fn copy(
        &mut self,
        source: &Document,
        target: &mut Document,
        object: &Object,
        depth: usize,
    ) -> Result<Object, PdfError> {
        if depth > MAX_OBJECT_DEPTH {
            return Err(PdfError::TooDeep {
                limit: MAX_OBJECT_DEPTH,
            });
        }
        match object {
            Object::Reference(id) => {
                if let Some(new) = self.ids.get(id) {
                    return Ok(Object::Reference(*new));
                }
                let source_object = source
                    .get_object(*id)
                    .map_err(|error| map_source(error, *id))?;
                let new = allocate(target)?;
                self.ids.insert(*id, new);
                let copied = self.copy(source, target, source_object, depth + 1)?;
                target.objects.insert(new, copied);
                Ok(Object::Reference(new))
            }
            Object::Array(items) => Ok(Object::Array(
                items
                    .iter()
                    .map(|item| self.copy(source, target, item, depth + 1))
                    .collect::<Result<_, _>>()?,
            )),
            Object::Dictionary(dictionary) => Ok(Object::Dictionary(
                self.copy_dictionary(source, target, dictionary, depth)?,
            )),
            Object::Stream(stream) => {
                let dictionary = self.copy_dictionary(source, target, &stream.dict, depth)?;
                Ok(Object::Stream(Stream {
                    dict: dictionary,
                    content: stream.content.clone(),
                    allows_compression: stream.allows_compression,
                    start_position: None,
                }))
            }
            scalar => Ok(scalar.clone()),
        }
    }

    fn copy_dictionary(
        &mut self,
        source: &Document,
        target: &mut Document,
        dictionary: &Dictionary,
        depth: usize,
    ) -> Result<Dictionary, PdfError> {
        let mut copied = Dictionary::new();
        for (key, value) in dictionary.iter() {
            let value = self.copy(source, target, value, depth + 1)?;
            copied.set(key.clone(), value);
        }
        Ok(copied)
    }
}

/// The placement matrix maps source page coordinates to the target page.
fn placement_matrix(frame: &PtRect, visible: [f64; 4], rotate: u16) -> [f64; 6] {
    let [x0, y0, x1, y1] = visible;
    let (width, height) = (x1 - x0, y1 - y0);
    let (display_width, display_height) = match rotate {
        90 | 270 => (height, width),
        _ => (width, height),
    };
    let scale = (frame.width / display_width).min(frame.height / display_height);
    let dx = frame.x + (frame.width - display_width * scale) / 2.0;
    let dy = frame.y + (frame.height - display_height * scale) / 2.0;

    match rotate {
        // (a, b, c, d, e, f): x' = a*x + c*y + e, y' = b*x + d*y + f.
        90 => [
            0.0,
            -scale,
            scale,
            0.0,
            dx - scale * y0,
            dy + scale * (width + x0),
        ],
        180 => [
            -scale,
            0.0,
            0.0,
            -scale,
            dx + scale * (width + x0),
            dy + scale * (height + y0),
        ],
        270 => [
            0.0,
            scale,
            -scale,
            0.0,
            dx + scale * (height + y0),
            dy - scale * x0,
        ],
        _ => [scale, 0.0, 0.0, scale, dx - scale * x0, dy - scale * y0],
    }
}

fn intersect(a: [f64; 4], b: [f64; 4]) -> Option<[f64; 4]> {
    let box_ = [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ];
    (box_[2] > box_[0] && box_[3] > box_[1]).then_some(box_)
}

fn quarter_turns(value: f64) -> Option<u16> {
    if !value.is_finite() {
        return None;
    }
    let turns = value.rem_euclid(360.0);
    if (turns / 90.0).fract() != 0.0 {
        return None;
    }
    Some((turns as u16) % 360)
}

fn inherited_box(
    source: &Document,
    page_id: ObjectId,
    key: &[u8],
) -> Result<Option<[f64; 4]>, PdfError> {
    let Some(object) = inherited_object(source, page_id, key)? else {
        return Ok(None);
    };
    let values = object
        .as_array()
        .map_err(|error| map_source(error, page_id))?;
    if values.len() != 4 {
        return Err(PdfError::UnsupportedObject {
            object: page_id,
            reason: format!(
                "{} does not hold four numbers",
                String::from_utf8_lossy(key)
            ),
        });
    }
    let mut box_ = [0.0; 4];
    for (slot, value) in box_.iter_mut().zip(values) {
        *slot = number(value).ok_or(PdfError::UnsupportedObject {
            object: page_id,
            reason: format!("{} holds a non-numeric bound", String::from_utf8_lossy(key)),
        })?;
    }
    Ok(Some(box_))
}

fn inherited_number(
    source: &Document,
    page_id: ObjectId,
    key: &[u8],
) -> Result<Option<f64>, PdfError> {
    let Some(object) = inherited_object(source, page_id, key)? else {
        return Ok(None);
    };
    number(&object)
        .map(Some)
        .ok_or(PdfError::UnsupportedObject {
            object: page_id,
            reason: format!("{} is not a number", String::from_utf8_lossy(key)),
        })
}

/// Resolve an inheritable page attribute, following `/Parent` at most once per
/// node.
pub(crate) fn inherited_object(
    source: &Document,
    page_id: ObjectId,
    key: &[u8],
) -> Result<Option<Object>, PdfError> {
    let mut node = source
        .get_dictionary(page_id)
        .map_err(|error| map_source(error, page_id))?;
    let mut seen = BTreeSet::new();
    loop {
        if let Ok(value) = node.get(key) {
            return Ok(Some(resolve(source, value)?));
        }
        let parent = match node.get(b"Parent") {
            Ok(Object::Reference(id)) => *id,
            Ok(_) | Err(_) => return Ok(None),
        };
        if !seen.insert(parent) {
            return Err(PdfError::UnsupportedObject {
                object: parent,
                reason: "reference cycle in the page tree".to_owned(),
            });
        }
        node = source
            .get_dictionary(parent)
            .map_err(|error| map_source(error, parent))?;
    }
}

fn resolve(source: &Document, object: &Object) -> Result<Object, PdfError> {
    match object {
        Object::Reference(id) => source
            .get_object(*id)
            .cloned()
            .map_err(|error| map_source(error, *id)),
        other => Ok(other.clone()),
    }
}

fn number(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(value) => Some(*value as f64),
        Object::Real(value) => Some(f64::from(*value)),
        _ => None,
    }
}

fn real(value: f64) -> Object {
    Object::Real(value as f32)
}

/// Reserve an id in the target without writing the object yet.
fn allocate(target: &mut Document) -> Result<ObjectId, PdfError> {
    target
        .try_new_object_id()
        .map_err(|error| PdfError::Parse(error.to_string()))
}

/// The page's effective `/Resources`: the dictionaries on the page and on
/// every ancestor, merged key by key with the nearest level winning.
///
/// `/Resources` is inheritable as a whole, but page trees split categories
/// across levels (a tree node owns the fonts, the page names only its own
/// XObjects). Merging keeps every category the page can paint with.
pub(crate) fn effective_resources(
    document: &Document,
    page_id: ObjectId,
) -> Result<Dictionary, PdfError> {
    let mut merged = Dictionary::new();
    let mut seen = BTreeSet::from([page_id]);
    let mut node_id = page_id;
    loop {
        let node = document
            .get_dictionary(node_id)
            .map_err(|error| map_source(error, node_id))?;
        if let Ok(value) = node.get(b"Resources") {
            let resources = match value {
                Object::Dictionary(dictionary) => dictionary.clone(),
                Object::Reference(id) => match document
                    .get_object(*id)
                    .map_err(|error| map_source(error, *id))?
                {
                    Object::Dictionary(dictionary) => dictionary.clone(),
                    _ => return Err(resources_not_a_dictionary()),
                },
                _ => return Err(resources_not_a_dictionary()),
            };
            for (key, value) in resources.iter() {
                if !merged.has(key) {
                    merged.set(key.clone(), value.clone());
                }
            }
        }
        let parent = node.get(b"Parent").and_then(Object::as_reference);
        match parent {
            Ok(parent) if seen.insert(parent) => node_id = parent,
            Ok(parent) => {
                return Err(PdfError::UnsupportedObject {
                    object: parent,
                    reason: "reference cycle in the page tree".to_owned(),
                });
            }
            Err(_) => break,
        }
    }
    Ok(merged)
}

fn resources_not_a_dictionary() -> PdfError {
    PdfError::Parse("page /Resources is not a dictionary".to_owned())
}

/// A name unused by the target page's `/XObject` resources.
fn unique_name(target: &Document, page: u32, next: &mut u32) -> Result<String, PdfError> {
    let Some(page_id) = target.get_pages().get(&page).copied() else {
        return Err(PdfError::MissingPage { page });
    };
    let resources = effective_resources(target, page_id)?;
    let xobjects = match resources.get(b"XObject") {
        Ok(Object::Dictionary(dictionary)) => Some(dictionary),
        Ok(Object::Reference(id)) => target.get_dictionary(*id).ok(),
        _ => None,
    };
    loop {
        *next += 1;
        let candidate = format!("PdfForm{next}");
        let taken =
            xobjects.is_some_and(|dictionary| dictionary.get(candidate.as_bytes()).ok().is_some());
        if !taken {
            return Ok(candidate);
        }
    }
}

/// Add the Form XObject to the target page's `/Resources/XObject` dictionary.
fn insert_xobject(
    target: &mut Document,
    page: u32,
    name: &str,
    object: ObjectId,
) -> Result<(), PdfError> {
    let page_id = target
        .get_pages()
        .get(&page)
        .copied()
        .ok_or(PdfError::MissingPage { page })?;
    let resources = page_resources_direct(target, page_id)?;
    let xobjects = match resources.get_mut(b"XObject") {
        Ok(Object::Dictionary(dictionary)) => dictionary,
        Ok(_) => {
            return Err(PdfError::Parse(
                "page /XObject resource is not a dictionary".to_owned(),
            ));
        }
        Err(_) => {
            resources.set("XObject", Object::Dictionary(Dictionary::new()));
            resources
                .get_mut(b"XObject")
                .and_then(Object::as_dict_mut)
                .map_err(|error| PdfError::Parse(error.to_string()))?
        }
    };
    xobjects.set(name, Object::Reference(object));
    Ok(())
}

/// Return the page's resource dictionary as a direct object, keeping inherited
/// and referenced entries so adding a name cannot drop a font.
fn page_resources_direct(
    target: &mut Document,
    page_id: ObjectId,
) -> Result<&mut Dictionary, PdfError> {
    let mut resources = effective_resources(target, page_id)?;
    if let Ok(Object::Reference(id)) = resources.get(b"XObject") {
        let replaced = target
            .get_object(*id)
            .cloned()
            .map_err(|error| map_source(error, *id))?;
        resources.set("XObject", replaced);
    }
    target
        .get_dictionary_mut(page_id)
        .map_err(|error| map_source(error, page_id))?
        .set("Resources", Object::Dictionary(resources));
    target
        .get_dictionary_mut(page_id)
        .map_err(|error| map_source(error, page_id))?
        .get_mut(b"Resources")
        .and_then(Object::as_dict_mut)
        .map_err(|error| PdfError::Parse(error.to_string()))
}

fn map_source(error: LopdfError, object: ObjectId) -> PdfError {
    match error {
        LopdfError::Decryption(_) | LopdfError::InvalidPassword | LopdfError::AlreadyEncrypted => {
            PdfError::Encrypted
        }
        obj_error @ (LopdfError::ObjectNotFound(_)
        | LopdfError::ReferenceCycle(_)
        | LopdfError::InvalidObjectStream(_)) => PdfError::UnsupportedObject {
            object,
            reason: obj_error.to_string(),
        },
        other => PdfError::Parse(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_page_pdf(media: [f64; 4], crop: Option<[f64; 4]>, rotate: Option<i64>) -> Vec<u8> {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            media.iter().map(|value| real(*value)).collect::<Vec<_>>(),
        );
        if let Some(crop) = crop {
            page.set(
                "CropBox",
                crop.iter().map(|value| real(*value)).collect::<Vec<_>>(),
            );
        }
        if let Some(rotate) = rotate {
            page.set("Rotate", rotate);
        }
        page.set("Contents", Object::Reference((4, 0)));
        let mut content = Dictionary::new();
        content.set("Length", 30);
        let stream = Stream::new(content, b"BT /F1 12 Tf 10 10 Td (embedded) Tj ET".to_vec());

        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(stream);
        document.trailer.set("Root", Object::Reference((1, 0)));
        target_bytes(&mut document)
    }

    fn target_bytes(document: &mut Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("serialise");
        bytes
    }

    fn empty_target_with(page_count: u32) -> Document {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set(
            "Kids",
            (0..page_count)
                .map(|index| Object::Reference((3 + index, 0)))
                .collect::<Vec<_>>(),
        );
        pages.set("Count", page_count);
        document.add_object(catalog);
        document.add_object(pages);
        for _ in 0..page_count {
            let mut page = Dictionary::new();
            page.set("Type", "Page");
            page.set("Parent", Object::Reference((2, 0)));
            page.set(
                "MediaBox",
                vec![real(0.0), real(0.0), real(595.0), real(842.0)],
            );
            document.add_object(page);
        }
        document.trailer.set("Root", Object::Reference((1, 0)));
        document
    }

    fn empty_target() -> Document {
        empty_target_with(1)
    }

    #[test]
    fn a_page_becomes_a_form_xobject_painted_on_the_target_page() {
        let bytes = one_page_pdf([0.0, 0.0, 595.0, 842.0], None, None);
        let mut target = empty_target();
        merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(10.0, 10.0, 300.0, 400.0),
            }],
        )
        .unwrap();

        let pages = target.get_pages();
        let page_id = pages[&1];
        let page = target.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let name = String::from_utf8(xobjects.iter().next().unwrap().0.clone()).unwrap();
        let form = xobjects
            .get(name.as_bytes())
            .unwrap()
            .as_reference()
            .and_then(|id| target.get_object(id))
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(
            form.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Form"
        );
        assert_eq!(form.dict.get(b"BBox").unwrap().as_array().unwrap().len(), 4);
        assert!(form.content.windows(2).any(|window| window == b"ET"));
        let content = target.get_page_content(page_id);
        assert!(content.windows(3).any(|window| window == b" Do"));
    }

    #[test]
    fn crop_box_and_rotation_are_applied() {
        let bytes = one_page_pdf(
            [0.0, 0.0, 595.0, 842.0],
            Some([10.0, 20.0, 585.0, 800.0]),
            Some(90),
        );
        let mut target = empty_target();
        merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(10.0, 10.0, 300.0, 300.0),
            }],
        )
        .unwrap();
        let page_id = target.get_pages()[&1];
        let page = target.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let name = String::from_utf8(xobjects.iter().next().unwrap().0.clone()).unwrap();
        let form = xobjects
            .get(name.as_bytes())
            .unwrap()
            .as_reference()
            .and_then(|id| target.get_object(id))
            .unwrap()
            .as_stream()
            .unwrap();
        let bbox: Vec<f64> = form
            .dict
            .get(b"BBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|value| number(value).unwrap())
            .collect();
        assert_eq!(bbox, vec![10.0, 20.0, 585.0, 800.0]);

        let content = String::from_utf8(target.get_page_content(page_id)).unwrap();
        let expected = placement_matrix(
            &PtRect::new(10.0, 10.0, 300.0, 300.0),
            [10.0, 20.0, 585.0, 800.0],
            90,
        );
        let command = format!(
            "{:.6} {:.6} {:.6} {:.6} {:.6} {:.6} cm /{name} Do",
            expected[0], expected[1], expected[2], expected[3], expected[4], expected[5]
        );
        assert!(content.contains(&command), "{content}");
    }

    fn dangling_resource_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![real(0.0), real(0.0), real(595.0), real(842.0)],
        );
        let mut resources = Dictionary::new();
        let mut xobjects = Dictionary::new();
        xobjects.set("Bad", Object::Reference((99, 0)));
        resources.set("XObject", Object::Dictionary(xobjects));
        page.set("Resources", Object::Dictionary(resources));
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.trailer.set("Root", Object::Reference((1, 0)));
        target_bytes(&mut document)
    }

    #[test]
    fn an_unresolved_resource_reports_its_object_number() {
        let bytes = dangling_resource_pdf();
        let mut target = empty_target();
        let error = merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(0.0, 0.0, 10.0, 10.0),
            }],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PdfError::UnsupportedObject {
                object: (99, 0),
                ..
            }
        ));
    }

    #[test]
    fn missing_page_is_rejected() {
        let bytes = one_page_pdf([0.0, 0.0, 595.0, 842.0], None, None);
        let mut target = empty_target();
        let error = merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 2,
                target_page: 1,
                frame: PtRect::new(0.0, 0.0, 10.0, 10.0),
            }],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PdfError::EmbeddedPageOutOfRange { page: 2, pages: 1 }
        ));
    }

    #[test]
    fn placement_matrix_centres_a_rotated_page() {
        let matrix = placement_matrix(
            &PtRect::new(0.0, 0.0, 200.0, 100.0),
            [0.0, 0.0, 100.0, 50.0],
            90,
        );
        // Rotated size is 50 x 100; scale = min(200/50, 100/100) = 1.
        assert_eq!(matrix, [0.0, -1.0, 1.0, 0.0, 75.0, 100.0]);
    }

    /// Two pages that both name the same font object.
    fn two_page_shared_font_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set(
            "Kids",
            vec![Object::Reference((3, 0)), Object::Reference((4, 0))],
        );
        pages.set("Count", 2);
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Shared");
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![real(0.0), real(0.0), real(595.0), real(842.0)],
        );
        let mut resources = Dictionary::new();
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference((5, 0)));
        resources.set("Font", Object::Dictionary(fonts));
        page.set("Resources", Object::Dictionary(resources));
        let second = page.clone();
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(second);
        document.add_object(font);
        document.trailer.set("Root", Object::Reference((1, 0)));
        target_bytes(&mut document)
    }

    /// The `/F1` font reference inside the Form XObject painted on `page`.
    fn form_font(target: &Document, page: u32) -> ObjectId {
        let page_id = target.get_pages()[&page];
        let page = target.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let form = xobjects
            .iter()
            .next()
            .unwrap()
            .1
            .as_reference()
            .and_then(|id| target.get_object(id))
            .unwrap()
            .as_stream()
            .unwrap();
        let resources = form.dict.get(b"Resources").unwrap().as_dict().unwrap();
        resources
            .get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"F1")
            .unwrap()
            .as_reference()
            .unwrap()
    }

    #[test]
    fn pages_of_one_payload_share_copied_resources() {
        let bytes = Bytes::from(two_page_shared_font_pdf());
        let mut target = empty_target_with(2);
        merge(
            &mut target,
            &[
                EmbeddedPage {
                    bytes: bytes.clone(),
                    page: 1,
                    target_page: 1,
                    frame: PtRect::new(10.0, 10.0, 100.0, 100.0),
                },
                EmbeddedPage {
                    bytes,
                    page: 2,
                    target_page: 2,
                    frame: PtRect::new(10.0, 10.0, 100.0, 100.0),
                },
            ],
        )
        .unwrap();
        assert_eq!(form_font(&target, 1), form_font(&target, 2));
    }

    #[test]
    fn inherited_page_resources_survive_xobject_insertion() {
        let bytes = one_page_pdf([0.0, 0.0, 595.0, 842.0], None, None);
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Inherited");
        let mut resources = Dictionary::new();
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference((4, 0)));
        resources.set("Font", Object::Dictionary(fonts));
        // The page tree node owns the resources; the page itself names none.
        pages.set("Resources", Object::Dictionary(resources));
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![real(0.0), real(0.0), real(595.0), real(842.0)],
        );
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(font);
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut target = document;

        merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(0.0, 0.0, 10.0, 10.0),
            }],
        )
        .unwrap();

        let page_id = target.get_pages()[&1];
        let page = target.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(
            resources
                .get(b"Font")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"F1")
                .is_ok()
        );
        assert!(
            resources
                .get(b"XObject")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"PdfForm1")
                .is_ok()
        );
    }

    /// The Form XObject's `/Resources` on `page`.
    fn form_resources(target: &Document, page: u32) -> &Dictionary {
        let page_id = target.get_pages()[&page];
        let page = target.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let form = xobjects
            .iter()
            .next()
            .unwrap()
            .1
            .as_reference()
            .and_then(|id| target.get_object(id))
            .unwrap()
            .as_stream()
            .unwrap();
        form.dict.get(b"Resources").unwrap().as_dict().unwrap()
    }

    /// A target page that names only its XObjects, under a tree node that
    /// owns the font category.
    fn target_with_split_resources() -> Document {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference((4, 0)));
        let mut inherited = Dictionary::new();
        inherited.set("Font", Object::Dictionary(fonts));
        pages.set("Resources", Object::Dictionary(inherited));
        let mut xobjects = Dictionary::new();
        xobjects.set("Existing", Object::Reference((5, 0)));
        let mut resources = Dictionary::new();
        resources.set("XObject", Object::Dictionary(xobjects));
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![real(0.0), real(0.0), real(595.0), real(842.0)],
        );
        page.set("Resources", Object::Dictionary(resources));
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Inherited");
        let mut existing = Dictionary::new();
        existing.set("Type", "XObject");
        existing.set("Subtype", "Form");
        existing.set("BBox", vec![real(0.0), real(0.0), real(10.0), real(10.0)]);

        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(font);
        document.add_object(Stream::new(existing, Vec::new()));
        document.trailer.set("Root", Object::Reference((1, 0)));
        document
    }

    #[test]
    fn resources_on_the_page_and_on_the_tree_node_are_both_kept() {
        let bytes = one_page_pdf([0.0, 0.0, 595.0, 842.0], None, None);
        let mut target = target_with_split_resources();
        merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(0.0, 0.0, 10.0, 10.0),
            }],
        )
        .unwrap();

        let page_id = target.get_pages()[&1];
        let page = target.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        assert!(
            resources
                .get(b"Font")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"F1")
                .is_ok()
        );
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        assert!(xobjects.get(b"Existing").is_ok());
        assert!(xobjects.get(b"PdfForm1").is_ok());
    }

    /// An embedded page whose own `/Resources` is a reference holding an
    /// XObject, under a tree node that owns the font category.
    fn split_resources_pdf() -> Vec<u8> {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference((6, 0)));
        let mut inherited = Dictionary::new();
        inherited.set("Font", Object::Dictionary(fonts));
        pages.set("Resources", Object::Dictionary(inherited));
        let mut resources = Dictionary::new();
        let mut xobjects = Dictionary::new();
        xobjects.set("X1", Object::Reference((5, 0)));
        resources.set("XObject", Object::Dictionary(xobjects));
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![real(0.0), real(0.0), real(595.0), real(842.0)],
        );
        page.set("Resources", Object::Reference((4, 0)));
        let mut nested = Dictionary::new();
        nested.set("Type", "XObject");
        nested.set("Subtype", "Form");
        nested.set("BBox", vec![real(0.0), real(0.0), real(10.0), real(10.0)]);
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Split");

        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(resources);
        document.add_object(Stream::new(nested, Vec::new()));
        document.add_object(font);
        document.trailer.set("Root", Object::Reference((1, 0)));
        target_bytes(&mut document)
    }

    #[test]
    fn a_merged_form_carries_resources_from_every_level() {
        let bytes = split_resources_pdf();
        let mut target = empty_target();
        merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(0.0, 0.0, 10.0, 10.0),
            }],
        )
        .unwrap();

        let resources = form_resources(&target, 1);
        let font_id = resources
            .get(b"Font")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"F1")
            .unwrap()
            .as_reference()
            .unwrap();
        let font = target.get_object(font_id).unwrap().as_dict().unwrap();
        assert_eq!(
            font.get(b"BaseFont").unwrap().as_name().unwrap(),
            b"AAAAAA+Split"
        );
        assert!(
            resources
                .get(b"XObject")
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"X1")
                .is_ok()
        );
    }

    #[test]
    fn a_too_deep_resource_graph_reports_the_limit() {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut deep = Object::Array(Vec::new());
        for _ in 0..=MAX_OBJECT_DEPTH {
            deep = Object::Array(vec![deep]);
        }
        let mut resources = Dictionary::new();
        resources.set("Deep", deep);
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set(
            "MediaBox",
            vec![real(0.0), real(0.0), real(595.0), real(842.0)],
        );
        page.set("Resources", Object::Dictionary(resources));
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.trailer.set("Root", Object::Reference((1, 0)));
        let bytes = target_bytes(&mut document);

        let mut target = empty_target();
        let error = merge(
            &mut target,
            &[EmbeddedPage {
                bytes: Bytes::from(bytes),
                page: 1,
                target_page: 1,
                frame: PtRect::new(0.0, 0.0, 10.0, 10.0),
            }],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            PdfError::TooDeep {
                limit: MAX_OBJECT_DEPTH
            }
        ));
    }
}
