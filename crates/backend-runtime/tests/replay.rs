use backend_runtime::{
    diary::Corpus,
    journal::Journal,
    replay::{Layout, Replay},
    Control,
};
use serde_json::json;
use std::time::Duration;
#[test]
fn original_journal_replay_is_ordered_idempotent_and_quarantines_dependents(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("managed.db");
    let db = rusqlite::Connection::open(&path)?;
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE files(path TEXT PRIMARY KEY,data BLOB NOT NULL,version TEXT NOT NULL,updated REAL NOT NULL);CREATE TABLE directories(path TEXT PRIMARY KEY);CREATE TABLE workspace_index_outbox(path TEXT PRIMARY KEY);")?;
    let user = "01234567-89ab-4cde-8fab-0123456789ab";
    for (k, v) in [("tenant", user), ("active", "1"), ("generation", "0")] {
        db.execute("INSERT INTO meta VALUES(?,?)", [k, v])?;
    }
    let corpus = Corpus::open(&path, user, 3.0)?;
    let journal = Journal::open(&dir.path().join("index.db"))?;
    let replay = Replay::new(Layout {
        root: String::new(),
        monthly_prefix: "Monthly".into(),
        entries_prefix: "Entries".into(),
        month_file_template: "{year}-{month02}.md".into(),
        index_file: "INDEX.md".into(),
        daily: false,
        index_enabled: true,
    })?;
    let xid = "41234567-89ab-4cde-8fab-0123456789ab";
    let payload = json!({"xid":xid,"day":"2026-10-28","sub_header":"12:00","body":diary_corpus_write::render_exchange("first","response",xid),"scaffold":true});
    journal.enqueue("exchange", &payload)?;
    journal.enqueue("exchange", &payload)?;
    journal.enqueue(
        "exchange_edit",
        &json!({"xid":xid,"month":"2026-10","new_me":"corrected","new_claude":"edited"}),
    )?;
    journal.enqueue(
        "index_month",
        &json!({"month":"2026-10.md","label":"October 2026"}),
    )?;
    journal.enqueue("index_update",&json!({"open_questions":[{"action":"add","text":"Question?"}],"timeline":[],"today":"2026-10-28"}))?;
    let control = Control::new(Duration::from_secs(5))?;
    assert_eq!(replay.pending(&corpus, &journal, 100, true, &control)?, 5);
    let content = corpus
        .read("Monthly/2026-10.md", &control)?
        .ok_or("month")?
        .content;
    assert_eq!(content.matches(xid).count(), 1);
    assert!(content.contains("corrected"));
    assert!(!content.contains("first"));
    assert!(corpus.read("AI Memory/README.md", &control)?.is_some());
    assert!(corpus
        .read("Monthly/INDEX.md", &control)?
        .ok_or("index")?
        .content
        .contains("Question?"));
    assert_eq!(replay.pending(&corpus, &journal, 100, true, &control)?, 0);
    assert_eq!(
        replay.months(&corpus, &control)?,
        json!({"source":"sidecar","months":[{"id":"2026-10","label":"October 2026"}]})
    );
    let month_view = replay.history(
        &corpus,
        Some("2026-10"),
        chrono::NaiveDate::from_ymd_opt(2026, 10, 28).ok_or("date")?,
        &control,
    )?;
    assert_eq!(
        month_view.get("todayLog").ok_or("todayLog")?,
        &json!(content)
    );
    assert_eq!(month_view.get("standing").ok_or("standing")?, "");
    let today_view = replay.history(
        &corpus,
        None,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 28).ok_or("date")?,
        &control,
    )?;
    assert!(today_view
        .get("todayLog")
        .ok_or("todayLog")?
        .as_str()
        .ok_or("log")?
        .contains(xid));
    assert!(today_view
        .get("todayLog")
        .ok_or("todayLog")?
        .as_str()
        .ok_or("log")?
        .contains("corrected"));
    assert!(today_view
        .get("standing")
        .ok_or("standing")?
        .as_str()
        .ok_or("standing")?
        .contains("Question?"));
    assert!(replay
        .history(
            &corpus,
            Some("2026-13"),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 28).ok_or("date")?,
            &control
        )
        .is_err());
    journal.enqueue("unknown", &json!({"xid":"broken"}))?;
    journal.enqueue(
        "exchange_edit",
        &json!({"xid":"broken","new_me":"bad","new_claude":"bad"}),
    )?;
    journal.enqueue(
        "index_month",
        &json!({"month":"2026-11.md","label":"November 2026"}),
    )?;
    assert_eq!(replay.pending(&corpus, &journal, 100, true, &control)?, 1);
    assert_eq!(journal.quarantined_count()?, 2);
    assert_eq!(journal.pending_count()?, 0);
    Ok(())
}
