//! Tests for the Drive tool port. Expected values come from reading gdrive-tools.cjs (line
//! numbers in the comments), not from running it: no Node oracle has been recorded for this
//! crate yet (see handoff/foundation/R5.md, step 4). Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use drive_tools::*;

const FIXTURE: &str = include_str!("fixtures/gdrive-tools-list.v1.json");

#[test]
fn catalogue_matches_the_shared_fixture_in_order() {
    let v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let rows = v["tools"].as_array().unwrap();
    assert_eq!(rows.len(), TOOLS.len());
    for (row, tool) in rows.iter().zip(TOOLS) {
        assert_eq!(row["name"].as_str().unwrap(), tool.name());
        assert_eq!(row["label"].as_str().unwrap(), tool.label());
        assert_eq!(row["write"].as_bool().unwrap(), tool.is_write());
    }
}

#[test]
fn names_round_trip_and_unknown_names_are_none() {
    for tool in TOOLS {
        assert_eq!(Tool::from_name(tool.name()), Some(tool));
    }
    assert_eq!(Tool::from_name("drive_empty_trash"), None);
    assert_eq!(Tool::from_name(""), None);
    assert_eq!(Tool::from_name("DRIVE_SEARCH_FILES"), None);
}

#[test]
fn plan_write_names_new_files_trimmed_and_capped() {
    assert_eq!(
        plan_write(Tool::CreateFile, Some("  notes.md \n"), None),
        WritePlan::NewFile {
            target: "notes.md".into()
        }
    );
    let long = "x".repeat(300);
    match plan_write(Tool::CreateFile, Some(&long), None) {
        WritePlan::NewFile { target } => assert_eq!(target.len(), 255),
        other => panic!("expected NewFile, got {other:?}"),
    }
}

#[test]
fn plan_write_has_no_target_for_empty_or_missing_names_and_for_reads() {
    assert_eq!(
        plan_write(Tool::CreateFile, Some("   "), None),
        WritePlan::NoTarget
    );
    assert_eq!(
        plan_write(Tool::CreateFile, None, None),
        WritePlan::NoTarget
    );
    for read in [
        Tool::SearchFiles,
        Tool::ReadFile,
        Tool::GetMetadata,
        Tool::ListRecent,
    ] {
        assert_eq!(
            plan_write(read, Some("a"), Some("abc")),
            WritePlan::NoTarget
        );
    }
}

