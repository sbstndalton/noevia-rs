#!/usr/bin/env node
'use strict';
// Passkey compatibility fixtures for crates/passkey (full-Rust migration M3).
//
// A software authenticator (node:crypto keys: P-256, P-384 with alg -7, Ed25519, RSA 2048/3072)
// builds registration and authentication responses the way browsers send them, plus malformed
// and hostile variants. Each one is verified by noevia-core's own @simplewebauthn/server (v14,
// from the checkout's server/node_modules) with the options core auth.cjs passes
// (requireUserVerification: true, an array of origins, one RP ID). The output records Node's
// verdict, its message, and for a registration exactly what auth.cjs stores (credential id, the
// public key bytes, counter, transports, device type, backed-up flag); for an authentication the
// new counter. crates/passkey/tests/compat.rs requires the same verdicts, the same stored fields
// and the same messages where Rust claims to reproduce them, and never a Rust accept where Node
// refuses.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules> \
//     node tools/gen-passkey-fixtures.cjs > crates/passkey/tests/fixtures/simplewebauthn-v14.json
//
// Keys and ECDSA signatures are random per run; CI runs the Rust test against the fresh output.
// Synthetic origins and users only; nothing touches the network.

const crypto = require('node:crypto');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout (with server/node_modules installed).\n');
  process.exit(2);
}
const server = path.join(path.resolve(checkout), 'server');
const from = (m) => require(require.resolve(m, { paths: [server] }));
const swa = from('@simplewebauthn/server');
const swaPkg = from('@simplewebauthn/server/package.json');
const cbor = from('@levischuck/tiny-cbor');

const b64u = (b) => Buffer.from(b).toString('base64url');
const sha256 = (b) => crypto.createHash('sha256').update(b).digest();
const ORIGIN = 'https://noevia.example.test';
const ORIGINS = [ORIGIN, 'https://old.example.test'];
const RP = 'noevia.example.test';

// ---- keys ------------------------------------------------------------------------------------

function makeKey(kind) {
  if (kind === 'es256' || kind === 'p384-alg-7') {
    const curve = kind === 'es256' ? 'P-256' : 'P-384';
    const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: curve });
    const jwk = publicKey.export({ format: 'jwk' });
    const cose = new Map([[1, 2], [3, -7], [-1, curve === 'P-256' ? 1 : 2], [-2, Buffer.from(jwk.x, 'base64url')], [-3, Buffer.from(jwk.y, 'base64url')]]);
    return { kind, alg: -7, cose, sign: (d) => crypto.sign(curve === 'P-256' ? 'sha256' : 'sha256', d, privateKey), curve };
  }
  if (kind === 'eddsa') {
    const { privateKey, publicKey } = crypto.generateKeyPairSync('ed25519');
    const jwk = publicKey.export({ format: 'jwk' });
    const cose = new Map([[1, 1], [3, -8], [-1, 6], [-2, Buffer.from(jwk.x, 'base64url')]]);
    return { kind, alg: -8, cose, sign: (d) => crypto.sign(null, d, privateKey) };
  }
  if (kind === 'rs256' || kind === 'rs256-3072') {
    const { privateKey, publicKey } = crypto.generateKeyPairSync('rsa', { modulusLength: kind === 'rs256' ? 2048 : 3072 });
    const jwk = publicKey.export({ format: 'jwk' });
    const cose = new Map([[1, 3], [3, -257], [-1, Buffer.from(jwk.n, 'base64url')], [-2, Buffer.from(jwk.e, 'base64url')]]);
    return { kind, alg: -257, cose, sign: (d) => crypto.sign('sha256', d, privateKey) };
  }
  if (kind === 'es384') {
    const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-384' });
    const jwk = publicKey.export({ format: 'jwk' });
    const cose = new Map([[1, 2], [3, -35], [-1, 2], [-2, Buffer.from(jwk.x, 'base64url')], [-3, Buffer.from(jwk.y, 'base64url')]]);
    return { kind, alg: -35, cose, sign: (d) => crypto.sign('sha384', d, privateKey) };
  }
  throw new Error(kind);
}

