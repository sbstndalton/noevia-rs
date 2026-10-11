#![forbid(unsafe_code)]
use std::{path::PathBuf, process::Command};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: generate-oracle SERVICES/app/api.py")?,
    );
    // Load only actual pure policy functions and regex assignments. No service imports,
    // FastAPI lifespan, real files, sockets, Docker or business startup are executed.
    let script = r#"
import ast, hashlib, json, pathlib, re, sys, types
p=pathlib.Path(sys.argv[1]); raw=p.read_bytes(); tree=ast.parse(raw)
selected=[]
for n in tree.body:
    if isinstance(n, ast.Assign) and any(isinstance(t,ast.Name) and t.id=='_SECRET_PATTERNS' for t in n.targets): selected.append(n)
    if isinstance(n,ast.FunctionDef) and n.name in ('redact_log_line','backend_logs'):
        n.decorator_list=[]; selected.append(n)
space={'re':re}; exec(compile(ast.Module(body=selected,type_ignores=[]),str(p),'exec'),space)
cases=[]
lines=['plain engine log','Authorization: Bearer abcdefghijklmnop123456','HF_TOKEN=hf_abcdefghijklmnopqrstuvwxyz0123','config {"api_key": "sk-live-abcdefghijklmnopqrstuv", "ctx": 8192}','remote https://admin:hunter2secret@nextcloud.example/remote.php','cookie cowork_session=deadbeefcafebabe1234 ok','token: abc123xyz','n_tokens = 512','prompt tokens: 40','slot session: 3','n_ctx_slot = 8192','fatal failure','warn: pending','wrn pending','err pending','bearer ABCDEFGH','aws AKIA1234567890123456','github ghp_abcdefghijklmnopqrstuvwxyz','jwt eyJabcdefgh.abcdefgh.abcdefgh','APİ_KEY=syntheticSECRETxyz','apı_key=syntheticSECRETxyz','ſecret=syntheticSECRETxyz','']
for raw in lines+['a\r\nb\r\n','a\x0bb\x1cc\u0085d\u2028e\u2029','\n\n','']:
    for q,level,tail in [('', '',400),('hunter2','',0),('WARN','warn',-9),('','error',99999)]:
        observed=[]
        space['services']=types.SimpleNamespace(_effective_container_names=lambda:['synthetic-engine'],container_logs=lambda name,tail:(observed.append(tail) or True,raw))
        answer=space['backend_logs']('synthetic-engine',q,level,tail)
        cases.append(dict(raw=raw,q=q,level=level,tail=tail,clamped=observed[0],answer=answer))
sp=p.with_name('services.py'); st=ast.parse(sp.read_bytes()); selected=[n for n in st.body if isinstance(n,ast.FunctionDef) and n.name in ('discover_llama_containers','_effective_container_names')]
space={'DockerException':Exception}; exec(compile(ast.Module(body=selected,type_ignores=[]),str(sp),'exec'),space)
inventories=[]
for configured in [[],['configured','configured'],['model-loader','stopped','configured']]:
    inventory=[dict(name='stopped',first_image_tag='GHCR.IO/GGML-ORG/LLAMA.CPP:server'),dict(name='model-loader',first_image_tag='llama.cpp:server'),dict(name='other',first_image_tag='postgres'),dict(name='stopped',first_image_tag='llama.cpp'),dict(name='cpu',first_image_tag='vendor/llama.cpp:server')]
    containers=[types.SimpleNamespace(name=c['name'],image=types.SimpleNamespace(tags=[c['first_image_tag']],short_id='sha256:123')) for c in inventory]
    space['settings']=types.SimpleNamespace(llama_container_names=configured)
    space['_docker_client']=lambda:types.SimpleNamespace(containers=types.SimpleNamespace(list=lambda all:containers))
    inventories.append(dict(configured=configured,inventory=inventory,expected=space['_effective_container_names']()))
print(json.dumps(dict(source_sha256=hashlib.sha256(p.read_bytes()).hexdigest(),services_sha256=hashlib.sha256(sp.read_bytes()).hexdigest(),cases=cases,inventories=inventories),ensure_ascii=False,indent=2))
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(source)
        .output()?;
    if !output.status.success() {
        return Err("Python oracle generation failed".into());
    }
    use std::io::Write;
    std::io::stdout().write_all(&output.stdout)?;
    Ok(())
}
