"""Regenerate synthetic contract fixtures with CPython 3.12.
Usage: python3.12 generate_fixtures.py SERVICES_CHECKOUT
"""
import io
import json
import math
from pathlib import Path
import random
import sqlite3
import struct
import sys
import tempfile
assert sys.version_info[:2] == (3,12), "Use CPython 3.12 for the compatibility fixture"
sys.path.insert(0, str(Path(sys.argv[1])/"diary"))
from agent.managed_storage import ManagedCorpusBackend
from agent.workspace_export import archive
root=Path(__file__).resolve().parent
rng=random.Random(1071)
values=[0.,-0.,1.,-1.,1e-4,1e-5,1e15,1e16,1700000000.,1700000000.123456,1.7976931348623157e308,5e-324]
for exponent in range(-20,21):
    for x in [10.**exponent,-10.**exponent]:
        values += [x,math.nextafter(x,math.inf),math.nextafter(x,-math.inf)]
for _ in range(8000):
    value=struct.unpack('>d',rng.randbytes(8))[0]
    if math.isfinite(value): values.append(value)
def bits(value): return struct.pack('>d',value).hex()
(root/"python-floats.json").write_text(json.dumps([[bits(x),str(x)] for x in values],separators=(',',':'))+"\n")
clock=[(x,debounce) for x in [0.,-0.,1e-5,1e16,1700000000.,1700000000.123456] for debounce in [0.,-0.,3.,-1e-5]]
(root/"clock-cases.json").write_text(json.dumps([[bits(x),bits(d),str(x),str(x+d)] for x,d in clock])+"\n")
with tempfile.TemporaryDirectory(prefix="synthetic-import-fixtures-") as directory:
    backend=ManagedCorpusBackend(directory,"00000000-0000-0000-0000-000000000001")
    with backend.db() as db:
        schema=";\n".join(r[0] for r in db.execute("SELECT sql FROM sqlite_master WHERE type='table' ORDER BY name"))+";\n"
    (root/"managed-schema.sql").write_text(schema)
    backend.activate({"note.MD":b"# Synthetic\r\n\x00\xff"},{},["empty"])
    (root/"workspace.zip").write_bytes(archive(backend,"",{"preserve":[None,False,"日記"]}))
print("Generated CPython repr, clock, actual managed schema and synthetic archive fixtures")