const enc = (v) => Buffer.from(cbor.encodeCBOR(v));
const u8 = (b) => new Uint8Array(b);
/** tiny-cbor wants Uint8Arrays (a Buffer is one) and Maps. */
const coseBytes = (cose) => enc(new Map([...cose].map(([k, v]) => [k, Buffer.isBuffer(v) ? u8(v) : v])));

// ---- authenticator data ------------------------------------------------------------------------

const FLAG = { UP: 1, UV: 4, BE: 8, BS: 16, AT: 64, ED: 128 };

function authData({ rp = RP, flags = FLAG.UP | FLAG.UV, counter = 0, credId = null, keyBytes = null, aaguid = Buffer.alloc(16), ext = null, tail = null }) {
  const parts = [sha256(Buffer.from(rp, 'ascii')), Buffer.from([flags]), Buffer.alloc(4)];
  parts[2].writeUInt32BE(counter >>> 0);
  if (credId) {
    const len = Buffer.alloc(2); len.writeUInt16BE(credId.length);
    parts.push(aaguid, len, credId, keyBytes);
  }
  if (ext) parts.push(ext);
  if (tail) parts.push(tail);
  return Buffer.concat(parts);
}

function clientData(fields) {
  return b64u(Buffer.from(JSON.stringify(fields)));
}

// ---- registration ------------------------------------------------------------------------------

async function nodeRegistration(response, expected) {
  try {
    const v = await swa.verifyRegistrationResponse({ response, expectedChallenge: expected.challenge, expectedOrigin: expected.origins, expectedRPID: expected.rpId, requireUserVerification: true });
    if (!v.verified || !v.registrationInfo) return { ok: false, message: 'passkey registration failed' };
    const info = v.registrationInfo; const cred = info.credential;
    // Exactly what auth.cjs registrationVerify stores.
    return { ok: true, stored: { id: cred.id, publicKey: b64u(Buffer.from(cred.publicKey)), counter: cred.counter, transports: JSON.stringify(cred.transports || []), deviceType: info.credentialDeviceType, backedUp: info.credentialBackedUp ? 1 : 0, fmt: info.fmt } };
  } catch (e) {
    return { ok: false, message: String(e && e.message) };
  }
}

async function nodeAuthentication(response, expected, stored) {
  try {
    const v = await swa.verifyAuthenticationResponse({ response, expectedChallenge: expected.challenge, expectedOrigin: expected.origins, expectedRPID: expected.rpId,
      credential: { id: stored.id, publicKey: new Uint8Array(Buffer.from(stored.publicKey, 'base64url')), counter: stored.counter, transports: JSON.parse(stored.transports) }, requireUserVerification: true });
    if (!v.verified) return { ok: false, message: 'authentication failed' };
    return { ok: true, newCounter: v.authenticationInfo.newCounter };
  } catch (e) {
    return { ok: false, message: String(e && e.message) };
  }
}

const challenge = () => b64u(crypto.randomBytes(32));

