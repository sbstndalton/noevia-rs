//! The permit a tool call holds at its outbound boundary (sbstndalton/noevia-rs#425).
//!
//! A chat gate checks a grant's fence and then hands the call to an executor, which still has awaits to get through (a server
//! lookup, credentials, address checks, the MCP handshake) before the request that does the work goes out. A grant that is
//! revoked or expires in one of those awaits must not still dispatch the write. The executor therefore asks the permit
//! **synchronously, immediately before each outbound send** -- after its last await, with nothing between the answer and the
//! send -- and a refusal means that request is never made.
//!
//! [`DispatchPermit::authorize`] is check-and-consume in one step: `true` means this send is authorised and counted against the
//! grant (the gate's `started()`), `false` that the grant is gone. It is asked once per outbound `tools/call`, so a retry (an
//! OAuth retry after a refused token) is asked again and may be refused on its own.
//!
//! [`GrantPermit`] is the ordinary form: a grant's identity and the revocation generation it was redeemed at, valid while the
//! authority still reports that grant at that generation.
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Asked at the outbound boundary, never earlier. Must not block or await: its answer is followed at once by the send.
pub trait DispatchPermit: Send + Sync + fmt::Debug {
    /// Whether this send may go out; `true` also spends the permit for it.
    fn authorize(&self) -> bool;
}

/// The authority over grants: what generation a grant is at now (`None`: no such grant, closed or expired).
pub trait GrantAuthority: Send + Sync {
    /// The grant's current revocation generation, or `None` when it is no longer a grant at all.
    fn generation(&self, grant: &str) -> Option<u64>;
}

/// A permit for a call under `grant` redeemed at `generation`, or `None` when the grant is not at that generation now
/// (it was revoked, closed or never was).
pub fn permit_for(
    authority: &Arc<dyn GrantAuthority>,
    grant: &str,
    generation: u64,
) -> Option<Arc<dyn DispatchPermit>> {
    (authority.generation(grant) == Some(generation)).then(|| {
        Arc::new(GrantPermit::new(Arc::clone(authority), grant, generation))
            as Arc<dyn DispatchPermit>
    })
}

/// A grant's identity and the generation it was redeemed at, valid while `authority` still reports it there.
pub struct GrantPermit {
    grant: String,
    generation: u64,
    authority: Arc<dyn GrantAuthority>,
    sent: AtomicUsize,
}

impl GrantPermit {
    pub fn new(authority: Arc<dyn GrantAuthority>, grant: &str, generation: u64) -> GrantPermit {
        GrantPermit {
            grant: grant.to_owned(),
            generation,
            authority,
            sent: AtomicUsize::new(0),
        }
    }

    /// How many sends this permit authorised.
    pub fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for GrantPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrantPermit")
            .field("grant", &self.grant)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl DispatchPermit for GrantPermit {
    fn authorize(&self) -> bool {
        let ok = self.authority.generation(&self.grant) == Some(self.generation);
        if ok {
            self.sent.fetch_add(1, Ordering::SeqCst);
        }
        ok
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Grants(Mutex<Vec<(String, u64)>>);
    impl GrantAuthority for Grants {
        fn generation(&self, grant: &str) -> Option<u64> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .find(|(g, _)| g == grant)
                .map(|(_, n)| *n)
        }
    }

    #[test]
    fn a_permit_holds_while_its_grant_is_at_its_generation_and_each_send_is_counted() {
        let grants = Arc::new(Grants(Mutex::new(vec![("g1".into(), 3)])));
        let authority: Arc<dyn GrantAuthority> = grants.clone();
        assert!(
            permit_for(&authority, "g1", 2).is_none(),
            "another generation is not this grant"
        );
        assert!(permit_for(&authority, "nobody", 3).is_none());
        assert!(permit_for(&authority, "g1", 3).is_some());
        let permit = GrantPermit::new(grants.clone(), "g1", 3);
        assert!(permit.authorize() && permit.authorize());
        assert_eq!(permit.sent(), 2);
        grants.0.lock().unwrap().iter_mut().for_each(|g| g.1 = 4); // revoked: the generation moved
        assert!(!permit.authorize());
        assert_eq!(permit.sent(), 2, "a refused send is not counted");
        grants.0.lock().unwrap().clear(); // closed
        assert!(!permit.authorize());
    }
}
