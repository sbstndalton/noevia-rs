//! noevia#1212: nested `find -exec find -exec …` chains answer in linear time, the same answer as
//! code-actions.cjs after noevia#1201 (more than 64 `-exec`s: every class but none/read, never
//! standing), and a reading-work refusal comes back in well under 10 ms.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use code_actions::call;

/// `find .`, then `n` times `-exec find .`, then `-print`, then `n` terminators (the #1212 probe).
fn chain(n: usize) -> String {
    format!(
        "find .{} -print{}",
        " -exec find .".repeat(n),
        r" \;".repeat(n)
    )
}

fn classify(command: &str) -> (u32, String, Duration) {
    let wire = format!(
        "[{{\"kind\":\"execute\",\"rawInput\":{{\"command\":{},\"noeviaOutsideWorkspace\":false}},\"locations\":[]}}]",
        quote(command)
    );
    let mut input = vec![1u8];
    input.extend(wire.as_bytes());
    let t = Instant::now();
    let (status, reply) = call(&input);
    (status, String::from_utf8(reply).unwrap(), t.elapsed())
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Best of a few runs, so a busy CI host does not make the bound flaky.
fn fastest(command: &str) -> (u32, String, Duration) {
    let mut best = classify(command);
    for _ in 0..4 {
        let r = classify(command);
        if r.2 < best.2 {
            best = r;
        }
    }
    best
}

const EVERY: &str = r#""actions":["external_account","git_push","delete","open_browser","install_dependency","execute_command","network","edit_file"]"#;

#[test]
fn deep_chains_answer_like_the_js() {
    for n in [40, 64] {
        let (status, reply, _) = fastest(&chain(n));
        assert_eq!(status, 0, "{n}: {reply}");
        assert!(
            reply.contains(r#""actions":["execute_command"]"#),
            "{n}: {reply}"
        );
        assert!(reply.contains(r#""standable":false"#), "{n}: {reply}");
    }
    for n in [65, 1000] {
        let (status, reply, _) = fastest(&chain(n));
        assert_eq!(status, 0, "{n}: {reply}");
        assert!(reply.contains(EVERY), "{n}: {reply}");
        assert!(reply.contains(r#""standable":false"#), "{n}: {reply}");
    }
}

#[test]
fn a_1000_level_chain_answers_in_under_10_ms() {
    let (status, reply, took) = fastest(&chain(1000));
    assert_eq!(status, 0, "{reply}");
    eprintln!("1000 levels: {took:?}");
    assert!(took < Duration::from_millis(10), "{took:?}");
    let (_, _, took) = fastest(&chain(64));
    eprintln!("64 levels: {took:?}");
    assert!(took < Duration::from_millis(10), "64 levels: {took:?}");
}

#[test]
fn reading_work_refusals_come_back_in_under_10_ms() {
    // 64 `-exec`s that each run to the end of a long tail: the JS reads the tail 64 times.
    let tail = " x".repeat(5_000);
    let shapes = [
        format!("find .{}{tail}", " -exec ls".repeat(64)),
        format!("find .{}{tail}", " -exec find .".repeat(64)),
        format!(
            "sh -c 'find .{} {}'",
            " -exec ls".repeat(64),
            "y ".repeat(5_000)
        ),
        // Over the work bound on its own: the lexer stops.
        "x".repeat(300 * 1024),
        "a ".repeat(150 * 1024),
        // Over the request cap: refused before any reading.
        "x".repeat(2 * 1024 * 1024),
    ];
    for shape in &shapes {
        let (status, reply, took) = fastest(shape);
        assert_eq!(status, 1, "{}", &reply[..reply.len().min(200)]);
        assert_eq!(reply, r#"{"error":"too_large"}"#);
        eprintln!("refusal ({} chars): {took:?}", shape.len());
        assert!(took < Duration::from_millis(10), "{took:?}");
    }
}

#[test]
fn many_distinct_write_targets_answer_in_linear_time() {
    // `[...new Set(writes)]` over tens of thousands of distinct targets.
    let n = 10_000;
    let command: String = std::iter::once("find .".to_string())
        .chain((0..n).map(|i| format!(" -fprint f{i}")))
        .collect();
    let (status, reply, took) = fastest(&command);
    assert_eq!(status, 0, "{}", &reply[..reply.len().min(200)]);
    assert!(reply.contains(&format!("\"f{}\"]", n - 1)));
    eprintln!("{n} distinct writes: {took:?}");
    assert!(took < Duration::from_millis(100), "{took:?}");
}
