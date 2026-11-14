#![allow(clippy::indexing_slicing, clippy::expect_used, clippy::unwrap_used)]

use super::*;

fn near(line: &str) -> Option<Vec<&'static str>> {
    methods_near(line).map(|v| {
        v.iter()
            .map(|m| *METHODS.iter().find(|k| **k == m.as_str()).unwrap())
            .collect()
    })
}

#[test]
fn a_strict_or_loose_comparison_on_either_side_names_the_method() {
    for (line, want) in [
        ("if (req.method === 'PUT' && p === '/api/x') {", vec!["PUT"]),
        ("if (method == \"GET\") return", vec!["GET"]),
        ("if (method==='DELETE')", vec!["DELETE"]),
        ("if ('POST' === req.method && p === '/api/x')", vec!["POST"]),
        ("if (\"PATCH\"==method)", vec!["PATCH"]),
        // Mixed quotes are allowed, as in the regexes (`['"]` on each side).
        ("if (req.method === 'PUT\")", vec!["PUT"]),
        // Several comparisons on one line: sorted, once each.
        (
            "if ((req.method === 'PUT' || req.method === 'POST') && p === '/api/x')",
            vec!["POST", "PUT"],
        ),
        (
            "if (req.method === 'GET' || 'GET' === req.method)",
            vec!["GET"],
        ),
    ] {
        assert_eq!(near(line), Some(want), "{line}");
    }
}

#[test]
fn an_includes_list_names_every_method_in_it() {
    assert_eq!(
        near("if (['GET', 'HEAD'].includes(req.method) && p === '/api/x')"),
        Some(vec!["GET", "HEAD"])
    );
    assert_eq!(
        near("if ([\"PUT\",\"PATCH\"].includes(method))"),
        Some(vec!["PATCH", "PUT"])
    );
    // Not the request method, or not `.includes(` right after the bracket: nothing.
    assert_eq!(near("if (['GET'].includes(verb))"), None);
    assert_eq!(near("if (['GET'] .includes(method))"), None);
    // `[^\]]*` stops at the first `]`; an earlier `[` is part of the list text.
    assert_eq!(near("x[a, ['PUT'].includes(req.method)"), Some(vec!["PUT"]));
}

#[test]
fn a_negation_anywhere_on_the_line_names_nothing() {
    for line in [
        "if (req.method !== 'PUT') return false; if (req.method === 'GET') x();",
        "if (method != \"POST\")",
        "if ('GET' !== req.method)",
        "if (\"HEAD\"!=method)",
        "if (['GET'].includes(req.method) && req.method !== 'HEAD')",
        // Negation 3 (council p12): a negated list tests the other methods.
        "if (!['GET', 'POST', 'PUT'].includes(req.method)) return false;",
        "if (! [\"GET\"].includes(method)) x();",
        // routes/models.cjs:704 at core 538df471: POST-only protection, then a negated list.
        "const protectedAction = req.method === 'POST' && ['/api/models/prune-missing', '/api/models/delete'].includes(ctx.path) || ctx.path.startsWith('/api/model-manager/') && !['GET', 'HEAD', 'OPTIONS'].includes(req.method);",
    ] {
        assert_eq!(near(line), None, "{line}");
    }
}

#[test]
fn no_comparison_or_an_unknown_method_names_nothing() {
    for line in [
        "if (p === '/api/x') return handle(req, res);",
        "const method = req.method;",
        // Not upper case, or not a probed method.
        "if (req.method === 'get')",
        "if (req.method === 'PROPFIND')",
        "if (['MKCOL', 'COPY'].includes(req.method))",
        // `\b` after `method` on the right-hand side: `methods` is another word.
        "if ('GET' === req.methods)",
        // A space inside `===` or between the quote and the name breaks the pattern.
        "if (req.method = == 'GET')",
        "if (req.method === ' GET')",
        "",
    ] {
        assert_eq!(near(line), None, "{line}");
    }
}

#[test]
fn an_unknown_method_beside_a_known_one_keeps_the_known_one() {
    assert_eq!(
        near("if (['PROPFIND', 'GET'].includes(req.method))"),
        Some(vec!["GET"])
    );
}

#[test]
fn names_on_the_left_need_the_bare_word_or_req_dot() {
    // `(?:req\.)?method` right after the comparison: `ctx.method` there is neither, so nothing on the right; on the
    // left the bare `method` inside `ctx.method` matches (named 1 has no `\b` before it).
    assert_eq!(near("if ('PUT' === ctx.method)"), None);
    assert_eq!(near("if (ctx.method === 'PUT')"), Some(vec!["PUT"]));
    // The left-hand form has no `\b` before `method`: `xmethod === 'GET'` names GET, as in the recorder.
    assert_eq!(near("if (xmethod === 'GET')"), Some(vec!["GET"]));
}

#[test]
fn non_ascii_text_around_a_comparison_is_skipped_safely() {
    assert_eq!(
        near("// é if (req.method === 'PUT') — ü"),
        Some(vec!["PUT"])
    );
    assert_eq!(near("'é' === req.method"), None);
}