function registrationResponse(key, opts = {}) {
  const credId = opts.credId || crypto.randomBytes(16);
  const ch = opts.challenge || challenge();
  const cdj = opts.cdj || clientData({ type: 'webauthn.create', challenge: opts.clientChallenge ?? ch, origin: opts.origin ?? ORIGIN, crossOrigin: false, ...(opts.clientExtra || {}) });
  const keyBytes = opts.keyBytes || coseBytes(key.cose);
  const ad = opts.authData || authData({ rp: opts.rp, flags: opts.flags ?? (FLAG.UP | FLAG.UV | FLAG.AT | (opts.extraFlags || 0)), counter: opts.counter ?? 0, credId, keyBytes, ext: opts.ext, tail: opts.tail });
  let attStmt = opts.attStmt || new Map();
  let fmt = opts.fmt || 'none';
  if (opts.packedSelf) {
    fmt = 'packed';
    const sig = key.sign(Buffer.concat([ad, sha256(Buffer.from(cdj, 'base64url'))]));
    attStmt = new Map([['alg', key.alg], ['sig', u8(opts.badSig ? Buffer.from(sig).fill(1, 8, 9) : sig)]]);
  }
  const ao = opts.attestationObject || b64u(enc(new Map([['fmt', fmt], ['attStmt', attStmt], ['authData', u8(ad)]])));
  const id = opts.id ?? b64u(credId);
  const response = {
    id, rawId: opts.rawId ?? id, type: opts.type ?? 'public-key',
    response: { clientDataJSON: cdj, attestationObject: ao, ...(opts.transports === undefined ? { transports: ['internal', 'hybrid'] } : opts.transports === null ? {} : { transports: opts.transports }) },
    clientExtensionResults: {}, authenticatorAttachment: 'platform',
  };
  return { response, expected: { challenge: ch, origins: ORIGINS, rpId: opts.expectRp || RP } };
}

function derHighS(der, curveOrder) {
  // SEQUENCE { INTEGER r, INTEGER s } -> s := n - s (still a valid signature).
  let i = 2; if (der[1] & 0x80) i += der[1] & 0x7f;
  const rLen = der[i + 1]; const r = der.subarray(i + 2, i + 2 + rLen); i += 2 + rLen;
  const sLen = der[i + 1]; const sv = BigInt(`0x${der.subarray(i + 2, i + 2 + sLen).toString('hex')}`);
  let hi = (curveOrder - sv).toString(16); if (hi.length % 2) hi = `0${hi}`;
  let sBytes = Buffer.from(hi, 'hex'); if (sBytes[0] & 0x80) sBytes = Buffer.concat([Buffer.from([0]), sBytes]);
  const body = Buffer.concat([Buffer.from([2, r.length]), r, Buffer.from([2, sBytes.length]), sBytes]);
  return Buffer.concat([Buffer.from([0x30, body.length]), body]);
}
const P256_N = BigInt('0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551');

function assertionResponse(key, credIdB64, opts = {}) {
  const ch = opts.challenge || challenge();
  const cdj = opts.cdj || clientData({ type: opts.cdType ?? 'webauthn.get', challenge: opts.clientChallenge ?? ch, origin: opts.origin ?? ORIGIN, crossOrigin: opts.crossOrigin ?? false, ...(opts.clientExtra || {}) });
  const ad = opts.authData || authData({ rp: opts.rp, flags: opts.flags ?? (FLAG.UP | FLAG.UV | (opts.extraFlags || 0)), counter: opts.counter ?? 0, ext: opts.ext });
  let sig = key.sign(Buffer.concat([ad, sha256(Buffer.from(cdj, 'base64url'))]));
  if (opts.highS) sig = derHighS(Buffer.from(sig), P256_N);
  if (opts.badSig) { sig = Buffer.from(sig); sig[sig.length - 3] ^= 1; }
  // Padding shortens what @hexagon/base64 decodes (one byte per '='), whatever came before it.
  const sigText = b64u(sig) + (opts.padSig || '');
  const response = {
    id: opts.id ?? credIdB64, rawId: opts.rawId ?? opts.id ?? credIdB64, type: 'public-key',
    response: { clientDataJSON: cdj, authenticatorData: opts.authenticatorData ?? b64u(ad), signature: opts.signature ?? sigText, ...(opts.userHandle === undefined ? { userHandle: b64u(Buffer.from('synthetic-user')) } : { userHandle: opts.userHandle }) },
    clientExtensionResults: {}, authenticatorAttachment: 'platform',
  };
  return { response, expected: { challenge: ch, origins: ORIGINS, rpId: opts.expectRp || RP } };
}

