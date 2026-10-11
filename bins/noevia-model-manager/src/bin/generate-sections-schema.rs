//! Qualification-only actual-source schema evaluator. Rust owns output/hash/orchestration.
#![forbid(unsafe_code)]
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Command};
const LOADER: &str = r#"
import ast,dataclasses,json,sys,types
path=sys.argv[1]
tree=ast.parse(open(path,encoding='utf-8').read(),path)
env={'dataclass':dataclasses.dataclass}
nodes=[n for n in tree.body if (isinstance(n,ast.ClassDef) and n.name=='Field') or (isinstance(n,(ast.Assign,ast.AnnAssign)) and n.lineno>=36 and n.lineno<318)]
exec(compile(ast.Module(body=nodes,type_ignores=[]),path,'exec'),env)
ini=types.SimpleNamespace(**env)
api=sys.argv[2]
tree=ast.parse(open(api,encoding='utf-8').read(),api)
env={'ini':ini}
nodes=[n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name in ('_field','_schema')]
exec(compile(ast.Module(body=nodes,type_ignores=[]),api,'exec'),env)
print(json.dumps(env['_schema'](),ensure_ascii=False))
"#;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(
        args.next()
            .ok_or("actual model-manager/app path required")?,
    );
    let output = PathBuf::from(args.next().ok_or("output path required")?);
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let result = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()))
        .args(["-c", LOADER])
        .arg(source.join("ini.py"))
        .arg(source.join("api.py"))
        .output()?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
    }
    if result.stdout.len() > 128 * 1024 {
        return Err("schema exceeds qualification cap".into());
    }
    let value: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    let bytes = serde_json::to_vec_pretty(&value)?;
    std::fs::write(&output, &bytes)?;
    println!("{} {:x}", output.display(), Sha256::digest(&bytes));
    Ok(())
}
