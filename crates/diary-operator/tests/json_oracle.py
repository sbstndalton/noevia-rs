"""Synthetic immutable-manifest edge cases using the real Python restore loader.
The Python CLI leaves restore exceptions uncaught. Preserve exception-only output
byte-for-byte, retain the full crash trace separately; never normalize messages.
"""
import contextlib,hashlib,io,json,os,pathlib,shutil,subprocess,sys,traceback
P=pathlib.Path
mode,binary,evidence=sys.argv[1:];evidence=P(evidence);root=evidence/'json-edges';root.mkdir(parents=True,exist_ok=True)
sys.path.insert(0,str(evidence/'python'));from agent.backup_restore import restore
tenant='00000000-0000-4000-8000-000000000123'
base='{"format":"noevia-diary-backup-v1","tenant":"'+tenant+'","files":[],"settings":SETTINGS}'
S=[('duplicates','{"x":1,"x":2,"a":0}'),('big-int','123456789012345678901234567890123456789012345678901234567890'),('floats','[1.0,-0.0,1e20,1e-7,1e-4,1e16,0.00000001,1.2345678901234567]'),('nonfinite','[NaN,Infinity,-Infinity,1e9999,-1e9999]'),('unicode','"\\u00e9 \\ud83d\\ude42 \\u007f"'),('surrogate','"\\ud800"'),('escaped','"line\\n\\t\\\\\\\"end"'),('big-key','{"123456789012345678901234567890":1}'),('bool-null','[true,false,null]')]
cases=[(name,base.replace('SETTINGS',s).encode()) for name,s in S]
body=base.replace('SETTINGS','{}')
cases += [('bom',b'\xef\xbb\xbf'+body.encode()),('utf16-le',b'\xff\xfe'+body.encode('utf-16-le')),('utf16-be',b'\xfe\xff'+body.encode('utf-16-be')),('crlf',body.replace(',',',\r\n').encode()),('empty',b''),('comment',b'// no comments\n'+body.encode()),('multi',body.encode()+b'\n{}'),('invalid-utf8',body.replace('{}','"BAD"').encode().replace(b'BAD',b'\xff')),('trailing-comma',body.replace('{}','{"x":1,}').encode()),('missing-colon',body.replace('{}','{"x" 1}').encode()),('bad-escape',body.replace('{}','"\\q"').encode()),('bad-unicode',body.replace('{}','"\\uXXXX"').encode()),('unterminated',body[:-1].encode()),('duplicate-format',body.replace('"format":','"format":"bad","format":').encode())]
cases.append(('decimal-limit',base.replace('SETTINGS','9'*4301).encode()))
cases.extend([('utf32-le',b'\xff\xfe\x00\x00'+body.encode('utf-32-le')),('utf32-be',b'\x00\x00\xfe\xff'+body.encode('utf-32-be')),('utf16-no-bom',body.encode('utf-16-le')),('utf32-no-bom',body.encode('utf-32-be'))])
cases.extend([('utf16-truncated',b'\xff\xfe{'),('utf32-truncated',b'\x00\x00\xfe\xff\x00\x00{'),('utf8-bom-invalid',b'\xef\xbb\xbf\xff'),('utf8-truncated',b'\xe2\x82'),('utf8-continuation',b'\xe2\x82\xff')])
results=[]
for name,body in cases:
    manifest=root/(hashlib.sha256(body).hexdigest()+'.json');manifest.write_bytes(body);destination=root/'destination'
    if destination.exists():shutil.rmtree(destination)
    if mode=='record':
        try:out=json.dumps(restore(root,manifest,destination),indent=2)+'\n';rc=0;err=''
        except Exception as e:
            out='';rc=1;err=''.join(traceback.format_exception_only(e));(root/(name+'.traceback')).write_text(''.join(traceback.format_exception(e)))
    else:
        p=subprocess.run([binary,str(root),str(manifest),str(destination)],capture_output=True);rc,out,err=p.returncode,p.stdout.decode(),p.stderr.decode()
    results.append({'case':name,'input_hex':body.hex(),'exit':rc,'stdout':out,'stderr':err,'destination_created':destination.exists()})
if mode=='record':(evidence/'edge-json-python312.json').write_text(json.dumps(results,indent=2)+'\n');print('recorded',len(results))
else:
    old=json.loads((evidence/'edge-json-python312.json').read_text());diff=[{'expected':a,'actual':b} for a,b in zip(old,results) if a!=b]
    report={'total':len(results),'passed':len(results)-len(diff),'differences':diff};(evidence/'edge-json-replay.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report,indent=2));sys.exit(bool(diff))
