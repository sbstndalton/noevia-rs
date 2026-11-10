//! One boot-owned backup loop. No environment ownership switch; the paired Core build has no loop.
use crate::App;
use js_json::JValue;
use server_auth::Role;
use server_diary::{backup::Schedule, Tenant};
use server_projects::{source_storage::Failure, storage_latch as latch};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

fn fingerprint(t: &Tenant) -> Option<String> {
    t.storage_identity()
        .map(|s| latch::fingerprint(s.kind, s.base_url, s.bucket, s.username, &s.secret))
}

async fn run_user(app: Arc<App>, id: String, role: Role) -> Option<Option<JValue>> {
    let Some(diary) = app.diary.as_ref() else {
        return Some(None);
    };
    let Some(dir) = app.config.data_dir.clone() else {
        return Some(None);
    };
    let controller = app.feature_context.as_ref()?.controller();
    let (layer, config, user, data) = (
        Arc::clone(&app.identity),
        diary.sidecar().config().clone(),
        id.clone(),
        dir.clone(),
    );
    let tenant = tokio::task::spawn_blocking(move || {
        let store = match layer.store_blocking() {
            Ok(s) => s,
            Err(_) => return Some(Err(())),
        };
        // Same authenticated native Ready lease that gated Node's boot; loss stops probes.
        if store.read(|r| controller.admission(r)).is_err() {
            return None;
        }
        let tenant = match Tenant::load_user(&user, role, &store, &data, &config) {
            Ok(t) => t,
            Err(_) => return Some(Err(())),
        };
        if let Some(fp) = fingerprint(&tenant) {
            // Fail closed if the shared hold cannot be read.
            if !matches!(
                latch::blocked(&data, &user, &fp, server_projects::wall_ms()),
                Ok(None)
            ) {
                return None;
            }
        }
        Some(Ok(tenant))
    })
    .await;
    let tenant = match tenant {
        Ok(Some(Ok(t))) => t,
        Ok(None) => return None,
        _ => return Some(None),
    };
    let reply = diary.sidecar().storage_backup(&tenant).await;
    let progress = reply
        .as_ref()
        .ok()
        .filter(|r| r.ok)
        .and_then(|r| r.body.clone());
    let fp = fingerprint(&tenant);
    let _ = tokio::task::spawn_blocking(move || {
        // Core never settled a probe when fetchJson threw before producing a reply.
        if reply.is_err() {
            return;
        }
        if let Some(fp) = fp {
            let b = reply.as_ref().ok().and_then(|r| r.body.as_ref());
            let kind = b.and_then(|b| match b.get("code").as_str() {
                Some("storageLoginRejected") => Some("login"),
                Some("storageThrottled") => Some("throttled"),
                _ if b.get("loginRefused") == &JValue::Bool(true) => Some("login"),
                _ => None,
            });
            let now = server_projects::wall_ms();
            if let Some(kind) = kind {
                if kind == "throttled"
                    && b.is_some_and(|b| {
                        server_diary::routes::js_number(b.get("retryAfter")).is_err()
                    })
                {
                    return;
                }
                let retry_after = b
                    .and_then(|b| server_diary::routes::js_number(b.get("retryAfter")).ok())
                    .filter(|n| n.is_finite() && *n > 0.0)
                    .map(|n| n.ceil() as u64);
                let _ = latch::note(&dir, &id, &fp, Failure { kind, retry_after }, now);
            } else if reply.as_ref().is_ok_and(|r| r.ok)
                && b.is_some_and(|b| b.get("storageContacted") == &JValue::Bool(true))
            {
                let _ = latch::clear(&dir, &id);
            } else {
                let _ = latch::give_back_probe(&dir, &id, &fp, now);
            }
        }
    })
    .await;
    Some(progress)
}

pub fn spawn(app: &Arc<App>, mut stop: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let mut schedule = Schedule::default();
        let interval = Duration::from_secs(30);
        loop {
            tokio::select! { biased; _ = stop.wait_for(|s| *s) => break, _ = tokio::time::sleep(interval) => {} }
            let layer = Arc::clone(&app.identity);
            let writes = Arc::clone(&app.writes);
            // Native account-store readiness is required before any tenant is probed.
            let users = tokio::task::spawn_blocking(move || {
                writes.writer().ok()?;
                let store = layer.store_blocking().ok()?;
                store.read(|r| r.rows("SELECT u.id, u.role FROM users u JOIN user_features f ON f.user_id=u.id WHERE f.diary_enabled != 0 ORDER BY u.created_at", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))).ok()
            }).await.ok().flatten();
            let Some(users) = users else {
                continue;
            };
            let roles = Arc::new(
                users
                    .iter()
                    .cloned()
                    .collect::<std::collections::HashMap<_, _>>(),
            );
            let ids = users.into_iter().map(|(id, _)| id).collect();
            let now = server_projects::wall_ms().max(0) as u64;
            schedule
                .tick_until(
                    ids,
                    now,
                    30_000,
                    |id| {
                        let role = if roles.get(&id).is_some_and(|r| r == "admin") {
                            Role::Admin
                        } else {
                            Role::Member
                        };
                        run_user(Arc::clone(&app), id, role)
                    },
                    Some(stop.clone()),
                )
                .await;
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_ready_is_required_and_stop_joins_without_a_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            server_store::rusqlite::Connection::open(server_store::db_path(dir.path())).unwrap();
        db.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let cfg = crate::config::Config::from_lookup(|k| match k {
            "UI_DATA_DIR" => Some(dir.path().display().to_string()),
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:9".into()),
            "NOEVIA_START_ID" => Some("0123456789abcdef0123456789abcdef".into()),
            "DIARY_BASE_URL" => Some(base.clone()),
            _ => None,
        })
        .unwrap();
        let app = App::new(cfg);
        assert!(app.feature_context.is_some());
        assert!(run_user(
            app.clone(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
            Role::Admin
        )
        .await
        .is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
        for _ in 0..2 {
            let (tx, rx) = watch::channel(false);
            let task = spawn(&app, rx);
            tx.send(true).unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .is_ok());
        }
    }
}
