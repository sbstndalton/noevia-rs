"""Read frozen source DDL; execute only its CREATE script in in-memory SQLite."""
import argparse,ast,hashlib,json,sqlite3,sys
from pathlib import Path
SHA="19e0cad75e79781dc1ef41dd3b00ffa375f4306f1ed00a9e3f6d50528a1e3f7b"

def main():
    p=argparse.ArgumentParser();p.add_argument('--source',type=Path,required=True);p.add_argument('--output',type=Path,default=Path(__file__).parent/'fixtures/managed-schema.json');a=p.parse_args()
    if sys.implementation.name!='cpython' or sys.version_info[:3]!=(3,12,15):raise SystemExit('requires CPython3.12.15')
    source=a.source.read_bytes()
    if hashlib.sha256(source).hexdigest()!=SHA:raise SystemExit('frozen source hash mismatch')
    scripts=[c.args[0].value for c in ast.walk(ast.parse(source)) if isinstance(c,ast.Call) and isinstance(c.func,ast.Attribute) and c.func.attr=='executescript' and c.args and isinstance(c.args[0],ast.Constant) and isinstance(c.args[0].value,str) and 'CREATE TABLE IF NOT EXISTS files' in c.args[0].value]
    if len(scripts)!=1:raise SystemExit('missing unique schema source')
    with sqlite3.connect(':memory:') as db:
        db.executescript(scripts[0]);tables=[]
        rows=db.execute("SELECT name,sql FROM sqlite_schema WHERE type='table' ORDER BY name").fetchall()
        if {r[0] for r in rows}!={'backups','directories','files','meta','workspace_imports','workspace_index_outbox'}:raise SystemExit('unexpected table set')
        for name,sql in rows:
            columns=[dict(zip(['cid','name','type','notnull','default','pk','hidden'],row)) for row in db.execute('PRAGMA table_xinfo('+name+')')]
            tables.append(dict(name=name,sql=sql,columns=columns))
    result=dict(sourceSha256=SHA,pythonVersion=list(sys.version_info[:3]),scope='Actual store schema only; proposed receipt table is not part of the frozen schema.',tables=tables)
    a.output.write_text(json.dumps(result,indent=2)+'\n')
if __name__=='__main__':main()
