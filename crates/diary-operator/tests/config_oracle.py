"""Config oracle: no app import, no runtime initialization, no remote calls."""
import contextlib, io, json, logging, os, pathlib, subprocess, sys
mode,target,evidence=sys.argv[1:]; P=pathlib.Path; evidence=P(evidence); evidence.mkdir(parents=True,exist_ok=True)
root=evidence/'config-fixtures'; root.mkdir(exist_ok=True)
if mode=='record':
    sys.path.insert(0,str(evidence/'python'))
    from agent.config import load_config
scenarios=[('missing',None,{}),('empty','',{}),('yaml','llm:\n  chat_model: synthetic-chat\n  aux:\n    model: default\ncorpus:\n  index_enabled: true\n',{}),('env','llm: {}\n',{'LLM_CHAT_MODEL':' chat ','LLM_AUX_MODEL':'default','CORPUS_ROOT':'new','CORPUS_REMOTE_ROOT':'old','DIARY_PORT':' 8123 ','DIARY_INDEX_ENABLED':' OFF '}),('legacy','',{'CORPUS_REMOTE_ROOT':'old'}),('explicit','',{'LLM_CHAT_MODEL':'chat','LLM_AUX_MODEL':'small'}),('default','',{'LLM_CHAT_MODEL':'default','LLM_AUX_MODEL':'default'})]
# Every override, including deliberately empty strings.
mapping={}
if mode=='record':
    import ast
    tree=ast.parse((evidence/'python/agent/config.py').read_text())
    for node in ast.walk(tree):
        if isinstance(node,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='mapping' for t in node.targets): mapping=ast.literal_eval(node.value)
    (evidence/'config-mapping.json').write_text(json.dumps(mapping))
else: mapping=json.loads((evidence/'config-mapping.json').read_text())
for key in mapping:
    scenarios.append((key,'', {key:'8010' if key=='DIARY_PORT' else 'synthetic' if key=='DIARY_INDEX_ENABLED' else ''}))
results=[]
for name,yaml,env in scenarios:
    path=root/(name+'.yaml')
    if yaml is not None: path.write_text(yaml)
    clean={k:v for k,v in os.environ.items() if k not in mapping and k!='DIARY_CONFIG'}; clean.update(env)
    if mode=='record':
        old=dict(os.environ); os.environ.clear(); os.environ.update(clean)
        log=io.StringIO(); handler=logging.StreamHandler(log); logging.getLogger().addHandler(handler)
        try: data=load_config(str(path)).as_dict(); error=None
        except Exception as e: data=None; error=type(e).__name__+': '+str(e)
        finally: logging.getLogger().removeHandler(handler); os.environ.clear();os.environ.update(old)
        warnings=log.getvalue()
    else:
        p=subprocess.run([target,str(path)],env=clean,capture_output=True,text=True)
        data=json.loads(p.stdout) if p.returncode==0 else None; error=p.stderr.strip() if p.returncode else None; warnings=p.stderr if p.returncode==0 else ''
    results.append({'case':name,'data':data,'error':error,'warnings':warnings})
if mode=='record': (evidence/'config-oracle.json').write_text(json.dumps(results,indent=2)+'\n'); print('Recorded',len(results),'config cases')
else:
    expected=json.loads((evidence/'config-oracle.json').read_text()); failed=[{'expected':a,'actual':b} for a,b in zip(expected,results) if a!=b]
    report={'passed':len(results)-len(failed),'total':len(results),'differences':failed}; (evidence/'config-replay.json').write_text(json.dumps(report,indent=2)+'\n'); print(json.dumps(report,indent=2)); sys.exit(bool(failed))
