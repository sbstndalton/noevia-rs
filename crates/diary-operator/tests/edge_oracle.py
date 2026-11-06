"""Real Python config loader and backup reader, synthetic inputs only.
Only interpreter frames from uncaught exceptions are excluded; complete exception
class/message (including YAML marks) and all ordinary warnings/argparse stderr match.
"""
import ast, contextlib, io, json, logging, os, pathlib, subprocess, sys, traceback, hashlib
P=pathlib.Path
mode, binary, evidence=sys.argv[1:]; evidence=P(evidence); root=evidence/'edges';root.mkdir(parents=True,exist_ok=True)
sys.path.insert(0,str(evidence/'python'))
from agent.config import load_config
# Import original restore with its safe_key-only frozen dependency, never app.py.
from agent.backup_restore import restore
S=[
('duplicate','x: first\nx: last\n'),
('duplicate-nested','llm:\n  chat_model: first\n  chat_model: last\n'),
('anchor','a: &a {x: 1, nested: [one, two]}\nb: *a\n'),
('merge','a: &a {x: 1, y: 2}\nb: {<<: *a, x: 3}\n'),
('merge-order','a: &a {x: 1, z: 0}\nb: &b {x: 2, y: 1}\nc: {<<: [*a, *b]}\n'),
('alias-env','llm: &a {chat_model: chat}\ncopy: *a\n',{'LLM_CHAT_MODEL':'synthetic'}),
('tag-str','x: !!str 123\n'),('tag-int','x: !!int "0123"\n'),
('tag-float','x: !!float "1"\n'),('tag-null','x: !!null anything\n'),
('tag-map','x: !!map {a: 1}\n'),('tag-seq','x: !!seq [1, 2]\n'),
('tag-unknown','x: !synthetic value\n'),
('key-types','1: integer\ntrue: boolean\nnull: nothing\n2.5: float\n"1": string\n'),
('key-list','? [a, b]\n: value\n'),
('big-int','x: 123456789012345678901234567890123456789012345678901234567890\n'),
('number-formats','hex: 0xFf\noctal: 0123\nbinary: 0b101\nsexagesimal: 1:20:30\nunderscores: 1_000_000\n'),
('float-formats','x: [1.0, -0.0, 1.0e+20, 1.0e-07, 1e3, 1:20.5, .5, 1., 0.0001]\n'),
('nonfinite','x: [.nan, .NaN, .NAN, .inf, -.Inf, +.INF]\n'),
('bools','x: [yes, Yes, YES, no, No, NO, on, On, ON, off, Off, OFF, true, FALSE, y, n]\n'),
('quoted','x: ["yes", \'0123\', "null", "1.0", ".nan"]\n'),
('bom','\ufeffx: bom\n'),('crlf','x: one\r\ny: two\r\n'),
('empty',''),('empty-doc','---\n...\n'),('null-doc','null\n'),('false-doc','false\n'),('zero-doc','0\n'),
('multi-doc','---\nx: 1\n---\ny: 2\n'),('multi-empty','---\n---\n'),
('comments','# leading\nx: yes # trailing\n# end\n'),
('unicode','x: "\\u00e9 \\U0001F642 \\x7f"\n'),
('invalid-utf8',b'x: "\xff"\n'),
('bad-env-int','',{'DIARY_PORT':'not-an-int'}),
('big-env-int','',{'DIARY_PORT':'123456789012345678901234567890'}),
('underscored-env-int','',{'DIARY_PORT':' +8_010 '}),
('aux-nonstring','llm: {chat_model: 42, aux: {model: default}}\n'),
('tag-bytes','x: !!binary SGVsbG8=\n'),('tag-date','x: 2026-01-02\n'),('tag-set','x: !!set {a: null, b: null}\n'),
('tag-omap','x: !!omap [{a: 1}, {b: 2}]\n'),
('tag-pairs','x: !!pairs [{a: 1}, {a: 2}]\n'),
('tag-uri','x: !<tag:yaml.org,2002:str> 123\n'),
('tag-datetime','x: 2026-01-02T03:04:05Z\n'),
('root-list','[one, two]\n'),('root-number','42\n'),
('alias-cycle','x: &a [*a]\n'),
('key-zero','0: first\n-0.0: second\nfalse: third\n'),
('quote-warning','llm: {chat_model: "chat\'quote", aux: {model: default}}\n'),
('decimal-limit','x: '+('9'*4301)+'\n'),
('hex-limit','x: 0x'+('f'*3600)+'\n'),
('unicode-env-int','',{'DIARY_PORT':'١٢٣٤'}),
('control-env-bool','',{'DIARY_INDEX_ENABLED':'\x1cOFF\x1f'}),
('warning-before-error','llm: {chat_model: 42}\n',{'CORPUS_REMOTE_ROOT':'synthetic'}),
('utf8-truncated',b'x: "\xe2\x82'),
('utf8-continuation',b'x: "\xe2\x82\xff'),
]
# Python's exact config load→JSON projection, with normal warnings on stderr.
def reference(path, env):
    old=dict(os.environ); os.environ.clear();os.environ.update(env)
    log=io.StringIO(); handler=logging.StreamHandler(log);logging.getLogger().addHandler(handler)
    try:
        data=load_config(str(path)).as_dict()
        output=json.dumps(data,indent=2)+'\n'; rc=0; error=''
    except Exception as e:
        output='';rc=1;error=''.join(traceback.format_exception_only(e))
    finally: logging.getLogger().removeHandler(handler);os.environ.clear();os.environ.update(old)
    return rc,output,log.getvalue()+error
results=[]
for row in S:
    name,body,*overrides=row;path=root/(name+'.yaml');path.write_bytes(body.encode() if isinstance(body,str) else body)
    env={k:v for k,v in os.environ.items() if not k.startswith(('LLM_','S3_','DIARY_','CORPUS_','WEBDAV_')) and k not in ('DB_PATH','EMBEDDING_REINDEX_DUTY')};env.update(overrides[0] if overrides else {})
    if mode=='record': rc,out,err=reference(path,env)
    else:
        p=subprocess.run([binary,str(path)],env=env,capture_output=True);rc,out,err=p.returncode,p.stdout.decode(),p.stderr.decode()
    results.append({'case':name,'input_hex':path.read_bytes().hex(),'env':overrides[0] if overrides else {},'exit':rc,'stdout':out,'stderr':err})
if mode=='record':
    (evidence/'edge-config-python312.json').write_text(json.dumps(results,indent=2)+'\n');print('recorded',len(results))
else:
    old=json.loads((evidence/'edge-config-python312.json').read_text());diff=[{'expected':a,'actual':b} for a,b in zip(old,results) if a!=b]
    report={'total':len(results),'passed':len(results)-len(diff),'differences':diff};(evidence/'edge-config-replay.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2));sys.exit(bool(diff))
