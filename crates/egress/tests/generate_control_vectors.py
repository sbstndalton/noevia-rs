"""Independent stdlib vectors for byte authentication, not runtime qualification.

Fixed synthetic inputs only. Never imports/invokes Rust or Node or modifies the
original ngr1 vectors. The new protocol has no retired implementation oracle.
"""
import argparse
import base64
import hashlib
import hmac
import json
from pathlib import Path

CAPABILITY = "noevia-code-egress-control/1"


def mac(key, data):
    return hmac.new(key, data, hashlib.sha256).digest()


def digest(data):
    return hashlib.sha256(data).hexdigest()


def b64(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True)


def vectors():
    master, nonce = bytes(range(32)), bytes(range(32, 64))
    boot, session = bytes(range(16)), bytes(range(16, 32))
    file_key = mac(master, (CAPABILITY + ":web-key\0").encode() + nonce)
    control_key = mac(file_key, (CAPABILITY + ":control-key\0").encode())
    data_key = mac(file_key, (CAPABILITY + ":grant-key\0").encode() + boot + session)
    fingerprint = digest(file_key)
    grant = compact(dict(v=1, act="grant", task="task-1", hosts=["example.com"],
                         iat=1700000000000, exp=1700000060000, idle=30000,
                         nonce=b64(bytes(range(16)))))
    signed = "ngr1." + b64(grant.encode())
    cases = []
    for i, action in enumerate(("ready", "renew", "activity", "revoke", "forget")):
        path = "/v1/control/" + action
        request_id = bytes([i + 1] * 16).hex()
        deadline = 1700000030000 + i
        known_boot = None if action == "ready" else boot.hex()
        # Deliberately opaque examples: semantic operation schemas/state are a
        # separate runtime stage; these fixtures authenticate exact bytes only.
        body = compact(dict(example=action, sessionId=session.hex(), sequence=i))
        transcript = (f"{CAPABILITY}:request\nPOST\n{path}\n{request_id}\n"
                      f"{deadline}\n{known_boot or ''}\n{fingerprint}\n"
                      f"{digest(body.encode())}\n")
        tag = mac(control_key, transcript.encode()).hex()
        status = 200 if i % 2 == 0 else 409
        response = compact(dict(example=action, result="synthetic"))
        reply = (f"{CAPABILITY}:response\n{tag}\n{status}\n"
                 f"{digest(response.encode())}\n")
        cases.append(dict(route=path, requestId=request_id, deadlineMs=deadline,
                          bootId=known_boot, keyFingerprint=fingerprint,
                          body=body, requestMac=tag, responseStatus=status,
                          responseBody=response,
                          responseMac=mac(control_key, reply.encode()).hex()))
    return dict(capability=CAPABILITY, scope="byte-authentication only; no runtime admission",
                masterKey=master.hex(), webNonce=nonce.hex(), fileKey=file_key.hex(),
                fingerprint=fingerprint, bootId=boot.hex(), sessionId=session.hex(),
                grantToken=signed + "." + b64(mac(data_key, signed.encode())),
                cases=cases)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    target = Path(__file__).resolve().parents[3] / "contracts/egress-control.v1.vectors.json"
    expected = (json.dumps(vectors(), indent=2) + "\n").encode()
    if args.check:
        if target.read_bytes() != expected:
            raise SystemExit("control vectors differ; no files changed")
    else:
        target.write_bytes(expected)
