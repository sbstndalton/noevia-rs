//! `DELETE /api/admin/users/{id}`, the part that is the auth database's (sbstndalton/noevia#2190,
//! #2191 round 9): core auth.cjs `deleteUser(actorId, userId, username)`, which was Node's under
//! `exempt()`. The cleanup beyond the database (the account's folder, its Drive, MCP and ChatGPT
//! credentials, its Diary tenant) is the front's route, bins/noevia-server admin_user_routes.rs.
//!
//! One IMMEDIATE transaction, so the last-administrator check and the deletion cannot be split by
//! another writer (Node counted before it opened its transaction): the account must exist and the
//! typed name must equal its name exactly; the last active administrator (or the only one) cannot
//! go; the invitations and recovery links the account created are revoked first (their keys have no
//! cascade; the audit trail keeps the provenance); then the row, and with it every session,
//! passkey, device grant, app password, feature row, appearance, storage connection, tool policy,
//! sharing opt-in and Diary connector, by the foreign keys; then the audit row `user.delete`.

use crate::users;
use js_json::JValue;
use server_store::rusqlite::types::Value;
use server_store::{RustAccountDeletion, StoreError, WriteTx};

/// How a deletion ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// The account is gone.
    Done,
    /// No such account, or the name typed is not its name (`deleteUser` answers false).
    Refused,
    /// It is the last active administrator (`deleteUser` throws `cannot delete the last active administrator`).
    LastAdmin,
}

/// core's message for [`Deleted::LastAdmin`].
pub const LAST_ADMIN: &str = "cannot delete the last active administrator";

