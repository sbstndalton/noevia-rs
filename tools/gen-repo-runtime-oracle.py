#!/usr/bin/env python3
"""rs214: actual companion private repository runtime, synthetic Docker faults and metadata.
Run with: python tools/gen-repo-runtime-oracle.py <services>/model-manager/app.
No semantic service functions are replaced. The timeout world shortens the real Docker
SDK deadline to 10ms and stalls only its synthetic Unix socket for 30ms.
"""
import importlib.util,sys,copy
from pathlib import Path
root=Path(__file__).resolve().parent.parent
app=Path(sys.argv[1]).resolve()
spec=importlib.util.spec_from_file_location("fit_oracle",root/"tools/gen-fit-oracle.py")
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
m.RESERVE_FORMS=list(dict.fromkeys([*m.RESERVE_FORMS,"1e1",".5","1.","+1E-2","1_0","1__0","\u00a08\u00a0","1e309","0x10","1,5","١٢"]))
orig=m.repo_cases
def cases():
 out=orig()
 base=copy.deepcopy(out[0])
 for ident,env,asks in [
 ("extended-nonfinite",{},[({"layers":32,"ctx":32768,"set":{"context_length":float("inf")}},[(4*m.GIB,True)],None),({"layers":32,"ctx":32768,"set":{"context_length":float("nan")}},[(4*m.GIB,True)],None)]),
 ("extended-large-group",{},[({"layers":32,"ctx":32768},[(2**53+1,True)],None),({"layers":32,"ctx":32768},[(2**127-1,True)],None)]),
 ("extended-projector",{},[({"layers":32,"ctx":32768},[(4*m.GIB,True)],2**53),({"layers":32,"ctx":32768},[(4*m.GIB,True)],2**53+1),(None,[(1,False)],2**100)]),
 ("extended-reserve",{"HOST_RAM_RESERVE_GB":"1e1"},[({"layers":32,"ctx":32768},[(4*m.GIB,True)],None)]),
 ("extended-context-json",{},[({"layers":32,"ctx":32768,"set":{"context_length":[1]}},[(4*m.GIB,True)],None),({"layers":32,"ctx":32768,"set":{"context_length":{"qa":1}}},[(4*m.GIB,True)],None),({"layers":32,"ctx":32768,"set":{"context_length":True}},[(4*m.GIB,True)],None)]),
 ("extended-docker-api-error",{},[({"layers":32,"ctx":32768},[(4*m.GIB,True)],None)]),
 ("extended-docker-timeout",{},[({"layers":32,"ctx":32768},[(4*m.GIB,True)],None)]),
 ("extended-missing-docker",{},[({"layers":32,"ctx":32768},[(4*m.GIB,True)],None)]),
 ]:
  c=copy.deepcopy(base);c.update(id=ident,env=env,asks=asks)
  if ident=="extended-missing-docker":c["docker"]=None
  if ident in ("extended-docker-api-error","extended-docker-timeout"):c["docker"]["qa_fault"]=ident
  out.append(c)
 return out
original_cases=cases
def cases_with_configured_faults():
 out=original_cases()
 for fault in ("extended-missing-docker","extended-docker-api-error","extended-docker-timeout"):
  c=copy.deepcopy(next(case for case in out if case["id"]==fault))
  c["id"]=fault+"-configured-cached"
  c["env"]["LLAMA_CONTAINERS"]="llama-cuda"
  out.append(c)
 return out
m.repo_cases=cases_with_configured_faults
import docker, threading
real_get=m.bo.Daemon.do_GET
def fault_get(self):
 fault=(m.bo.STATE["world"] or {}).get("qa_fault")
 if fault=="extended-docker-api-error":
  self.send_response(500);self.send_header("Content-Length","0");self.end_headers();return
 if fault=="extended-docker-timeout":
  threading.Event().wait(0.03);return
 return real_get(self)
m.bo.Daemon.do_GET=fault_get
real_from_env=docker.from_env
def from_env(*args,**kwargs):
 if (m.bo.STATE["world"] or {}).get("qa_fault")=="extended-docker-timeout":kwargs["timeout"]=0.01
 return real_from_env(*args,**kwargs)
