"""Full, unmodified CLI stderr/stdout/exit parity against the original Python CLI."""
import json,os,pathlib,subprocess,sys
mode,binary,evidence=sys.argv[1:];P=pathlib.Path;evidence=P(evidence)
cases=[[],['a'],['a','b'],['a','b','c','extra'],['--bad','a','b','c'],['a','b','c','extra','--bad'],['a','b','c','--bad','extra'],['-h'],['--help'],['--hel'],['--he'],['--h'],['-hfoo'],['-h=foo'],['-hh'],['--help=foo'],['--hel=foo'],['--help','--bad'],['--bad','-h'],['-1e3'],['-1'],['-.5'],['--','--help'],['-'],['a','--bad','b']]
command=[sys.executable,'-m','agent.backup_restore'] if mode=='record' else [binary]
results=[]
for args in cases:
 p=subprocess.run(command+args,capture_output=True,env={**os.environ,'PYTHONDONTWRITEBYTECODE':'1','PYTHONPATH':str(evidence/'python')})
 results.append({'args':args,'exit':p.returncode,'stdout':p.stdout.decode(),'stderr':p.stderr.decode()})
if mode=='record':(evidence/'argv-python312.json').write_text(json.dumps(results,indent=2)+'\n');print('recorded',len(results))
else:
 old=json.loads((evidence/'argv-python312.json').read_text());diff=[{'expected':a,'actual':b} for a,b in zip(old,results) if a!=b];report={'total':len(results),'passed':len(results)-len(diff),'differences':diff};(evidence/'argv-replay.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2));sys.exit(bool(diff))
