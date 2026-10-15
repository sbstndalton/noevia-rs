//! Separate M4HTTP01 response; BODY echo frames are never reinterpreted implicitly.
use crate::{isolated::Error, protocol::TransportFailure, LIST_BYTES};
pub const TAG: &[u8; 8] = b"M4HTTP01";
pub const METADATA: usize = 18;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Dns,
    Tcp,
    Tls,
    Headers,
    Body,
    Finished,
}
pub enum Reply<'a> {
    Http {
        status: u16,
        capped: bool,
        payload: &'a [u8],
    },
    Failure {
        phase: Phase,
        failure: TransportFailure,
    },
}
fn phase_code(phase: Phase) -> u8 {
    match phase {
        Phase::Dns => 0,
        Phase::Tcp => 1,
        Phase::Tls => 2,
        Phase::Headers => 3,
        Phase::Body => 4,
        Phase::Finished => 5,
    }
}
fn phase(code: u8) -> Result<Phase, Error> {
    match code {
        0 => Ok(Phase::Dns),
        1 => Ok(Phase::Tcp),
        2 => Ok(Phase::Tls),
        3 => Ok(Phase::Headers),
        4 => Ok(Phase::Body),
        5 => Ok(Phase::Finished),
        _ => Err(Error::Protocol),
    }
}
fn failure_code(failure: TransportFailure) -> u8 {
    match failure {
        TransportFailure::PreHeaderReset => 1,
        TransportFailure::PreHeaderAborted => 2,
        TransportFailure::PreHeaderBrokenPipe => 3,
        TransportFailure::PreHeaderUnexpectedEof => 4,
        // Parent-side only (a Body-phase Timeout); never encoded as its own code.
        TransportFailure::Timeout | TransportFailure::BodyTimeout => 5,
        TransportFailure::Redirect => 6,
        TransportFailure::Tls => 7,
        TransportFailure::Dns => 8,
        TransportFailure::Header => 9,
        TransportFailure::Body => 10,
        TransportFailure::Other => 11,
    }
}
fn failure(code: u8) -> Result<TransportFailure, Error> {
    match code {
        1 => Ok(TransportFailure::PreHeaderReset),
        2 => Ok(TransportFailure::PreHeaderAborted),
        3 => Ok(TransportFailure::PreHeaderBrokenPipe),
        4 => Ok(TransportFailure::PreHeaderUnexpectedEof),
        5 => Ok(TransportFailure::Timeout),
        6 => Ok(TransportFailure::Redirect),
        7 => Ok(TransportFailure::Tls),
        8 => Ok(TransportFailure::Dns),
        9 => Ok(TransportFailure::Header),
        10 => Ok(TransportFailure::Body),
        11 => Ok(TransportFailure::Other),
        _ => Err(Error::Protocol),
    }
}
fn combination(phase: Phase, failure: TransportFailure) -> bool {
    if failure.retryable() {
        return matches!(phase, Phase::Tcp | Phase::Headers);
    }
    match failure {
        TransportFailure::Dns => phase == Phase::Dns,
        TransportFailure::Tls => phase == Phase::Tls,
        TransportFailure::Header | TransportFailure::Redirect => phase == Phase::Headers,
        TransportFailure::Body => phase == Phase::Body,
        TransportFailure::Timeout | TransportFailure::Other => phase != Phase::Finished,
        _ => false,
    }
}
pub fn encode(reply: Reply<'_>, cap: usize) -> Result<Vec<u8>, Error> {
    if cap > LIST_BYTES {
        return Err(Error::Output);
    }
    let (kind, phase, status, code, flags, payload) = match reply {
        Reply::Http {
            status,
            capped,
            payload,
        } => {
            if !(200..=599).contains(&status)
                || payload.len() > cap + 1
                || capped != (payload.len() == cap + 1)
                || (status >= 300 && !payload.is_empty())
            {
                return Err(Error::Protocol);
            }
            (0, 5, status, 0, u8::from(capped), payload)
        }
        Reply::Failure { phase, failure } => {
            if !combination(phase, failure) {
                return Err(Error::Protocol);
            }
            (1, phase_code(phase), 0, failure_code(failure), 0, &[][..])
        }
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(METADATA + payload.len())
        .map_err(|_| Error::Output)?;
    bytes.extend_from_slice(TAG);
    bytes.extend_from_slice(&[kind, phase]);
    bytes.extend_from_slice(&status.to_be_bytes());
    bytes.extend_from_slice(&[code, flags]);
    bytes.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| Error::Output)?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(payload);
    Ok(bytes)
}
fn metadata(bytes: &[u8], cap: usize) -> Result<usize, Error> {
    if cap > LIST_BYTES || bytes.get(..8) != Some(TAG.as_slice()) {
        return Err(Error::Protocol);
    }
    let size: [u8; 4] = bytes
        .get(14..18)
        .ok_or(Error::Protocol)?
        .try_into()
        .map_err(|_| Error::Protocol)?;
    let size = usize::try_from(u32::from_be_bytes(size)).map_err(|_| Error::Protocol)?;
    if size > cap + 1 {
        return Err(Error::Output);
    }
    Ok(size)
}
pub(crate) fn validate_metadata(bytes: &[u8], cap: usize) -> Result<usize, Error> {
    let size = metadata(bytes, cap)?;
    let kind = *bytes.get(8).ok_or(Error::Protocol)?;
    let phase = phase(*bytes.get(9).ok_or(Error::Protocol)?)?;
    let status = u16::from_be_bytes(
        bytes
            .get(10..12)
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?,
    );
    let code = *bytes.get(12).ok_or(Error::Protocol)?;
    let flags = *bytes.get(13).ok_or(Error::Protocol)?;
    match kind {
        0 if phase == Phase::Finished
            && code == 0
            && flags <= 1
            && (200..=599).contains(&status)
            && ((flags == 1) == (size == cap + 1))
            && (status < 300 || size == 0) =>
        {
            Ok(size)
        }
        1 if status == 0 && flags == 0 && size == 0 => {
            let failure = failure(code)?;
            if !combination(phase, failure) {
                return Err(Error::Protocol);
            }
            Ok(size)
        }
        _ => Err(Error::Protocol),
    }
}
pub fn decode(bytes: &[u8], cap: usize) -> Result<Reply<'_>, Error> {
    let size = validate_metadata(bytes, cap)?;
    if bytes.len() != METADATA + size {
        return Err(Error::Protocol);
    }
    match bytes.get(8) {
        Some(0) => Ok(Reply::Http {
            status: u16::from_be_bytes(
                bytes
                    .get(10..12)
                    .ok_or(Error::Protocol)?
                    .try_into()
                    .map_err(|_| Error::Protocol)?,
            ),
            capped: bytes.get(13) == Some(&1),
            payload: bytes.get(METADATA..).ok_or(Error::Protocol)?,
        }),
        Some(1) => Ok(Reply::Failure {
            phase: phase(*bytes.get(9).ok_or(Error::Protocol)?)?,
            failure: failure(*bytes.get(12).ok_or(Error::Protocol)?)?,
        }),
        _ => Err(Error::Protocol),
    }
}
/// Independent finite fragmentation accumulator. EOF/exit/reap are caller gates.
pub struct Frame {
    bytes: Vec<u8>,
    cap: usize,
}
impl Frame {
    pub fn new(cap: usize) -> Result<Self, Error> {
        if cap > LIST_BYTES {
            return Err(Error::Output);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(METADATA + cap + 1)
            .map_err(|_| Error::Output)?;
        Ok(Self { bytes, cap })
    }
    pub fn feed(&mut self, part: &[u8]) -> Result<(), Error> {
        if self
            .bytes
            .len()
            .checked_add(part.len())
            .is_none_or(|n| n > METADATA + self.cap + 1)
        {
            return Err(Error::Output);
        }
        self.bytes.extend_from_slice(part);
        if self.bytes.len() >= METADATA {
            let size = validate_metadata(&self.bytes, self.cap)?;
            if self.bytes.len() > METADATA + size {
                return Err(Error::Protocol);
            }
        }
        Ok(())
    }
    pub fn finish(&self) -> Result<Reply<'_>, Error> {
        decode(&self.bytes, self.cap)
    }
}
