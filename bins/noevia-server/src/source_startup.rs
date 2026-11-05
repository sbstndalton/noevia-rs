//! Startup source migration runs to completion before serving. Failures are per account.
use crate::App;
use std::sync::Arc;

pub async fn run(app: &Arc<App>) {
    let Some(data_dir) = app.config.data_dir.clone() else {
        return;
    };
    let a = Arc::clone(app);
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let writer = a.writes.writer().map_err(|_| "account store unavailable")?;
        let users: Vec<String> = writer
            .read(|r| r.rows("SELECT id FROM users", [], |row| row.get::<_, String>(0)))
            .map_err(|_| "account list unavailable")?;
        for id in users {
            let Ok(dir) = server_projects::store::user_dir(&data_dir, &id) else {
                continue;
            };
            if let Err(error) =
                server_projects::source_startup::migrate(&dir, a.config.rust_projects)
            {
                eprintln!(
                    "[sources] could not move source text out of projects.json for {id}: {error:?}"
                );
            }
        }
        Ok(())
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        eprintln!("[sources] startup migration skipped");
    }
}
