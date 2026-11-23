//! Preflight for embedded PDF items.
//!
//! Moved out of `printkit-core` so the document model stays a leaf crate; the
//! engine runs this before rendering, where the PDF dependency already lives.
//!
//! The document layer must reject a PDF item before the renderer runs: an
//! encrypted PDF, a page number past the end of the file, a payload that is
//! not a PDF, or an object the merge cannot copy is a problem with the
//! document, not a renderer failure. Parsing happens in memory; this crate
//! still performs no IO.
//!
//! The page tree, the trailer and every reference reachable from the page's
//! content and resources are inspected. Fonts and images are copied later by
//! `printkit-pdf`, which also decodes content streams; this module only proves
//! the object graph is complete.

use std::collections::{BTreeMap, BTreeSet};

use lopdf::{Document, Error as LopdfError, Object, ObjectId};

/// How far the preflight follows references before calling the graph corrupt.
const MAX_GRAPH_DEPTH: usize = 64;

/// Why an embedded PDF cannot be used for the requested page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedProblem {
    /// The trailer carries an `/Encrypt` dictionary.
    Encrypted,
    /// The requested page is past the end of the document.
    PageOutOfRange {
        /// One-based page requested by the item.
        page: u32,
        /// Number of pages in the payload.
        pages: u32,
    },
    /// The payload does not start with a PDF header.
    NotPdf,
    /// A reference reachable from the page does not resolve.
    Unsupported {
        /// Object id inside the embedded PDF that the walker could not follow.
        object: ObjectId,
        /// Why the object cannot be used: an unresolvable reference, a wrong
        /// object kind, or a cycle in the page tree.
        reason: String,
    },
    /// The reference graph is deeper than the walker follows.
    TooDeep {
        /// Maximum reference depth the walker follows.
        limit: usize,
    },
    /// The file cannot be parsed, or has no page tree.
    Malformed(String),
}

/// An embedded PDF parsed once per data URI, with its page tree resolved.
///
/// Several items may point at the same payload; holding the parsed document
/// lets each item check its own page without re-parsing or inheriting the
/// previous item's verdict.
#[derive(Debug)]
pub struct EmbeddedPdf {
    document: Document,
    pages: BTreeMap<u32, ObjectId>,
}

impl EmbeddedPdf {
    /// Parse `bytes` and prove it is an unencrypted PDF with a page tree.
    pub fn parse(bytes: &[u8]) -> Result<Self, EmbeddedProblem> {
        if !bytes.starts_with(b"%PDF-") {
            return Err(EmbeddedProblem::NotPdf);
        }
        let document = Document::load_mem(bytes).map_err(load_error)?;
        // An inline `/Encrypt` dictionary is as binding as a reference; both
        // mean the payload's strings and streams are encrypted.
        if document.trailer.get(b"Encrypt").is_ok() {
            return Err(EmbeddedProblem::Encrypted);
        }
        let pages = document.get_pages();
        if pages.is_empty() {
            return Err(EmbeddedProblem::Malformed(
                "the embedded PDF has no page tree".to_owned(),
            ));
        }
        Ok(Self { document, pages })
    }

    /// Check that `page` exists and that everything the merge copies from it
    /// resolves. Returns the page count.
    pub fn check_page(&self, page: u32) -> Result<u32, EmbeddedProblem> {
        let pages = u32::try_from(self.pages.len()).unwrap_or(u32::MAX);
        let page_id = self
            .pages
            .get(&page)
            .copied()
            .ok_or(EmbeddedProblem::PageOutOfRange { page, pages })?;
        check_page_graph(&self.document, page_id)?;
        Ok(pages)
    }
}

/// Walk the references the merge will copy: `/Contents` and the `/Resources`
/// of the page and of every ancestor.
///
/// A page tree may split resource categories across levels, so each level's
/// dictionary is a root of its own. `/Parent` is skipped inside [`walk`]
/// because every page points back into the tree, and following it there would
/// inspect the whole document instead of one page.
fn check_page_graph(document: &Document, page_id: ObjectId) -> Result<(), EmbeddedProblem> {
    let mut roots = Vec::new();
    let mut seen_nodes = BTreeSet::from([page_id]);
    let mut node_id = page_id;
    loop {
        let node =
            document
                .get_dictionary(node_id)
                .map_err(|error| EmbeddedProblem::Unsupported {
                    object: node_id,
                    reason: error.to_string(),
                })?;
        if let Ok(contents) = node.get(b"Contents") {
            roots.push(contents.clone());
        }
        if let Ok(resources) = node.get(b"Resources") {
            roots.push(resources.clone());
        }
        let parent = node.get(b"Parent").and_then(Object::as_reference);
        match parent {
            Ok(parent) if seen_nodes.insert(parent) => node_id = parent,
            Ok(parent) => {
                return Err(EmbeddedProblem::Unsupported {
                    object: parent,
                    reason: "reference cycle in the page tree".to_owned(),
                });
            }
            Err(_) => break,
        }
    }

    let mut seen = BTreeSet::new();
    for root in &roots {
        walk(document, root, 0, &mut seen)?;
    }
    Ok(())
}

