//! WebAuthn verification for the Rust port of noevia-core's passkeys (full-Rust migration M3):
//! @simplewebauthn/server v14's `verifyRegistrationResponse` and `verifyAuthenticationResponse`
//! as core auth.cjs calls them (`requireUserVerification: true`, an array of expected origins,
//! one RP ID, the default algorithms EdDSA, ES256 and RS256), check for check and in the same
//! order, so a passkey registered under Node signs in under Rust and the reverse.
//!
//! The credentials Node stored stay as they are: `public_key` is the COSE key as tiny-cbor
//! re-encodes it ([`cbor`]), `counter` the signature counter, `backed_up`/`device_type` from the
//! backup flags, `transports` the client's JSON. CI generates real registrations and assertions
//! with @simplewebauthn (tools/gen-passkey-fixtures.cjs) and requires the same verdicts here
//! (tests/compat.rs).
//!
//! Attestation: `none` (what browsers send for `attestation: 'none'`, which Node requests) and
//! `packed` self-attestation are verified as Node does. A `packed` statement with a certificate
//! chain and the `fido-u2f`, `tpm`, `android-key`, `android-safetynet` and `apple` formats are
//! refused (Node would check them against metadata, roots and, for some, the network): stricter,
//! never wider. Error messages are Node's where the input has the expected types
//! ([`VerifyError::exact`]); V8's TypeError and JSON.parse texts for malformed shapes are not
//! reproduced word for word.

pub mod b64;
pub mod cbor;
pub mod cose;

use cbor::Cbor;
use js_json::JValue;
use sha2::Digest;

/// generateRegistrationOptions' default `supportedAlgorithmIDs` on Node 22 (no ML-DSA).
pub const SUPPORTED_ALGS: [f64; 3] = [-8.0, -7.0, -257.0];

/// Why a ceremony was refused: Node's message, and whether it is Node's text exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    pub message: String,
    pub exact: bool,
}

impl VerifyError {
    fn exact(m: impl Into<String>) -> Self {
        VerifyError {
            message: m.into(),
            exact: true,
        }
    }
    fn loose(m: impl Into<String>) -> Self {
        VerifyError {
            message: m.into(),
            exact: false,
        }
    }
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for VerifyError {}

fn s(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_else(|_| "[object Object]".into())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub up: bool,
    pub uv: bool,
    pub be: bool,
    pub bs: bool,
    pub at: bool,
    pub ed: bool,
}

/// parseAuthenticatorData's result.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthData {
    pub rp_id_hash: Vec<u8>,
    pub flags: Flags,
    pub counter: u32,
    pub aaguid: Option<Vec<u8>>,
    pub credential_id: Option<Vec<u8>>,
    /// The COSE key as tiny-cbor re-encodes it.
    pub credential_public_key: Option<Vec<u8>>,
}

/// Firefox 117's EdDSA key with a map header of 3 instead of 4 (parseAuthenticatorData).
const BAD_EDDSA_CBOR: [u8; 17] = [
    0xa3, 0x01, 0x63, 0x4f, 0x4b, 0x50, 0x03, 0x27, 0x20, 0x67, 0x45, 0x64, 0x32, 0x35, 0x35,
    0x31, 0x39,
];

fn clamp(data: &[u8], start: usize, end: usize) -> &[u8] {
    let end = end.min(data.len());
    data.get(start.min(end)..end).unwrap_or(&[])
}

/// `for (const [key, value] of decoded)` in convertMapToObjectDeep: a Map, a string or an array
/// of iterables destructures; anything else throws.
fn extensions_iterable(v: &Cbor) -> bool {
    match v {
        Cbor::Map(_) | Cbor::Text(_) => true,
        Cbor::Bytes(b) => b.is_empty(),
        Cbor::Array(items) => items.iter().all(|i| match i {
            Cbor::Map(_) | Cbor::Text(_) | Cbor::Array(_) => true,
            Cbor::Bytes(b) => b.is_empty() || b.len() >= 2 || b.len() == 1,
            _ => false,
        }),
        _ => false,
    }
}

