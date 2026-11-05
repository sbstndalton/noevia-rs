#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;

pub(super) fn parse(text: &str) -> JValue {
    wtf16::parse(text).unwrap()
}
pub(super) fn base() -> &'static str {
    "native-autotune-0123456789abcdef.json"
}
pub(super) fn owner(dir: &Path, state: Option<&str>) -> (Owner, String) {
    if let Some(s) = state {
        std::fs::write(dir.join(base()), s).unwrap();
    }
    let o = Owner::new(dir);
    let a = o.attach(base(), true).unwrap();
    let w = a.get("writer").as_str().unwrap().to_owned();
    (o, w)
}

#[test]
fn a_node_written_file_loads_and_the_owner_rewrites_the_same_format() {
    let d = tempfile::tempdir().unwrap();
    let text = r#"{"history":{"m":[{"at":1}]},"settings":{"m":{"allowQ5Kv":true}},"job":{"id":"j","model":"m","status":"failed","models":[]}}"#;
    let (o, w) = owner(d.path(), Some(text));
    let snap = o.sync(&parse(&format!(r#"{{"writer":"{w}"}}"#)));
    assert_eq!(snap.0, 200);
    assert_eq!(snap.1.get("state"), &parse(text));
    // A commit writes the file with JSON.stringify's bytes, mode 0600, and keeps settings.
    let c = o.commit(&parse(&format!(
        r#"{{"writer":"{w}","jobRev":1,"history":{{"m":[]}},"job":{{"id":"k","model":"m","status":"running","models":[]}}}}"#
    )));
    assert_eq!(c.0, 200, "{:?}", c.1);
    let on_disk = std::fs::read_to_string(d.path().join(base())).unwrap();
    assert_eq!(
        on_disk,
        r#"{"history":{"m":[]},"settings":{"m":{"allowQ5Kv":true}},"job":{"id":"k","model":"m","status":"running","models":[]}}"#
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(d.path().join(base()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn corrupt_state_is_empty_and_a_non_object_is_refused() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join(base()), "{not json").unwrap();
    let (o, w) = owner(d.path(), None);
    let snap = o.sync(&parse(&format!(r#"{{"writer":"{w}"}}"#)));
    assert_eq!(
        snap.1.get("state"),
        &parse(r#"{"history":{},"settings":{}}"#)
    );
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join(base()), "[1]").unwrap();
    assert!(Owner::new(d.path()).attach(base(), true).is_err());
}

#[test]
fn only_a_state_file_name_inside_the_data_directory_can_be_bound() {
    let d = tempfile::tempdir().unwrap();
    for name in [
        "../native-autotune-0123456789abcdef.json",
        "native-autotune-0123456789abcdef.json/x",
        "other.json",
        "native-autotune-XYZ.json",
        "native-autotune-0123456789abcde.json",
    ] {
        assert!(Owner::new(d.path()).attach(name, true).is_err(), "{name}");
    }
    let o = Owner::new(d.path());
    o.attach(base(), true).unwrap();
    assert!(o
        .attach("native-autotune-fedcba9876543210.json", true)
        .is_err());
}

// ---- helpers ------------------------------------------------------------------------------------

fn outside(revision: &str, models: &[(&str, bool, bool)]) -> Outside {
    let rows: Vec<String> = models
        .iter()
        .map(|(n, c, s)| format!(r#""{n}":{{"configured":{c},"system":{s}}}"#))
        .collect();
    Outside::parse(&parse(&format!(
        r#"{{"revision":"{revision}","allowBelowQ5Kv":false,"models":{{{}}}}}"#,
        rows.join(",")
    )))
    .unwrap()
}
fn commit(o: &Owner, w: &str, job_rev: u64, job: &str, history: &str) -> Answer {
    o.commit(&parse(&format!(
        r#"{{"writer":"{w}","jobRev":{job_rev},"history":{history},"job":{job}}}"#
    )))
}
fn tok(a: &Answer) -> String {
    a.1.get("token").as_str().unwrap().to_owned()
}
fn job_rev(a: &Answer) -> u64 {
    a.1.get("jobRev").as_num().unwrap() as u64
}
fn models_running() -> &'static str {
    r#"{"id":"j","model":"m","status":"running","_revision":"r1","models":[{"model":"m","status":"running","phases":[{"id":"context","status":"running","steps":[{"status":"running"}]}]}]}"#
}
fn models_interrupted() -> &'static str {
    r#"{"id":"j","model":"m","status":"interrupted","_revision":"r1","_reloadPending":true,"models":[{"model":"m","status":"interrupted","phases":[{"id":"context","status":"interrupted","steps":[{"status":"interrupted"}]}]}]}"#
}
fn read_disk(d: &Path) -> JValue {
    parse(&std::fs::read_to_string(d.join(base())).unwrap())
}

// ---- writer, job revision and incarnation fencing --------------------------------------------------

#[test]
fn a_newer_attach_voids_the_older_writer_and_its_holds() {
    let d = tempfile::tempdir().unwrap();
    let (o, w1) = owner(d.path(), None);
    assert_eq!(
        o.start_hold(&parse(&format!(r#"{{"writer":"{w1}"}}"#)), true)
            .0,
        200
    );
    let w2 = o
        .attach(base(), true)
        .unwrap()
        .get("writer")
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(w1, w2);
    // The first writer is refused everywhere, and its start hold is gone.
    for a in [
        commit(&o, &w1, 1, r#"{"id":"x","status":"running"}"#, "{}"),
        o.start_hold(&parse(&format!(r#"{{"writer":"{w1}"}}"#)), true),
        o.sync(&parse(&format!(r#"{{"writer":"{w1}"}}"#))),
    ] {
        assert_eq!(a.0, 409, "{:?}", a.1);
        assert_eq!(a.1.get("fenced").as_str(), Some("writer"));
    }
    assert_eq!(
        o.start_hold(&parse(&format!(r#"{{"writer":"{w2}"}}"#)), true)
            .0,
        200
    );
}

#[test]
fn the_actors_stale_job_copy_is_refused_after_the_owner_changed_the_job() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    let c = commit(&o, &w, 1, models_interrupted(), "{}");
    assert_eq!(c.0, 200);
    let rev = job_rev(&c);
    // The owner clears the finished job (not while reload is pending), so clear it by first
    // committing the same job with the flag off.
    let job = models_interrupted().replace(r#""_reloadPending":true,"#, "");
    assert_eq!(commit(&o, &w, rev, &job, "{}").0, 200);
    let facts = o.clear_facts();
    let cleared = o.clear(facts.1.get("token").as_str().unwrap());
    assert_eq!(cleared.0, 200, "{:?}", cleared.1);
    // The actor still holds the old job revision: its next save is refused and carries the owner's state.
    let stale = commit(&o, &w, rev, &job, "{}");
    assert_eq!(stale.0, 409);
    assert_eq!(stale.1.get("fenced").as_str(), Some("job"));
    assert!(matches!(
        stale.1.get("state").get("state").get("job"),
        JValue::Undefined
    ));
    let fresh = stale.1.get("state").get("jobRev").as_num().unwrap() as u64;
    assert_eq!(commit(&o, &w, fresh, &job, "{}").0, 200);
}

#[test]
fn an_unbound_owner_and_an_unavailable_actor_answer_differently() {
    let d = tempfile::tempdir().unwrap();
    let o = Owner::new(d.path());
    assert_eq!(o.availability(), None);
    assert_eq!(o.cancel_facts().0, 503);
    assert_eq!(o.clear_facts().0, 503);
    o.attach(base(), false).unwrap();
    assert_eq!(o.availability(), Some(false));
    let a = o.cancel_facts();
    assert_eq!((a.0, a.1.get("available").as_bool()), (200, Some(false)));
    assert_eq!(
        o.commit(&parse(r#"{"writer":"x","jobRev":1,"history":{}}"#))
            .0,
        409
    );
}

// ---- cancel -----------------------------------------------------------------------------------------

#[test]
fn cancel_matches_the_exact_run_and_a_resumed_run_is_not_retargeted() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    commit(&o, &w, 1, models_running(), "{}");
    let facts = o.cancel_facts();
    let first_generation = facts.1.get("generation").as_str().unwrap().to_owned();
    assert_eq!(facts.1.get("job").get("status").as_str(), Some("running"));
    let t = o.cancel_begin(&tok(&facts)).unwrap();
    assert_eq!(
        t.reply.1.get("generation").as_str(),
        Some(first_generation.as_str())
    );
    // Recording the request changed the token: the same facts cannot cancel twice.
    assert_eq!(o.cancel_begin(&tok(&facts)).unwrap_err().0, 409);
    // The actor took the abort and the run ended; the job is resumed as a new run (new epoch), same id.
    o.cancel_undo(&t);
    let facts2 = o.cancel_facts();
    assert_ne!(
        tok(&facts2),
        tok(&facts),
        "withdrawing the request does not revive the facts it consumed"
    );
    let reg = o.commit(&parse(&format!(
        r#"{{"writer":"{w}","jobRev":1,"history":{{}},"register":true,"job":{models_running}}}"#,
        models_running = models_running()
    )));
    assert_eq!(reg.0, 200);
    assert_ne!(
        o.cancel_facts().1.get("generation").as_str(),
        Some(first_generation.as_str())
    );
    assert_eq!(
        o.cancel_begin(&tok(&facts)).unwrap_err().0,
        409,
        "stale token from the earlier run"
    );
}

#[test]
fn cancel_of_an_absent_job_or_a_foreign_token_applies_nothing() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    let facts = o.cancel_facts();
    assert!(facts.1.get("job") == &JValue::Null);
    assert_eq!(o.cancel_begin(&tok(&facts)).unwrap_err().0, 409, "no job");
    commit(&o, &w, 1, models_running(), "{}");
    assert_eq!(o.cancel_begin(&"0".repeat(64)).unwrap_err().0, 409);
    // Another owner (another boot) never accepts this one's tokens.
    let d2 = tempfile::tempdir().unwrap();
    let (o2, w2) = owner(d2.path(), None);
    commit(&o2, &w2, 1, models_running(), "{}");
    assert_eq!(o2.cancel_begin(&tok(&o.cancel_facts())).unwrap_err().0, 409);
}

// ---- clear ----------------------------------------------------------------------------------------

#[test]
fn clear_removes_only_a_finished_job_and_nothing_else() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(
        d.path(),
        Some(r#"{"history":{"m":[{"at":1}]},"settings":{"m":{"allowQ5Kv":true}}}"#),
    );
    // nothing to clear
    let none = o.clear_facts();
    assert_eq!(none.1.get("jobPresent").as_bool(), Some(false));
    assert_eq!(o.clear(&tok(&none)).0, 409);
    // running: refused
    commit(&o, &w, 1, models_running(), r#"{"m":[{"at":1}]}"#);
    let running = o.clear_facts();
    assert_eq!(o.clear(&tok(&running)).0, 409);
    // reload pending: refused
    commit(&o, &w, 1, models_interrupted(), r#"{"m":[{"at":1}]}"#);
    let pending = o.clear_facts();
    assert_eq!(pending.1.get("reloadPending").as_bool(), Some(true));
    assert_eq!(o.clear(&tok(&pending)).0, 409);
    // finished: removed; history, settings and the rest of the file stay
    let job = models_interrupted().replace(r#""_reloadPending":true,"#, "");
    commit(&o, &w, 1, &job, r#"{"m":[{"at":1}]}"#);
    let done = o.clear_facts();
    // a stale token (state moved on) is refused, a fresh one works
    commit(&o, &w, 1, &job, r#"{"m":[{"at":1}]}"#);
    assert_eq!(o.clear(&tok(&done)).0, 409);
    let fresh = o.clear_facts();
    assert_eq!(o.clear(&tok(&fresh)), (200, parse(r#"{"applied":true}"#)));
    let disk = read_disk(d.path());
    assert!(matches!(disk.get("job"), JValue::Undefined));
    assert_eq!(disk.get("history"), &parse(r#"{"m":[{"at":1}]}"#));
    assert_eq!(disk.get("settings"), &parse(r#"{"m":{"allowQ5Kv":true}}"#));
    // the start hold refuses even a finished job
    commit(
        &o,
        &w,
        o.sync(&parse(&format!(r#"{{"writer":"{w}"}}"#)))
            .1
            .get("jobRev")
            .as_num()
            .unwrap() as u64,
        &job,
        "{}",
    );
    let held = o.start_hold(&parse(&format!(r#"{{"writer":"{w}"}}"#)), true);
    assert_eq!(held.0, 200);
    let facts = o.clear_facts();
    assert_eq!(facts.1.get("starting").as_bool(), Some(true));
    assert_eq!(o.clear(&tok(&facts)).0, 409);
}

// ---- settings ---------------------------------------------------------------------------------------

#[test]
fn a_settings_token_is_good_for_exactly_the_state_and_outside_facts_it_names() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    let outs = outside("r1", &[("m", true, false)]);
    let facts = o.settings_facts(&JValue::from("m"), Some(&outs));
    assert_eq!(facts.1.get("settingsAvailable").as_bool(), Some(true));
    let token = facts
        .1
        .get("facts")
        .get("token")
        .as_str()
        .unwrap()
        .to_owned();
    // models.ini moved on: the same facts are refused
    let moved = outside("r2", &[("m", true, false)]);
    assert_eq!(o.settings_commit("m", true, &token, &moved).0, 409);
    // the job changed (a tune started): refused
    let c = commit(&o, &w, 1, models_running(), "{}");
    assert_eq!(c.0, 200);
    assert_eq!(o.settings_commit("m", true, &token, &outs).0, 409);
    // fresh facts apply once and persist; the old token cannot apply twice
    let fresh = o.settings_facts(&JValue::from("m"), Some(&outs));
    assert_eq!(
        fresh
            .1
            .get("facts")
            .get("guard")
            .get("job")
            .get("status")
            .as_str(),
        Some("running")
    );
    let token = fresh
        .1
        .get("facts")
        .get("token")
        .as_str()
        .unwrap()
        .to_owned();
    let applied = o.settings_commit("m", true, &token, &outs);
    assert_eq!(applied.0, 200);
    assert_eq!(
        applied
            .1
            .get("facts")
            .get("guard")
            .get("allowQ5Kv")
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        read_disk(d.path()).get("settings"),
        &parse(r#"{"m":{"allowQ5Kv":true}}"#)
    );
    assert_eq!(o.settings_commit("m", false, &token, &outs).0, 409);
    // turning it off removes the entry
    let t2 = o
        .settings_facts(&JValue::from("m"), Some(&outs))
        .1
        .get("facts")
        .get("token")
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(o.settings_commit("m", false, &t2, &outs).0, 200);
    assert_eq!(read_disk(d.path()).get("settings"), &parse("{}"));
    // The actor's commit keeps the settings the owner wrote.
    let t3 = o
        .settings_facts(&JValue::from("m"), Some(&outs))
        .1
        .get("facts")
        .get("token")
        .as_str()
        .unwrap()
        .to_owned();
    o.settings_commit("m", true, &t3, &outs);
    let rev = o
        .sync(&parse(&format!(r#"{{"writer":"{w}"}}"#)))
        .1
        .get("jobRev")
        .as_num()
        .unwrap() as u64;
    let kept = commit(&o, &w, rev, models_running(), "{}");
    assert_eq!(
        kept.1.get("settings"),
        &parse(r#"{"m":{"allowQ5Kv":true}}"#)
    );
}

#[test]
fn an_unnamed_model_name_cannot_pollute_the_settings_prototype() {
    let d = tempfile::tempdir().unwrap();
    let (o, _w) = owner(d.path(), None);
    let outs = outside("r1", &[("__proto__", true, false)]);
    let token = o
        .settings_facts(&JValue::from("__proto__"), Some(&outs))
        .1
        .get("facts")
        .get("token")
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(o.settings_commit("__proto__", true, &token, &outs).0, 200);
    let disk = std::fs::read_to_string(d.path().join(base())).unwrap();
    assert!(
        disk.contains(r#""settings":{"__proto__":{"allowQ5Kv":true}}"#),
        "{disk}"
    );
}

// ---- resume: facts, the preparation hold and the cross-await rule -------------------------------------

#[test]
fn a_preparation_holds_the_state_and_only_the_reload_note_may_change_it() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    commit(&o, &w, 1, models_interrupted(), "{}");
    let outs = outside("r1", &[("m", true, false)]);
    let facts = o.resume_facts(&outs);
    assert_eq!(
        facts.1.get("job").get("status").as_str(),
        Some("interrupted")
    );
    let t = tok(&facts);
    let sels = parse("[]");
    // A stale token cannot prepare; the right one takes the hold, a second preparation is refused.
    assert_eq!(
        o.resume_prepare_begin(&"0".repeat(64), &JValue::from("identities"), &sels, &outs)
            .unwrap_err()
            .0,
        409
    );
    let prep = o
        .resume_prepare_begin(&t, &JValue::from("identities"), &sels, &outs)
        .unwrap();
    assert_eq!(
        o.resume_prepare_begin(&t, &JValue::from("identities"), &sels, &outs)
            .unwrap_err()
            .0,
        409
    );
    assert_eq!(
        o.start_hold(&parse(&format!(r#"{{"writer":"{w}"}}"#)), true)
            .0,
        409,
        "busy during preparation"
    );
    assert_eq!(o.clear_facts().1.get("starting").as_bool(), Some(true));
    // The actor's single allowed change: the re-read note and the flag.
    let rev = o
        .sync(&parse(&format!(r#"{{"writer":"{w}"}}"#)))
        .1
        .get("jobRev")
        .as_num()
        .unwrap() as u64;
    let noted = models_interrupted().replace(
        r#""_reloadPending":true,"#,
        r#""_reloadPending":false,"log":[{"at":5,"text":"The model server re-read the restored models.ini."}],"#,
    );
    assert_eq!(commit(&o, &w, rev, &noted, "{}").0, 200);
    let done = o.resume_prepare_end(prep.id, Ok(parse(r#"{"identities":[]}"#)), &outs);
    assert_eq!(done.0, 200, "{:?}", done.1);
    assert_eq!(done.1.get("applied").as_bool(), Some(true));
    assert_eq!(done.1.get("starting").as_bool(), Some(false));
    assert!(done.1.get("identities") != &JValue::Undefined);
    // The hold is gone.
    assert_eq!(
        o.start_hold(&parse(&format!(r#"{{"writer":"{w}"}}"#)), true)
            .0,
        200
    );
}

#[test]
fn any_other_change_across_a_preparation_makes_it_stale() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    commit(&o, &w, 1, models_interrupted(), "{}");
    let outs = outside("r1", &[("m", true, false)]);
    let t = tok(&o.resume_facts(&outs));
    // an unrelated actor change (a note with other text)
    let prep = o
        .resume_prepare_begin(&t, &JValue::from("identities"), &parse("[]"), &outs)
        .unwrap();
    let rev = o
        .sync(&parse(&format!(r#"{{"writer":"{w}"}}"#)))
        .1
        .get("jobRev")
        .as_num()
        .unwrap() as u64;
    let other = models_interrupted().replace(
        r#""_reloadPending":true,"#,
        r#""_reloadPending":false,"log":[{"at":5,"text":"something else"}],"#,
    );
    commit(&o, &w, rev, &other, "{}");
    assert_eq!(
        o.resume_prepare_end(prep.id, Ok(parse(r#"{"identities":[]}"#)), &outs)
            .0,
        409
    );
    // a settings commit during a preparation
    let t = tok(&o.resume_facts(&outs));
    let prep = o
        .resume_prepare_begin(&t, &JValue::from("identities"), &parse("[]"), &outs)
        .unwrap();
    let st = o
        .settings_facts(&JValue::from("m"), Some(&outs))
        .1
        .get("facts")
        .get("token")
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(o.settings_commit("m", true, &st, &outs).0, 200);
    assert_eq!(
        o.resume_prepare_end(prep.id, Ok(parse(r#"{"identities":[]}"#)), &outs)
            .0,
        409
    );
    // models.ini moved on between the stages
    let t = tok(&o.resume_facts(&outs));
    let prep = o
        .resume_prepare_begin(&t, &JValue::from("identities"), &parse("[]"), &outs)
        .unwrap();
    let moved = outside("r2", &[("m", true, false)]);
    assert_eq!(
        o.resume_prepare_end(prep.id, Ok(parse(r#"{"identities":[]}"#)), &moved)
            .0,
        409
    );
    // an abandoned preparation (the caller went away) releases the hold
    let t = tok(&o.resume_facts(&outs));
    let prep = o
        .resume_prepare_begin(&t, &JValue::from("listing"), &parse("[]"), &outs)
        .unwrap();
    assert_eq!(
        o.resume_prepare_begin(
            &tok(&o.resume_facts(&outs)),
            &JValue::from("listing"),
            &parse("[]"),
            &outs
        )
        .unwrap_err()
        .0,
        409
    );
    o.resume_prepare_abandon(prep.id);
    assert!(o
        .resume_prepare_begin(
            &tok(&o.resume_facts(&outs)),
            &JValue::from("listing"),
            &parse("[]"),
            &outs
        )
        .is_ok());
}

#[test]
fn the_reload_fault_and_invalid_selectors() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    commit(&o, &w, 1, models_interrupted(), "{}");
    let outs = outside("r1", &[("m", true, false)]);
    let bad = parse(r#"[{"index":3,"field":"model"}]"#);
    assert_eq!(
        o.resume_prepare_begin(
            &tok(&o.resume_facts(&outs)),
            &JValue::from("identities"),
            &bad,
            &outs
        )
        .unwrap_err()
        .0,
        503
    );
    let prep = o
        .resume_prepare_begin(
            &tok(&o.resume_facts(&outs)),
            &JValue::from("identities"),
            &parse(r#"[{"index":0,"field":"model"}]"#),
            &outs,
        )
        .unwrap();
    assert_eq!(
        prep.selectors,
        vec![(0, "model".to_owned(), "m".to_owned())]
    );
    let r = o.resume_prepare_end(prep.id, Err(true), &outs);
    assert_eq!((r.0, r.1.get("fault").as_str()), (409, Some("reload")));
}

#[test]
fn the_listing_stage_adopts_a_pending_revision_the_way_recovery_does() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    let job = models_interrupted().replace(
        r#""_revision":"r1","#,
        r#""_revision":"r1","_pendingRevision":"r2","#,
    );
    commit(&o, &w, 1, &job, "{}");
    let outs = outside("r2", &[("m", true, false)]);
    let prep = o
        .resume_prepare_begin(
            &tok(&o.resume_facts(&outs)),
            &JValue::from("listing"),
            &parse("[]"),
            &outs,
        )
        .unwrap();
    let done = o.resume_prepare_end(prep.id, Ok(parse(r#"{"models":[]}"#)), &outs);
    assert_eq!(done.0, 200);
    assert_eq!(done.1.get("job").get("revision").as_str(), Some("r2"));
    assert_eq!(done.1.get("job").get("pendingRevision"), &JValue::Null);
    let disk = read_disk(d.path());
    assert_eq!(disk.get("job").get("_revision").as_str(), Some("r2"));
    assert!(matches!(
        disk.get("job").get("_pendingRevision"),
        JValue::Undefined
    ));
    // A different file keeps the job's revision (the revision check still refuses it later).
    let d2 = tempfile::tempdir().unwrap();
    let (o, w) = owner(d2.path(), None);
    commit(&o, &w, 1, &job, "{}");
    let outs = outside("r9", &[("m", true, false)]);
    let prep = o
        .resume_prepare_begin(
            &tok(&o.resume_facts(&outs)),
            &JValue::from("listing"),
            &parse("[]"),
            &outs,
        )
        .unwrap();
    let done = o.resume_prepare_end(prep.id, Ok(parse(r#"{"models":[]}"#)), &outs);
    assert_eq!(done.1.get("job").get("revision").as_str(), Some("r1"));
}

#[test]
fn a_launching_resume_patches_persists_and_owes_the_actor_a_launch() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    let job = models_interrupted().replace(r#""_reloadPending":true,"#, "");
    commit(&o, &w, 1, &job, r#"{"m":[{"at":1}]}"#);
    let outs = outside("r1", &[("m", true, false)]);
    let plan =
        parse(r#"{"kind":"launch","reset":[{"index":0,"phases":[{"index":0,"keepSteps":null}]}]}"#);
    let stale = o.resume_commit(&"1".repeat(64), &plan, &outs);
    assert_eq!(stale.answer.0, 409);
    let t = tok(&o.resume_facts(&outs));
    let done = o.resume_commit(&t, &plan, &outs);
    assert_eq!(done.answer.0, 200, "{:?}", done.answer.1);
    let launch = done.launch.unwrap();
    assert_eq!(launch.job.get("status").as_str(), Some("running"));
    let JValue::Arr(models) = launch.job.get("models") else {
        panic!()
    };
    assert_eq!(models[0].get("status").as_str(), Some("pending"));
    assert_eq!(
        models[0].get("phases"),
        &parse(r#"[{"id":"context","status":"pending","steps":[{"status":"pending"}]}]"#)
    );
    assert_eq!(launch.history, parse(r#"{"m":[{"at":1}]}"#));
    assert!(done.answer.1.get("publicJob").get("_revision") == &JValue::Undefined);
    assert_eq!(
        read_disk(d.path()).get("job").get("status").as_str(),
        Some("running")
    );
    // The same token cannot launch twice (the run epoch moved).
    assert_eq!(o.resume_commit(&t, &plan, &outs).answer.0, 409);
    // The actor never took it: no running job without a runner.
    o.launch_failed(&launch);
    let disk = read_disk(d.path());
    assert_eq!(disk.get("job").get("status").as_str(), Some("interrupted"));
    assert!(disk.get("job").get("error").as_str().is_some());
    // A late failure notice for an older launch changes nothing.
    let before = read_disk(d.path());
    o.launch_failed(&launch);
    assert_eq!(read_disk(d.path()), before);
}

#[test]
fn an_invalid_patch_changes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let (o, w) = owner(d.path(), None);
    let job = models_interrupted().replace(r#""_reloadPending":true,"#, "");
    commit(&o, &w, 1, &job, "{}");
    let outs = outside("r1", &[("m", true, false)]);
    for plan in [
        r#"{"kind":"nope"}"#,
        r#"{"kind":"filter","keep":[1],"skipped":[],"complete":false,"model":null}"#,
        r#"{"kind":"filter","keep":[],"skipped":[],"complete":false,"model":null}"#,
        r#"{"kind":"launch","reset":[{"index":4,"phases":[]}]}"#,
        r#"{"kind":"launch","reset":[{"index":0,"phases":[{"index":0,"keepSteps":[2]}]}]}"#,
    ] {
        let before = read_disk(d.path());
        let r = o.resume_commit(&tok(&o.resume_facts(&outs)), &parse(plan), &outs);
        assert_eq!(r.answer.0, 503, "{plan}");
        assert_eq!(read_disk(d.path()), before);
    }
    // A queue filter completes the job without a launch.
    let plan = parse(
        r#"{"kind":"filter","keep":[],"skipped":[{"index":0,"reason":"gone"}],"complete":true,"model":null}"#,
    );
    let r = o.resume_commit(&tok(&o.resume_facts(&outs)), &plan, &outs);
    assert_eq!(r.answer.0, 200);
    assert!(r.launch.is_none());
    let disk = read_disk(d.path());
    assert_eq!(disk.get("job").get("status").as_str(), Some("passed"));
    assert_eq!(
        disk.get("job").get("skipped"),
        &parse(r#"[{"model":"m","reason":"gone"}]"#)
    );
}

// ---- status -------------------------------------------------------------------------------------------

#[test]
fn status_facts_keep_private_and_credential_fields_out_and_use_only_own_entries() {
    let d = tempfile::tempdir().unwrap();
    let text = r#"{"job":{"id":"j","model":"m\ud800","status":"failed","originalText":"never-public","unknown":{"secret":"never-public"},"models":[{"model":"m","phases":[{"steps":[{"value":{"originalRevision":"never-public","Authorization":"never-public","access_token":"never-public","url":"https://u:p@host/path","answer":"visible"}}]}]}]},"history":{"m":[{"at":1,"unknown":{"credentials":"never-public"},"sampling":{"values":{"api_key":"never-public","temp":0.7}},"quality":{"checks":[{"originalText":"never-public","answer":"visible"}]}}],"__proto__":[{"at":3,"context":8192}]},"settings":{"m":{"allowQ5Kv":{"token":"never-public"}},"__proto__":{"allowQ5Kv":true}}}"#;
    let (o, _w) = owner(d.path(), Some(text));
    let out = o.status_facts(&[JValue::from("m"), JValue::from("__proto__")], false);
    assert_eq!(out.0, 200);
    let s = wtf16::stringify(&out.1).unwrap();
    assert!(!s.contains("never-public"), "{s}");
    assert!(s.contains("visible"));
    assert!(s.contains(r#"m\ud800"#));
    let JValue::Arr(entries) = out.1.get("entries") else {
        panic!()
    };
    assert_eq!(entries[0].get("allowQ5Kv").as_bool(), Some(false));
    assert_eq!(entries[1].get("allowQ5Kv").as_bool(), Some(true));
    assert_eq!(
        entries[1].get("history"),
        &parse(r#"[{"at":3,"context":8192}]"#)
    );
    let none = o.status_facts(&[JValue::from("constructor")], true);
    let JValue::Arr(none_entries) = none.1.get("entries") else {
        panic!()
    };
    assert_eq!(none_entries[0].get("history"), &JValue::Null);
    assert_eq!(none.1.get("allowBelowQ5Kv").as_bool(), Some(true));
    // missing and explicit-null jobs differ
    let d3 = tempfile::tempdir().unwrap();
    let (o, _) = owner(d3.path(), None);
    assert_eq!(
        o.status_facts(&[], false).1.get("jobPresent").as_bool(),
        Some(false)
    );
    let d4 = tempfile::tempdir().unwrap();
    let (o, w) = owner(d4.path(), None);
    assert_eq!(commit(&o, &w, 1, "null", "{}").0, 200);
    let n = o.status_facts(&[], false);
    assert_eq!(
        (n.1.get("jobPresent").as_bool(), n.1.get("job")),
        (Some(true), &JValue::Null)
    );
    // too many or oversized selectors are refused
    assert_eq!(
        o.status_facts(
            &[JValue::from("a"), JValue::from("b"), JValue::from("c")],
            false
        )
        .0,
        503
    );
    assert_eq!(o.status_facts(&[JValue::Num(1.0)], false).0, 503);
}

#[test]
fn malformed_stored_data_is_refused_not_served() {
    let d = tempfile::tempdir().unwrap();
    let (o, _w) = owner(
        d.path(),
        Some(r#"{"history":{"m":{"headers":{"Authorization":"never-public"}}}}"#),
    );
    let r = o.status_facts(&[JValue::from("m")], false);
    assert_eq!(r.0, 503);
    assert!(!wtf16::stringify(&r.1).unwrap().contains("never-public"));
}

// ---- the recorded Node actor: the owner answers what the retired bridges answered -------------------------

fn norm(v: &JValue) -> JValue {
    // What crosses the wire: undefined fields are gone, as JSON.stringify drops them.
    parse(&wtf16::stringify(v).unwrap())
}
fn without(v: &JValue, drop: &[&str]) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(k, _)| !drop.contains(&k.as_str()))
                .map(|(k, x)| (k.clone(), without(x, drop)))
                .collect(),
        ),
        JValue::Arr(items) => JValue::Arr(items.iter().map(|x| without(x, drop)).collect()),
        other => other.clone(),
    }
}
/// Object keys in any order (the wire order is JS's own; these answers are read by key).
fn same(a: &JValue, b: &JValue) -> bool {
    same_fields(a, b)
}
/// Deliberate difference: Node read `state.history["__proto__"]` through the prototype chain and
/// answered Object.prototype ({}) for a model with no history of its own; the owner reads own
/// entries only, so that is an empty list like any other model.
fn own_entries_only(want: &JValue) -> JValue {
    let mut want = want.clone();
    if let JValue::Obj(fields) = &mut want {
        for (k, v) in fields.iter_mut() {
            if matches!(k.as_str(), "history" | "longHistory")
                && matches!(v, JValue::Obj(f) if f.is_empty())
            {
                *v = JValue::Arr(Vec::new());
            }
        }
    }
    want
}
fn expect_same(what: &str, got: &JValue, want: &JValue) {
    assert!(
        same(got, want),
        "{what}\n got: {}\nwant: {}",
        wtf16::stringify(got).unwrap(),
        wtf16::stringify(want).unwrap()
    );
}

#[test]
fn the_owner_answers_every_recorded_node_actor_scenario_the_same_way() {
    let fixture = parse(include_str!(
        "../../tests/fixtures/autotune-owner-oracle.json"
    ));
    let JValue::Arr(scenarios) = fixture.get("scenarios") else {
        panic!("scenarios")
    };
    assert!(scenarios.len() >= 12);
    let strip_run = ["token", "generation", "available"];
    for sc in scenarios {
        let name = sc.get("name").as_str().unwrap();
        let outside = Outside::parse(sc.get("outside")).unwrap();
        let fresh = || {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(
                d.path().join(base()),
                wtf16::stringify(sc.get("state")).unwrap(),
            )
            .unwrap();
            let o = Owner::new(d.path());
            o.attach(base(), true).unwrap();
            (d, o)
        };
        // status
        for (field, keys) in [
            ("status", vec!["m", "m-long"]),
            ("statusEmpty", vec![]),
            ("statusProto", vec!["__proto__", "constructor"]),
        ] {
            let (_d, o) = fresh();
            let keys: Vec<JValue> = keys.into_iter().map(JValue::from).collect();
            let got = o.status_facts(&keys, false);
            match sc.get(field).get("ok") {
                JValue::Undefined => assert_eq!(got.0, 503, "{name} {field}"),
                want => {
                    assert_eq!(got.0, 200, "{name} {field}");
                    expect_same(
                        &format!("{name} {field}"),
                        &without(&norm(&got.1), &["available"]),
                        want,
                    );
                }
            }
        }
        // cancel
        {
            let (_d, o) = fresh();
            let got = o.cancel_facts();
            expect_same(
                &format!("{name} cancel"),
                &without(&norm(&got.1), &strip_run),
                sc.get("cancel").get("ok"),
            );
        }
        // clear
        {
            let (d, o) = fresh();
            let facts = o.clear_facts();
            expect_same(
                &format!("{name} clear facts"),
                &without(&norm(&facts.1), &strip_run),
                sc.get("clear").get("facts"),
            );
            let r = o.clear(facts.1.get("token").as_str().unwrap());
            let want = sc.get("clear").get("result").get("ok");
            assert_eq!(
                r.1.get("applied").as_bool(),
                want.get("applied").as_bool(),
                "{name} clear"
            );
            expect_same(
                &format!("{name} clear disk"),
                &read_disk(d.path()),
                sc.get("clear").get("disk"),
            );
        }
        // settings
        let JValue::Arr(settings) = sc.get("settings") else {
            continue;
        };
        for entry in settings {
            let model = entry.get("model").as_str().unwrap();
            let allow = entry.get("allow").as_bool().unwrap();
            let (d, o) = fresh();
            let facts = o.settings_facts(&JValue::from(model), Some(&outside));
            match entry.get("facts").get("ok") {
                JValue::Undefined => {}
                want => {
                    let token = facts
                        .1
                        .get("facts")
                        .get("token")
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let want = own_entries_only(want);
                    expect_same(
                        &format!("{name} settings facts {model}"),
                        &without(&norm(facts.1.get("facts")), &["token"]),
                        &want,
                    );
                    let commit = o.settings_commit(model, allow, &token, &outside);
                    let want = entry.get("commit").get("ok");
                    assert_eq!(
                        commit.1.get("applied").as_bool(),
                        want.get("applied").as_bool(),
                        "{name} settings commit {model}"
                    );
                    if want.get("applied").as_bool() == Some(true) {
                        expect_same(
                            &format!("{name} settings commit facts {model}"),
                            &without(&norm(commit.1.get("facts")), &["token"]),
                            &own_entries_only(want.get("facts")),
                        );
                    }
                    expect_same(
                        &format!("{name} settings disk {model}"),
                        &read_disk(d.path()),
                        entry.get("disk"),
                    );
                }
            }
        }
        // resume
        let (_d, o) = fresh();
        let facts = o.resume_facts(&outside);
        expect_same(
            &format!("{name} resume facts"),
            &without(&norm(&facts.1), &strip_run),
            sc.get("resume").get("facts").get("ok"),
        );
        let JValue::Arr(plans) = sc.get("resume").get("plans") else {
            panic!("plans")
        };
        for p in plans {
            let plan_name = p.get("name").as_str().unwrap();
            if matches!(p.get("plan"), JValue::Undefined | JValue::Null) {
                continue;
            }
            let (d, o) = fresh();
            let token = o
                .resume_facts(&outside)
                .1
                .get("token")
                .as_str()
                .unwrap()
                .to_owned();
            let done = o.resume_commit(&token, p.get("plan"), &outside);
            let want = p.get("result");
            let ignore = ["token", "generation", "available", "finishedAt"];
            if want.get("error").as_bool() == Some(true) {
                assert_eq!(done.answer.0, 503, "{name} {plan_name}");
                continue;
            }
            let want = want.get("ok");
            assert_eq!(
                done.answer.1.get("applied").as_bool(),
                want.get("applied").as_bool(),
                "{name} {plan_name}"
            );
            if want.get("applied").as_bool() == Some(true) {
                expect_same(
                    &format!("{name} {plan_name} answer"),
                    &without(&norm(&done.answer.1), &ignore),
                    &without(want, &ignore),
                );
                expect_same(
                    &format!("{name} {plan_name} disk"),
                    &without(&read_disk(d.path()), &ignore),
                    &without(p.get("disk"), &ignore),
                );
                // A launch with nothing left to run ends at once as passed: no runner is owed.
                assert_eq!(
                    done.launch.is_some(),
                    p.get("plan").get("kind").as_str() == Some("launch")
                        && done.answer.1.get("publicJob").get("status").as_str() == Some("running")
                );
            } else {
                expect_same(
                    &format!("{name} {plan_name} untouched"),
                    &read_disk(d.path()),
                    &parse(&wtf16::stringify(sc.get("state")).unwrap()),
                );
            }
        }
    }
}

#[test]
fn retired_or_delayed_actor_sessions_cannot_attach_over_the_new_writer() {
    let d = tempfile::tempdir().unwrap();
    let o = Owner::new(d.path());
    let open = |actor: &str| parse(&format!(r#"{{"actor":"{actor}","open":true}}"#));
    let attach = |actor: &str, incarnation: u64| {
        parse(&format!(
            r#"{{"actor":"{actor}","incarnation":{incarnation},"base":"{}","available":true}}"#,
            base()
        ))
    };
    let a = "a".repeat(32);
    let b = "b".repeat(32);
    assert_eq!(o.attach_actor(&open(&a)).0, 200);
    assert_eq!(o.attach_actor(&attach(&a, 1)).0, 200);
    assert_eq!(o.attach_actor(&open(&b)).0, 200);
    let current = o.attach_actor(&attach(&b, 2));
    assert_eq!(current.0, 200);
    assert_eq!(o.attach_actor(&attach(&a, 1)).0, 409);
    assert_eq!(o.attach_actor(&open(&a)).0, 409);
    assert_eq!(o.attach_actor(&attach(&b, 2)), current);
}

#[test]
fn reload_note_cannot_erase_settings_cancel_history_or_outside_changes() {
    for change in ["settings", "cancel", "history", "revision", "system"] {
        let d = tempfile::tempdir().unwrap();
        let (o, w) = owner(d.path(), None);
        commit(&o, &w, 1, models_interrupted(), "{}");
        let outs = outside("r1", &[("m", true, false)]);
        let t = tok(&o.resume_facts(&outs));
        let p = o
            .resume_prepare_begin(&t, &JValue::from("identities"), &parse("[]"), &outs)
            .unwrap();
        if change == "settings" {
            let f = o.settings_facts(&JValue::from("m"), Some(&outs));
            let token = f.1.get("facts").get("token").as_str().unwrap();
            assert_eq!(o.settings_commit("m", true, token, &outs).0, 200);
        }
        if change == "cancel" {
            let token = tok(&o.cancel_facts());
            o.cancel_begin(&token).unwrap();
        }
        let noted = models_interrupted().replace(r#""_reloadPending":true,"#, r#""_reloadPending":false,"log":[{"at":5,"text":"The model server re-read the restored models.ini."}],"#);
        let hist = if change == "history" {
            r#"{"m":[1]}"#
        } else {
            "{}"
        };
        commit(&o, &w, 1, &noted, hist);
        let fresh = match change {
            "revision" => outside("r2", &[("m", true, false)]),
            "system" => outside("r1", &[("m", true, true)]),
            _ => outs.clone(),
        };
        assert_eq!(
            o.resume_prepare_end(p.id, Ok(parse(r#"{"identities":[]}"#)), &fresh)
                .0,
            409,
            "{change}"
        );
    }
}

#[test]
fn a_new_actor_waits_for_the_old_writers_effects_to_settle() {
    let d = tempfile::tempdir().unwrap();
    let o = Owner::new(d.path());
    let a = "a".repeat(32);
    let b = "b".repeat(32);
    let request = |actor: &str, open: bool, n: u64| {
        parse(&format!(
            r#"{{"actor":"{actor}","open":{open},"incarnation":{n},"base":"{}","available":true}}"#,
            base()
        ))
    };
    o.attach_actor(&request(&a, true, 0));
    let first = o.attach_actor(&request(&a, false, 1));
    let writer = first.1.get("writer").as_str().unwrap();
    assert_eq!(
        commit(&o, writer, 1, r#"{"id":"j","status":"running"}"#, "{}").0,
        200
    );
    o.attach_actor(&request(&b, true, 0));
    assert_eq!(o.attach_actor(&request(&b, false, 2)).0, 409);
    assert_eq!(
        commit(&o, writer, 1, r#"{"id":"j","status":"interrupted"}"#, "{}").0,
        200
    );
    assert_eq!(o.attach_actor(&request(&b, false, 2)).0, 200);
    assert_eq!(o.attach_actor(&request(&a, false, 1)).0, 409);
}
