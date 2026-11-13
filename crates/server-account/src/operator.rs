//! The operator's invitation (sbstndalton/noevia#2582): `noevia-server --operator-invitation
//! <member|admin>`, run with shell access inside the web container, makes ONE invitation for an
//! install that already has accounts. It writes what `POST /api/admin/invitations` writes, through
//! the same [`Writer`] (its table authorizer, SQLite's locking and busy timeout, so it is safe
//! next to a running server) and the same insert ([`crate::account::insert_invitation`]): a digest
//! of a 32-byte random token, the 24-hour expiry, single use. It never creates an account, a
//! session or a password; the invitation is accepted by the normal `POST
//! /api/auth/invitations/accept`.
//!
//! `invitations.created_by` must name an account, so the row records the earliest active
//! administrator (the account the admin route would have needed); the audit event has no actor
//! and its detail names the command (`"via":"operator-invitation"`). There is no route to this:
//! only the binary's command line reaches it.

use crate::account::insert_invitation;
use crate::open::user_count;
use crate::util;
use js_json::JValue;
use server_store::{StoreError, Writer};

/// What the audit detail's `via` names.
pub const VIA: &str = "operator-invitation";

/// The roles an invitation may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Member,
    Admin,
}

impl Role {
    /// `member` or `admin`, exactly; anything else is refused.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "member" => Some(Role::Member),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Member => "member",
            Role::Admin => "admin",
        }
    }
}

/// The invitation made: the token is shown once and stored only as its digest.
#[derive(Debug)]
pub struct Invitation {
    pub token: String,
    pub expires_at: i64,
}

/// Why no invitation was made.
#[derive(Debug)]
pub enum Refusal {
    /// No account exists: first-run setup makes the first one.
    EmptyStore,
    /// Accounts exist but no active administrator could have made the invitation.
    NoActiveAdministrator,
    Store(StoreError),
}

impl From<StoreError> for Refusal {
    fn from(e: StoreError) -> Self {
        Refusal::Store(e)
    }
}

/// Makes one invitation for `role`, checked and written in one write transaction.
pub fn create(writer: &Writer, role: Role, now: i64) -> Result<Invitation, Refusal> {
    let token = util::random_token(32);
    let detail = JValue::obj([
        ("role", JValue::from(role.as_str())),
        ("via", JValue::from(VIA)),
    ]);
    let out = writer.write(|tx| {
        let r = tx.reader();
        if user_count(&r)? == 0 {
            return Ok(Err(Refusal::EmptyStore));
        }
        // The account the legacy token resolves to (server-auth identity.rs): the earliest active
        // administrator. Disabling or deleting it removes its open invitations, this one included.
        let creator = r.row(
            "SELECT id FROM users WHERE role='admin' AND disabled_at IS NULL ORDER BY created_at, id LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )?;
        let Some(creator) = creator else {
            return Ok(Err(Refusal::NoActiveAdministrator));
        };
        let expires_at = insert_invitation(tx, &token, &creator, None, role.as_str(), &detail, now)?;
        Ok(Ok(expires_at))
    })?;
    let expires_at = out?;
    Ok(Invitation { token, expires_at })
}