/// parseAuthenticatorData.
pub fn parse_authenticator_data(data: &[u8]) -> Result<AuthData, VerifyError> {
    if data.len() < 37 {
        return Err(VerifyError::exact(format!(
            "Authenticator data was {} bytes, expected at least 37 bytes",
            data.len()
        )));
    }
    let rp_id_hash = clamp(data, 0, 32).to_vec();
    let f = data.get(32).copied().unwrap_or(0);
    let flags = Flags {
        up: f & 1 != 0,
        uv: f & (1 << 2) != 0,
        be: f & (1 << 3) != 0,
        bs: f & (1 << 4) != 0,
        at: f & (1 << 6) != 0,
        ed: f & (1 << 7) != 0,
    };
    let counter = u32::from_be_bytes(
        clamp(data, 33, 37)
            .try_into()
            .map_err(|_| VerifyError::loose("authenticator data too short"))?,
    );
    let mut pointer = 37usize;
    let (mut aaguid, mut credential_id, mut credential_public_key) = (None, None, None);
    if flags.at {
        aaguid = Some(clamp(data, pointer, pointer + 16).to_vec());
        pointer += 16;
        let len_bytes: [u8; 2] = clamp(data, pointer, pointer + 2)
            .try_into()
            .map_err(|_| VerifyError::exact("Offset is outside the bounds of the DataView"))?;
        let id_len = usize::from(u16::from_be_bytes(len_bytes));
        pointer += 2;
        credential_id = Some(clamp(data, pointer, pointer + id_len).to_vec());
        pointer += id_len;
        let mut rest = clamp(data, pointer, data.len()).to_vec();
        if rest.starts_with(&BAD_EDDSA_CBOR) {
            if let Some(b) = rest.first_mut() {
                *b = 0xa4;
            }
        }
        let (key, _) = cbor::decode_first(&rest)
            .map_err(|_| VerifyError::loose("credential public key is not well formed CBOR"))?;
        let encoded = cbor::encode(&key);
        pointer += encoded.len();
        credential_public_key = Some(encoded);
    }
    if flags.ed {
        let rest = clamp(data, pointer, data.len());
        let (ext, _) = cbor::decode_first(rest)
            .map_err(|_| VerifyError::loose("extension data is not well formed CBOR"))?;
        let encoded = cbor::encode(&ext);
        let again = cbor::decode_first(&encoded).map_err(|_| {
            VerifyError::loose("Error decoding authenticator extensions: not well formed")
        })?;
        if !extensions_iterable(&again.0) {
            return Err(VerifyError::loose("extension data is not iterable"));
        }
        pointer += encoded.len();
    }
    if data.len() > pointer {
        return Err(VerifyError::exact(
            "Leftover bytes detected while parsing authenticator data",
        ));
    }
    Ok(AuthData {
        rp_id_hash,
        flags,
        counter,
        aaguid,
        credential_id,
        credential_public_key,
    })
}

/// matchExpectedRPID for one RP ID: `toHash(fromASCIIString(rpId))` (each UTF-16 unit's low byte).
fn rp_id_matches(hash: &[u8], rp_id: &str) -> bool {
    let ascii: Vec<u8> = rp_id.encode_utf16().map(|u| (u & 0xff) as u8).collect();
    sha2::Sha256::digest(&ascii).as_slice() == hash
}

/// parseBackupFlags.
fn backup_flags(flags: Flags) -> Result<(&'static str, bool), VerifyError> {
    let device = if flags.be {
        "multiDevice"
    } else {
        "singleDevice"
    };
    if device == "singleDevice" && flags.bs {
        return Err(VerifyError::exact(
            "Single-device credential indicated that it was backed up, which should be impossible.",
        ));
    }
    Ok((device, flags.bs))
}

/// decodeClientDataJSON for a string: base64url, UTF-8, JSON.parse.
fn client_data(raw: &str) -> Result<JValue, VerifyError> {
    js_json::parse(&b64::to_utf8_string(raw))
        .map_err(|_| VerifyError::loose("clientDataJSON is not valid JSON"))
}

/// What Node stores for a new passkey (auth.cjs registrationVerify).
#[derive(Debug, Clone, PartialEq)]
pub struct NewCredential {
    /// `isoBase64URL.fromBuffer(credentialID)` from the authenticator data.
    pub id: String,
    pub public_key: Vec<u8>,
    pub counter: u32,
    /// `response.response.transports`, whatever JSON the client sent (Node stores
    /// `JSON.stringify(transports || [])`).
    pub transports: JValue,
    /// `credentialDeviceType`: `singleDevice` or `multiDevice`.
    pub device_type: &'static str,
    pub backed_up: bool,
    pub fmt: String,
}

/// The expected values of one ceremony, as auth.cjs passes them.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    pub challenge: &'a str,
    /// `passkeyOrigins()`.
    pub origins: &'a [String],
    pub rp_id: &'a str,
}

