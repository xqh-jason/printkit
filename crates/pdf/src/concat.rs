//! Concatenate complete PDFs into one spooler document.
//!
//! A batch must reach the spooler as one job. That means one PDF whose page
//! tree carries every page of every input in order; the objects each page
//! references (contents, fonts, XObjects) are copied with references rewritten
//! into the destination id space.
//!
//! The copy is recursive with a placeholder for every object already scheduled,
//! so an annotation that points back at its page cannot loop.

use std::collections::BTreeMap;

use bytes::Bytes;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::PdfError;

/// A page resource graph deeper than this is refused instead of exhausting the
/// stack, matching `merge` and `preflight`.
const MAX_OBJECT_DEPTH: usize = 64;

/// Concatenate `pdfs` in order and return the merged bytes plus page count.
pub fn concat(pdfs: &[Bytes]) -> Result<(Bytes, u32), PdfError> {
    concat_with_title(pdfs, None)
}

/// Concatenate and stamp a shared title (batch jobs arrive as one PDF).
pub fn concat_with_title(pdfs: &[Bytes], title: Option<&str>) -> Result<(Bytes, u32), PdfError> {
    if pdfs.is_empty() {
        return Err(PdfError::Empty);
    }
    if pdfs.len() == 1 && title.is_none() {
        // One input needs no rebuild; validation still happens above this
        // call, and re-serialising a 1000-page file would only cost time.
        let pages = page_count(&pdfs[0])?;
        return Ok((pdfs[0].clone(), pages));
    }

    let mut destination = Document::with_version("1.5");
    let pages_id = allocate(&mut destination, Object::Null);
    let catalog_id = allocate(&mut destination, Object::Null);
    let mut kids = Vec::new();
    let mut total_pages = 0_u32;

    for bytes in pdfs {
        let source =
            Document::load_mem(bytes).map_err(|error| PdfError::Parse(error.to_string()))?;
        if source.trailer.get(b"Encrypt").is_ok() {
            return Err(PdfError::Encrypted);
        }
        let mut map = BTreeMap::new();
        for page_id in source.get_pages().values().copied() {
            let page = source
                .get_dictionary(page_id)
                .map_err(|error| PdfError::Parse(error.to_string()))?
                .clone();
            let destination_id = allocate(&mut destination, Object::Null);
            let mut copied = Dictionary::new();
            for (key, value) in page.iter() {
                // The parent points into the source page tree, which is not
                // copied; it is rewritten below.
                if key.as_slice() == b"Parent" {
                    continue;
                }
                copied.set(
                    key.clone(),
                    copy_object(&source, value, &mut map, &mut destination, 0)?,
                );
            }
            // `/Resources`, `/MediaBox`, `/CropBox` and `/Rotate` are
            // inheritable: the flattened page has a new parent, so a value
            // that lived on the source tree nodes must move onto the page.
            if !page.has(b"Resources") {
                let resources = crate::merge::effective_resources(&source, page_id)?;
                if !resources.is_empty() {
                    copied.set(
                        "Resources",
                        copy_object(
                            &source,
                            &Object::Dictionary(resources),
                            &mut map,
                            &mut destination,
                            0,
                        )?,
                    );
                }
            }
            for key in [b"MediaBox".as_slice(), b"CropBox", b"Rotate"] {
                if page.has(key) {
                    continue;
                }
                if let Some(value) = crate::merge::inherited_object(&source, page_id, key)? {
                    copied.set(
                        key.to_vec(),
                        copy_object(&source, &value, &mut map, &mut destination, 0)?,
                    );
                }
            }
            copied.set("Parent", Object::Reference(pages_id));
            destination
                .objects
                .insert(destination_id, Object::Dictionary(copied));
            kids.push(Object::Reference(destination_id));
            total_pages = total_pages.saturating_add(1);
        }
    }

    if kids.is_empty() {
        return Err(PdfError::Empty);
    }

    let mut pages = Dictionary::new();
    pages.set("Type", "Pages");
    pages.set("Kids", kids);
    pages.set("Count", i64::from(total_pages));
    destination
        .objects
        .insert(pages_id, Object::Dictionary(pages));

    let mut catalog = Dictionary::new();
    catalog.set("Type", "Catalog");
    catalog.set("Pages", Object::Reference(pages_id));
    destination
        .objects
        .insert(catalog_id, Object::Dictionary(catalog));
    destination
        .trailer
        .set("Root", Object::Reference(catalog_id));
    if let Some(title) = title {
        crate::set_info(&mut destination, title);
    }

    let mut buffer = Vec::new();
    destination
        .save_to(&mut buffer)
        .map_err(|error| PdfError::Save(error.to_string()))?;
    if !crate::is_pdf(&buffer) {
        return Err(PdfError::NotPdf);
    }
    Ok((Bytes::from(buffer), total_pages))
}