docker.from_env=from_env
sys.argv=[str(root/"tools/gen-fit-oracle.py"),str(app)]
import contextlib,io,json
capture=io.StringIO()
with contextlib.redirect_stdout(capture):
 m.main()
data=json.loads(capture.getvalue())
import os,tempfile,threading,json,copy,io
bo=m.bo
temp=tempfile.TemporaryDirectory(prefix="qa-repo-shape-");base=Path(temp.name)
os.environ.update(NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED="1",DATA_DIR=str(base/"data"),MODELS_DIR=str(base/"models"),MODEL_LOADER_TOKEN="",DOCKER_HOST="unix://"+str(base/"docker.sock"))
(base/"data").mkdir();(base/"models").mkdir()
sys.path.insert(0,str(app.parent))
from app import main,services,hw
from app.config import settings
from starlette.testclient import TestClient
case=copy.deepcopy(data["repo"][0])
case["env"]["LLAMA_CONTAINERS"]="llama-cuda"
bo.STATE["world"]=case["docker"]
settings.llama_containers="llama-cuda";settings.gpu_vram="";settings.host_ram_reserve_gb=32
hw.load_pushed(json.loads(case["cache"]))
hw.open=lambda *args,**kwargs:io.StringIO("MemTotal: 67108864 kB\n")
mode={"value":""}
import docker
shape_real_from_env=docker.from_env
def shape_from_env(*args,**kwargs):
 if mode["value"]=="list-timeout":kwargs["timeout"]=0.01
 return shape_real_from_env(*args,**kwargs)
docker.from_env=shape_from_env
class Fault(bo.Daemon):
 def do_GET(self):
  path=self.path.split("?")[0]
  if path.startswith("/v1."):path="/"+path.split("/",2)[2]
  if path=="/containers/json":
   if mode["value"]=="api500-numeric-message":return self.send(500,{"message":123})
   if mode["value"]=="api404-numeric-message":return self.send(404,{"message":123})
   if mode["value"]=="api500-missing-message":return self.send(500,{})
   if mode["value"]=="api500-invalid-json":return self.send(500,b"qa")
   if mode["value"]=="api500-long-message":return self.send(500,{"message":"q"*8192})
   if mode["value"]=="list-timeout":
    threading.Event().wait(0.03);return
   if mode["value"]=="invalid-json":return self.send(200,b"{bad")
   if mode["value"]=="wrong-list-type":return self.send(200,{"wrong":[]})
   if mode["value"]=="missing-list-id":return self.send(200,[{}])
  if path.startswith("/containers/") and path.endswith("/json") and len(path.split("/"))==4 and mode["value"] in ("missing-name","nonstring-name"):
   c=copy.deepcopy(case["docker"]["containers"][0]);c.pop("Name");
   if mode["value"]=="nonstring-name":c["Name"]=123
   return self.send(200,c)
  if path.startswith("/images/") and mode["value"]=="invalid-tags":
   c=copy.deepcopy(case["docker"]["images"][0]);c["RepoTags"]=[1];return self.send(200,c)
  return super().do_GET()
server=bo.UnixServer(str(base/"docker.sock"),Fault)
threading.Thread(target=server.serve_forever,daemon=True).start()
client=TestClient(main.app,raise_server_exceptions=False)
ask=case["asks"][1]
body={"groups":[{"bytes":int(b),"estimate":e} for b,e in ask["groups"]],"summary":json.loads(ask["summary"]),"mmprojBytes":None}
out=[]
for value in ["invalid-json","wrong-list-type","missing-list-id","missing-name","invalid-tags","list-timeout","nonstring-name","api500-numeric-message","api404-numeric-message","api500-missing-message","api500-invalid-json","api500-long-message"]:
 mode["value"]=value
 r=client.post("/api/v1/internal/search-repo-runtime",json=body)
 out.append({"fault":value,"status":r.status_code,"body":r.text})
server.shutdown();temp.cleanup()
data["docker_shapes"]={"case":case,"ask":ask,"answers":out}

json.dump({key:data[key] for key in ("sources","repo","reserve_forms","docker_shapes")},sys.stdout,indent=1,sort_keys=True,ensure_ascii=True)
sys.stdout.write("\n")
