#!/usr/bin/env python3
"""Actual api.start_download/download_draft_head under PLANNING, synthetic adapters only.
No startup thread, transfer, external network, real model or hardware read is run.
Usage: python3 -I tools/gen-download-plan-oracle.py SERVICES/model-manager/app
"""
import asyncio, hashlib, json, sys, tempfile
from pathlib import Path
sys.path.insert(0,str(Path(sys.argv[1]).resolve().parent))
from app import api, hf, db, downloader, services
from app.config import settings
import httpx
from fastapi import HTTPException
FILES=[
 dict(path='qa-main.gguf',size=100,quant=None,shard_base='qa-main.gguf',shard_index=None,shard_total=None),
 dict(path='parts/qa-split-00001-of-00002.gguf',size=60,quant=None,shard_base='qa-split.gguf',shard_index=1,shard_total=2),
 dict(path='parts/qa-split-00002-of-00002.gguf',size=70,quant=None,shard_base='qa-split.gguf',shard_index=2,shard_total=2),
 *[dict(path=f'mmproj-{n}.gguf',size=n+1,quant=None,shard_base=f'mmproj-{n}.gguf',shard_index=None,shard_total=None) for n in range(5)],
 dict(path='qa-mtp-large.gguf',size=80,quant=None,shard_base='qa-mtp-large.gguf',shard_index=None,shard_total=None),
 dict(path='qa-mtp-small.gguf',size=20,quant=None,shard_base='qa-mtp-small.gguf',shard_index=None,shard_total=None),
 dict(path='imatrix.dat',size=5,quant=None,shard_base='imatrix.dat',shard_index=None,shard_total=None),
]
CASES=[('missing-repo',{}),('bad-target',{'target':'nope'}),('missing-file',{'repo':'qa-owner/qa-repo','path':'missing'}),
 ('main',{'repo':'qa-owner/qa-repo','path':'qa-main.gguf'}),('shards',{'repo':'qa-owner/qa-repo','shardBase':'qa-split.gguf'}),
 ('wrong-shard',{'repo':'qa-owner/qa-repo','shardBase':'../escape.gguf'}),('projector',{'repo':'qa-owner/qa-repo','path':'mmproj-0.gguf'}),
 ('imatrix',{'repo':'qa-owner/qa-repo','path':'imatrix.dat'}),('target',{'repo':'qa-owner/qa-repo','path':'qa-main.gguf','target':'qa-target'}),
 ('url',{'url':'https://example.test/qa-file.gguf'}),('url-explicit',{'url':'http://example.test/path','filename':' q.bin '}),
 ('url-params',{'url':'https://example.test/qa.gguf;parameter'}),
 ('url-encoded-semicolon',{'url':'https://example.test/qa%3Bparameter.gguf'}),
 ('url-parent-params',{'url':'https://example.test/parent;parameter/qa.gguf'}),
 ('url-broken-bracket',{'url':'http://[broken]/x.gguf'}),
 ('url-broken-bracket-explicit',{'url':'http://[broken]/x.gguf','filename':'qa.gguf'}),
 ('url-missing-bracket',{'url':'http://[::1/x.gguf'}),
 ('url-bracket-ipv4',{'url':'http://[127.0.0.1]/qa.gguf'}),
 ('url-ipvfuture',{'url':'http://[v1.qa]/qa.gguf'}),
 ('url-ipv6-scope',{'url':'http://[fe80::1%25qa]/qa.gguf'}),
 ('url-nfkc-delimiter',{'url':'https://qa／example.test/qa.gguf'}),
 ('url-unquote',{'url':'https://example.test/qa%20file.gguf'}),('url-bad-name',{'url':'https://example.test/qa%2Fescape.gguf'}),
 ('url-no-name',{'url':'https://example.test/'}),('url-bad-scheme',{'url':'file:///qa.gguf'}),('url-imatrix',{'url':'https://example.test/IMATRIX.dat'}),
 ('url-head-fail',{'url':'https://example.test/qa-file.gguf','qaHeadStatus':503}),
 ('url-head-missing',{'url':'https://example.test/qa-file.gguf','qaHeadLength':None}),
 ('url-head-invalid',{'url':'https://example.test/qa-file.gguf','qaHeadLength':'invalid'}),
 ('escape',{'repo':'qa-owner/qa-repo','path':'..'}),
 ('zero-head',{'repo':'qa-owner/qa-repo','path':'qa-main.gguf','qaZeroHead':True}),
 ('draft',{'path':'qa-mtp-small.gguf','qaSection':'qa'}),('draft-missing',{'path':'nope','qaSection':'qa'}),
 ('draft-no-folder',{'path':'qa-mtp-small.gguf','qaSection':'missing'}),('draft-unknown-source',{'path':'qa-mtp-small.gguf','qaSection':'unknown'}),
 ('draft-oversize',{'path':'qa-mtp-large.gguf','qaSection':'qa','qaOversize':True})]
