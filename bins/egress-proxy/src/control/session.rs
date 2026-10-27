//! Native monotonic session core. Not yet wired to HTTP or forwarding.
//!
//! The enclosing controller MUST serialize authenticated Ready/renewal/admission
//! with epoch replacement and drain, deduplicate request IDs before mutation, and
//! recheck after async DNS/connect work. These primitives do not do those jobs.
use egress::control::{derive_epoch_keys, EpochKeys, FileKey, MAX_SAFE_INTEGER};
use egress::SignedGrant;
use std::time::{Duration, Instant};

pub const SESSION_LEASE: Duration = Duration::from_secs(15);
pub const CLIENT_RENEW_INTERVAL: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Entropy,
    Expired,
    Retired,
    Sequence,
    Clock,
    Authentication,
    Internal,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control session {self:?}")
    }
}
impl std::error::Error for Error {}

/// Private historical control proof verifier; cannot mint, export a key or admit
/// network access. Ordinary absolute validity is always checked at actual now.
#[derive(Clone)]
pub(super) struct OwnershipVerifier(egress::GrantKey);
impl OwnershipVerifier {
    pub(super) fn verify(&self, token: &[u8], wall_ms: u64) -> Result<egress::SignedGrant, Error> {
        egress::verify(&self.0, token, wall_ms).map_err(|_| Error::Authentication)
    }
}
/// One process incarnation. Production identity generation is OS CSPRNG only.
/// Do not regenerate this per request; new sessions retain this process boot ID.
pub struct Boot {
    id: [u8; 16],
}
impl std::fmt::Debug for Boot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Boot(<redacted>)")
    }
}
impl Boot {
    pub fn generate() -> Result<Self, Error> {
        Ok(Self { id: random_id()? })
    }
    /// Call only after authenticated Ready preconditions and old-epoch drain.
    /// Each successful call gets a fresh random session and distinct data MAC key.
    pub fn start_session(&self, file: &FileKey) -> Result<Session, Error> {
        let id = random_id()?;
        Session::start_at(file, self.id, id, Instant::now())
    }
    pub fn id(&self) -> String {
        hex(&self.id)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Live,
    Expired,
    Retired,
}
/// No Clone: one serialized owner must observe and latch expiry before admission.
pub struct Session {
    boot: [u8; 16],
    id: [u8; 16],
    fingerprint: String,
    keys: EpochKeys,
    deadline: Instant,
    last_observed: Instant,
    sequence: u64,
    state: State,
}
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Session(<redacted>)")
    }
}
/// Observation only, never an authority token. Sending a cached proof cannot renew.
/// HTTP must additionally bind this to request MAC, elapsed time and current epoch.
#[derive(Debug, Clone)]
pub struct Proof {
    pub boot_id: String,
    pub session_id: String,
    pub key_fingerprint: String,
    pub sequence: u64,
    pub remaining: Duration,
}
impl Session {
    fn start_at(file: &FileKey, boot: [u8; 16], id: [u8; 16], now: Instant) -> Result<Self, Error> {
        Ok(Self {
            boot,
            id,
            fingerprint: file.fingerprint(),
            keys: derive_epoch_keys(file, &boot, &id).map_err(|_| Error::Internal)?,
            deadline: now.checked_add(SESSION_LEASE).ok_or(Error::Clock)?,
            last_observed: now,
            sequence: 0,
            state: State::Live,
        })
    }
    fn check_at(&mut self, now: Instant) -> Result<(), Error> {
        match self.state {
            State::Expired => return Err(Error::Expired),
            State::Retired => return Err(Error::Retired),
            State::Live => {}
        }
        if now < self.last_observed {
            self.state = State::Expired;
            return Err(Error::Clock);
        }
        self.last_observed = now;
        if now >= self.deadline {
            self.state = State::Expired;
            return Err(Error::Expired);
        }
        Ok(())
    }
    fn proof_at(&mut self, now: Instant) -> Result<Proof, Error> {
        self.check_at(now)?;
        Ok(Proof {
            boot_id: hex(&self.boot),
            session_id: hex(&self.id),
            key_fingerprint: self.fingerprint.clone(),
            sequence: self.sequence,
            remaining: self
                .deadline
                .checked_duration_since(now)
                .ok_or(Error::Clock)?,
        })
    }
    pub fn proof(&mut self) -> Result<Proof, Error> {
        self.proof_at(Instant::now())
    }
    /// Native ordered extension only. Same-sequence retry observes the OLD deadline;
    /// it never extends it. The controller must also reject same-ID/different-body
    /// and changed-ID sequence reuse according to the complete replay contract.
    pub fn renew(&mut self, sequence: u64) -> Result<Proof, Error> {
        self.renew_at(sequence, Instant::now())
    }
    fn renew_at(&mut self, sequence: u64, now: Instant) -> Result<Proof, Error> {
        self.check_at(now)?;
        if sequence == 0 || sequence > MAX_SAFE_INTEGER {
            return Err(Error::Sequence);
        }
        if sequence == self.sequence {
            return self.proof_at(now);
        }
        if self.sequence.checked_add(1) != Some(sequence) {
            return Err(Error::Sequence);
        }
        let deadline = now.checked_add(SESSION_LEASE).ok_or(Error::Clock)?;
        self.sequence = sequence;
        self.deadline = deadline;
        self.proof_at(now)
    }
    /// Irreversible. Actual replacement acknowledgement still requires old work drain.
    #[cfg(test)]
    pub(super) fn expire_for_test(&mut self) {
        self.state = State::Expired;
    }
    pub(super) fn ownership_verifier(&self) -> OwnershipVerifier {
        OwnershipVerifier(self.keys.grant.clone())
    }
    pub fn retire(&mut self) {
        self.state = State::Retired;
    }
    /// Stateless cryptographic check under this live boot+session data key. The
    /// enclosing admission must additionally enforce action, lease/replay ledger,
    /// target policy and serialized epoch identity, then recheck after async work.
    /// No key getter lets a caller accidentally retain an unfenced admission key.
    pub fn verify_token(&mut self, token: &[u8], wall_ms: u64) -> Result<SignedGrant, Error> {
        self.check_at(Instant::now())?;
        let grant =
            egress::verify(&self.keys.grant, token, wall_ms).map_err(|_| Error::Authentication)?;
        self.check_at(Instant::now())?;
        Ok(grant)
    }
}
fn random_id() -> Result<[u8; 16], Error> {
    random_id_with(|out| getrandom::fill(out).map_err(|_| Error::Entropy))
}
fn random_id_with(mut fill: impl FnMut(&mut [u8]) -> Result<(), Error>) -> Result<[u8; 16], Error> {
    let mut out = [0u8; 16];
    fill(&mut out)?;
    if out.iter().all(|v| *v == 0) {
        return Err(Error::Entropy);
    }
    Ok(out)
}
fn hex(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(now: Instant) -> Result<Session, Error> {
        Session::start_at(&FileKey::from_bytes([7; 32]), [1; 16], [2; 16], now)
    }
    #[test]
    fn native_deadline_is_exact_latched_and_never_extended_by_observation() -> Result<(), Error> {
        let now = Instant::now();
        let mut s = session(now)?;
        assert_eq!(s.proof_at(now)?.remaining, SESSION_LEASE);
        assert_eq!(
            s.proof_at(now + Duration::from_secs(14))?.remaining,
            Duration::from_secs(1)
        );
        assert_eq!(s.proof_at(now + SESSION_LEASE).err(), Some(Error::Expired));
        assert_eq!(
            s.renew_at(1, now + SESSION_LEASE).err(),
            Some(Error::Expired)
        );
        assert_eq!(s.proof_at(now).err(), Some(Error::Expired));
        Ok(())
    }
    #[test]
    fn renew_is_ordered_idempotent_and_extends_from_native_now() -> Result<(), Error> {
        let now = Instant::now();
        let mut s = session(now)?;
        assert_eq!(s.renew_at(0, now).err(), Some(Error::Sequence));
        assert_eq!(s.renew_at(2, now).err(), Some(Error::Sequence));
        assert_eq!(
            s.renew_at(1, now + Duration::from_secs(5))?.remaining,
            SESSION_LEASE
        );
        assert_eq!(
            s.renew_at(1, now + Duration::from_secs(10))?.remaining,
            Duration::from_secs(10)
        );
        assert_eq!(
            s.renew_at(3, now + Duration::from_secs(10)).err(),
            Some(Error::Sequence)
        );
        assert_eq!(
            s.renew_at(MAX_SAFE_INTEGER + 1, now + Duration::from_secs(10))
                .err(),
            Some(Error::Sequence)
        );
        assert_eq!(
            s.renew_at(2, now + Duration::from_secs(10))?.remaining,
            SESSION_LEASE
        );
        assert_eq!(
            s.renew_at(1, now + Duration::from_secs(11)).err(),
            Some(Error::Sequence)
        );
        assert_eq!(
            s.proof_at(now + Duration::from_secs(25)).err(),
            Some(Error::Expired)
        );
        Ok(())
    }
    #[test]
    fn retirement_clock_regression_and_sequence_exhaustion_fail_closed() -> Result<(), Error> {
        let now = Instant::now();
        let mut s = session(now)?;
        s.retire();
        assert_eq!(s.renew_at(1, now).err(), Some(Error::Retired));
        let mut s = session(now)?;
        let _ = s.proof_at(now + Duration::from_secs(1))?;
        assert_eq!(s.proof_at(now).err(), Some(Error::Clock));
        assert_eq!(
            s.renew_at(1, now + Duration::from_secs(2)).err(),
            Some(Error::Expired)
        );
        let mut s = session(now)?;
        s.sequence = MAX_SAFE_INTEGER;
        assert_eq!(
            s.renew_at(MAX_SAFE_INTEGER + 1, now).err(),
            Some(Error::Sequence)
        );
        assert_eq!(s.renew_at(MAX_SAFE_INTEGER, now)?.remaining, SESSION_LEASE);
        Ok(())
    }
    #[test]
    fn data_mac_is_bound_to_boot_session_and_file_key() -> Result<(), Error> {
        let now = Instant::now();
        let file = FileKey::from_bytes([7; 32]);
        let mut current = Session::start_at(&file, [1; 16], [2; 16], now)?;
        let claims = SignedGrant {
            act: egress::Act::Grant,
            task: "task.1".into(),
            hosts: vec!["example.com".into()],
            iat: 1000,
            exp: 2000,
            idle: 1000,
            nonce: "AAECAwQFBgcICQoLDA0ODw".into(),
        };
        let token = egress::mint(&current.keys.grant, &claims).ok_or(Error::Internal)?;
        assert_eq!(current.verify_token(token.as_bytes(), 1000)?.task, "task.1");
        for mut other in [
            Session::start_at(&file, [3; 16], [2; 16], now)?,
            Session::start_at(&file, [1; 16], [3; 16], now)?,
            Session::start_at(&FileKey::from_bytes([8; 32]), [1; 16], [2; 16], now)?,
        ] {
            assert_eq!(
                other.verify_token(token.as_bytes(), 1000).err(),
                Some(Error::Authentication)
            );
        }
        let _ = current.proof_at(now + SESSION_LEASE).err();
        assert_eq!(
            current.verify_token(token.as_bytes(), 1000).err(),
            Some(Error::Expired)
        );
        Ok(())
    }
    #[test]
    fn entropy_faults_have_no_deterministic_fallback() -> Result<(), Error> {
        assert_eq!(random_id_with(|_| Err(Error::Entropy)), Err(Error::Entropy));
        assert_eq!(random_id_with(|_| Ok(())), Err(Error::Entropy));
        assert_eq!(
            random_id_with(|out| {
                out.fill(3);
                Err(Error::Entropy)
            }),
            Err(Error::Entropy)
        );
        assert_eq!(
            random_id_with(|out| {
                out.fill(3);
                Ok(())
            })?,
            [3; 16]
        );
        let boot = Boot::generate()?;
        let file = FileKey::from_bytes([7; 32]);
        let mut a = boot.start_session(&file)?;
        let mut b = boot.start_session(&file)?;
        let pa = a.proof()?;
        let pb = b.proof()?;
        assert_eq!(pa.boot_id, pb.boot_id);
        assert_ne!(pa.session_id, pb.session_id);
        assert!(!format!("{a:?}").contains(&pa.session_id));
        assert_eq!(
            a.verify_token(b"not-a-token", 1).err(),
            Some(Error::Authentication)
        );
        a.retire();
        assert_eq!(
            a.verify_token(b"not-a-token", 1).err(),
            Some(Error::Retired)
        );
        Ok(())
    }
}