fn walk(
    document: &Document,
    object: &Object,
    depth: usize,
    seen: &mut BTreeSet<ObjectId>,
) -> Result<(), EmbeddedProblem> {
    if depth > MAX_GRAPH_DEPTH {
        return Err(EmbeddedProblem::TooDeep {
            limit: MAX_GRAPH_DEPTH,
        });
    }
    match object {
        Object::Reference(id) => {
            if !seen.insert(*id) {
                return Ok(());
            }
            let target =
                document
                    .get_object(*id)
                    .map_err(|error| EmbeddedProblem::Unsupported {
                        object: *id,
                        reason: error.to_string(),
                    })?;
            walk(document, target, depth + 1, seen)
        }
        Object::Array(items) => {
            for item in items {
                walk(document, item, depth + 1, seen)?;
            }
            Ok(())
        }
        Object::Dictionary(dictionary) => {
            for (key, value) in dictionary.iter() {
                if key == b"Parent" {
                    continue;
                }
                walk(document, value, depth + 1, seen)?;
            }
            Ok(())
        }
        Object::Stream(stream) => walk(
            document,
            &Object::Dictionary(stream.dict.clone()),
            depth + 1,
            seen,
        ),
        _ => Ok(()),
    }
}

/// An encrypted file can fail while loading instead of exposing its trailer;
/// both shapes mean the caller supplied a password-protected PDF.
fn load_error(error: LopdfError) -> EmbeddedProblem {
    match error {
        LopdfError::Decryption(_) | LopdfError::InvalidPassword => EmbeddedProblem::Encrypted,
        LopdfError::AlreadyEncrypted => EmbeddedProblem::Encrypted,
        other => EmbeddedProblem::Malformed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_page_document() -> Document {
        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", lopdf::Object::Reference((2, 0)));
        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![lopdf::Object::Reference((3, 0))]);
        pages.set("Count", 1);
        let mut page = lopdf::Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", lopdf::Object::Reference((2, 0)));
        page.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);

        let mut document = Document::with_version("1.4");
        document.add_object(catalog);
        document.add_object(pages);
        document.add_object(page);
        document
            .trailer
            .set("Root", lopdf::Object::Reference((1, 0)));
        document
    }

    fn saved(document: &mut Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("serialise");
        bytes
    }

    #[test]
    fn accepts_page_one_of_a_single_page_pdf() {
        let pdf = EmbeddedPdf::parse(&saved(&mut one_page_document())).expect("parse");
        assert_eq!(pdf.check_page(1).unwrap(), 1);
    }

    #[test]
    fn rejects_a_page_past_the_end() {
        let pdf = EmbeddedPdf::parse(&saved(&mut one_page_document())).expect("parse");
        assert_eq!(
            pdf.check_page(2),
            Err(EmbeddedProblem::PageOutOfRange { page: 2, pages: 1 })
        );
    }

    #[test]
    fn rejects_a_payload_without_a_pdf_header() {
        assert_eq!(
            EmbeddedPdf::parse(b"data:application/pdf;base64,AA==").unwrap_err(),
            EmbeddedProblem::NotPdf
        );
    }

    #[test]
    fn rejects_a_document_without_pages() {
        let error = EmbeddedPdf::parse(b"%PDF-1.4\n%%EOF").unwrap_err();
        assert!(matches!(error, EmbeddedProblem::Malformed(_)));
    }

    #[test]
    fn rejects_an_inline_encrypt_dictionary() {
        let mut document = one_page_document();
        let mut encrypt = lopdf::Dictionary::new();
        encrypt.set("Filter", "Standard");
        document.trailer.set("Encrypt", Object::Dictionary(encrypt));
        assert_eq!(
            EmbeddedPdf::parse(&saved(&mut document)).unwrap_err(),
            EmbeddedProblem::Encrypted
        );
    }

    fn with_page_resources(resources: Object) -> Document {
        let mut document = one_page_document();
        document
            .get_dictionary_mut((3, 0))
            .unwrap()
            .set("Resources", resources);
        document
    }

    #[test]
    fn rejects_a_resource_graph_deeper_than_the_walk_limit() {
        let mut deep = Object::Array(Vec::new());
        for _ in 0..=MAX_GRAPH_DEPTH {
            deep = Object::Array(vec![deep]);
        }
        let mut resources = lopdf::Dictionary::new();
        resources.set("Deep", deep);
        let mut document = with_page_resources(Object::Dictionary(resources));
        let pdf = EmbeddedPdf::parse(&saved(&mut document)).expect("parse");
        assert_eq!(
            pdf.check_page(1),
            Err(EmbeddedProblem::TooDeep {
                limit: MAX_GRAPH_DEPTH
            })
        );
    }

    #[test]
    fn checks_resources_inherited_from_every_page_tree_level() {
        let mut page_resources = lopdf::Dictionary::new();
        page_resources.set("XObject", Object::Dictionary(lopdf::Dictionary::new()));
        let mut document = with_page_resources(Object::Dictionary(page_resources));
        let mut fonts = lopdf::Dictionary::new();
        fonts.set("F1", Object::Reference((99, 0)));
        let mut inherited = lopdf::Dictionary::new();
        inherited.set("Font", Object::Dictionary(fonts));
        document
            .get_dictionary_mut((2, 0))
            .unwrap()
            .set("Resources", Object::Dictionary(inherited));
        let pdf = EmbeddedPdf::parse(&saved(&mut document)).expect("parse");
        assert!(matches!(
            pdf.check_page(1),
            Err(EmbeddedProblem::Unsupported {
                object: (99, 0),
                ..
            })
        ));
    }
}