async def main():
 out=[]
 for ident,body0 in CASES:
  body=dict(body0); section=body.pop('qaSection',None); calls=[]
  files=[dict(f) for f in FILES]
  if ident=='escape':files.append(dict(FILES[0],path='..',shard_base='..'))
  if body.pop('qaZeroHead',False):files[-2]['size']=0
  if body.pop('qaOversize',False):files[-3]['size']=2*1024**3+1
  status=body.pop('qaHeadStatus',200);length=body.pop('qaHeadLength','123')
  async def detail(repo):calls.append(['repo',repo]);return hf.HfRepoDetail(repo,[hf.HfFile(**f) for f in files],None)
  hf.repo_detail=detail
  class Client:
   def __init__(self,**kw):pass
   async def __aenter__(self):return self
   async def __aexit__(self,*args):pass
   async def head(self,url):calls.append(['head',url]);return httpx.Response(status,headers={} if length is None else {'content-length':length})
  original=httpx.AsyncClient;httpx.AsyncClient=Client
  with tempfile.TemporaryDirectory(prefix='qa-plan-') as tmp:
   root=Path(tmp);(root/'qa-target').mkdir();(root/'qa-folder').mkdir();(root/'qa-folder'/'qa-main.gguf').write_bytes(b'qa')
   ini=root/'models.ini';ini.write_text('[qa]\nmodel = /models/qa-folder/qa-main.gguf\n[unknown]\nmodel = /models/qa-folder/unknown.gguf\n')
   (root/'qa-folder'/'unknown.gguf').write_bytes(b'qa')
   settings.models_dir=root;settings.models_ini_path=ini;settings.data_dir=root/'data';settings.model_download_targets='qa-target';settings.data_dir.mkdir()
   services._mount_points=lambda:set()
   db._DB_PATH=None
   db.init()
   with db._conn() as c:c.execute("INSERT INTO download_history(repo_id,filename,dest_path,total_bytes,status,started_at) VALUES('qa-owner/qa-repo','qa-main.gguf','qa-folder/qa-main.gguf',100,'done',0)")
   planned=[];token=downloader.PLANNING.set(planned)
   try:
    response=await api.download_draft_head(section,body) if section is not None else await api.start_download(body)
    code=200
   except HTTPException as e:code=e.status_code;response={'detail':e.detail}
   except Exception as e:code=500;response={'exception':type(e).__name__}
   finally:downloader.PLANNING.reset(token);httpx.AsyncClient=original
   assert not downloader.manager.snapshot(), 'planning queued work'
   out.append(dict(id=ident,body=body,section=section,files=files,head_status=status,head_length=length,status=code,response=response,jobs=planned,calls=calls))
 print(json.dumps(dict(source_sha256={n:hashlib.sha256((Path(sys.argv[1])/n).read_bytes()).hexdigest() for n in ['api.py','helpers.py','downloader.py']},cases=out),sort_keys=True,indent=2))
asyncio.run(main())
