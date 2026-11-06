"""Config path/error parity, with stable relative paths and no message normalization."""
import io,json,logging,os,pathlib,subprocess,sys,traceback
mode,binary,evidence=sys.argv[1:];P=pathlib.Path;evidence=P(evidence).resolve();root=evidence/'io-edges';root.mkdir(exist_ok=True)
sys.path.insert(0,str(evidence/'python'));from agent.config import load_config
(root/'folder').mkdir(exist_ok=True);(root/'denied.yaml').write_text('x: synthetic\n');(root/'denied.yaml').chmod(0)
(root/'closed').mkdir(exist_ok=True);(root/'closed/config.yaml').write_text('x: synthetic\n');(root/'closed').chmod(0)
if not (root/'broken').is_symlink():(root/'broken').symlink_to('absent')
cases=[('directory',['folder'],{}),('empty-path',[''],{}),('empty-env',[],{'DIARY_CONFIG':''}),('missing',['missing'],{}),('missing-env',[],{'DIARY_CONFIG':'missing'}),('denied',['denied.yaml'],{}),('closed-parent',['closed/config.yaml'],{}),('broken-link',['broken'],{})]
results=[]
try:
 for name,args,env in cases:
    if mode=='record':
        old=dict(os.environ);oldcwd=P.cwd();os.environ.clear();os.environ.update(env);os.chdir(root)
        try:out=json.dumps(load_config(args[0] if args else None).as_dict(),indent=2)+'\n';rc=0;err=''
        except Exception as e:
            out='';rc=1;err=''.join(traceback.format_exception_only(e))
            (root/(name+'.traceback')).write_text(''.join(traceback.format_exception(e)))
        finally:os.chdir(oldcwd);os.environ.clear();os.environ.update(old)
    else:
        p=subprocess.run([binary,*args],env=env,cwd=root,capture_output=True);rc,out,err=p.returncode,p.stdout.decode(),p.stderr.decode()
    results.append({'case':name,'args':args,'env':env,'exit':rc,'stdout':out,'stderr':err})
finally:(root/'denied.yaml').chmod(0o600);(root/'closed').chmod(0o700)
if mode=='record':(evidence/'io-python312.json').write_text(json.dumps(results,indent=2)+'\n');print('recorded',len(results))
else:
 old=json.loads((evidence/'io-python312.json').read_text());diff=[{'expected':a,'actual':b} for a,b in zip(old,results) if a!=b];report={'total':len(results),'passed':len(results)-len(diff),'differences':diff};(evidence/'io-replay.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2));sys.exit(bool(diff))
