"""Synthetic actual-Python-schema/crossprocess qualification.
Run via devrun: python python_interop.py SERVICES_CHECKOUT IMPORT_BINARY
No actual Diary paths, server, backup, network, inference or selector activation.
"""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time
sys.path.insert(0,str(Path(sys.argv[1])/"diary"))
from agent import managed_storage as managed
from agent import workspace_import as reference
from agent.workspace_export import archive
BIN=str(Path(sys.argv[2]).resolve())
TENANT="00000000-0000-0000-0000-000000000001"
OTHER="00000000-0000-0000-0000-000000000002"
MAGIC=b"noevia-workspace-import-apply-v1\0"
TABLES=["files","directories","workspace_imports","workspace_index_outbox","meta","backups"]
count=0

def target(path, generation="1"):
    original=time.time
    time.time=lambda:1000.25
    try:
        backend=managed.ManagedCorpusBackend(path,TENANT)
        backend.activate({"keep.md":b"keep"},{'entry_layout':'monthly'})
        with backend.db() as db:
            backend.set_meta(db,"generation",generation)
        return backend
    finally:
        time.time=original

def packet(backend,body,name="Copy",**changes):
    destination="Imports/"+name
    header={"version":1,"dbPath":str(backend.db_path),"expectedTenant":TENANT,
            "destination":destination,"reviewedFingerprint":reference.fingerprint(body,destination),
            "debounce":backend.debounce}
    header.update(changes)
    metadata=json.dumps(header,ensure_ascii=False,allow_nan=False).encode()
    return MAGIC+struct.pack("<I",len(metadata))+metadata+struct.pack("<Q",len(body))+body

def run(request, expected=0):
    result=subprocess.run([BIN,"apply-v1"],input=request,capture_output=True,timeout=45)
    assert result.returncode==expected,(result.returncode,result.stdout,result.stderr)
    assert not result.stderr,result.stderr
    return json.loads(result.stdout) if expected==0 else result.stdout

def state(backend):
    with backend.db() as db:
        return {t:[tuple(r) for r in db.execute(f"SELECT * FROM {t} ORDER BY 1")] for t in TABLES}