fn credential_shape<'a>(response: &'a JValue, what: &str) -> Result<&'a JValue, VerifyError> {
    if response.is_nullish() {
        return Err(VerifyError::exact(format!(
            "Cannot destructure property 'id' of 'response' as it is {}.",
            if matches!(response, JValue::Null) {
                "null"
            } else {
                "undefined"
            }
        )));
    }
    let id = response.get("id");
    if !id.truthy() {
        return Err(VerifyError::exact("Missing credential ID"));
    }
    let raw = response.get("rawId");
    // `id !== rawId`: two parsed objects are never the same object.
    let same = match (id, raw) {
        (JValue::Str(a), JValue::Str(b)) => a == b,
        (JValue::Num(a), JValue::Num(b)) => a == b,
        (JValue::Bool(a), JValue::Bool(b)) => a == b,
        _ => false,
    };
    if !same {
        return Err(VerifyError::exact("Credential ID was not base64url-encoded"));
    }
    let kind = response.get("type");
    if kind.as_str() != Some("public-key") {
        return Err(VerifyError::exact(format!(
            "Unexpected credential type {}, expected \"public-key\"",
            s(kind)
        )));
    }
    let inner = response.get("response");
    if inner.is_nullish() && what == "registration" {
        return Err(VerifyError::exact(format!(
            "Cannot read properties of {} (reading 'clientDataJSON')",
            if matches!(inner, JValue::Null) {
                "null"
            } else {
                "undefined"
            }
        )));
    }
    Ok(inner)
}

fn destructure_client<'a>(cd: &'a JValue, name: &str) -> Result<&'a JValue, VerifyError> {
    if cd.is_nullish() {
        return Err(VerifyError::exact(format!(
            "Cannot destructure property 'type' of '{name}' as it is {}.",
            if matches!(cd, JValue::Null) {
                "null"
            } else {
                "undefined"
            }
        )));
    }
    Ok(cd)
}

fn origin_ok(origin: &JValue, origins: &[String]) -> bool {
    origin.as_str().is_some_and(|o| origins.iter().any(|e| e == o))
}

