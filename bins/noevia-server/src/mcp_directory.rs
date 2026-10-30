//! The MCP directory table (sbstndalton/noevia#2202 round 2), ported from core `directory-mcp.cjs`:
//! the servers an administrator added (`directory_mcp_servers`), the shared key slot of each, and the
//! OAuth and per-account flags. The Rust front is the table's single writer (`RustMcpCredentials`,
//! per statement); Node re-reads it (core `directory-mcp.cjs` through `shared-reread.cjs`) and
//! keeps only the chat-time MCP client.
//!
//! Every write is one transaction that checks what it relies on, and the shared key is replaced
//! only while it still holds the ciphertext it started from, so two administrators cannot both win.
//! A shared key is sealed unbound (`enc:v1`, as core sealed it) so a rollback to a Node that owns
//! the table reads it. Nothing here logs or returns a key.
use crate::mcp_broker::{
    check_header_values, declared_of, fail, now_ms, Declared, Fail, Mcp,
};
use crate::mcp_egress;
use crate::mcp_tools::{self, Tool};
use crate::App;
use js_json::JValue;
use server_store::RustMcpCredentials;
use std::sync::Arc;

/// core directory-mcp.cjs `MAX_SERVERS`.
pub const MAX_SERVERS: usize = 20;

/// One row of `directory_mcp_servers`.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub registry_name: String,
    pub title: String,
    pub url: String,
    pub added_by: Option<String>,
    pub added_at: i64,
    pub headers_enc: Option<String>,
    pub oauth: bool,
    pub personal: bool,
    /// `declared_json`, an array.
    pub declared: JValue,
}

/// A server to add.
pub struct New {
    pub registry_name: String,
    pub title: String,
    pub url: String,
    /// The declared key headers as the registry (or the custom form) gave them: an array of
    /// `{ name, required, secret, description, template }`.
    pub declared: JValue,
    /// The values the administrator typed (`{ name: value }`).
    pub values: JValue,
    pub oauth: bool,
    pub personal: bool,
}

/// What one listing of a server's tools came to.
#[derive(Debug, Clone)]
pub struct Found {
    pub tools: Vec<Tool>,
}

/// core `idFor(name)`: `dir-` and the name lower-cased, runs of anything but `[a-z0-9]` made one `-`,
/// cut at 34 characters.
pub fn id_for(name: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c);
        } else {
            gap = true;
        }
    }
    // core trims one dash at each end and then cuts: the cut may end on a dash. (A trailing gap added
    // none and a leading one was never added, which is that trim.)
    let cut: String = slug.chars().take(34).collect();
    format!("dir-{cut}")
}

/// A declared header as core stores it: `{ name, required, secret, description, template }`.
pub fn normalize_declared(items: &JValue) -> JValue {
    let JValue::Arr(items) = items else {
        return JValue::Arr(Vec::new());
    };
    JValue::Arr(
        items
            .iter()
            .filter_map(|h| {
                let name = h.get("name").as_str()?;
                Some(JValue::obj([
                    ("name", JValue::Str(name.to_owned())),
                    ("required", JValue::Bool(h.get("required").truthy())),
                    (
                        "secret",
                        JValue::Bool(h.get("secret").as_bool() != Some(false)),
                    ),
                    (
                        "description",
                        JValue::Str(js_json::js_slice(
                            h.get("description").as_str().unwrap_or(""),
                            300,
                        )),
                    ),
                    (
                        "template",
                        match h.get("template") {
                            JValue::Str(t) if !t.is_empty() => JValue::Str(t.clone()),
                            _ => JValue::Null,
                        },
                    ),
                ]))
            })
            .collect(),
    )
}

fn headers_json(headers: &[(String, String)]) -> String {
    js_json::stringify(&JValue::obj(
        headers
            .iter()
            .map(|(k, v)| (k.clone(), JValue::Str(v.clone()))),
    ))
    .unwrap_or_default()
}

