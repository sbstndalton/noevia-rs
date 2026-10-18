#!/usr/bin/env python3
"""Actual pinned ASGI framework, stubbed endpoint execution only. No startup/network/Docker."""
import asyncio,hashlib,inspect,json,os,sys
from pathlib import Path
from urllib.parse import unquote
import fastapi,starlette,pydantic,pydantic_core
for key in tuple(os.environ):
 if key.startswith("NOEVIA_NATIVE_MODEL_MANAGER_"):os.environ.pop(key)
sys.path.insert(0,str(Path(sys.argv[1]).resolve().parent))
from app import main
from app.config import settings
settings.model_loader_token="qa-token"
registered=[]
for route in main.app.routes:
 if not getattr(route,"path","").startswith("/api/v1/"):continue
 registered.append((next(iter(route.methods)),route.path))
 def replace(path,async_call):
  if async_call:
   async def call(**kwargs):return {"owner":path,"arguments":kwargs}
  else:
   def call(**kwargs):return {"owner":path,"arguments":kwargs}
  return call
 route.dependant.call=replace(route.path,inspect.iscoroutinefunction(route.dependant.call))
paths=list(dict.fromkeys(path for _,path in registered))
def example(path):
 return path.replace("{name}","qa-backend").replace("{pid}","1").replace("{run_id}","1").replace("{job_id}","qa-job")
cases=[]
for path in paths:
 for method in ["GET","POST","PUT","DELETE","PATCH","OPTIONS","HEAD"]:
  for slash in ["","/"]:
   cases.append((method,example(path)+slash+"?qa=%2F&repo=qa-owner%2Fqa-repo",True,"{}"))
 for method,p in registered:
  if p==path:cases.append((method,example(path).replace("/api/","/%61pi/")+"?repo=qa-owner%2Fqa-repo",True,"{}"));break
for method,path,body in [
 ("POST","/api/v1/backends/qa%2Dbackend/%72estart","{}"),
 ("POST","/api/v1/backends/qa%252Fbackend/test","{bad"),
 ("POST","/api/v1/backends/qa%2Fbackend/test","{bad"),
 ("POST","/api/v1/backends/qa%2Dbackend/test","{bad"),
 ("PATCH","/api/v1/settings///?qa=%2F","{bad"),
 ("GET","/api%2Fv1%2Fsettings/","{}"),
 ("GET","/api/v1/sections/qa%252Fbad/","{}"),
 ("GET","/api/v1/sections/qa%23x/?qa=%23","{}"),
 ("GET","/api/v1/sections/qa%3Fx/?qa=%3F","{}"),
 ("GET","/api/v1/sections/qa%FF/","{}"),
 ("GET","/api/v1/sections/qa%zz/","{}"),
 ("GET","/api/v1/settings%0A","{}"),
 ("GET","/api/v1/settings%0A%0A","{}"),
 ("POST","/api/v1/backends/qa/restart%0A","{}"),
 ("GET","/api/v1/sections/qa%0A","{}"),
 ("POST","/api/v1/unknown","{bad"),
 ("GET","/api/v1/settings/extra","{}")]:cases.append((method,path,True,body))
cases += [("HEAD","/api/v1/settings/",False,"{bad"),("POST","/api/v1/settings",False,"{bad"),("POST","/api/v1/backends/qa/test",False,"{bad")]
async def probe(method,target,auth,body):
 raw,_,query=target.partition("?");messages=[];sent=False
 async def receive():
  nonlocal sent
  if not sent:sent=True;return {"type":"http.request","body":body.encode(),"more_body":False}
  await asyncio.Future()
 async def send(message):messages.append(message)
 headers=[(b"host",b"qa.example:8080"),(b"content-type",b"application/json")]+([(b"x-model-loader-token",b"qa-token")]if auth else [])
 scope={"type":"http","asgi":{"version":"3.0"},"http_version":"1.1","method":method,"scheme":"http","path":unquote(raw),"raw_path":raw.encode(),"query_string":query.encode(),"root_path":"","headers":headers,"server":("qa.example",8080),"client":("synthetic",1)}
 await asyncio.wait_for(main.app(scope,receive,send),5)
 start=next(m for m in messages if m["type"]=="http.response.start")
 data=b"".join(m.get("body",b"")for m in messages if m["type"]=="http.response.body")
 headers={k.decode():v.decode() for k,v in start["headers"] if k in(b"allow",b"location")}
 return dict(method=method,path=target,auth=auth,request_body=body,status=start["status"],headers=headers,body=""if method=="HEAD"else data.decode())
async def run():
 out=[]
 for c in cases:out.append(await probe(*c))
 print(json.dumps(dict(framework={"fastapi":fastapi.__version__,"starlette":starlette.__version__,"pydantic":pydantic.__version__,"pydantic_core":pydantic_core.__version__},source_sha256={n:hashlib.sha256((Path(sys.argv[1])/n).read_bytes()).hexdigest()for n in["api.py","main.py"]},registered=registered,cases=out),ensure_ascii=True,sort_keys=True,indent=2))
asyncio.run(run())