#[test]
fn plan_write_refuses_a_path_as_a_file_id_and_names_the_refusal() {
    match plan_write(Tool::TrashFile, None, Some("noevia projects/x/notes.md")) {
        WritePlan::Refused(text) => assert_eq!(
            text,
            "\"noevia projects/x/notes.md\" is not a Google Drive file id. The Google Drive tools only change files in the connected Google Drive; to change a file uploaded to this project, use the project file tools (project_append_file or project_replace_text)"
        ),
        other => panic!("expected Refused, got {other:?}"),
    }
    // A missing id is refused with the empty string, as the JS does (`''`).
    match plan_write(Tool::UpdateFile, None, None) {
        WritePlan::Refused(text) => {
            assert!(text.starts_with("\"\" is not a Google Drive file id."))
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn plan_write_trims_the_id_before_checking_it() {
    assert_eq!(
        plan_write(Tool::UpdateFile, None, Some("  abc_DEF-9  ")),
        WritePlan::ExistingFile {
            file_id: "abc_DEF-9".into()
        }
    );
}

#[test]
fn refused_id_is_cut_to_200_units_and_escaped() {
    let id = format!("{}\"\n", "a".repeat(250));
    match plan_write(Tool::TrashFile, None, Some(&id)) {
        WritePlan::Refused(text) => {
            let shown = format!("\"{}\"", "a".repeat(200));
            assert!(text.starts_with(&format!("{shown} is not a Google Drive file id.")));
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    match plan_write(Tool::TrashFile, None, Some("a\u{1}b")) {
        WritePlan::Refused(text) => assert!(text.starts_with("\"a\\u0001b\" is not")),
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn drive_id_alphabet_and_length() {
    assert!(is_drive_id("a"));
    assert!(is_drive_id(&"z".repeat(200)));
    assert!(!is_drive_id(&"z".repeat(201)));
    assert!(!is_drive_id(""));
    assert!(!is_drive_id("a.b"));
    assert!(!is_drive_id("a/b"));
    assert!(!is_drive_id("café"));
    assert!(!is_drive_id("a b"));
}

#[test]
fn file_line_follows_the_template_and_js_truthiness() {
    let full = FileLine {
        name: "Plan".into(),
        id: "f1".into(),
        mime_type: Some("text/plain".into()),
        size: Some("12".into()),
        modified_time: Some("2026-11-01T10:00:00Z".into()),
        web_view_link: None,
    };
    assert_eq!(
        file_line(&full),
        "- Plan (id f1) · text/plain · 12 bytes · changed 2026-11-01T10:00:00Z"
    );
    let bare = FileLine {
        name: "Plan".into(),
        id: "f1".into(),
        mime_type: Some(String::new()),
        size: Some(String::new()),
        ..FileLine::default()
    };
    assert_eq!(file_line(&bare), "- Plan (id f1) · file");
    // A string "0" is truthy in JS, so it is shown.
    let zero = FileLine {
        size: Some("0".into()),
        ..bare.clone()
    };
    assert_eq!(file_line(&zero), "- Plan (id f1) · file · 0 bytes");
}

#[test]
fn list_replies_and_empty_replies() {
    assert_eq!(found_reply(&[]), NO_MATCHES);
    assert_eq!(recent_reply(&[]), NO_FILES);
    let a = FileLine {
        name: "A".into(),
        id: "1".into(),
        ..FileLine::default()
    };
    let b = FileLine {
        name: "B".into(),
        id: "2".into(),
        ..FileLine::default()
    };
    assert_eq!(
        found_reply(&[a.clone(), b.clone()]),
        "Found 2:\n- A (id 1) · file\n- B (id 2) · file"
    );
    assert_eq!(
        recent_reply(&[a.clone(), b]),
        "- A (id 1) · file\n- B (id 2) · file"
    );
    let linked = FileLine {
        web_view_link: Some("https://example.test/x".into()),
        ..a
    };
    assert_eq!(
        metadata_reply(&linked),
        "- A (id 1) · file\nLink: https://example.test/x"
    );
}

#[test]
fn write_replies_match_the_templates() {
    assert_eq!(created_reply("n.md", "9", None), "Created n.md (id 9)");
    assert_eq!(
        created_reply("n.md", "9", Some("https://example.test/9")),
        "Created n.md (id 9): https://example.test/9"
    );
    assert_eq!(created_reply("n.md", "9", Some("")), "Created n.md (id 9)");
    assert_eq!(updated_reply("n.md", "9"), "Updated n.md (id 9).");
    assert_eq!(trashed_reply("n.md"), "Moved n.md to the Drive trash.");
    assert_eq!(existing_target("n.md", "9"), "n.md (id 9)");
    assert_eq!(
        error_reply(None),
        "ERROR: Google Drive could not be reached."
    );
    assert_eq!(
        error_reply(Some("")),
        "ERROR: Google Drive could not be reached."
    );
    assert_eq!(error_reply(Some("quota")), "ERROR: quota");
}

#[test]
fn whole_read_fits_the_cap_and_is_complete() {
    let r = shape_read("a.txt", "hello", false, 100);
    assert_eq!(r.text, "a.txt:\nhello");
    assert!(r.complete);
}

#[test]
fn read_over_the_cap_is_cut_with_the_marker_and_is_partial() {
    // header "a.txt:\n" = 7 units, marker "\n[truncated]" = 12 units, cap 30 leaves 11 for text.
    let r = shape_read("a.txt", "abcdefghijklmnopqrstuvwxyz0123", false, 30);
    assert_eq!(r.text, "a.txt:\nabcdefghijk\n[truncated]");
    assert_eq!(units(&r.text), 30);
    assert!(!r.complete);
}

#[test]
fn drive_truncation_is_partial_even_when_it_fits() {
    let r = shape_read("a.txt", "short", true, 1000);
    assert_eq!(r.text, "a.txt:\nshort\n[truncated]");
    assert!(!r.complete);
}

#[test]
fn a_header_longer_than_the_cap_leaves_no_text() {
    let r = shape_read("a-very-long-file-name.txt", "body", false, 5);
    assert_eq!(r.text, "a-very-long-file-name.txt:\n\n[truncated]");
    assert!(!r.complete);
}

#[test]
fn cuts_count_utf16_units_and_never_split_a_character() {
    // U+1F600 is two UTF-16 units; a budget of one unit keeps none of it.
    assert_eq!(units("😀"), 2);
    assert_eq!(slice_units("😀a", 1), "");
    assert_eq!(slice_units("😀a", 2), "😀");
    assert_eq!(slice_units("ab", 10), "ab");
    // header "e:\n" = 3 units, marker = 12, cap 17 leaves 2 units: one emoji (2) fits, two do not.
    let r = shape_read("e", "😀😀😀😀😀😀😀😀", false, 17);
    assert_eq!(r.text, "e:\n😀\n[truncated]");
    assert!(!r.complete);
}

#[test]
fn read_gate_allows_an_update_only_after_a_full_read_in_the_same_scope() {
    let mut gate = ReadGate::new();
    let scope = scope_key(Some("ex1"), Some("chat1"));
    assert_eq!(
        check_update(&gate, "u1", &scope, Some("f1"), 0),
        Err(NOT_READ_IN_FULL)
    );
    gate.remember("u1", &scope, "f1", Some("7"), true, 1_000);
    assert_eq!(
        check_update(&gate, "u1", &scope, Some("f1"), 2_000),
        Ok(UpdateCheck::ExpectVersion("7".into()))
    );
    // Another exchange, another user, or another file: not read here.
    let other_exchange = scope_key(Some("ex2"), Some("chat1"));
    assert_eq!(
        check_update(&gate, "u1", &other_exchange, Some("f1"), 2_000),
        Err(NOT_READ_IN_FULL)
    );
    assert_eq!(
        check_update(&gate, "u2", &scope, Some("f1"), 2_000),
        Err(NOT_READ_IN_FULL)
    );
    assert_eq!(
        check_update(&gate, "u1", &scope, Some("f2"), 2_000),
        Err(NOT_READ_IN_FULL)
    );
}

#[test]
fn a_partial_read_never_allows_an_update() {
    let mut gate = ReadGate::new();
    let scope = scope_key(Some("ex1"), None);
    gate.remember("u1", &scope, "f1", Some("7"), false, 0);
    assert_eq!(
        check_update(&gate, "u1", &scope, Some("f1"), 1),
        Err(READ_PARTLY)
    );
}

#[test]
fn a_read_expires_after_thirty_minutes() {
    let mut gate = ReadGate::new();
    let scope = scope_key(Some("ex1"), None);
    gate.remember("u1", &scope, "f1", Some("7"), true, 0);
    assert!(check_update(&gate, "u1", &scope, Some("f1"), READ_TTL_MS).is_ok());
    assert_eq!(
        check_update(&gate, "u1", &scope, Some("f1"), READ_TTL_MS + 1),
        Err(NOT_READ_IN_FULL)
    );
}

#[test]
fn a_non_drive_id_falls_through_without_a_version() {
    let gate = ReadGate::new();
    assert_eq!(
        check_update(&gate, "u1", "c:x", Some("noevia projects/a.md"), 0),
        Ok(UpdateCheck::NotDriveId)
    );
    assert_eq!(
        check_update(&gate, "u1", "c:x", None, 0),
        Ok(UpdateCheck::NotDriveId)
    );
}

#[test]
fn an_empty_exchange_key_falls_back_to_the_chat_scope() {
    assert_eq!(scope_key(Some(""), Some("c9")), "c:c9");
    assert_eq!(scope_key(None, None), "c:");
    assert_eq!(scope_key(Some("e"), Some("c9")), "x:e");
}

#[test]
fn remember_ignores_missing_versions_and_ids() {
    let mut gate = ReadGate::new();
    gate.remember("u1", "c:x", "f1", None, true, 0);
    gate.remember("u1", "c:x", "", Some("1"), true, 0);
    assert!(gate.last_read("u1", "c:x", "f1", 0).is_none());
    assert!(gate.last_read("u1", "c:x", "", 0).is_none());
}

#[test]
fn the_gate_keeps_at_most_read_max_entries_and_drops_the_oldest() {
    let mut gate = ReadGate::new();
    for i in 0..READ_MAX + 5 {
        gate.remember("u1", "c:x", &format!("f{i}"), Some("1"), true, 0);
    }
    assert!(gate.last_read("u1", "c:x", "f0", 0).is_none());
    assert!(gate.last_read("u1", "c:x", "f4", 0).is_none());
    assert!(gate.last_read("u1", "c:x", "f5", 0).is_some());
    assert!(gate
        .last_read("u1", "c:x", &format!("f{}", READ_MAX + 4), 0)
        .is_some());
}

#[test]
fn repeating_one_read_does_not_push_out_the_others() {
    let mut gate = ReadGate::new();
    gate.remember("u1", "c:x", "keep", Some("1"), true, 0);
    for i in 0..10_000u64 {
        gate.remember("u1", "c:x", "hot", Some("1"), true, i);
    }
    assert!(gate.last_read("u1", "c:x", "keep", 0).is_some());
    assert!(gate.last_read("u1", "c:x", "hot", 9_999).is_some());
}

#[test]
fn json_string_matches_json_stringify_for_the_characters_that_matter() {
    assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
    assert_eq!(json_string("\u{8}\u{c}\n\r\t"), r#""\b\f\n\r\t""#);
    assert_eq!(json_string("\u{1f}\u{0}"), r#""\u001f\u0000""#);
    // JSON.stringify leaves DEL, U+2028 and non-ASCII alone.
    assert_eq!(json_string("\u{7f}\u{2028}é"), "\"\u{7f}\u{2028}é\"");
}

#[test]
fn js_trim_uses_the_ecmascript_whitespace_set() {
    assert_eq!(js_trim("\u{feff} a \u{a0}"), "a");
    assert_eq!(js_trim("\u{85}a"), "\u{85}a");
    assert_eq!(js_trim("\u{2028}\u{3000}"), "");
}

#[test]
fn hostile_long_inputs_do_not_panic() {
    let big = "😀".repeat(100_000);
    let r = shape_read("n", &big, false, 1_000);
    assert!(units(&r.text) <= 1_000);
    assert!(!r.complete);
    let _ = plan_write(Tool::CreateFile, Some(&big), None);
    let _ = plan_write(Tool::UpdateFile, None, Some(&big));
    let line = file_line(&FileLine {
        name: big.clone(),
        id: big,
        ..FileLine::default()
    });
    assert!(line.starts_with("- "));
}
