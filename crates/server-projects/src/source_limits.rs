//! core source-limits.cjs (sbstndalton/noevia#2191 round 8): the project source limits in one place.
//!
//! - `PROJECT_SOURCE_LIMIT`, `PROJECT_DOCUMENT_LIMIT` and `PROJECT_IMAGE_LIMIT` (positive safe
//!   integers; anything else is ignored), defaults from contracts/folder-import.json `projectLimits`
//!   (1000 / 200 / 12); the document limit never exceeds the source limit.
//! - [`kind_of`], [`counts`] and [`refusal`]: which of the three pools a source belongs to, and
//!   whether one more fits.
//!
//! The per-document limits (`PROJECT_DOCUMENT_READ_BYTES`, the page and text limits, the sync's OCR
//! page allowance) are [`crate::extract::Limits`]; [`sync_ocr_pages`] reads the one it lacks.
use js_json::JValue;

fn positive(raw: Option<String>) -> Option<usize> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    // `Number(raw)` then `Number.isSafeInteger(n) && n > 0`.
    let n = crate::document_pages::string_to_number(&raw);
    (n.is_finite() && n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0)
        .then_some(n as usize)
}

/// `limits(env)` with the process environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub sources: usize,
    pub documents: usize,
    pub images: usize,
}

pub fn limits() -> Limits {
    limits_from(|k| std::env::var(k).ok())
}

pub fn limits_from(env: impl Fn(&str) -> Option<String>) -> Limits {
    let d = crate::folder_import::contract().project_limits;
    let sources = positive(env("PROJECT_SOURCE_LIMIT")).unwrap_or(d.0);
    Limits {
        sources,
        documents: sources.min(positive(env("PROJECT_DOCUMENT_LIMIT")).unwrap_or(d.1)),
        images: positive(env("PROJECT_IMAGE_LIMIT")).unwrap_or(d.2),
    }
}

/// `documentLimits().syncOcrPages` (`PROJECT_SYNC_OCR_PAGES`, default 40): the OCR retry pages one
/// source sync may spend across all its documents.
pub fn sync_ocr_pages() -> usize {
    positive(std::env::var("PROJECT_SYNC_OCR_PAGES").ok()).unwrap_or(40)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Image,
    Document,
    Text,
}

fn extension_matches(name: &str, list: &[&str]) -> bool {
    let lower = name.to_lowercase();
    list.iter().any(|e| {
        lower
            .strip_suffix(e)
            .is_some_and(|rest| rest.ends_with('.'))
    })
}

const DOCUMENT: [&str; 12] = [
    "pdf", "doc", "docx", "odt", "rtf", "ppt", "pptx", "xls", "xlsx", "ods", "odp", "epub",
];
const IMAGE: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

/// `kindOf(name)` for a bare name.
pub fn kind_of_name(name: &str) -> Kind {
    if extension_matches(name, &IMAGE) {
        Kind::Image
    } else if extension_matches(name, &DOCUMENT) {
        Kind::Document
    } else {
        Kind::Text
    }
}

/// `kindOf(file)` for a source record: its attachment group wins, then its document field, then
/// the name.
pub fn kind_of(file: &JValue) -> Kind {
    let group = file.get("attachment").get("group");
    let group = group.as_str().filter(|g| !g.is_empty());
    let name = file.get("name").as_str().unwrap_or("");
    if group == Some("Images")
        || file.get("attachment").get("assetId").truthy()
        || (group.is_none() && extension_matches(name, &IMAGE))
    {
        return Kind::Image;
    }
    if group == Some("Documents")
        || file.get("document").truthy()
        || (group.is_none() && extension_matches(name, &DOCUMENT))
    {
        return Kind::Document;
    }
    Kind::Text
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub sources: usize,
    pub documents: usize,
    pub images: usize,
}

pub fn counts(files: &[JValue]) -> Counts {
    let mut c = Counts::default();
    for f in files {
        c.sources += 1;
        match kind_of(f) {
            Kind::Document => c.documents += 1,
            Kind::Image => c.images += 1,
            Kind::Text => {}
        }
    }
    c
}

/// A cap one more source would pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
    pub max: usize,
}

/// `refusal(files, name, { imagesHeld })`.
pub fn refusal(files: &[JValue], name: &str, images_held: Option<usize>) -> Option<Refusal> {
    let l = limits();
    let c = counts(files);
    let k = kind_of_name(name);
    if c.sources >= l.sources {
        return Some(Refusal {
            code: "projectLimit",
            message: format!("A project holds at most {} sources.", l.sources),
            max: l.sources,
        });
    }
    if k == Kind::Document && c.documents >= l.documents {
        return Some(Refusal {
            code: "documentLimit",
            message: format!(
                "A project holds at most {} documents (PDF, DOCX and similar).",
                l.documents
            ),
            max: l.documents,
        });
    }
    if k == Kind::Image && images_held.unwrap_or(c.images) >= l.images {
        return Some(Refusal {
            code: "imageLimit",
            message: format!("A project holds at most {} vision images.", l.images),
            max: l.images,
        });
    }
    None
}

/// Node's `path.extname(name)`: from the last dot of the last segment, none for a leading dot alone.
pub fn extname(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    if base == ".." {
        return String::new();
    }
    match base.rfind('.') {
        Some(i) if i > 0 => base.get(i..).unwrap_or("").to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn file(name: &str, group: Option<&str>) -> JValue {
        let mut items = vec![("name".to_string(), JValue::Str(name.into()))];
        if let Some(g) = group {
            items.push((
                "attachment".into(),
                JValue::obj([("group", JValue::Str(g.into()))]),
            ));
        }
        JValue::Obj(items)
    }

    #[test]
    fn kinds_follow_the_group_then_the_name() {
        assert_eq!(kind_of(&file("a.PNG", None)), Kind::Image);
        assert_eq!(kind_of(&file("a.docx", None)), Kind::Document);
        assert_eq!(kind_of(&file("a.txt", None)), Kind::Text);
        // A group wins over the name.
        assert_eq!(kind_of(&file("a.png", Some("Documents"))), Kind::Document);
        assert_eq!(kind_of(&file("a.pdf", Some("Text"))), Kind::Text);
        assert_eq!(kind_of_name("scan.JPEG"), Kind::Image);
        assert_eq!(kind_of_name("pdf"), Kind::Text, "no dot, no extension");
        let all = [file("a.pdf", None), file("b.png", None), file("c.md", None)];
        assert_eq!(
            counts(&all),
            Counts {
                sources: 3,
                documents: 1,
                images: 1
            }
        );
    }

    #[test]
    fn limits_read_the_environment_as_core_does() {
        let env = |k: &str| match k {
            "PROJECT_SOURCE_LIMIT" => Some("5".to_string()),
            "PROJECT_DOCUMENT_LIMIT" => Some("9".to_string()),
            "PROJECT_IMAGE_LIMIT" => Some("0".to_string()),
            _ => None,
        };
        let l = limits_from(env);
        // The document limit never passes the source limit; a non-positive image limit is ignored.
        assert_eq!(
            l,
            Limits {
                sources: 5,
                documents: 5,
                images: 12
            }
        );
        assert_eq!(
            limits_from(|_| None),
            Limits {
                sources: 1000,
                documents: 200,
                images: 12
            }
        );
    }

    #[test]
    fn extname_is_nodes() {
        assert_eq!(extname("a.b.TXT"), ".TXT");
        assert_eq!(extname(".gitignore"), "");
        assert_eq!(extname("dir.d/file"), "");
        assert_eq!(extname("name."), ".");
    }
}
