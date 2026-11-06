"""Freeze/replay the offline Python oracle. Synthetic fixtures only; no imports of app.py.
Usage: oracle.py record SERVICES EVIDENCE | replay BINARY EVIDENCE
"""
import ast, hashlib, json, os, pathlib, shutil, subprocess, sys
P = pathlib.Path
mode, target, evidence = sys.argv[1:]
evidence = P(evidence); evidence.mkdir(parents=True, exist_ok=True)
fixture = evidence / 'fixtures'; fixture.mkdir(exist_ok=True)
if mode == 'record':
    source = P(target) / 'diary/agent'
    provenance=json.loads((P(__file__).parent/'fixtures/SOURCE.json').read_text())
    def original(name):
        result=subprocess.run(['git','-C',target,'show',provenance['commit']+':diary/agent/'+name],capture_output=True)
        data=result.stdout if result.returncode==0 else (source/name).read_bytes()
        assert hashlib.sha256(data).hexdigest()==provenance['sha256']['diary/agent/'+name], 'original source hash mismatch: '+name
        return data
    tree = ast.parse(original('managed_storage.py'))
    safe = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'safe_key')
    pkg = evidence/'python/agent'; pkg.mkdir(parents=True,exist_ok=True)
    (pkg/'__init__.py').write_text('')
    (pkg/'managed_storage.py').write_text(ast.unparse(safe)+'\n')
    # Freeze the original public implementation even after its __main__ becomes native.
    (pkg/'backup_restore.py').write_bytes(original('backup_restore.py'))
    (pkg/'config.py').write_bytes(original('config.py'))
    command = [sys.executable,'-m','agent.backup_restore']
else: command = [target]
tenant='00000000-0000-4000-8000-000000000123'
root=fixture/'backup'; root.mkdir(exist_ok=True)
object_key=f'Notes/noevia-backups/{tenant}/objects/synthetic/payload'
payload=b'synthetic diary\x00\xff\n'
obj=root/object_key; obj.parent.mkdir(parents=True,exist_ok=True); obj.write_bytes(payload)
base={'format':'noevia-diary-backup-v1','tenant':tenant,'files':[{'path':'nested/note.bin','object':object_key,'sha256':hashlib.sha256(payload).hexdigest()}], 'directories':['empty/deep'], 'settings':{'z':'caf\u00e9 \U0001f642','a':[1,True,None]}}
cases=[('valid',base),('empty',{**base,'files':[]}),('unsupported',{**base,'format':'bad'}),('bad-files',{**base,'files':{}}),('tenant',{**base,'tenant':'bad'}),('wrong-tenant',{**base,'files':[{**base['files'][0],'object':'other/payload'}]}),('duplicate',{**base,'files':base['files']*2}),('overlap',{**base,'directories':['nested/note.bin/child']}),('same-path',{**base,'directories':['nested/note.bin']}),('bad-hash',{**base,'files':[{**base['files'][0],'sha256':'0'*64}]}),('escape',{**base,'files':[{**base['files'][0],'path':'../escape'}]}),('empty-key',{**base,'directories':['']}),('backslash',{**base,'directories':['a\\b']}),('dot',{**base,'directories':['a/./b']}),('null-key',{**base,'directories':[None]}),('existing',base),('checksum',base),('missing-object',{**base,'files':[{**base['files'][0],'object':object_key+'-absent'}]}),('symlink-escape',base)]
# The source must stay inside root after symlink resolution.
link=root/(object_key+'-link')
if not link.is_symlink(): link.symlink_to(obj)
cases.append(('symlink-inside',{**base,'files':[{**base['files'][0],'object':object_key+'-link'}]}))
outside=fixture/'outside'; outside.write_bytes(payload)
escape=root/(object_key+'-outside')
if not escape.is_symlink(): escape.symlink_to(outside)
cases[18]=('symlink-escape',{**base,'files':[{**base['files'][0],'object':object_key+'-outside'}]})
def snapshot(path):
    if not path.exists(): return None
    return {str(p.relative_to(path)):{'mode':oct(p.stat().st_mode&0o777),'data':None if p.is_dir() else p.read_bytes().hex()} for p in [path,*sorted(path.rglob('*'))]}
results=[]
for name,manifest in cases:
    body=json.dumps(manifest,ensure_ascii=False).encode()
    mp=fixture/(hashlib.sha256(body).hexdigest()+'.json'); mp.write_bytes(body)
    dest=fixture/'destination'
    if dest.exists(): shutil.rmtree(dest)
    if name=='existing': dest.mkdir()
    if name=='checksum': mp=fixture/'bad.json'; mp.write_bytes(body)
    env={**os.environ,'PYTHONDONTWRITEBYTECODE':'1','PYTHONPATH':str(evidence/'python')}
    p=subprocess.run(command+[str(root),str(mp),str(dest)],capture_output=True,env=env)
    stderr=p.stderr.decode(); error=stderr.strip().split('\n')[-1] if stderr else ''
    results.append({'case':name,'exit':p.returncode,'stdout':p.stdout.decode(),'exception':error.replace(str(fixture),'$FIXTURE'),'tree':snapshot(dest)})
    (evidence/(mode+'-'+name+'.stderr')).write_text(stderr)
for name,args in [('help',['--help']),('short-help',['-h']),('abbrev-help',['--hel']),('missing',[]),('one',['a']),('two',['a','b']),('extra',['a','b','c','d']),('unknown',['--bad','a','b','c'])]:
    p=subprocess.run(command+args,capture_output=True,env={**os.environ,'PYTHONDONTWRITEBYTECODE':'1','PYTHONPATH':str(evidence/'python')})
    results.append({'case':'argv-'+name,'exit':p.returncode,'stdout':p.stdout.decode(),'stderr':p.stderr.decode()})
if mode=='record':
    (evidence/'oracle.json').write_text(json.dumps(results,indent=2)+'\n')
else:
    expected=json.loads((evidence/'oracle.json').read_text())
    failed=[{'expected':a,'actual':b} for a,b in zip(expected,results) if a!=b]
    (evidence/'replay.json').write_text(json.dumps({'passed':len(results)-len(failed),'total':len(results),'differences':failed},indent=2)+'\n')
    print(json.dumps({'passed':len(results)-len(failed),'total':len(results),'differences':failed},indent=2))
    sys.exit(bool(failed))
print(f'Recorded {len(results)} Python cases')
