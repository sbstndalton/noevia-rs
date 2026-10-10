//! @simplewebauthn v14 `verifySignature` over a COSE public key (helpers/iso/isoCrypto/verify.js
//! and verifyEC2/verifyRSA/verifyOKP), with WebCrypto's verification done by RustCrypto.
//!
//! The checks run in Node's order with Node's JS comparisons on the decoded CBOR map. Supported:
//! EC2 on P-256, P-384 and P-521 (ECDSA with the alg's hash, or the attestation's override),
//! RSA (PKCS#1 v1.5 with SHA-1/256/384/512, PSS with SHA-256/384/512 and the hash length as salt)
//! and OKP Ed25519. Refused where Node would verify (stricter): AKP / ML-DSA keys (Node 22 cannot
//! make them, so none is stored), ECDSA with a hash shorter than half the curve (P-521 with
//! SHA-256), RSA moduli over 16384 bits, and signatures that are BER but not DER.

use crate::cbor::Cbor;
use sha2::Digest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Hash::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Hash::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Hash::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

const ALG_NUMBERS: &[f64] = &[
    -7.0, -8.0, -35.0, -36.0, -37.0, -38.0, -39.0, -47.0, -48.0, -49.0, -50.0, -257.0, -258.0,
    -259.0, -65535.0,
];
const ALG_NAMES: &[&str] = &[
    "ES256", "EdDSA", "ES384", "ES512", "PS256", "PS384", "PS512", "ES256K", "ML_DSA_44",
    "ML_DSA_65", "ML_DSA_87", "RS256", "RS384", "RS512", "RS1",
];

/// cose.js `isCOSEAlg`: `Object.values(COSEALG)` holds both the numbers and the names.
pub fn is_cose_alg(alg: &Cbor) -> bool {
    match alg {
        Cbor::Num(n) => ALG_NUMBERS.contains(n),
        Cbor::Text(t) => ALG_NAMES.contains(&t.as_str()),
        _ => false,
    }
}

fn is_cose_crv(crv: &Cbor) -> bool {
    match crv {
        Cbor::Num(n) => [1.0, 2.0, 3.0, 6.0, 8.0].contains(n),
        Cbor::Text(t) => ["P256", "P384", "P521", "ED25519", "SECP256K1"].contains(&t.as_str()),
        _ => false,
    }
}

fn is_cose_kty(kty: &Cbor) -> bool {
    match kty {
        Cbor::Num(n) => [1.0, 2.0, 3.0, 7.0].contains(n),
        Cbor::Text(t) => ["OKP", "EC2", "RSA", "AKP"].contains(&t.as_str()),
        _ => false,
    }
}

fn num(v: Option<&Cbor>) -> Option<f64> {
    match v {
        Some(Cbor::Num(n)) => Some(*n),
        _ => None,
    }
}

fn truthy(v: Option<&Cbor>) -> bool {
    v.is_some_and(Cbor::truthy)
}

