#!/usr/bin/env python3
"""Run actual hf.validate_token against a synthetic httpx transport, never the network."""
import asyncio, hashlib, json, sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]).resolve().parent))
from app import hf
import httpx
real_client = httpx.AsyncClient
cases = [
    ("empty", "", 200, "{}"),
    ("name", "synthetic-token", 200, '{"name":"synthetic-user","fullname":"Full Name"}'),
    ("fullname", "synthetic-token", 200, '{"name":"","fullname":"Full Name"}'),
    ("unknown", "synthetic-token", 200, '{}'),
    ("numeric-name", "synthetic-token", 200, '{"name":42}'),
    ("compound-name", "synthetic-token", 200, '{"name":["a",1]}'),
    ("rejected", "synthetic-token", 401, 'invalid token'),
    ("redirect", "synthetic-token", 302, 'redirect'),
    ("unicode-prefix", "synthetic-token", 403, 'é'*121),
]
async def main():
    out=[]
    for ident, token, status, body in cases:
        seen=[]
        def handler(request):
            seen.append(str(request.url))
            assert str(request.url) == hf.HF_API + '/whoami-v2'
            assert request.headers['Authorization'] == 'Bearer ' + token
            return httpx.Response(status, text=body)
        hf.httpx.AsyncClient=lambda **kw: real_client(transport=httpx.MockTransport(handler), **kw)
        ok,message=await hf.validate_token(token)
        assert len(seen)==int(bool(token))
        out.append(dict(id=ident,token=token,status=status,body=body,expected=dict(ok=ok,message=message)))
    json.dump(dict(oracle=dict(source=hashlib.sha256(Path(hf.__file__).read_bytes()).hexdigest()), cases=out),sys.stdout,ensure_ascii=False,indent=1,sort_keys=True)
    print()
asyncio.run(main())