def start(request):
    child=subprocess.Popen([BIN,"apply-v1"],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    pool=concurrent.futures.ThreadPoolExecutor(1)
    return child,pool,pool.submit(child.communicate,request)

with tempfile.TemporaryDirectory(prefix="synthetic-managed-import-") as directory:
    root=Path(directory)
    source=managed.ManagedCorpusBackend(root/"source",OTHER)
    source.activate({"raw/note.MD":b"# Synthetic\r\n","日記/binary.bin":b"\x00\xff",
                     "manifest.json":b"source manifest"},{},["empty/nested"])
    body=archive(source,"",{"mustNotReplaceTargetSettings":[False,None,"日記"]})

    for i,generation in enumerate(["1","999999999999999999999999999999999999","-2"," +001_000 ","١٢"]):
        rust=target(root/f"rust{i}",generation)
        python=target(root/f"python{i}",generation)
        result=run(packet(rust,body))
        with rust.db() as db: now=float(rust.meta(db,"snapshot_time"))
        original=time.time
        time.time=lambda:now
        try:
            expected=reference.apply(python,body,"Copy",reference.fingerprint(body,"Imports/Copy"))
        finally: time.time=original
        assert {k:v for k,v in result.items() if k!="version"}==expected
        assert state(rust)==state(python),generation
        assert rust.settings()=={'entry_layout':'monthly'}
        with rust.db() as db:
            assert db.execute("SELECT typeof(updated) FROM files WHERE path LIKE 'Imports/%'").fetchall()[0][0]=="real"
            assert db.execute("SELECT typeof(value) FROM meta WHERE key='snapshot_time'").fetchone()[0]=="text"
        count+=1

    # Receipt-before-conflict makes retry harmless after imported content changed.
    backend=target(root/"retry")
    request=packet(backend,body)
    assert not run(request)["alreadyApplied"]
    _,etag=backend.get("Imports/Copy/raw/note.MD")
    backend.put("Imports/Copy/raw/note.MD",b"edited",if_match=etag)
    before=state(backend)
    assert run(request)["alreadyApplied"]
    assert state(backend)==before
    count+=1

    # Each durable mutation phase must roll back all earlier writes on SQLite failure.
    for table in ["files","directories","workspace_index_outbox","meta","workspace_imports"]:
        backend=target(root/f"fault-{table}")
        with backend.db() as db:
            db.execute(f"CREATE TRIGGER synthetic_fault BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'synthetic fault'); END")
        before=state(backend)
        assert run(packet(backend,body),3)==b"unavailable: database\n"
        assert state(backend)==before
        count+=1

    for collision in ["Imports","Imports/Copy","Imports/Copy/child"]:
        backend=target(root/("conflict-"+collision.replace("/","-")))
        backend.put(collision,b"existing")
        before=state(backend)
        assert run(packet(backend,body),2)==b"reject: conflict\n"
        assert state(backend)==before
        count+=1
    for noncollision in ["Imports/Copycat","Imports/copy","imports/Copy"]:
        backend=target(root/("case-"+noncollision.replace("/","-")))
        backend.put(noncollision,b"keep")
        run(packet(backend,body))
        assert backend.get(noncollision)[0]==b"keep"
        count+=1
    backend=target(root/"wildcards")
    backend.put("Imports/AX",b"keep")
    run(packet(backend,body,name="A%_"))
    count+=1

    # No creation/repair or tenant substitution, including deletion/replacement before open.
    backend=target(root/"identity")
    before=state(backend)
    assert run(packet(backend,body,expectedTenant=OTHER),3)==b"unavailable: tenant\n"
    assert state(backend)==before
    with backend.db() as db: backend.set_meta(db,"active","0")
    assert run(packet(backend,body),3)==b"unavailable: inactive\n"
    backend.db_path.unlink()
    assert run(packet(backend,body),3)==b"unavailable: database\n"
    assert not backend.db_path.exists()
    replacement=managed.ManagedCorpusBackend(root/"replacement",OTHER)
    replacement.activate({}, {})
    shutil.copyfile(replacement.db_path,backend.db_path)
    assert run(packet(backend,body),3)==b"unavailable: tenant\n"
    count+=1
    missing=target(root/"no-create")
    absent=root/"absent.db"
    assert run(packet(missing,body,dbPath=str(absent)),3)==b"unavailable: database\n"
    assert not absent.exists()
    empty=root/"empty.db"
    empty.write_bytes(b"")
    assert run(packet(missing,body,dbPath=str(empty)),3)==b"unavailable: schema\n"
    assert empty.read_bytes()==b""
    malformed=root/"not-sqlite.db"
    malformed.write_bytes(b"synthetic not sqlite")
    assert run(packet(missing,body,dbPath=str(malformed)),3)==b"unavailable: database\n"
    assert malformed.read_bytes()==b"synthetic not sqlite"
    count+=2
    for table in ["workspace_imports","workspace_index_outbox","meta"]:
        backend=target(root/f"schema-{table}")
        with backend.db() as db: db.execute(f"DROP TABLE {table}")
        assert run(packet(backend,body),3)==b"unavailable: schema\n"
        count+=1
    backend=target(root/"fingerprint")
    before=state(backend)
    assert run(packet(backend,body,reviewedFingerprint="0"*64),2)==b"reject: fingerprint\n"
    assert run(packet(backend,b"bad"),2)==b"reject: invalid\n"
    assert run(packet(backend,body)+b"x",2)==b"reject: frame\n"
    assert state(backend)==before
    count+=1

    # The source path does not introduce a new marker-existence requirement.
    backend=target(root/"marker")
    backend.marker.unlink()
    run(packet(backend,body))
    count+=1

    # Same request in two processes commits exactly one generation/outbox.
    backend=target(root/"parallel")
    request=packet(backend,body)
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        outcomes=list(pool.map(run,[request,request]))
    assert sorted(r["alreadyApplied"] for r in outcomes)==[False,True]
    with backend.db() as db:
        assert backend.meta(db,"generation")=="2"
        assert db.execute("SELECT count(*) FROM workspace_imports").fetchone()[0]==1
        assert db.execute("SELECT count(*) FROM workspace_index_outbox").fetchone()[0]==1
    count+=1

    # A concurrent managed writer's commit is observed before APPLY collision checks.
    backend=target(root/"writer")
    with backend.db() as db:
        db.execute("BEGIN IMMEDIATE")
        child,pool,future=start(packet(backend,body))
        time.sleep(.15)
        assert child.poll() is None
        db.execute("INSERT INTO files VALUES (?,?,?,?)",("Imports/Copy",b"writer","synthetic",1.))
    stdout,stderr=future.result(45); pool.shutdown()
    assert child.returncode==2 and stdout==b"reject: conflict\n" and not stderr
    count+=1

    # Readers may hold a snapshot; their release lets the transaction commit.
    backend=target(root/"reader")
    with backend.db() as db:
        db.execute("BEGIN")
        db.execute("SELECT * FROM files").fetchall()
        child,pool,future=start(packet(backend,body))
        time.sleep(.15)
        assert child.poll() is None
    stdout,stderr=future.result(45); pool.shutdown()
    assert child.returncode==0 and not stderr and not json.loads(stdout)["alreadyApplied"]
    count+=1

    # Concurrent deletion of unrelated managed source is preserved, never recreated.
    backend=target(root/"delete")
    with backend.db() as db:
        db.execute("BEGIN IMMEDIATE")
        child,pool,future=start(packet(backend,body))
        db.execute("DELETE FROM files WHERE path='keep.md'")
    stdout,stderr=future.result(45); pool.shutdown()
    assert child.returncode==0 and backend.get("keep.md")[0] is None
    count+=1

    # Lost stdout after commit is uncertain; retry consults the durable receipt.
    backend=target(root/"lost")
    request=packet(backend,body)
    child=subprocess.Popen([BIN,"apply-v1"],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    child.stdout.close()
    child.stdin.write(request); child.stdin.close()
    assert child.wait(45)==3
    assert run(request)["alreadyApplied"]
    count+=1

    # No cap is silently added to prefixed file length or derived directory count.
    long_source=managed.ManagedCorpusBackend(root/"long-source",OTHER)
    long_path="x"*65525
    long_source.activate({long_path:b"binary"},{},[])
    backend=target(root/"long-target")
    run(packet(backend,archive(long_source,"",{}),name="日"*100))
    assert backend.get("Imports/"+"日"*100+"/"+long_path)[0]==b"binary"
    dir_source=managed.ManagedCorpusBackend(root/"dir-source",OTHER)
    dir_source.activate({}, {}, [f"d{i}" for i in range(5000)])
    backend=target(root/"dir-target")
    run(packet(backend,archive(dir_source,"",{})))
    with backend.db() as db: assert db.execute("SELECT count(*) FROM directories").fetchone()[0]==5002
    count+=2
print(f"{count} actual Python-schema/crossprocess differential, rollback and failure scenarios passed")
