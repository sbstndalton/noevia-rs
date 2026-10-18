#!/usr/bin/env python3
"""Actual FastAPI path/int/method/auth contract; synthetic SQLite only, no startup/network."""
import asyncio,hashlib,json,os,sys,tempfile
import fastapi,starlette,pydantic,pydantic_core
from urllib.parse import unquote
from pathlib import Path
for key in list(os.environ):
 if key.startswith("NOEVIA_NATIVE_MODEL_MANAGER_"):os.environ.pop(key)
sys.path.insert(0,str(Path(sys.argv[1]).resolve().parent))
from app import main,api,db,helpers
from app.config import settings
TOKEN="synthetic-model-loader-token-0123456789abcdef"
PREFIX="/api/v1/benchmark/runs/"
segments=["1","0001","+1","1.0","1.00","1_0","+01.00","-0","-1","%31","%2B1","%201%20","%C2%A01%C2%A0","1e0","1.1","1.",".0","_1","1__0","abc","%FF","bad%zz","%252F","%2F","1%2F2","9223372036854775808","-9223372036854775809","0"*80+"1","9"*120,"1.0_0","1_.0","1._0","%EF%BC%91","%D9%A1","%1C1%1C","9"*4301,"0"*4301+"1","9"*4301+"x","+"+"9"*4301,"1."+"0"*4301,"0"*100+"9"*4300,"0__1","0_-1","0_0001","00_","0_.0","00.0","+-1","-+1","--1","-0_-1"]
cases=[(s,"GET",PREFIX+s,True) for s in segments]
cases += [("encoded-namespace","GET","/%61pi/v1/benchmark/runs/%31",True),("redirect-multiple","GET",PREFIX+"1//",True),("redirect-post-multiple","POST",PREFIX+"abc///?qa=2",True),("redirect-encoded-slashes","GET",PREFIX+"1%2F%2F",True),("redirect-question","GET",PREFIX+"1%3Fqa/",True),("redirect-fragment","GET",PREFIX+"1%23qa/",True),("redirect-unicode","GET",PREFIX+"%C3%A9/",True),("redirect-space","GET",PREFIX+"%201%20/",True),("empty","GET",PREFIX,True),("collection","GET",PREFIX[:-1],True),("nested","GET",PREFIX+"1/extra",True)]
for method in ["GET","HEAD","POST","PUT","DELETE","OPTIONS","PATCH"]:
 for segment in ["1","abc","1/"]:
  cases.append((method+"-"+segment,method,PREFIX+segment+"?qa=1%20x",True))
cases += [("auth-before-validation","GET",PREFIX+"abc",False),("auth-before-redirect","POST",PREFIX+"1/",False),("auth-before-method","POST",PREFIX+"1",False)]
with tempfile.TemporaryDirectory(prefix="qa-bench-paths-") as tmp:
 root=Path(tmp);settings.data_dir=root;settings.model_loader_token=TOKEN;db._DB_PATH=None;db.init()
 with db._conn() as c:c.execute("INSERT INTO bench_run(id,backend,status,started_at,reps,max_tokens,note) VALUES(1,'synthetic','done',1,1,1,'')")
 helpers._backend_list=lambda:[]
 helpers._predicted_vram_gb=lambda alias:None
 async def request(method,path,auth):
  raw,_,query=path.partition("?")
  messages=[];sent=False
  async def receive():
   nonlocal sent
   if not sent:sent=True;return {"type":"http.request","body":b"","more_body":False}
   await asyncio.Future()
  async def send(message):messages.append(message)
  headers=[(b"host",b"testserver")]+([(b"x-model-loader-token",TOKEN.encode())] if auth else [])
  scope={"type":"http","asgi":{"version":"3.0"},"http_version":"1.1","method":method,"scheme":"http","path":unquote(raw),"raw_path":raw.encode(),"query_string":query.encode(),"root_path":"","headers":headers,"server":("testserver",80),"client":("synthetic",1)}
  try:await main.app(scope,receive,send)
  except Exception:
   assert any(m["type"]=="http.response.start" for m in messages)
  start=next(m for m in messages if m["type"]=="http.response.start")
  body=b"".join(m.get("body",b"") for m in messages if m["type"]=="http.response.body")
  headers={k.decode():v.decode() for k,v in start["headers"] if k.decode() in ["content-type","allow","location"]}
  return start["status"],"" if method=="HEAD" else body.decode(),headers
 async def collect():
  out=[]
  for ident,method,path,auth in cases:
   status,body,headers=await request(method,path,auth)
   out.append(dict(id=ident,method=method,path=path,auth=auth,status=status,body=body,headers=headers))
  return out
 out=asyncio.run(collect())
 print(json.dumps(dict(framework={"fastapi":fastapi.__version__,"starlette":starlette.__version__,"pydantic":pydantic.__version__,"pydantic_core":pydantic_core.__version__},source_sha256={n:hashlib.sha256((Path(sys.argv[1])/n).read_bytes()).hexdigest() for n in ["api.py","main.py","db.py"]},cases=out),ensure_ascii=True,sort_keys=True,indent=2))