/// Number of pages in a complete PDF.
/// Page count plus whether a merge rebuild would drop document metadata.
///
/// The batch merge copies page content only; `/AcroForm`, `/Outlines` and the
/// catalog `/Metadata` stream (as well as the trailer `/Info` dictionary) do
/// not travel with the pages. The caller warns instead of losing them
/// silently.
pub fn page_count_with_metadata(bytes: &[u8]) -> Result<(u32, bool), PdfError> {
    let (_, pages, metadata) = inspect(bytes)?;
    Ok((pages, metadata))
}

fn inspect(bytes: &[u8]) -> Result<(Document, u32, bool), PdfError> {
    if !crate::is_pdf(bytes) {
        return Err(PdfError::NotPdf);
    }
    #[cfg(feature = "parse-count")]
    crate::bump_parse_count();
    let document = Document::load_mem(bytes).map_err(|error| PdfError::Parse(error.to_string()))?;
    if document.trailer.get(b"Encrypt").is_ok() {
        return Err(PdfError::Encrypted);
    }
    let count = document.get_pages().len();
    let pages = u32::try_from(count).map_err(|_| PdfError::TooManyPages)?;
    let metadata = document_metadata(&document);
    Ok((document, pages, metadata))
}

fn document_metadata(document: &Document) -> bool {
    document.trailer.get(b"Info").is_ok()
        || document
            .trailer
            .get(b"Root")
            .ok()
            .and_then(|root| root.as_reference().ok())
            .and_then(|id| document.get_dictionary(id).ok())
            .is_some_and(|catalog| {
                catalog.has(b"AcroForm") || catalog.has(b"Outlines") || catalog.has(b"Metadata")
            })
}

/// Number of pages in one PDF, rejecting encrypted files.
pub fn page_count(bytes: &[u8]) -> Result<u32, PdfError> {
    let (_, pages, _) = inspect(bytes)?;
    Ok(pages)
}

fn allocate(document: &mut Document, object: Object) -> ObjectId {
    document.max_id = document.max_id.saturating_add(1);
    let id = (document.max_id, 0);
    document.objects.insert(id, object);
    id
}

