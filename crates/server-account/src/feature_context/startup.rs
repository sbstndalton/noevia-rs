//! Account-owned initialization callback for the serialized context registration.
//! Invoke inside Controller::register's Writer transaction, never after Ready.
use super::BootFacts;
use crate::users::audit;
use js_json::JValue;
use server_features::{Availability, Feature};
use server_store::{Reader, RustFeatureSettings, StoreError, WriteTx};

fn typed_boolean(reader: &Reader<'_>, key: &str) -> Result<Option<bool>, StoreError> {
    let value = reader.row(
        "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
        [key], |r| r.get::<_, Option<String>>(0),
    )?.flatten();
    Ok(value.as_deref().map(|s| s == "true"))
}
/// Exact typed canonical row, then the declared legacy key, then the catalog default.
/// This same read remains correct when an isolated legacy-copy statement failed.
pub fn selected(reader: &Reader<'_>, feature: Feature) -> Result<Option<bool>, StoreError> {
    let spec = server_features::spec(feature);
    if let Some(value) = typed_boolean(reader, spec.setting)? {
        return Ok(Some(value));
    }
    match spec.legacy {
        Some(name) => typed_boolean(reader, &format!("feature:{name}")),
        None => Ok(None),
    }
}
/// The transaction also contains the feature write when called for an OFF setter.
/// Delete pending/approved codes even when no grant exists. Audit failures propagate
/// so the enclosing Writer rolls every delete and setting change back together.
pub fn revoke_all(tx: &WriteTx<'_>, actor: Option<&str>, now: i64) -> Result<(), StoreError> {
    let affected = tx.reader().rows(
        "SELECT user_id,count(*) FROM device_grants GROUP BY user_id ORDER BY user_id",
        [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?;
    tx.execute("DELETE FROM device_authorizations", [])?;
    tx.execute("DELETE FROM device_tokens", [])?;
    tx.execute("DELETE FROM device_grants", [])?;
    for (user, count) in affected {
        audit(
            tx,
            "device.revoke_all",
            actor,
            Some(&user),
            &JValue::obj([
                ("count", JValue::Num(count as f64)),
                ("reason", JValue::from("feature-off")),
            ]),
            now,
        )?;
    }
    Ok(())
}
pub fn initialize(tx: &WriteTx<'_>, facts: &BootFacts, now: i64) -> Result<(), StoreError> {
    for (index, feature) in Feature::ALL.into_iter().enumerate() {
        let spec = server_features::spec(feature);
        if spec.legacy.is_none()
            || facts.env.get(index).copied().flatten().is_some()
            || typed_boolean(&tx.reader(), spec.setting)?.is_some()
        {
            continue;
        }
        if let Some(legacy) = selected(&tx.reader(), feature)? {
            if tx
                .set_feature_enabled(RustFeatureSettings::owned(), feature, legacy)
                .is_err()
            {
                // Do not expose trigger text/settings. Fallback is permitted only if the
                // original typed source can still be read; COMMIT remains authoritative.
                selected(&tx.reader(), feature)?;
                eprintln!("native feature legacy copy deferred; typed fallback retained");
            }
        }
    }
    let index = Feature::ALL
        .iter()
        .position(|f| *f == Feature::NativeClientAuth)
        .ok_or_else(super::unavailable)?;
    let stored = selected(&tx.reader(), Feature::NativeClientAuth)?;
    let evaluation = server_features::evaluate_with_context(
        Feature::NativeClientAuth,
        facts.env.get(index).copied().flatten(),
        stored.map(|v| if v { "true" } else { "false" }),
        if facts.trust_proxy {
            Availability::Available
        } else {
            Availability::Unavailable
        },
        None,
    );
    if !evaluation.enabled {
        revoke_all(tx, None, now)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_context::BrowserFact;
    use server_store::{db_path, rusqlite::Connection, RustAuth, Writer};
    type Error = Box<dyn std::error::Error>;
    fn fixture() -> Result<(tempfile::TempDir, Connection, Writer), Error> {
        let dir = tempfile::tempdir()?;
        let db = Connection::open(db_path(dir.path()))?;
        db.execute_batch(include_str!(
            "../../../server-store/tests/fixtures/node-schema.sql"
        ))?;
        let writer = Writer::open(dir.path(), RustAuth::owned())?;
        Ok((dir, db, writer))
    }
    fn facts() -> BootFacts {
        BootFacts {
            env: [None; 23],
            restart: [false; 2],
            browser: BrowserFact::Available,
            code: false,
            trust_proxy: true,
        }
    }
    fn pending(db: &Connection) -> Result<(), Error> {
        db.execute_batch("INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,created_at,expires_at,interval_ms,status) VALUES('dc','uc','synthetic','127.0.0.1','test',1,99999999,5000,'pending');")?;
        Ok(())
    }
    fn grant(db: &Connection) -> Result<(), Error> {
        db.execute_batch("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u1','one','one','One','member','synthetic','web1',1,1); INSERT INTO device_grants VALUES('g1','u1','test',1,1,99999999,'127.0.0.1','test'); INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at) VALUES('t1','g1','access',1,99999999);")?;
        Ok(())
    }
    fn count(db: &Connection, table: &str) -> Result<i64, Error> {
        // Test-only closed table names, never public input.
        Ok(db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
    #[test]
    fn startup_off_clears_pending_without_grants_and_audits_real_grant_counts() -> Result<(), Error>
    {
        let (_dir, db, writer) = fixture()?;
        pending(&db)?;
        writer.write(|tx| initialize(tx, &facts(), 123))?;
        assert_eq!(count(&db, "device_authorizations")?, 0);
        assert_eq!(count(&db, "audit_events")?, 0);
        pending(&db)?;
        grant(&db)?;
        writer.write(|tx| initialize(tx, &facts(), 456))?;
        for table in ["device_authorizations", "device_tokens", "device_grants"] {
            assert_eq!(count(&db, table)?, 0);
        }
        let event:(Option<String>,String,String,i64)=db.query_row("SELECT actor_user_id,target_user_id,detail,created_at FROM audit_events WHERE action='device.revoke_all'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        assert_eq!(
            event,
            (
                None,
                "u1".into(),
                "{\"count\":1,\"reason\":\"feature-off\"}".into(),
                456
            )
        );
        Ok(())
    }
    #[test]
    fn legacy_copy_and_revoke_roll_back_together_when_audit_fails() -> Result<(), Error> {
        let (_dir, db, writer) = fixture()?;
        pending(&db)?;
        grant(&db)?;
        db.execute_batch("INSERT INTO settings VALUES('feature:astraReview','true'); CREATE TRIGGER fail_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic'); END;")?;
        assert!(writer.write(|tx| initialize(tx, &facts(), 123)).is_err());
        for table in ["device_authorizations", "device_tokens", "device_grants"] {
            assert_eq!(count(&db, table)?, 1);
        }
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        db.execute_batch("DROP TRIGGER fail_audit")?;
        writer.write(|tx| initialize(tx, &facts(), 123))?;
        assert_eq!(count(&db, "device_grants")?, 0);
        assert_eq!(
            db.query_row(
                "SELECT value FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "true"
        );
        Ok(())
    }
    #[test]
    fn env_and_typed_rows_win_and_legacy_copy_failure_keeps_readable_fallback() -> Result<(), Error>
    {
        let (_dir, db, writer) = fixture()?;
        db.execute_batch("INSERT INTO settings VALUES('feature:astraReview','true'); INSERT INTO settings VALUES('feature:plannerReview','false'); INSERT INTO settings VALUES('feature:nativeClientAuth','true');")?;
        grant(&db)?;
        writer.write(|tx| initialize(tx, &facts(), 1))?;
        assert_eq!(count(&db, "device_grants")?, 1);
        writer.read(|r| {
            assert_eq!(selected(r, Feature::PlannerReview)?, Some(false));
            Ok(())
        })?;
        db.execute_batch("DELETE FROM settings WHERE key='feature:plannerReview'; CREATE TRIGGER fail_copy BEFORE INSERT ON settings WHEN NEW.key='feature:plannerReview' BEGIN SELECT RAISE(ABORT,'synthetic'); END;")?;
        writer.write(|tx| initialize(tx, &facts(), 2))?;
        writer.read(|r| {
            assert_eq!(selected(r, Feature::PlannerReview)?, Some(true));
            Ok(())
        })?;
        let mut locked = facts();
        let i = Feature::ALL
            .iter()
            .position(|f| *f == Feature::PlannerReview)
            .ok_or("flag")?;
        *locked.env.get_mut(i).ok_or("env")? = Some(false);
        db.execute_batch("DROP TRIGGER fail_copy")?;
        writer.write(|tx| initialize(tx, &locked, 3))?;
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        let mut no_proxy = facts();
        no_proxy.trust_proxy = false;
        writer.write(|tx| initialize(tx, &no_proxy, 4))?;
        assert_eq!(count(&db, "device_grants")?, 0);
        db.execute_batch("DELETE FROM settings WHERE key='feature:plannerReview'; UPDATE settings SET value=X'74727565' WHERE key='feature:astraReview';")?;
        writer.write(|tx| initialize(tx, &facts(), 5))?;
        writer.read(|r| {
            assert_eq!(selected(r, Feature::PlannerReview)?, None);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn registration_ready_requires_committed_cleanup_and_recovers_with_a_new_session(
    ) -> Result<(), Error> {
        use crate::feature_context::{
            source_fingerprint, wire, ContextSource, Control, Controller, FeatureRuntime,
        };
        use serde_json::json;
        use std::{
            sync::Arc,
            time::{Duration, Instant},
        };
        use uuid::Uuid;
        let (_dir, db, writer) = fixture()?;
        pending(&db)?;
        grant(&db)?;
        db.execute_batch("INSERT INTO settings VALUES('feature:astraReview','true'); CREATE TRIGGER fail_register_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic'); END;")?;
        let runtime = Arc::new(FeatureRuntime::new()?);
        let controller = Controller::new(Arc::clone(&runtime), [None; 23]);
        let start = "1".repeat(32);
        let request = Uuid::new_v4();
        let snapshot=writer.read(|r|Ok(json!({"sources":{"decision":source_fingerprint(r,ContextSource::Decision)?,"machines":source_fingerprint(r,ContextSource::InferenceMachines)?},"availability":{"stepSupervision":null,"toolGate":null,"chatFraming":null,"framingReasoner":null,"systemOneRouting":null}})))?;
        let mut message = json!({"op":"register","request":request.to_string(),"start":start,"incarnation":runtime.incarnation(),"boot":Uuid::new_v4().to_string(),"session":Uuid::new_v4().to_string(),"sequence":0,"facts":{"env":vec![Option::<bool>::None;23],"restart":[false,false],"browser":"available","code":false,"trustProxy":true},"snapshot":snapshot});
        let invoke = |body: &serde_json::Value, id| {
            wire::dispatch(
                body.to_string().as_bytes(),
                &start,
                &controller,
                &writer,
                &Control {
                    request: id,
                    deadline: Instant::now() + Duration::from_secs(2),
                    cancelled: Arc::default(),
                },
                |tx, facts| initialize(tx, facts, 123),
            )
        };
        assert!(invoke(&message, request).is_err());
        writer.read(|r| {
            assert!(controller.admission(r).is_err());
            Ok(())
        })?;
        assert_eq!(count(&db, "device_grants")?, 1);
        assert_eq!(count(&db, "device_authorizations")?, 1);
        db.execute_batch("DROP TRIGGER fail_register_audit")?;
        // A lost/failed registration cannot retry its cleanup into a Ready result.
        assert!(invoke(&message, request).is_err());
        assert_eq!(count(&db, "device_grants")?, 1);
        let recovery = Uuid::new_v4();
        *message.get_mut("request").ok_or("request")? = json!(recovery.to_string());
        *message.get_mut("session").ok_or("session")? = json!(Uuid::new_v4().to_string());
        let ready: serde_json::Value = serde_json::from_str(&invoke(&message, recovery)?)?;
        assert_eq!(ready.get("status"), Some(&json!("ready")));
        writer.read(|r| {
            controller.admission(r)?;
            Ok(())
        })?;
        assert_eq!(count(&db, "device_grants")?, 0);
        assert_eq!(count(&db, "device_authorizations")?, 0);
        assert_eq!(count(&db, "audit_events")?, 1);
        Ok(())
    }
}
