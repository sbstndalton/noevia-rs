#![forbid(unsafe_code)]
use std::{io::Write, path::PathBuf, process::Command};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: generate-host-oracle SERVICES/app")?,
    );
    // Fixture-only loader executes the actual functions with mocked proc files/time.
    // It imports no app, Docker, FastAPI or production startup code.
    let script = r#"
import ast, dataclasses, hashlib, io, json, pathlib, threading, types, sys
from collections import deque
p=pathlib.Path(sys.argv[1]); hw=p/'hw.py'; api=p/'api.py'
def load(path,names,space):
    nodes=[]
    for n in ast.parse(path.read_bytes()).body:
        if isinstance(n,(ast.FunctionDef,ast.ClassDef)) and n.name in names:
            n.decorator_list=[] if isinstance(n,ast.FunctionDef) else n.decorator_list
            nodes.append(n)
    exec(compile(ast.Module(body=nodes,type_ignores=[]),str(path),'exec'),space)
space={'dataclass':dataclasses.dataclass,'_last_cpu':None,'_LOCK':threading.Lock(),'_HOST_HISTORY':deque(maxlen=450)}
load(hw,{'HostPoint','_read_host','host_history'},space)
api_space={'Any':object,'Path':pathlib.Path,'dataclasses':dataclasses,'hw':types.SimpleNamespace(host_history=space['host_history'])}
load(api,{'_plain','host'},api_space)
rows=[]
def sample(stat,mem,ts):
    def op(path,**kwargs):
        value=stat if path=='/proc/stat' else mem
        if value is None: raise OSError('synthetic failure')
        return io.StringIO(value)
    space['open']=op;space['time']=types.SimpleNamespace(time=lambda:ts)
    answer=space['_read_host']()
    if answer is not None: space['_HOST_HISTORY'].append(answer)
    rows.append(dict(stat=stat,mem=mem,ts=ts,point=dataclasses.asdict(answer) if answer else None))
mem='MemTotal: 33554432 kB\nMemAvailable: 8388608 kB\n'
for i,(stat,m) in enumerate([
 ('cpu 1 2 3 40 5\ncpu0 ignored\n',mem),('cpu 11 2 3 45 5\n',mem),
 ('cpu 21 2 3 45 5\n',None),('cpu 31 2 3 55 5\n',mem),
 (None,mem),('cpu bad 0 0 1\n',mem),('cpu 1 2 3\n',mem),
 ('cpu 31 2 3 55 5\n',mem),('cpu 0 0 0 0\n','MemAvailable: 1048576 kB\n'),
 ('cpu 0 0 0 0\n',''),('cpu 1 0 0 0\n','MemTotal: bad kB\n'),
 ('cpu 2 0 0 0\n','MemTotal: 131072 kB\nMemAvailable: 262144 kB\n'),
 ('cpu 3 0 0 0\n','MemTotal: 123456 kB\nMemTotal: 524288 kB\n')]):sample(stat,m,1000.0+i)
# Seeded synthetic CPU/memory values exercise exact binary-float decimal rounding.
import random
r=random.Random(1330); total=100
for i in range(80):
    total+=r.randrange(1,200);sample(f'cpu {total} 0 0 {r.randrange(total)} 0\n',f'MemTotal: {r.randrange(10000000)} kB\nMemAvailable: {r.randrange(10000000)} kB\n',1100.0+i)
# Compact separate retention oracle: samples derive their values from actual _read_host.
samples_response=api_space['host']()
space['_last_cpu']=None;space['_HOST_HISTORY'].clear();rows_saved=rows;rows=[]
for i in range(460):sample(f'cpu {i} 0 0 {i}\n',mem,2000.0+i)
retention=dict(inputs=[{k:v for k,v in row.items() if k in ('stat','mem','ts')} for row in rows],retained=len(space['_HOST_HISTORY']),response=api_space['host']())
print(json.dumps(dict(hw_sha256=hashlib.sha256(hw.read_bytes()).hexdigest(),api_sha256=hashlib.sha256(api.read_bytes()).hexdigest(),cases=rows_saved,samples_response=samples_response,retention=retention),indent=2))
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(source)
        .output()?;
    if !output.status.success() {
        std::io::stderr().write_all(&output.stderr)?;
        return Err("Python host oracle generation failed".into());
    }
    std::io::stdout().write_all(&output.stdout)?;
    Ok(())
}