/// Deletes `target` for `actor`. `typed` is the request's `username` as JSON gave it.
pub fn delete_user(
    tx: &WriteTx<'_>,
    actor: &str,
    target: &str,
    typed: &JValue,
    now: i64,
) -> Result<Deleted, StoreError> {
    let r = tx.reader();
    let row = r.row(
        "SELECT username, role, disabled_at FROM users WHERE id=?1",
        [target],
        |row| {
            Ok((
                row.get::<_, Value>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Value>(2)?,
            ))
        },
    )?;
    let Some((username, role, disabled)) = row else {
        return Ok(Deleted::Refused);
    };
    // `user.username !== username`: a string equal to the stored name, byte for byte.
    if !matches!((&username, typed.as_str()), (Value::Text(have), Some(want)) if have == want) {
        return Ok(Deleted::Refused);
    }
    if matches!(&role, Value::Text(t) if t == "admin") {
        let count = |sql: &str| -> Result<i64, StoreError> {
            Ok(r.row(sql, [], |row| row.get::<_, i64>(0))?.unwrap_or(0))
        };
        if count("SELECT count(*) FROM users WHERE role='admin'")? <= 1
            || (!server_auth::js::truthy(&disabled)
                && count("SELECT count(*) FROM users WHERE role='admin' AND disabled_at IS NULL")?
                    <= 1)
        {
            return Ok(Deleted::LastAdmin);
        }
    }
    // Issued tokens reference their creator without ON DELETE CASCADE: revoke them with the
    // account, retaining provenance in audit_events.
    tx.execute("DELETE FROM invitations WHERE created_by=?1", [target])?;
    tx.execute("DELETE FROM recoveries WHERE created_by=?1", [target])?;
    tx.delete_user(RustAccountDeletion::owned(), target)?;
    users::audit(
        tx,
        "user.delete",
        Some(actor),
        Some(target),
        &JValue::obj::<&str>([]),
        now,
    )?;
    Ok(Deleted::Done)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use server_store::rusqlite::Connection;
    use server_store::{RustAuth, Writer};

    const A: &str = "00000000-0000-4000-8000-00000000000a";
    const B: &str = "00000000-0000-4000-8000-00000000000b";
    const M: &str = "00000000-0000-4000-8000-00000000000c";

    fn fixture() -> (tempfile::TempDir, Connection, Writer) {
        let dir = tempfile::tempdir().unwrap();
        let node = Connection::open(server_store::db_path(dir.path())).unwrap();
        node.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        for (id, name, role) in [
            (A, "Alpha", "admin"),
            (B, "Beta", "admin"),
            (M, "Member", "member"),
        ] {
            node.execute(
                "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?2,lower(?2),?2,?3,'x',?1,1,1)",
                (id, name, role),
            )
            .unwrap();
            node.execute(
                "INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,'c',1,1,9,'ua','ip')",
                (format!("s-{id}"), id),
            )
            .unwrap();
        }
        let writer = Writer::open(dir.path(), RustAuth::owned()).unwrap();
        (dir, node, writer)
    }

    fn run(w: &Writer, target: &str, typed: &str) -> Deleted {
        w.write(|tx| delete_user(tx, A, target, &JValue::from(typed), 5))
            .unwrap()
    }

    fn n(node: &Connection, sql: &str) -> i64 {
        node.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_member_goes_with_its_sessions_and_the_invitations_it_made_and_the_audit_row_stays() {
        let (_dir, node, w) = fixture();
        node.execute_batch(&format!(
            "INSERT INTO invitations(token_hash,created_by,role,expires_at,created_at) VALUES('i1','{M}','member',9,1);
             INSERT INTO recoveries(token_hash,user_id,created_by,expires_at,created_at) VALUES('r1','{A}','{M}',9,1);"
        ))
        .unwrap();
        assert_eq!(run(&w, M, "Member"), Deleted::Done);
        assert_eq!(
            n(&node, &format!("SELECT count(*) FROM users WHERE id='{M}'")),
            0
        );
        assert_eq!(
            n(
                &node,
                &format!("SELECT count(*) FROM sessions WHERE user_id='{M}'")
            ),
            0
        );
        assert_eq!(n(&node, "SELECT count(*) FROM invitations"), 0);
        assert_eq!(n(&node, "SELECT count(*) FROM recoveries"), 0);
        assert_eq!(
            n(&node, "SELECT count(*) FROM sessions"),
            2,
            "the others keep theirs"
        );
        let (action, actor, target, detail): (String, String, String, String) = node
            .query_row(
                "SELECT action, actor_user_id, target_user_id, detail FROM audit_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (
                action.as_str(),
                actor.as_str(),
                target.as_str(),
                detail.as_str()
            ),
            ("user.delete", A, M, "{}")
        );
    }

    #[test]
    fn the_name_must_match_exactly_and_the_account_must_exist() {
        let (_dir, node, w) = fixture();
        for typed in ["member", "Member ", "", "Beta"] {
            assert_eq!(run(&w, M, typed), Deleted::Refused, "{typed:?}");
        }
        let not_text = w
            .write(|tx| delete_user(tx, A, M, &JValue::Num(5.0), 5))
            .unwrap();
        assert_eq!(not_text, Deleted::Refused);
        let missing = w
            .write(|tx| delete_user(tx, A, M, &JValue::Undefined, 5))
            .unwrap();
        assert_eq!(missing, Deleted::Refused);
        assert_eq!(run(&w, "nobody", "Member"), Deleted::Refused);
        assert_eq!(n(&node, "SELECT count(*) FROM users"), 3);
        assert_eq!(
            n(&node, "SELECT count(*) FROM audit_events"),
            0,
            "a refusal leaves no trace"
        );
    }

    #[test]
    fn the_last_active_administrator_cannot_be_deleted_by_anyone() {
        let (_dir, node, w) = fixture();
        // Two administrators: one may go.
        assert_eq!(run(&w, B, "Beta"), Deleted::Done);
        // The only one left, deleting itself or by itself: refused.
        assert_eq!(run(&w, A, "Alpha"), Deleted::LastAdmin);
        assert_eq!(
            n(&node, &format!("SELECT count(*) FROM users WHERE id='{A}'")),
            1
        );
        // Two administrators, the other one disabled: the active one is the last active one.
        node.execute_batch(&format!(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at,disabled_at) VALUES('{B}','Beta','beta','Beta','admin','x','w',1,1,7);"
        ))
        .unwrap();
        assert_eq!(run(&w, A, "Alpha"), Deleted::LastAdmin);
        // A disabled administrator may be deleted while another is active.
        assert_eq!(run(&w, B, "Beta"), Deleted::Done);
        assert_eq!(n(&node, "SELECT count(*) FROM users WHERE role='admin'"), 1);
    }
}
