//! Private child Request consumption. Caller completes setup before announcing READY.
use crate::{
    isolated::{Error, BODY, INPUT, READY},
    protocol::Request,
    request_ipc,
};
use std::io::{Read, Write};
use zeroize::Zeroizing;
pub fn receive(input: &mut impl Read, output: &mut impl Write) -> Result<Request, Error> {
    output
        .write_all(READY)
        .and_then(|_| output.flush())
        .map_err(|_| Error::Protocol)?;
    let mut length = [0; 4];
    input.read_exact(&mut length).map_err(|_| Error::Protocol)?;
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| Error::Input)?;
    if length > INPUT {
        return Err(Error::Input);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    bytes.try_reserve_exact(length).map_err(|_| Error::Input)?;
    bytes.resize(length, 0);
    input.read_exact(&mut bytes).map_err(|_| Error::Protocol)?;
    let mut extra = [0];
    if input.read(&mut extra).map_err(|_| Error::Protocol)? != 0 {
        return Err(Error::Protocol);
    }
    request_ipc::decode(&bytes)
}
/// Explicit nested synthetic BODY transport, independent inner M4HTTP01 version.
/// Full list-cap envelope needs a separately reviewed expanded owner boundary.
pub fn nested(output: &mut impl Write, frame: &[u8]) -> Result<(), Error> {
    if frame.len() > crate::LIST_BYTES {
        return Err(Error::Output);
    }
    output
        .write_all(BODY)
        .and_then(|_| {
            output.write_all(
                &u32::try_from(frame.len())
                    .map_err(|_| std::io::Error::other("fixed framing refusal"))?
                    .to_be_bytes(),
            )
        })
        .and_then(|_| output.write_all(frame))
        .and_then(|_| output.flush())
        .map_err(|_| Error::Protocol)
}
pub fn network_reply(output: &mut impl Write, frame: &[u8], cap: usize) -> Result<(), Error> {
    if !matches!(cap, crate::LIST_BYTES | crate::TEXT_BYTES) {
        return Err(Error::Input);
    }
    crate::response_ipc::decode(frame, cap)?;
    output
        .write_all(crate::isolated::NET)
        .and_then(|_| {
            output.write_all(
                &u32::try_from(frame.len())
                    .map_err(|_| std::io::Error::other("fixed framing refusal"))?
                    .to_be_bytes(),
            )
        })
        .and_then(|_| output.write_all(frame))
        .and_then(|_| output.flush())
        .map_err(|_| Error::Protocol)
}