impl Mcp {
    /// The directory rows, oldest first. A missing table is an empty directory (core makes it); one
    /// that exists but cannot be read is a transient failure, never "no servers".
    pub fn dir_rows(&self, app: &App) -> Result<Vec<Row>, Fail> {
        let unreadable = |_| crate::mcp_broker::auth_error("transient");
        let writer = app.writes.writer().map_err(unreadable)?;
        let made = writer
            .read(|r| {
                r.row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='directory_mcp_servers'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        if made.is_none() {
            return Ok(Vec::new());
        }
        writer
            .read(|r| {
                r.rows(
                    "SELECT id, registry_name, title, url, added_by, added_at, headers_enc, oauth, personal, declared_json FROM directory_mcp_servers ORDER BY added_at, rowid",
                    [],
                    row_of,
                )
            })
            .map_err(|_| crate::mcp_broker::auth_error("transient"))
    }

    pub fn dir_find(&self, app: &App, id: &str) -> Result<Option<Row>, Fail> {
        Ok(self.dir_rows(app)?.into_iter().find(|r| r.id == id))
    }

    /// The names of the headers the shared key holds (core `keyHeaders`), never their values.
    fn key_header_names(&self, row: &Row) -> Vec<String> {
        let Some(raw) = row.headers_enc.as_deref() else {
            return Vec::new();
        };
        match self.open_json(raw, None) {
            Some(JValue::Obj(items)) => items.into_iter().map(|(k, _)| k).collect(),
            _ => Vec::new(),
        }
    }

    /// core `list()` for one row: the shape the admin page reads (no key, no ciphertext).
    pub fn list_json(&self, row: &Row) -> JValue {
        JValue::obj([
            ("id", JValue::Str(row.id.clone())),
            ("registryName", JValue::Str(row.registry_name.clone())),
            ("title", JValue::Str(row.title.clone())),
            ("url", JValue::Str(row.url.clone())),
            (
                "addedBy",
                row.added_by.clone().map_or(JValue::Null, JValue::Str),
            ),
            ("addedAt", JValue::Num(row.added_at as f64)),
            ("oauth", JValue::Bool(row.oauth)),
            ("personal", JValue::Bool(row.personal)),
            ("declaredHeaders", row.declared.clone()),
            (
                "keyHeaders",
                JValue::Arr(
                    self.key_header_names(row)
                        .into_iter()
                        .map(JValue::Str)
                        .collect(),
                ),
            ),
        ])
    }

    /// The shared key's headers, opened (a discovery with the stored key). Empty when there is none.
    pub fn shared_headers(&self, row: &Row) -> Vec<(String, String)> {
        let Some(raw) = row.headers_enc.as_deref() else {
            return Vec::new();
        };
        match self.open_json(raw, None) {
            Some(JValue::Obj(items)) => items
                .into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_owned())))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// core `add` (blocking): the checks in core's order, then one INSERT and its audit row in the
    /// same transaction. A personal server keeps no shared key.
    pub fn add_row(&self, app: &App, new: &New, actor: &str) -> Result<Row, Fail> {
        let declared = declared_of(&new.declared);
        let headers = check_header_values(&declared, &new.values);
        let id = id_for(&new.registry_name);
        let title: String = js_json::js_slice(
            if new.title.is_empty() {
                &new.registry_name
            } else {
                &new.title
            },
            80,
        );
        let qa = self.egress().qa_loopback();
        let writer = app
            .writes
            .writer()
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        let sealed_for = |headers: &[(String, String)]| -> Result<Option<String>, Fail> {
            if headers.is_empty() {
                return Ok(None);
            }
            self.seal(&headers_json(headers), None)
                .map(Some)
                .map_err(|_| fail("Keys cannot be stored on this server.", 500))
        };
        let declared_json =
            js_json::stringify(&normalize_declared(&new.declared)).unwrap_or_else(|| "[]".into());
        let now = now_ms();
        writer
            .write(|tx| {
                let r = tx.reader();
                let rows: Vec<(String, String)> = r.rows(
                    "SELECT id, registry_name FROM directory_mcp_servers",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if rows.len() >= MAX_SERVERS {
                    return Ok(Err(fail(
                        format!("At most {MAX_SERVERS} directory servers can be added."),
                        409,
                    )));
                }
                if !mcp_egress::hosted_url_ok(&new.url, qa) {
                    return Ok(Err(fail(
                        "Only hosted servers at an https address can be added.",
                        400,
                    )));
                }
                if rows.iter().any(|(_, name)| *name == new.registry_name) {
                    return Ok(Err(fail("That server is already added.", 409)));
                }
                if rows.iter().any(|(existing, _)| *existing == id) {
                    return Ok(Err(fail(
                        "A server with a similar name is already added.",
                        409,
                    )));
                }
                let headers = match &headers {
                    Ok(h) => h,
                    Err(e) => return Ok(Err(e.clone())),
                };
                let enc = if new.personal {
                    None
                } else {
                    match sealed_for(headers) {
                        Ok(e) => e,
                        Err(e) => return Ok(Err(e)),
                    }
                };
                tx.mcp_directory_insert(
                    RustMcpCredentials::owned(),
                    &id,
                    &new.registry_name,
                    &title,
                    &new.url,
                    Some(actor),
                    now,
                    enc.as_deref(),
                    new.oauth,
                    new.personal,
                    &declared_json,
                )?;
                let detail = js_json::stringify(&JValue::obj([
                    ("registryName", JValue::Str(new.registry_name.clone())),
                    ("url", JValue::Str(new.url.clone())),
                    (
                        "keyHeaders",
                        JValue::Arr(headers.iter().map(|(k, _)| JValue::Str(k.clone())).collect()),
                    ),
                ]))
                .unwrap_or_else(|| "{}".into());
                tx.execute(
                    "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                    server_store::rusqlite::params![actor, "mcp.directory.add", detail, now],
                )?;
                Ok(Ok(()))
            })
            .map_err(|_| crate::mcp_broker::auth_error("transient"))??;
        self.dir_find(app, &id)?
            .ok_or_else(|| crate::mcp_broker::auth_error("transient"))
    }

    /// core `setKeys` (blocking): replaces the shared key with `values` (checked against `declared`),
    /// only while the stored one is still the one this started from.
    pub fn set_row_keys(
        &self,
        app: &App,
        id: &str,
        declared: &[Declared],
        values: &JValue,
        actor: &str,
    ) -> Result<(), Fail> {
        let Some(row) = self.dir_find(app, id)? else {
            return Err(fail("No such server.", 404));
        };
        let headers = check_header_values(declared, values)?;
        let enc = if headers.is_empty() {
            None
        } else {
            Some(
                self.seal(&headers_json(&headers), None)
                    .map_err(|_| fail("Keys cannot be stored on this server.", 500))?,
            )
        };
        let writer = app
            .writes
            .writer()
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        let now = now_ms();
        let swapped = writer
            .write(|tx| {
                let n = tx.mcp_directory_swap_headers(
                    RustMcpCredentials::owned(),
                    id,
                    enc.as_deref(),
                    row.headers_enc.as_deref(),
                    Some(row.added_at),
                )?;
                if n == 1 {
                    let detail = js_json::stringify(&JValue::obj([
                        ("id", JValue::Str(id.to_owned())),
                        (
                            "keyHeaders",
                            JValue::Arr(
                                headers.iter().map(|(k, _)| JValue::Str(k.clone())).collect(),
                            ),
                        ),
                    ]))
                    .unwrap_or_else(|| "{}".into());
                    tx.execute(
                        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                        server_store::rusqlite::params![actor, "mcp.directory.keys", detail, now],
                    )?;
                }
                Ok(n)
            })
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        if swapped == 1 {
            Ok(())
        } else {
            Err(fail(
                "Another change to this server's key landed first. Try again.",
                409,
            ))
        }
    }

    /// core `remove` (blocking). The sign-ins, the app and every account's key are the caller's to
    /// forget first.
    pub fn remove_row(&self, app: &App, id: &str, actor: &str) -> Result<Row, Fail> {
        let Some(row) = self.dir_find(app, id)? else {
            return Err(fail("No such server.", 404));
        };
        let writer = app
            .writes
            .writer()
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        let now = now_ms();
        let detail = js_json::stringify(&JValue::obj([(
            "registryName",
            JValue::Str(row.registry_name.clone()),
        )]))
        .unwrap_or_else(|| "{}".into());
        let n = writer
            .write(|tx| {
                let n = tx.mcp_directory_delete(RustMcpCredentials::owned(), id)?;
                if n == 1 {
                    tx.execute(
                        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                        server_store::rusqlite::params![actor, "mcp.directory.remove", detail, now],
                    )?;
                }
                Ok(n)
            })
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        if n == 1 {
            Ok(row)
        } else {
            Err(fail("No such server.", 404))
        }
    }

    /// A rollback of a server this request added: removed only if it is still the row it added.
    /// `Ok(false)`: nothing was there to remove (another administrator removed or replaced it first).
    /// `Err`: the database could not be written, which the caller must not mistake for "nothing there".
    pub fn remove_added(&self, app: &App, row: &Row) -> Result<bool, Fail> {
        let writer = app
            .writes
            .writer()
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        let n = writer
            .write(|tx| {
                tx.mcp_directory_delete_added(RustMcpCredentials::owned(), &row.id, row.added_at)
            })
            .map_err(|_| crate::mcp_broker::auth_error("transient"))?;
        Ok(n == 1)
    }

    /// core `discoverOneServer`'s listing: the server's tools, the usable ones in its order. The error
    /// is in core's words.
    pub async fn discover_server(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Found, String> {
        let listed = mcp_egress::list_tools(self.egress(), url, headers, &self.rpc_id).await?;
        let (tools, _dropped) = mcp_tools::usable(&listed);
        Ok(Found { tools })
    }

    /// core `forget` for a removed server, as a closure-free async call the routes use.
    pub async fn forget_all(self: &Arc<Self>, app: &Arc<App>, id: &str) -> Result<(), Fail> {
        self.forget_server(app, id).await
    }
}

fn row_of(row: &server_store::rusqlite::Row<'_>) -> server_store::rusqlite::Result<Row> {
    let declared: String = row.get(9)?;
    Ok(Row {
        id: row.get(0)?,
        registry_name: row.get(1)?,
        title: row.get(2)?,
        url: row.get(3)?,
        added_by: row.get(4)?,
        added_at: row.get(5)?,
        headers_enc: row.get(6)?,
        oauth: row.get::<_, i64>(7)? != 0,
        personal: row.get::<_, i64>(8)? != 0,
        declared: js_json::parse(&declared)
            .ok()
            .filter(|v| matches!(v, JValue::Arr(_)))
            .unwrap_or(JValue::Arr(Vec::new())),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_core_id_for() {
        for (name, id) in [
            (
                "io.github.acme/Weather MCP",
                "dir-io-github-acme-weather-mcp",
            ),
            (
                "url:https://mcp.example.com/mcp",
                "dir-url-https-mcp-example-com-mcp",
            ),
            ("--x--", "dir-x"),
            ("ÅÄÖ", "dir-"),
            ("", "dir-"),
        ] {
            assert_eq!(id_for(name), id, "{name}");
        }
        // Cut at 34 characters of the slug.
        let long = id_for(&"a".repeat(50));
        assert_eq!(long, format!("dir-{}", "a".repeat(34)));
        // core trims first and cuts after, so a cut that lands on a dash keeps it.
        let dashed = id_for(&format!("{}-{}", "a".repeat(33), "b".repeat(5)));
        assert_eq!(dashed, format!("dir-{}-", "a".repeat(33)));
    }

    #[test]
    fn declared_headers_are_stored_the_way_core_stored_them() {
        let items = js_json::parse(
            r#"[{"name":"X-Api-Key","required":1,"description":"d","template":"Bearer {k}"},{"name":"Y","secret":false,"description":null},{"nope":1}]"#,
        )
        .unwrap();
        assert_eq!(
            js_json::stringify(&normalize_declared(&items)).unwrap(),
            r#"[{"name":"X-Api-Key","required":true,"secret":true,"description":"d","template":"Bearer {k}"},{"name":"Y","required":false,"secret":false,"description":"","template":null}]"#
        );
    }
}