async function main() {
  const out = { version: 1, generator: 'tools/gen-passkey-fixtures.cjs', simplewebauthn: swaPkg.version, registrations: [], authentications: [] };
  const reg = async (name, built) => {
    const result = await nodeRegistration(built.response, built.expected);
    out.registrations.push({ name, response: built.response, expected: built.expected, result });
    return result;
  };
  const auth = async (name, built, stored) => {
    const result = await nodeAuthentication(built.response, built.expected, stored);
    out.authentications.push({ name, response: built.response, expected: built.expected, stored, result });
    return result;
  };

  for (const kind of ['es256', 'eddsa', 'rs256', 'rs256-3072', 'p384-alg-7']) {
    for (let round = 0; round < 3; round += 1) {
      const key = makeKey(kind);
      const multi = round === 1;
      const r = await reg(`${kind} #${round}${multi ? ' multi-device backed up' : ''}`, registrationResponse(key, { extraFlags: multi ? FLAG.BE | FLAG.BS : 0, counter: round === 2 ? 7 : 0, transports: round === 2 ? null : undefined }));
      if (!r.ok) throw new Error(`${kind} did not register: ${r.message}`);
      const stored = r.stored;
      const counter = (c) => ({ ...stored, counter: c });
      const extra = multi ? FLAG.BE | FLAG.BS : 0;
      await auth(`${kind} #${round} sign in`, assertionResponse(key, stored.id, { counter: stored.counter + 1, extraFlags: extra }), stored);
      await auth(`${kind} #${round} counter 0 stays 0`, assertionResponse(key, stored.id, { counter: 0, extraFlags: extra }), counter(0));
      await auth(`${kind} #${round} counter not advanced`, assertionResponse(key, stored.id, { counter: 5, extraFlags: extra }), counter(5));
      await auth(`${kind} #${round} counter went back`, assertionResponse(key, stored.id, { counter: 4, extraFlags: extra }), counter(5));
      await auth(`${kind} #${round} counter jumps`, assertionResponse(key, stored.id, { counter: 4_000_000_000, extraFlags: extra }), counter(5));
      await auth(`${kind} #${round} bad signature`, assertionResponse(key, stored.id, { counter: 9, badSig: true }), counter(0));
      await auth(`${kind} #${round} wrong rp`, assertionResponse(key, stored.id, { rp: 'evil.example.test', counter: 9 }), counter(0));
      await auth(`${kind} #${round} legacy rp expected`, assertionResponse(key, stored.id, { rp: 'old.example.test', expectRp: 'old.example.test', counter: 9, origin: 'https://old.example.test' }), counter(0));
      await auth(`${kind} #${round} no UV`, assertionResponse(key, stored.id, { flags: FLAG.UP, counter: 9 }), counter(0));
      await auth(`${kind} #${round} no UP`, assertionResponse(key, stored.id, { flags: FLAG.UV, counter: 9 }), counter(0));
      await auth(`${kind} #${round} backed up single device`, assertionResponse(key, stored.id, { extraFlags: FLAG.BS, counter: 9 }), counter(0));
      await auth(`${kind} #${round} wrong challenge`, assertionResponse(key, stored.id, { clientChallenge: 'not-the-challenge', counter: 9 }), counter(0));
      await auth(`${kind} #${round} wrong origin`, assertionResponse(key, stored.id, { origin: 'https://evil.example.test', counter: 9 }), counter(0));
      await auth(`${kind} #${round} create type`, assertionResponse(key, stored.id, { cdType: 'webauthn.create', counter: 9 }), counter(0));
      await auth(`${kind} #${round} top origin`, assertionResponse(key, stored.id, { clientExtra: { topOrigin: 'https://frame.example.test' }, counter: 9 }), counter(0));
      await auth(`${kind} #${round} cross origin with top`, assertionResponse(key, stored.id, { crossOrigin: true, clientExtra: { topOrigin: 'https://frame.example.test' }, counter: 9 }), counter(0));
      await auth(`${kind} #${round} cross origin alone`, assertionResponse(key, stored.id, { crossOrigin: true, counter: 9 }), counter(0));
      await auth(`${kind} #${round} token binding ok`, assertionResponse(key, stored.id, { clientExtra: { tokenBinding: { status: 'supported' } }, counter: 9 }), counter(0));
      await auth(`${kind} #${round} token binding bad`, assertionResponse(key, stored.id, { clientExtra: { tokenBinding: { status: 'not-supported' } }, counter: 9 }), counter(0));
      await auth(`${kind} #${round} token binding string`, assertionResponse(key, stored.id, { clientExtra: { tokenBinding: 'present' }, counter: 9 }), counter(0));
      await auth(`${kind} #${round} user handle number`, assertionResponse(key, stored.id, { userHandle: 7, counter: 9 }), counter(0));
      await auth(`${kind} #${round} no user handle`, assertionResponse(key, stored.id, { userHandle: null, counter: 9 }), counter(0));
      await auth(`${kind} #${round} authenticatorData not base64url`, assertionResponse(key, stored.id, { authenticatorData: 'a+b/', counter: 9 }), counter(0));
      await auth(`${kind} #${round} padded signature`, assertionResponse(key, stored.id, { counter: 9, padSig: '=' }), counter(0));
      await auth(`${kind} #${round} double-padded signature`, assertionResponse(key, stored.id, { counter: 9, padSig: '==' }), counter(0));
      await auth(`${kind} #${round} extensions`, assertionResponse(key, stored.id, { extraFlags: FLAG.ED, ext: enc(new Map([['credProps', new Map([['rk', true]])]])), counter: 9 }), counter(0));
      await auth(`${kind} #${round} leftover bytes`, assertionResponse(key, stored.id, { authData: Buffer.concat([authData({ counter: 9 }), Buffer.from([0])]) }), counter(0));
      await auth(`${kind} #${round} short authenticator data`, assertionResponse(key, stored.id, { authenticatorData: b64u(Buffer.alloc(36)) }), counter(0));
      await auth(`${kind} #${round} id differs from rawId`, assertionResponse(key, stored.id, { rawId: 'x', counter: 9 }), counter(0));
      if (kind === 'es256') await auth(`${kind} #${round} high-S signature`, assertionResponse(key, stored.id, { highS: true, counter: 9 }), counter(0));
    }
  }

  // Registration refusals and odd shapes.
  const k = makeKey('es256');
  const e = makeKey('eddsa');
  await reg('wrong type', registrationResponse(k, { clientExtra: { type: 'webauthn.get' } }));
  await reg('wrong challenge', registrationResponse(k, { clientChallenge: 'nope' }));
  await reg('wrong origin', registrationResponse(k, { origin: 'https://evil.example.test' }));
  await reg('earlier origin', registrationResponse(k, { origin: 'https://old.example.test' }));
  await reg('wrong rp', registrationResponse(k, { rp: 'evil.example.test' }));
  await reg('no UV', registrationResponse(k, { flags: FLAG.UP | FLAG.AT }));
  await reg('no UP', registrationResponse(k, { flags: FLAG.UV | FLAG.AT }));
  await reg('no attested data', registrationResponse(k, { flags: FLAG.UP | FLAG.UV, authData: authData({ flags: FLAG.UP | FLAG.UV }) }));
  await reg('backed up single device', registrationResponse(k, { extraFlags: FLAG.BS }));
  await reg('unsupported alg ES384', registrationResponse(makeKey('es384')));
  await reg('alg missing', registrationResponse(k, { keyBytes: enc(new Map([[1, 2], [-1, 1], [-2, u8(k.cose.get(-2))], [-3, u8(k.cose.get(-3))]])) }));
  await reg('alg as text', registrationResponse(k, { keyBytes: enc(new Map([[1, 2], [3, 'ES256'], [-1, 1], [-2, u8(k.cose.get(-2))], [-3, u8(k.cose.get(-3))]])) }));
  await reg('none with statement', registrationResponse(k, { attStmt: new Map([['sig', u8(Buffer.alloc(4))]]) }));
  await reg('packed self attestation', registrationResponse(k, { packedSelf: true }));
  await reg('packed self attestation eddsa', registrationResponse(e, { packedSelf: true }));
  await reg('packed self attestation bad signature', registrationResponse(k, { packedSelf: true, badSig: true }));
  await reg('packed without alg', registrationResponse(k, { fmt: 'packed', attStmt: new Map([['sig', u8(Buffer.alloc(8))]]) }));
  await reg('packed without sig', registrationResponse(k, { fmt: 'packed', attStmt: new Map([['alg', -7]]) }));
  await reg('packed invalid alg', registrationResponse(k, { fmt: 'packed', attStmt: new Map([['alg', -1], ['sig', u8(Buffer.alloc(8))]]) }));
  await reg('unknown format', registrationResponse(k, { fmt: 'made-up' }));
  await reg('id differs from rawId', registrationResponse(k, { rawId: 'other' }));
  await reg('missing id', registrationResponse(k, { id: '' }));
  await reg('wrong credential type', registrationResponse(k, { type: 'password' }));
  await reg('token binding ok', registrationResponse(k, { clientExtra: { tokenBinding: { status: 'not-supported' } } }));
  await reg('token binding bad status', registrationResponse(k, { clientExtra: { tokenBinding: { status: 'notSupported' } } }));
  await reg('token binding string', registrationResponse(k, { clientExtra: { tokenBinding: 'present' } }));
  await reg('extensions', registrationResponse(k, { extraFlags: FLAG.ED, ext: enc(new Map([['credProps', new Map([['rk', true]])]])) }));
  await reg('leftover bytes', registrationResponse(k, { tail: Buffer.from([0, 1]) }));
  // A key whose CBOR is not the shortest form: Node re-encodes it shorter and walks on by the
  // re-encoded length, so the bytes after it no longer line up.
  const longForm = Buffer.from(coseBytes(k.cose)); const at = longForm.indexOf(Buffer.from([0x21, 0x58, 0x20]));
  const nonMinimal = Buffer.concat([longForm.subarray(0, at), Buffer.from([0x21, 0x59, 0x00, 0x20]), longForm.subarray(at + 3)]);
  await reg('non-minimal key encoding', registrationResponse(k, { keyBytes: nonMinimal }));
  await reg('transports odd value', registrationResponse(k, { transports: 'usb' }));
  await reg('transports empty', registrationResponse(k, { transports: [] }));
  await reg('long credential id', registrationResponse(k, { credId: crypto.randomBytes(1023) }));
  await reg('client data not JSON', registrationResponse(k, { cdj: b64u(Buffer.from('not json')) }));
  await reg('client data null', registrationResponse(k, { cdj: b64u(Buffer.from('null')) }));
  await reg('attestation object garbage', registrationResponse(k, { attestationObject: b64u(Buffer.from([0xff, 0x00])) }));
  // body.response absent (JSON has no undefined: the case says so instead).
  const none = { challenge: 'c', origins: ORIGINS, rpId: RP };
  out.registrations.push({ name: 'response missing', responseUndefined: true, response: null, expected: none, result: await nodeRegistration(undefined, none) });
  out.registrations.push({ name: 'response null', response: null, expected: none, result: await nodeRegistration(null, none) });
  out.registrations.push({ name: 'inner response missing', response: { id: 'a', rawId: 'a', type: 'public-key' }, expected: none, result: await nodeRegistration({ id: 'a', rawId: 'a', type: 'public-key' }, none) });

  process.stdout.write(`${JSON.stringify(out, null, 1)}\n`);
}

main().catch((err) => { process.stderr.write(`gen-passkey-fixtures failed: ${err && err.stack ? err.stack.split('\n').slice(0, 3).join(' ') : 'error'}\n`); process.exit(1); });
