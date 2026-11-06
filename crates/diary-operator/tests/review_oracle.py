"""Review regression oracle: real SafeLoader and the original prompt method."""
import ast,json,os,pathlib,subprocess,sys,traceback
P=pathlib.Path
mode,binary,evidence=sys.argv[1:];evidence=P(evidence).resolve();root=evidence/'review';root.mkdir(exist_ok=True)
sys.path.insert(0,str(evidence/'python'));from agent.config import Config,load_config
Y=[('bool-bad','x: !!bool garbage\n'),('scalar-map-value','x: !!str {=: true}\n'),('bool-case','x: !!bool tRuE\n'),('str-map','corpus: !!str {index_enabled: true}\n'),('bool-seq','x: !!bool [true]\n'),('int-map','x: !!int {a: 1}\n'),('seq-map','x: !!seq {a: 1}\n'),('map-seq','x: !!map [a]\n'),('map-scalar','x: !!map text\n'),('set-seq','x: !!set [a]\n'),('int-invalid','x: !!int garbage\n'),('int-empty','x: !!int ""\n'),('float-invalid','x: !!float garbage\n'),('binary-padding','x: !!binary abc\n'),('binary-unicode','x: !!binary é\n'),('timestamp-text','x: !!timestamp garbage\n'),('timestamp-month','x: !!timestamp 2026-13-01\n'),('timestamp-day','x: !!timestamp 2026-02-30\n'),('timestamp-hour','x: !!timestamp 2026-01-02T25:00:00Z\n'),('flow-eof','x: [1, 2\n'),('quote-eof','x: "unterminated\n'),('bad-flow','x: [1 2,}\n'),('unknown-alias','x: *absent\n')]
results=[]
for name,text in Y:
 path=root/(name+'.yaml');path.write_text(text)
 if mode=='record':
  try:out=json.dumps(load_config(str(path)).as_dict(),indent=2)+'\n';rc=0;err=''
  except Exception as exc:out='';rc=1;err=''.join(traceback.format_exception_only(exc));(root/(name+'.traceback')).write_text(''.join(traceback.format_exception(exc)))
 else:
  p=subprocess.run([binary,'config',str(path)],env={},capture_output=True,text=True);rc,out,err=p.returncode,p.stdout,p.stderr
 results.append({'case':name,'input_hex':path.read_bytes().hex(),'exit':rc,'stdout':out,'stderr':err})
if mode=='record':(evidence/'review-yaml-python312.json').write_text(json.dumps(results,indent=2)+'\n')
else:
 expected=json.loads((evidence/'review-yaml-python312.json').read_text());diff=[{'expected':a,'actual':b} for a,b in zip(expected,results) if a!=b];report={'total':len(results),'passed':len(results)-len(diff),'differences':diff};(evidence/'review-yaml-replay.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
# Execute the original method body without importing retrieval/app/runtime modules.
source=P(evidence/'python/agent/context.py').read_text();tree=ast.parse(source);cls=next(n for n in tree.body if isinstance(n,ast.ClassDef) and n.name=='ContextAssembler');fn=next(n for n in cls.body if isinstance(n,ast.FunctionDef) and n.name=='_system_prompt')
prompt=root/'prompt';(prompt/'agent').mkdir(parents=True,exist_ok=True);(prompt/'config/prompts').mkdir(parents=True,exist_ok=True);(prompt/'config/prompts/system.md').write_bytes(b'synthetic default\r\nprompt')
(prompt/'custom.txt').write_bytes(b'synthetic custom\r\nprompt');(prompt/'invalid.txt').write_bytes(b'bad\xff');(prompt/'denied.txt').write_text('synthetic');(prompt/'denied.txt').chmod(0);(prompt/'directory').mkdir(exist_ok=True)
namespace={'__file__':str(prompt/'agent/context.py')};exec(compile(ast.Module(body=[fn],type_ignores=[]),'<original-context>','exec'),namespace)
values=[('custom','custom.txt'),('missing','absent.txt'),('empty',''),('zero',0),('false',False),('null',None),('empty-list',[]),('int',42),('bool',True),('list',['custom.txt']),('map',{'a':1}),('directory','directory'),('denied','denied.txt'),('invalid-utf8','invalid.txt')]
rows=[]
try:
 for name,value in values:
  data={'prompts':{'system_path':value}};path=prompt/(name+'.yaml');path.write_text(json.dumps(data))
  if mode=='record':
   old=P.cwd();os.chdir(prompt)
   try:out=json.dumps(namespace['_system_prompt'](type('Self',(),{'cfg':Config(data)})()))+'\n';rc=0;err=''
   except Exception as exc:out='';rc=1;err=''.join(traceback.format_exception_only(exc))
   finally:os.chdir(old)
  else:
   p=subprocess.run([binary,'prompt',str(path),str(prompt/'config')],cwd=prompt,env={},capture_output=True,text=True);rc,out,err=p.returncode,p.stdout,p.stderr
  rows.append({'case':name,'config':data,'exit':rc,'stdout':out,'stderr':err})
finally:(prompt/'denied.txt').chmod(0o600)
if mode=='record':(evidence/'prompt-python312.json').write_text(json.dumps(rows,indent=2)+'\n');print('recorded',len(results),'YAML and',len(rows),'prompt cases')
else:
 expected=json.loads((evidence/'prompt-python312.json').read_text());diff=[{'expected':a,'actual':b} for a,b in zip(expected,rows) if a!=b];report={'total':len(rows),'passed':len(rows)-len(diff),'differences':diff};(evidence/'prompt-replay.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2))
