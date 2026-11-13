//! `filesContext` without an index (`rag.cjs:1264-1335`): the manifest, the source-completeness
//! notice and the whole-file budget. The caller runs retrieval and passes the excerpts it found
//! (R4-4); document notices are computed by the caller too (`document-sources.cjs`, not ported).

use prompt_framing::framing::frame_untrusted;
use prompt_framing::js::units;

use crate::hits::Hit;
use crate::names::{question_names, quote_name, quoted_names};

/// `DIRECT_INJECT_MAX` (`rag.cjs:98`), in UTF-16 code units.
pub const DIRECT_INJECT_MAX: usize = 2400;
/// `LARGE_FILE_HEAD` (`rag.cjs:103`), in UTF-16 code units.
pub const LARGE_FILE_HEAD: usize = 24_000;
/// The default of `FILES_CONTEXT_MAX_CHARS` (`rag.cjs:102`).
pub const DEFAULT_MAX_CHARS: usize = 120_000;
/// `READ_FILE_HINT` (`rag.cjs:1348`), leading space included.
pub const READ_FILE_HINT: &str =
    " Use read_project_file with PDF startPage/endPage and offset for pages beyond the summary.";

/// One attached file. `content` is the whole text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileIn {
    pub name: String,
    pub content: String,
    /// `documentNotice(f)`; `None` or empty means no notice.
    pub notice: Option<String>,
    /// `f.attachment.reason`; `None` or empty means no reason.
    pub attachment_reason: Option<String>,
}

/// A source reported to the caller: what the prompt holds for a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub file: String,
    pub body: String,
    pub score: Option<f64>,
    /// `"excerpt"` or `"file"`.
    pub kind: &'static str,
}

/// The assembled context. `text` is `None` only when there are no files.
#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    pub text: String,
    /// What the reply reports as its sources.
    pub placed: Vec<Source>,
    /// Everything the prompt holds.
    pub injected: Vec<Source>,
}

/// The inputs of one `filesContext` call.
pub struct Input<'a> {
    pub query: &'a str,
    pub files: &'a [FileIn],
    /// Excerpts already retrieved for this question (empty when there is no index).
    pub excerpts: &'a [Hit],
    pub attached: bool,
    /// `FILES_CONTEXT_MAX_CHARS` after its clamp.
    pub max_chars: usize,
}

fn len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The first `n` code units. Known difference: a cut inside a surrogate pair gives U+FFFD
/// (one unit, as the JS slice has one) where JS keeps the lone surrogate.
fn head(s: &str, n: usize) -> String {
    let u = units(s);
    String::from_utf16_lossy(u.get(..n.min(u.len())).unwrap_or(&[]))
}

/// `frameUntrusted(kind, label, text)` from the Rust port.
fn frame(kind: &str, label: &str, text: &str) -> String {
    String::from_utf16_lossy(&frame_untrusted(&units(kind), &units(label), &units(text)))
}

struct Candidate {
    file: String,
    body: String,
    text: String,
}

/// `filesContext`; `None` for no files.
pub fn files_context(input: &Input) -> Option<Context> {
    let files = input.files;
    if files.is_empty() {
        return None;
    }
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    let lead = if input.attached {
        "The user attached these files to this chat"
    } else {
        "Sources attached to this project"
    };
    let mut manifest = format!(
        "{lead} ({}): {}. Excerpts of the relevant ones follow; ask to read a file in full if you need more of it.",
        files.len(),
        quoted_names(&names)
    );

    let notices: Vec<String> = files
        .iter()
        .filter_map(|f| {
            f.notice.clone().filter(|s| !s.is_empty()).or_else(|| {
                f.attachment_reason
                    .as_ref()
                    .filter(|r| !r.is_empty())
                    .map(|r| format!("{}: {r}", quote_name(&f.name)))
            })
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    let mut placed: Vec<Source> = Vec::new();
    let mut injected: Vec<Source> = Vec::new();
    for h in input
        .excerpts
        .iter()
        .filter(|h| names.contains(&h.file.as_str()))
    {
        parts.push(frame("excerpt", &h.file, &h.body));
        let source = Source {
            file: h.file.clone(),
            body: h.body.clone(),
            score: Some(h.score),
            kind: "excerpt",
        };
        placed.push(source.clone());
        injected.push(source);
    }

    let used: usize = parts.iter().map(|p| len(p)).sum();
    let mut budget = i64::try_from(input.max_chars).unwrap_or(i64::MAX)
        - i64::try_from(used).unwrap_or(i64::MAX);

    let mut candidates: Vec<Candidate> = files
        .iter()
        .filter(|f| len(&f.content) <= DIRECT_INJECT_MAX)
        .map(|f| Candidate {
            file: f.name.clone(),
            body: f.content.clone(),
            text: frame("file", &f.name, &f.content),
        })
        .collect();
    if parts.is_empty() {
        for f in files.iter().filter(|f| len(&f.content) > DIRECT_INJECT_MAX) {
            let body = head(&f.content, LARGE_FILE_HEAD);
            candidates.push(Candidate {
                file: f.name.clone(),
                text: frame("file excerpts", &f.name, &body),
                body,
            });
        }
    }
    // Stable, so equal lengths keep their order, as Array.prototype.sort does.
    candidates.sort_by_key(|c| len(&c.text));

    let mut omitted: Vec<String> = Vec::new();
    for c in candidates {
        let n = i64::try_from(len(&c.text)).unwrap_or(i64::MAX);
        if n <= budget {
            budget -= n;
            let source = Source {
                file: c.file.clone(),
                body: c.body,
                score: None,
                kind: "file",
            };
            if question_names(input.query, &c.file) {
                placed.push(source.clone());
            }
            injected.push(source);
            parts.push(c.text);
        } else {
            omitted.push(c.file);
        }
    }
    if !omitted.is_empty() {
        let refs: Vec<&str> = omitted.iter().map(String::as_str).collect();
        manifest.push_str(&format!(
            " {} more file{} not included (prompt size limit): {}.",
            omitted.len(),
            if omitted.len() == 1 { "" } else { "s" },
            quoted_names(&refs)
        ));
    }

    let completeness = if notices.is_empty() {
        "Legacy text sources have no page completeness metadata.".to_owned()
    } else {
        notices.join("\n")
    };
    let coverage = format!(
        "Source completeness: {completeness}\nContext may contain excerpts only.{READ_FILE_HINT} Do not treat missing excerpts or failed/partial sources as evidence of absence."
    );

    let text = if parts.is_empty() {
        format!("{manifest}\n{coverage}")
    } else {
        let mut blocks = vec![manifest, coverage];
        blocks.extend(parts);
        blocks.join("\n\n")
    };
    Some(Context {
        text,
        placed,
        injected,
    })
}

#[cfg(test)]
mod tests {
    use super::head;

    #[test]
    fn a_head_cut_inside_a_surrogate_pair_is_the_named_difference() {
        // JS `slice(0, n)` keeps the lone high surrogate; Rust strings cannot hold one, so the
        // cut unit becomes U+FFFD. Same length in UTF-16 units, so the budget arithmetic agrees.
        let text = format!("{}😀tail", "a".repeat(3));
        let cut = head(&text, 4);
        assert_eq!(cut, "aaa\u{fffd}");
        assert_eq!(cut.encode_utf16().count(), 4);
        assert_eq!(head(&text, 5), "aaa😀");
        assert_eq!(head("short", 24_000), "short");
    }
}