/// `verifyRegistrationResponse({ response, expectedChallenge, expectedOrigin, expectedRPID,
/// requireUserVerification: true })` followed by auth.cjs's `verified && registrationInfo` check.
pub fn verify_registration(
    response: &JValue,
    expected: Expected<'_>,
) -> Result<NewCredential, VerifyError> {
    let att = credential_shape(response, "registration")?;
    let Some(cdj) = att.get("clientDataJSON").as_str() else {
        return Err(VerifyError::loose("clientDataJSON is not a string"));
    };
    let cd = client_data(cdj)?;
    let cd = destructure_client(&cd, "clientDataJSON")?;
    let kind = cd.get("type");
    if kind.as_str() != Some("webauthn.create") {
        return Err(VerifyError::exact(format!(
            "Unexpected registration response type: {}",
            s(kind)
        )));
    }
    let challenge = cd.get("challenge");
    if challenge.as_str() != Some(expected.challenge) {
        return Err(VerifyError::exact(format!(
            "Unexpected registration response challenge \"{}\", expected \"{}\"",
            s(challenge),
            expected.challenge
        )));
    }
    let origin = cd.get("origin");
    if !origin_ok(origin, expected.origins) {
        return Err(VerifyError::exact(format!(
            "Unexpected registration response origin \"{}\", expected one of: {}",
            s(origin),
            expected.origins.join(", ")
        )));
    }
    let tb = cd.get("tokenBinding");
    if tb.truthy() {
        if !matches!(tb, JValue::Obj(_) | JValue::Arr(_)) {
            return Err(VerifyError::exact(format!(
                "Unexpected value for TokenBinding \"{}\"",
                s(tb)
            )));
        }
        let status = tb.get("status");
        if !matches!(status.as_str(), Some("present" | "supported" | "not-supported")) {
            return Err(VerifyError::exact(format!(
                "Unexpected tokenBinding.status value of \"{}\"",
                s(status)
            )));
        }
    }
    let Some(ao) = att.get("attestationObject").as_str() else {
        return Err(VerifyError::loose("attestationObject is not a string"));
    };
    let ao = b64::to_buffer(ao);
    let (decoded, _) = cbor::decode_first(&ao)
        .map_err(|_| VerifyError::loose("attestationObject is not well formed CBOR"))?;
    if !matches!(decoded, Cbor::Map(_)) {
        return Err(VerifyError::loose(
            "decodedAttestationObject.get is not a function",
        ));
    }
    let fmt = decoded.get_text("fmt").cloned().unwrap_or(Cbor::Undefined);
    let Some(Cbor::Bytes(auth_data)) = decoded.get_text("authData") else {
        return Err(VerifyError::loose("authData is not a byte string"));
    };
    let att_stmt = decoded.get_text("attStmt").cloned().unwrap_or(Cbor::Undefined);
    let parsed = parse_authenticator_data(auth_data)?;
    if !rp_id_matches(&parsed.rp_id_hash, expected.rp_id) {
        return Err(VerifyError::exact("Unexpected RP ID hash"));
    }
    if !parsed.flags.up {
        return Err(VerifyError::exact(
            "User presence was required, but user was not present",
        ));
    }
    if !parsed.flags.uv {
        return Err(VerifyError::exact(
            "User verification was required, but user could not be verified",
        ));
    }
    let (Some(credential_id), Some(public_key), Some(_aaguid)) = (
        parsed.credential_id.clone(),
        parsed.credential_public_key.clone(),
        parsed.aaguid.clone(),
    ) else {
        return Err(VerifyError::exact(
            "No credential ID was provided by authenticator",
        ));
    };
    let (key, _) = cbor::decode_first(&public_key)
        .map_err(|_| VerifyError::loose("credential public key is not well formed CBOR"))?;
    if !matches!(key, Cbor::Map(_)) {
        return Err(VerifyError::loose("decodedPublicKey.get is not a function"));
    }
    let Some(Cbor::Num(alg)) = key.get_num(3.0) else {
        return Err(VerifyError::exact(
            "Credential public key was missing numeric alg",
        ));
    };
    if !SUPPORTED_ALGS.contains(alg) {
        return Err(VerifyError::exact(format!(
            "Unexpected public key alg \"{}\", expected one of \"-8, -7, -257\"",
            js_json::number_to_string(*alg)
        )));
    }
    let client_hash = sha2::Sha256::digest(b64::to_buffer(cdj));
    let fmt_name = match &fmt {
        Cbor::Text(t) => t.as_str(),
        _ => "",
    };
    let verified = match fmt_name {
        "none" => {
            match &att_stmt {
                Cbor::Map(items) if !items.is_empty() => {
                    return Err(VerifyError::exact(
                        "None attestation had unexpected attestation statement",
                    ))
                }
                Cbor::Null | Cbor::Undefined => {
                    return Err(VerifyError::loose("attStmt is missing"));
                }
                _ => {}
            }
            true
        }
        "packed" => {
            if !matches!(att_stmt, Cbor::Map(_)) {
                return Err(VerifyError::loose("attStmt.get is not a function"));
            }
            let sig = att_stmt.get_text("sig");
            let x5c = att_stmt.get_text("x5c");
            let a = att_stmt.get_text("alg");
            if !sig.is_some_and(Cbor::truthy) {
                return Err(VerifyError::exact(
                    "No attestation signature provided in attestation statement (Packed)",
                ));
            }
            if !a.is_some_and(Cbor::truthy) {
                return Err(VerifyError::exact(
                    "Attestation statement did not contain alg (Packed)",
                ));
            }
            if !a.is_some_and(cose::is_cose_alg) {
                return Err(VerifyError::exact(format!(
                    "Attestation statement contained invalid alg {} (Packed)",
                    a.map_or_else(|| "undefined".into(), Cbor::display)
                )));
            }
            if x5c.is_some_and(Cbor::truthy) {
                return Err(VerifyError::loose(
                    "packed attestation with a certificate chain is not supported by this server",
                ));
            }
            let Some(Cbor::Bytes(sig)) = sig else {
                return Err(VerifyError::loose("attestation signature is not a byte string"));
            };
            let mut base = auth_data.clone();
            base.extend_from_slice(&client_hash);
            cose::verify(&key, sig, &base, a).map_err(VerifyError::loose)?
        }
        "fido-u2f" | "android-safetynet" | "android-key" | "tpm" | "apple" => {
            return Err(VerifyError::loose(format!(
                "{fmt_name} attestation is not supported by this server"
            )));
        }
        _ => {
            return Err(VerifyError::exact(format!(
                "Unsupported Attestation Format: {}",
                fmt.display()
            )))
        }
    };
    if !verified {
        return Err(VerifyError::exact("passkey registration failed"));
    }
    let (device_type, backed_up) = backup_flags(parsed.flags)?;
    Ok(NewCredential {
        id: b64::from_buffer(&credential_id),
        public_key,
        counter: parsed.counter,
        transports: att.get("transports").clone(),
        device_type,
        backed_up,
        fmt: fmt_name.to_string(),
    })
}

/// A stored passkey, as auth.cjs passes it (`new Uint8Array(key.public_key)`, `key.counter`).
#[derive(Debug, Clone, Copy)]
pub struct Stored<'a> {
    pub public_key: &'a [u8],
    pub counter: f64,
}

