#!/usr/bin/env python3
"""Actual Python bench._peak_vram/_other_traffic/_read_load_ms, synthetic adapters only.
Usage: python3 -I tools/gen-bench-observe-oracle.py SERVICES/model-manager/app
No DB, real Docker, hardware, network, inference or module-source rewriting is used.
"""
import hashlib, importlib, importlib.util, json, sys, types
from pathlib import Path
source=Path(sys.argv[1]).resolve()
pkg=types.ModuleType('app'); pkg.__path__=[str(source)]; sys.modules['app']=pkg
for name in ['db','hw','services']:
    module=types.ModuleType('app.'+name); sys.modules['app.'+name]=module; setattr(pkg,name,module)
bench=importlib.import_module('app.bench')
telemetry=importlib.import_module('app.telemetry')
N=types.SimpleNamespace
stamp='2026-10-18T10:00:'
def line(sec,text): return stamp+sec+'Z '+text
def start(sec='01',alias='qa-a'): return line(sec,f'ensure_model: model name={alias} is not loaded, loading...')
def end(sec='02'): return line(sec,'llama_server: listening on http://127.0.0.1:18001')
load_cases=[('empty',''),('free',start()+'\n'+end()),('waiting',start()+'\n'+line('01.5','ensure_model: waiting until model name=qa-a is fully loaded')+'\n'+end()),
 ('eviction',line('00','ensure_model: evicting idle LRU name=old to make room for name=qa-a')+'\n'+start()+'\n'+end()),
 ('wrong-alias',start(alias='qa-b')+'\n'+end()),('reversed',start('03')+'\n'+end('01')),('invalid','bad ensure_model: model name=qa-a is not loaded\n'+end()),
 ('first-episode',start()+'\n'+end()+'\n'+start('05')+'\n'+end('08')),('unicode-split',start()+'\u2028'+end()),
 ('fraction',start('01.123456789')+'\n'+end('02.234567891'))]
# Reuse synthetic router-log builders, not implementation code, from the existing oracle.
spec=importlib.util.spec_from_file_location('synthetic_logs',Path(__file__).with_name('gen-telemetry-oracle.py'))
fixtures=importlib.util.module_from_spec(spec); spec.loader.exec_module(fixtures)
contention_cases=[]
for name,text in fixtures.logs().items():
    for aliases in [[],['syn-a'],['syn-a','syn-b','syn-c','syn-d','syn-e']]:
        for since in [0.0,1792317603.123456,9999999999.0]:
            contention_cases.append((name,text,aliases,since))
current=''; calls=[]
class Container:
    def logs(self,**kw):
        calls.append(kw)
        return current.encode('utf-8')
pkg.services._docker_client=lambda: N(containers=N(get=lambda name: Container()))
loads=[]
for name,text in load_cases:
    current=text; calls.clear()
    loads.append(dict(id=name,text=text,alias='qa-a',want=bench._read_load_ms('qa-backend','qa-a'),calls=list(calls)))
contention=[]
for name,text,aliases,since in contention_cases:
    current=text; calls.clear()
    contention.append(dict(id=name,text=text,aliases=aliases,since=since,want=bench._other_traffic('qa-backend',set(aliases),since),calls=list(calls)))
peaks=[]
for values,aggregate in [([],None),([],2.675),([1.005,2.675],9.0),([0.0,-0.0,1.125,1.135],0.0),([1,2],3)]:
    gpu=None if aggregate is None else N(cards=[N(vram_used_gb=v) for v in values],vram_used_gb=aggregate)
    pkg.hw.stats_for=lambda _: N(gpu=gpu)
    peaks.append(dict(cards=values,aggregate=aggregate,want=json.dumps(bench._peak_vram('qa-backend'))))
print(json.dumps(dict(source_sha256={n:hashlib.sha256((source/n).read_bytes()).hexdigest() for n in ['bench.py','telemetry.py']},load=loads,contention=contention,peak=peaks),ensure_ascii=True,sort_keys=True,indent=2))
