//! Destination-owned effects and incidents use the SAME publication transaction as INI CAS.
//! No status listing can discharge an uncertain dispatch. Only its enrolled owner may report
//! terminal settlement; recovery admission remains closed while an effect can still execute.
use super::{conflict, io_error, Fence, Guard, Receipt};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// This exact destination owner has received the operation and may arm it once.
    Received,
    /// The same operation was already enrolled. Do not dispatch it again.
    Existing,
}

impl Guard {
    pub fn incident(&self) -> std::io::Result<Option<String>> {
        self._connection
            .query_row(
                "SELECT id FROM preset_incident WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(io_error)
    }
    /// Replace the incident identity. Late completion of an older incident cannot clear this one.
    pub fn save_incident(&self, id: &str) -> std::io::Result<()> {
        self._connection.execute("INSERT INTO preset_incident VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET id=excluded.id", [id]).map_err(io_error)?;
        Ok(())
    }
    pub fn clear_incident(&self, id: &str) -> std::io::Result<bool> {
        if self.effects_pending()? {
            return Ok(false);
        }
        Ok(self
            ._connection
            .execute(
                "DELETE FROM preset_incident WHERE singleton=1 AND id=?1",
                [id],
            )
            .map_err(io_error)?
            == 1)
    }
    pub fn effects_pending(&self) -> std::io::Result<bool> {
        self._connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM preset_operations WHERE effect_pending<>0)",
                [],
                |r| r.get(0),
            )
            .map_err(io_error)
    }
    /// Mark an unknown effect BEFORE returning exclusion. This fences the native generation,
    /// preserves its operation owner and creates a fresh incident even when one already exists.
    pub fn raise_recovery(&self, fence: &Fence, incident: &str) -> std::io::Result<()> {
        let receipt = self.status(fence)?.ok_or_else(conflict)?;
        if matches!(
            receipt.state.as_str(),
            "settled" | "conflict" | "reconciled"
        ) {
            return Ok(());
        }
        self._connection
            .execute(
                "UPDATE preset_operations SET state='recovery' WHERE id=?1",
                [&fence.operation],
            )
            .map_err(io_error)?;
        self.save_incident(incident)?;
        let mut authority = self.authority()?.ok_or_else(conflict)?;
        authority.active = false;
        authority.recovery = true;
        self.save_authority(&authority)
    }
    pub fn refuse_unstarted(&self, fence: &Fence, status: u16) -> std::io::Result<()> {
        let receipt = self.status(fence)?.ok_or_else(conflict)?;
        if receipt.owner.is_some() || receipt.state != "issued" {
            return Err(conflict());
        }
        self._connection.execute("UPDATE preset_operations SET state='settled',outcome='refused',http_status=?2 WHERE id=?1", params![fence.operation,status]).map_err(io_error)?;
        Ok(())
    }
    /// A running destination reads the INI under this logical barrier. The SQLite transaction
    /// itself is never held across HTTP; ALL migrated writers consult the durable pending bit.
    pub fn publication_idle(&self, operation: Option<&str>) -> std::io::Result<()> {
        self.authority()?;
        let busy: bool = self._connection.query_row("SELECT EXISTS(SELECT 1 FROM preset_operations WHERE id IS NOT ?1 AND (effect_pending<>0 OR (kind IN ('pull','register') AND state IN ('issued','received','running','recovery'))))", [operation], |r| r.get(0)).map_err(io_error)?;
        if busy {
            Err(conflict())
        } else {
            Ok(())
        }
    }
    /// Enroll a single destination owner and immutable target, before the remote dispatch.
    pub fn claim(&self, fence: &Fence, owner: &str, target: &str) -> std::io::Result<Claim> {
        if owner.is_empty() || owner.len() > 128 || target.is_empty() || target.len() > 4096 {
            return Err(conflict());
        }
        let receipt = self.status(fence)?.ok_or_else(conflict)?;
        if let Some(old_owner) = &receipt.owner {
            if old_owner != owner || receipt.target.as_deref() != Some(target) {
                return Err(conflict());
            }
            return Ok(Claim::Existing);
        }
        self.validate(fence)?;
        if receipt.state != "issued" {
            return Err(conflict());
        }
        self._connection.execute("UPDATE preset_operations SET owner=?2,target=?3,state='received' WHERE id=?1 AND state='issued'", params![fence.operation,owner,target]).map_err(io_error)?;
        Ok(Claim::Received)
    }
    /// The only transition that authorizes this owner to dispatch. A duplicate cannot arm again.
    /// The caller checks the actual INI hash in this transaction before arming a reload.
    pub fn arm(&self, fence: &Fence, owner: &str) -> std::io::Result<()> {
        self.validate(fence)?;
        if self._connection.execute("UPDATE preset_operations SET state='running',effect_pending=1 WHERE id=?1 AND owner=?2 AND state='received'", params![fence.operation,owner]).map_err(io_error)? != 1 { return Err(conflict()); }
        Ok(())
    }
    /// Only the enrolled destination can report causal terminal evidence. This bookkeeping may
    /// finish after lease loss; it never clears an incident or grants further execution coverage.
    pub fn finish_owned(
        &self,
        fence: &Fence,
        owner: &str,
        outcome: &str,
        http_status: Option<u16>,
    ) -> std::io::Result<()> {
        if !matches!(outcome, "completed" | "failed" | "refused") {
            return Err(conflict());
        }
        let receipt = self.status(fence)?.ok_or_else(conflict)?;
        if receipt.owner.as_deref() != Some(owner) {
            return Err(conflict());
        }
        if matches!(receipt.state.as_str(), "settled" | "conflict") {
            return Ok(());
        }
        self._connection.execute("UPDATE preset_operations SET state='settled',effect_pending=0,outcome=?3,http_status=?4 WHERE id=?1 AND owner=?2 AND state IN ('received','running','recovery')", params![fence.operation,owner,outcome,http_status]).map_err(io_error)?;
        Ok(())
    }
    pub fn owned_status(&self, fence: &Fence, owner: &str) -> std::io::Result<Option<Receipt>> {
        Ok(self
            .status(fence)?
            .filter(|r| r.owner.as_deref() == Some(owner)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn rig() -> (tempfile::TempDir, Guard, Fence) {
        let dir = tempfile::tempdir().unwrap();
        let guard = Guard::at(&dir.path().join("authority.sqlite")).unwrap();
        guard.authority().unwrap();
        guard
            .save_authority(&super::super::AuthorityRecord {
                incarnation: "front".into(),
                lease: "lease".into(),
                generation: 1,
                active: true,
                until_ms: super::super::now_ms() + 60_000,
                recovery: false,
            })
            .unwrap();
        let fence = Fence {
            incarnation: "front".into(),
            lease: "lease".into(),
            generation: 1,
            operation: "operation".into(),
            kind: "reload".into(),
            expected_revision: None,
        };
        guard.begin(&fence).unwrap();
        (dir, guard, fence)
    }
    #[test]
    fn only_the_enrolled_effect_owner_can_terminalize_and_duplicates_cannot_arm() {
        let (_dir, guard, fence) = rig();
        assert_eq!(
            guard.claim(&fence, "owner", "target").unwrap(),
            Claim::Received
        );
        assert_eq!(
            guard.claim(&fence, "owner", "target").unwrap(),
            Claim::Existing
        );
        assert!(guard.claim(&fence, "other", "target").is_err());
        guard.arm(&fence, "owner").unwrap();
        assert!(guard.arm(&fence, "owner").is_err());
        assert!(guard.settle(&fence).is_err());
        assert!(guard
            .finish_owned(&fence, "other", "completed", Some(200))
            .is_err());
        assert!(guard.publication_idle(None).is_err());
        assert!(guard.effects_pending().unwrap());
        guard
            .finish_owned(&fence, "owner", "completed", Some(200))
            .unwrap();
        assert!(!guard.effects_pending().unwrap());
        assert!(guard.publication_idle(None).is_ok());
        assert_eq!(
            guard.status(&fence).unwrap().unwrap().outcome.as_deref(),
            Some("completed")
        );
    }
    #[test]
    fn an_unknown_effect_and_a_newer_incident_survive_stale_completion() {
        let (_dir, guard, fence) = rig();
        guard.claim(&fence, "owner", "target").unwrap();
        guard.arm(&fence, "owner").unwrap();
        guard.raise_recovery(&fence, "A").unwrap();
        assert!(!guard.clear_incident("A").unwrap());
        assert!(guard.clear_operations().is_err());
        guard.raise_recovery(&fence, "B").unwrap();
        assert!(!guard.clear_incident("A").unwrap());
        assert_eq!(guard.incident().unwrap().as_deref(), Some("B"));
        guard
            .finish_owned(&fence, "owner", "failed", Some(503))
            .unwrap();
        assert!(!guard.clear_incident("A").unwrap());
        assert_eq!(guard.incident().unwrap().as_deref(), Some("B"));
        assert!(guard.clear_incident("B").unwrap());
    }
}