/// `verifyAuthenticationResponse({ response, expectedChallenge, expectedOrigin, expectedRPID,
/// credential, requireUserVerification: true })` and auth.cjs's `verified` check: the new counter.
pub fn verify_authentication(
    response: &JValue,
    expected: Expected<'_>,
    stored: Stored<'_>,
) -> Result<u32, VerifyError> {
    let ar = credential_shape(response, "authentication")?;
    let Some(cdj) = ar.get("clientDataJSON").as_str() else {
        return Err(VerifyError::exact(
            "Credential response clientDataJSON was not a string",
        ));
    };
    let cd = client_data(cdj)?;
    let cd = destructure_client(&cd, "clientDataJSON")?;
    let kind = cd.get("type");
    if kind.as_str() != Some("webauthn.get") {
        return Err(VerifyError::exact(format!(
            "Unexpected authentication response type: {}",
            s(kind)
        )));
    }
    let challenge = cd.get("challenge");
    if challenge.as_str() != Some(expected.challenge) {
        return Err(VerifyError::exact(format!(
            "Unexpected authentication response challenge \"{}\", expected \"{}\"",
            s(challenge),
            expected.challenge
        )));
    }
    let (cross, top) = (cd.get("crossOrigin"), cd.get("topOrigin"));
    if top.truthy() {
        // Truthy topOrigin: refused without expectedTopOrigin (cross-origin) or as a violation.
        return Err(VerifyError::loose(if cross.truthy() {
            "cross-origin authentication response without an expected top origin"
        } else {
            "unexpected top origin within a non-cross-origin authentication response"
        }));
    }
    let origin = cd.get("origin");
    if !origin_ok(origin, expected.origins) {
        return Err(VerifyError::exact(format!(
            "Unexpected authentication response origin \"{}\", expected one of: {}",
            s(origin),
            expected.origins.join(", ")
        )));
    }
    let Some(ad) = ar.get("authenticatorData").as_str().filter(|v| b64::is_base64url(v)) else {
        return Err(VerifyError::exact(
            "Credential response authenticatorData was not a base64url string",
        ));
    };
    let Some(sig) = ar.get("signature").as_str().filter(|v| b64::is_base64url(v)) else {
        return Err(VerifyError::exact(
            "Credential response signature was not a base64url string",
        ));
    };
    let uh = ar.get("userHandle");
    if uh.truthy() && uh.as_str().is_none() {
        return Err(VerifyError::exact(
            "Credential response userHandle was not a string",
        ));
    }
    let tb = cd.get("tokenBinding");
    if tb.truthy() {
        if !matches!(tb, JValue::Obj(_) | JValue::Arr(_)) {
            return Err(VerifyError::exact(
                "ClientDataJSON tokenBinding was not an object",
            ));
        }
        let status = tb.get("status");
        if !matches!(status.as_str(), Some("present" | "supported" | "notSupported")) {
            return Err(VerifyError::exact(format!(
                "Unexpected tokenBinding status {}",
                s(status)
            )));
        }
    }
    let auth_data = b64::to_buffer(ad);
    let parsed = parse_authenticator_data(&auth_data)?;
    if !rp_id_matches(&parsed.rp_id_hash, expected.rp_id) {
        return Err(VerifyError::exact("Unexpected RP ID hash"));
    }
    if !parsed.flags.up {
        return Err(VerifyError::exact("User not present during authentication"));
    }
    if !parsed.flags.uv {
        return Err(VerifyError::exact(
            "User verification required, but user could not be verified",
        ));
    }
    let client_hash = sha2::Sha256::digest(b64::to_buffer(cdj));
    let mut base = auth_data.clone();
    base.extend_from_slice(&client_hash);
    let signature = b64::to_buffer(sig);
    let counter = f64::from(parsed.counter);
    if (counter > 0.0 || stored.counter > 0.0) && counter <= stored.counter {
        return Err(VerifyError::exact(format!(
            "Response counter value {} was lower than expected {}",
            parsed.counter,
            js_json::number_to_string(stored.counter)
        )));
    }
    backup_flags(parsed.flags)?;
    let (key, _) = cbor::decode_first(stored.public_key)
        .map_err(|_| VerifyError::loose("stored public key is not well formed CBOR"))?;
    if !matches!(key, Cbor::Map(_)) {
        return Err(VerifyError::loose("cosePublicKey.get is not a function"));
    }
    if !cose::verify(&key, &signature, &base, None).map_err(VerifyError::loose)? {
        return Err(VerifyError::exact("authentication failed"));
    }
    Ok(parsed.counter)
}