/// Copy one object, remapping every reference into the destination document.
fn copy_object(
    source: &Document,
    object: &Object,
    map: &mut BTreeMap<ObjectId, ObjectId>,
    destination: &mut Document,
    depth: usize,
) -> Result<Object, PdfError> {
    if depth > MAX_OBJECT_DEPTH {
        return Err(PdfError::TooDeep {
            limit: MAX_OBJECT_DEPTH,
        });
    }
    match object {
        Object::Reference(id) => {
            if let Some(copied) = map.get(id) {
                return Ok(Object::Reference(*copied));
            }
            let target = match source.get_object(*id) {
                Ok(target) => target.clone(),
                Err(_) => return Ok(Object::Null),
            };
            let copied_id = allocate(destination, Object::Null);
            map.insert(*id, copied_id);
            let copied = copy_object(source, &target, map, destination, depth + 1)?;
            destination.objects.insert(copied_id, copied);
            Ok(Object::Reference(copied_id))
        }
        Object::Array(items) => Ok(Object::Array(
            items
                .iter()
                .map(|item| copy_object(source, item, map, destination, depth + 1))
                .collect::<Result<_, _>>()?,
        )),
        Object::Dictionary(dictionary) => {
            let mut copied = Dictionary::new();
            for (key, value) in dictionary.iter() {
                copied.set(
                    key.clone(),
                    copy_object(source, value, map, destination, depth + 1)?,
                );
            }
            Ok(Object::Dictionary(copied))
        }
        Object::Stream(stream) => {
            let mut dictionary = Dictionary::new();
            for (key, value) in stream.dict.iter() {
                dictionary.set(
                    key.clone(),
                    copy_object(source, value, map, destination, depth + 1)?,
                );
            }
            Ok(Object::Stream(Stream::new(
                dictionary,
                stream.content.clone(),
            )))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use lopdf::{Object, Stream};

    use super::*;

    /// A one-page PDF with one content stream and one embedded-subset font.
    fn one_page(label: &str) -> Bytes {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let content = format!("BT /F1 12 Tf 72 770 Td ({label}) Tj ET");
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);
        page.set("Contents", Object::Reference((4, 0)));
        let mut resources = Dictionary::new();
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference((5, 0)));
        resources.set("Font", Object::Dictionary(fonts));
        page.set("Resources", Object::Dictionary(resources));
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Fake");
        document.add_object(font);
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        Bytes::from(bytes)
    }

    #[test]
    fn concatenates_pages_in_input_order() {
        let first = one_page("first");
        let second = one_page("second");
        let (merged, pages) = concat(&[first, second]).unwrap();
        assert_eq!(pages, 2);
        let document = Document::load_mem(&merged).unwrap();
        assert_eq!(document.get_pages().len(), 2);
        let text = document.extract_text(&[1, 2]).unwrap();
        assert!(text.contains("first"), "{text}");
        assert!(text.contains("second"), "{text}");
    }

    #[test]
    fn a_batch_title_is_stamped_on_the_merged_document() {
        let merged = concat_with_title(
            &[one_page("first"), one_page("second")],
            Some("Batch invoice"),
        )
        .unwrap()
        .0;
        let document = Document::load_mem(&merged).unwrap();
        let info_id = document
            .trailer
            .get(b"Info".as_slice())
            .unwrap()
            .as_reference()
            .unwrap();
        let info = document.get_dictionary(info_id).unwrap();
        let title = info.get(b"Title".as_slice()).unwrap().as_str().unwrap();
        assert_eq!(&title[..2], &[0xFE, 0xFF]);
        let decoded: String = title[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .map(|unit| char::from_u32(u32::from(unit)).unwrap_or('?'))
            .collect();
        assert_eq!(decoded, "Batch invoice");
    }

    #[test]
    fn a_single_input_is_not_rebuilt() {
        let original = one_page("solo");
        let (merged, pages) = concat(std::slice::from_ref(&original)).unwrap();
        assert_eq!(pages, 1);
        assert_eq!(merged, original);
    }

    #[test]
    fn an_encrypted_input_is_refused() {
        let mut document = Document::with_version("1.4");
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut encrypt = Dictionary::new();
        encrypt.set("Filter", "Standard");
        let encrypted_id = document.add_object(encrypt);
        document
            .trailer
            .set("Encrypt", Object::Reference(encrypted_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        assert!(matches!(
            concat(&[Bytes::from(bytes.clone()), one_page("x")]),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(page_count(&bytes), Err(PdfError::Encrypted)));
    }

    /// A one-page PDF whose `/Resources` and `/MediaBox` live on the page
    /// tree node, as producers that share one resource dictionary do.
    fn one_page_with_inherited_attributes(label: &str) -> Bytes {
        let mut document = Document::with_version("1.4");
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", Object::Reference((2, 0)));
        let mut resources = Dictionary::new();
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference((5, 0)));
        resources.set("Font", Object::Dictionary(fonts));
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference((3, 0))]);
        pages.set("Count", 1);
        pages.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);
        pages.set("Resources", Object::Dictionary(resources));
        let content = format!("BT /F1 12 Tf 72 770 Td ({label}) Tj ET");
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", Object::Reference((2, 0)));
        page.set("Contents", Object::Reference((4, 0)));
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(Stream::new(Dictionary::new(), content.into_bytes()));
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "AAAAAA+Inherited");
        document.add_object(font);
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        Bytes::from(bytes)
    }

    fn nested_array(depth: usize) -> Object {
        if depth == 0 {
            Object::Integer(0)
        } else {
            Object::Array(vec![nested_array(depth - 1)])
        }
    }

    fn one_page_with_nested_object(depth: usize) -> Bytes {
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
        page.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);
        page.set("Contents", Object::Reference((4, 0)));
        page.set("Deep", nested_array(depth));
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document.add_object(Stream::new(Dictionary::new(), Vec::new()));
        document.trailer.set("Root", Object::Reference((1, 0)));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        Bytes::from(bytes)
    }

    #[test]
    fn a_deeply_nested_object_is_refused_instead_of_overflowing_the_stack() {
        let error = concat(&[one_page_with_nested_object(80), one_page("second")]).unwrap_err();
        assert!(
            matches!(error, PdfError::TooDeep { limit } if limit == MAX_OBJECT_DEPTH),
            "{error:?}"
        );
    }

    #[test]
    fn inherited_page_attributes_survive_concatenation() {
        let merged = concat(&[
            one_page_with_inherited_attributes("first"),
            one_page_with_inherited_attributes("second"),
        ])
        .unwrap()
        .0;
        let document = Document::load_mem(&merged).unwrap();
        let pages = document.get_pages();
        assert_eq!(pages.len(), 2);
        for (number, page_id) in &pages {
            let page = document.get_dictionary(*page_id).unwrap();
            assert!(
                page.has(b"MediaBox"),
                "page {number} lost its inherited /MediaBox"
            );
            assert!(
                page.has(b"Resources"),
                "page {number} lost its inherited /Resources"
            );
            let fonts = page
                .get(b"Resources")
                .and_then(Object::as_dict)
                .and_then(|resources| resources.get(b"Font"))
                .and_then(Object::as_dict)
                .unwrap();
            let font_id = fonts.get(b"F1").and_then(Object::as_reference).unwrap();
            let font = document.get_dictionary(font_id).unwrap();
            assert_eq!(
                font.get(b"BaseFont").and_then(Object::as_name).unwrap(),
                b"AAAAAA+Inherited"
            );
        }
    }
}