/// mapCoseAlgToWebCryptoHashAlgName.
pub fn hash_for(alg: Option<&Cbor>) -> Result<Hash, String> {
    let n = num(alg);
    let msg = || {
        format!(
            "Could not map COSE alg value of {} to a WebCrypto hash alg name",
            alg.map_or_else(|| "undefined".into(), Cbor::display)
        )
    };
    match n {
        Some(x) if x == -65535.0 => Ok(Hash::Sha1),
        Some(x) if [-7.0, -37.0, -257.0].contains(&x) => Ok(Hash::Sha256),
        Some(x) if [-35.0, -38.0, -258.0].contains(&x) => Ok(Hash::Sha384),
        Some(x) if [-36.0, -39.0, -259.0, -8.0].contains(&x) => Ok(Hash::Sha512),
        _ => Err(msg()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAlg {
    Ed25519,
    Ecdsa,
    Pkcs1,
    Pss,
    MlDsa,
}

fn key_alg_for(alg: Option<&Cbor>) -> Result<KeyAlg, String> {
    match num(alg) {
        Some(x) if x == -8.0 => Ok(KeyAlg::Ed25519),
        Some(x) if [-7.0, -35.0, -36.0, -47.0].contains(&x) => Ok(KeyAlg::Ecdsa),
        Some(x) if [-257.0, -258.0, -259.0, -65535.0].contains(&x) => Ok(KeyAlg::Pkcs1),
        Some(x) if [-37.0, -38.0, -39.0].contains(&x) => Ok(KeyAlg::Pss),
        Some(x) if [-48.0, -49.0, -50.0].contains(&x) => Ok(KeyAlg::MlDsa),
        _ => Err(format!(
            "Could not map COSE alg value of {} to a WebCrypto key alg name",
            alg.map_or_else(|| "undefined".into(), Cbor::display)
        )),
    }
}

/// A DER `INTEGER`'s content bytes from `der` at `pos`, and the position after it.
fn der_len(der: &[u8], pos: usize) -> Option<(usize, usize)> {
    let first = *der.get(pos)?;
    if first < 0x80 {
        return Some((usize::from(first), pos + 1));
    }
    let n = usize::from(first & 0x7f);
    if n == 0 || n > 4 {
        return None;
    }
    let mut len = 0usize;
    for i in 0..n {
        len = (len << 8) | usize::from(*der.get(pos + 1 + i)?);
    }
    Some((len, pos + 1 + n))
}

fn der_integer(der: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    if *der.get(pos)? != 0x02 {
        return None;
    }
    let (len, start) = der_len(der, pos + 1)?;
    let end = start.checked_add(len)?;
    Some((der.get(start..end)?, end))
}

/// unwrapEC2Signature: the DER `SEQUENCE { r INTEGER, s INTEGER }` as `r || s`, each normalised
/// to the curve's component length (toNormalizedBytes).
fn unwrap_ec2_signature(sig: &[u8], component: usize) -> Result<Vec<u8>, String> {
    let bad = || "Invalid EC2 signature".to_string();
    if sig.first() != Some(&0x30) {
        return Err(bad());
    }
    let (len, start) = der_len(sig, 1).ok_or_else(bad)?;
    let end = start.checked_add(len).ok_or_else(bad)?;
    let body = sig.get(..end).ok_or_else(bad)?;
    let (r, p) = der_integer(body, start).ok_or_else(bad)?;
    let (s, p) = der_integer(body, p).ok_or_else(bad)?;
    if p != end {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(component * 2);
    for part in [r, s] {
        if part.len() < component {
            out.extend(std::iter::repeat_n(0u8, component - part.len()));
            out.extend_from_slice(part);
        } else if part.len() == component {
            out.extend_from_slice(part);
        } else if part.len() == component + 1
            && part.first() == Some(&0)
            && part.get(1).is_some_and(|b| b & 0x80 == 0x80)
        {
            out.extend_from_slice(part.get(1..).unwrap_or(&[]));
        } else {
            return Err(format!(
                "Invalid signature component length {}, expected {component}",
                part.len()
            ));
        }
    }
    Ok(out)
}

fn bytes(v: Option<&Cbor>) -> Option<&[u8]> {
    match v {
        Some(Cbor::Bytes(b)) => Some(b),
        _ => None,
    }
}

fn verify_ec2(
    key: &Cbor,
    sig: &[u8],
    data: &[u8],
    hash_override: Option<&Cbor>,
) -> Result<bool, String> {
    let crv = key.get_num(-1.0);
    if !crv.is_some_and(is_cose_crv) {
        return Err(format!(
            "unknown COSE curve {}",
            crv.map_or_else(|| "undefined".into(), Cbor::display)
        ));
    }
    let component = match num(crv) {
        Some(x) if x == 1.0 => 32,
        Some(x) if x == 2.0 => 48,
        Some(x) if x == 3.0 => 66,
        _ => {
            return Err(format!(
                "Unexpected COSE crv value of {} (EC2)",
                crv.map_or_else(|| "undefined".into(), Cbor::display)
            ))
        }
    };
    let rs = unwrap_ec2_signature(sig, component)?;
    let alg = key.get_num(3.0);
    if !truthy(alg) {
        return Err("Public key was missing alg (EC2)".into());
    }
    let (x, y) = (key.get_num(-2.0), key.get_num(-3.0));
    if !truthy(x) {
        return Err("Public key was missing x (EC2)".into());
    }
    if !truthy(y) {
        return Err("Public key was missing y (EC2)".into());
    }
    let mut hash = hash_for(alg)?;
    if let Some(o) = hash_override.filter(|o| o.truthy()) {
        hash = hash_for(Some(o))?;
    }
    let (Some(x), Some(y)) = (bytes(x), bytes(y)) else {
        return Err("Invalid keyData".into());
    };
    if x.len() != component || y.len() != component {
        return Err("Invalid keyData".into());
    }
    let mut sec1 = Vec::with_capacity(1 + 2 * component);
    sec1.push(0x04);
    sec1.extend_from_slice(x);
    sec1.extend_from_slice(y);
    let digest = hash.digest(data);
    macro_rules! ecdsa {
        ($curve:ident) => {{
            use $curve::ecdsa::signature::hazmat::PrehashVerifier;
            let key = $curve::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
                .map_err(|_| "Invalid keyData".to_string())?;
            let Ok(sig) = $curve::ecdsa::Signature::from_slice(&rs) else {
                return Ok(false);
            };
            Ok(key.verify_prehash(&digest, &sig).is_ok())
        }};
    }
    match component {
        32 => ecdsa!(p256),
        48 => ecdsa!(p384),
        _ => ecdsa!(p521),
    }
}

fn verify_okp(key: &Cbor, sig: &[u8], data: &[u8]) -> Result<bool, String> {
    use ed25519_dalek::Verifier;
    let alg = key.get_num(3.0);
    if !truthy(alg) {
        return Err("Public key was missing alg (OKP)".into());
    }
    if !alg.is_some_and(is_cose_alg) {
        return Err(format!(
            "Public key had invalid alg {} (OKP)",
            alg.map_or_else(|| "undefined".into(), Cbor::display)
        ));
    }
    let crv = key.get_num(-1.0);
    if !truthy(crv) {
        return Err("Public key was missing crv (OKP)".into());
    }
    let x = key.get_num(-2.0);
    if !truthy(x) {
        return Err("Public key was missing x (OKP)".into());
    }
    if num(crv) != Some(6.0) {
        return Err(format!(
            "Unexpected COSE crv value of {} (OKP)",
            crv.map_or_else(|| "undefined".into(), Cbor::display)
        ));
    }
    let x: [u8; 32] = bytes(x)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| "Invalid keyData".to_string())?;
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(&x).map_err(|_| "Invalid keyData".to_string())?;
    let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else {
        return Ok(false);
    };
    // RFC 8032 cofactorless verification with a canonical S, as OpenSSL's Ed25519.
    Ok(key.verify(data, &sig).is_ok())
}

fn verify_rsa(
    key: &Cbor,
    sig: &[u8],
    data: &[u8],
    hash_override: Option<&Cbor>,
) -> Result<bool, String> {
    use rsa::pkcs1v15::Pkcs1v15Sign;
    use rsa::{BigUint, Pss, RsaPublicKey};
    let alg = key.get_num(3.0);
    let (n, e) = (key.get_num(-1.0), key.get_num(-2.0));
    if !truthy(alg) {
        return Err("Public key was missing alg (RSA)".into());
    }
    if !alg.is_some_and(is_cose_alg) {
        return Err(format!(
            "Public key had invalid alg {} (RSA)",
            alg.map_or_else(|| "undefined".into(), Cbor::display)
        ));
    }
    if !truthy(n) {
        return Err("Public key was missing n (RSA)".into());
    }
    if !truthy(e) {
        return Err("Public key was missing e (RSA)".into());
    }
    let kind = key_alg_for(alg)?;
    let mut hash = hash_for(alg)?;
    if let Some(o) = hash_override.filter(|o| o.truthy()) {
        hash = hash_for(Some(o))?;
    }
    if !matches!(kind, KeyAlg::Pkcs1 | KeyAlg::Pss) {
        return Err(format!(
            "Unexpected RSA key algorithm {} ({})",
            alg.map_or_else(|| "undefined".into(), Cbor::display),
            match kind {
                KeyAlg::Ed25519 => "Ed25519",
                KeyAlg::Ecdsa => "ECDSA",
                KeyAlg::MlDsa => "ML-DSA",
                _ => "",
            }
        ));
    }
    let (Some(n), Some(e)) = (bytes(n), bytes(e)) else {
        return Err("Invalid keyData".into());
    };
    let pk = RsaPublicKey::new_with_max_size(
        BigUint::from_bytes_be(n),
        BigUint::from_bytes_be(e),
        16384,
    )
    .map_err(|_| "Invalid keyData".to_string())?;
    let digest = hash.digest(data);
    let ok = match (kind, hash) {
        (KeyAlg::Pkcs1, Hash::Sha1) => pk.verify(Pkcs1v15Sign::new::<sha1::Sha1>(), &digest, sig),
        (KeyAlg::Pkcs1, Hash::Sha256) => {
            pk.verify(Pkcs1v15Sign::new::<sha2::Sha256>(), &digest, sig)
        }
        (KeyAlg::Pkcs1, Hash::Sha384) => {
            pk.verify(Pkcs1v15Sign::new::<sha2::Sha384>(), &digest, sig)
        }
        (KeyAlg::Pkcs1, Hash::Sha512) => {
            pk.verify(Pkcs1v15Sign::new::<sha2::Sha512>(), &digest, sig)
        }
        (_, Hash::Sha256) => pk.verify(Pss::new_with_salt::<sha2::Sha256>(32), &digest, sig),
        (_, Hash::Sha384) => pk.verify(Pss::new_with_salt::<sha2::Sha384>(48), &digest, sig),
        (_, Hash::Sha512) => pk.verify(Pss::new_with_salt::<sha2::Sha512>(64), &digest, sig),
        // RSA-PSS with SHA-1 has a salt length of 0 in simplewebauthn.
        (_, Hash::Sha1) => pk.verify(Pss::new_with_salt::<sha1::Sha1>(0), &digest, sig),
    };
    Ok(ok.is_ok())
}

/// `verifySignature({ signature, data, credentialPublicKey, hashAlgorithm })`: `Ok(verified)`, or
/// the error Node throws.
pub fn verify(
    key: &Cbor,
    sig: &[u8],
    data: &[u8],
    hash_override: Option<&Cbor>,
) -> Result<bool, String> {
    let kty = key.get_num(1.0);
    let is = |want: f64| kty.is_some_and(is_cose_kty) && num(kty) == Some(want);
    if is(2.0) {
        return verify_ec2(key, sig, data, hash_override);
    }
    if is(3.0) {
        return verify_rsa(key, sig, data, hash_override);
    }
    if is(1.0) {
        return verify_okp(key, sig, data);
    }
    if is(7.0) {
        return Err("ML-DSA (AKP) public keys are not supported by this port".into());
    }
    Err(format!(
        "Signature verification with public key of kty {} is not supported by this method",
        kty.map_or_else(|| "undefined".into(), Cbor::display)
    ))
}
