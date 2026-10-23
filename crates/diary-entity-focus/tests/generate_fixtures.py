#!/usr/bin/env python3
"""Differential fixtures from the frozen entity_focus.py, wholly synthetic."""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import json
import random
import sys
from datetime import date
from pathlib import Path

SOURCE_HASH = "dd4312e26a22cf2132f18bbd0ca4d7fd55a885706b8d0aa8997c14e94eeb7632"

def load(path: Path):
    digest=hashlib.sha256(path.read_bytes()).hexdigest()
    if digest!=SOURCE_HASH: raise SystemExit(f"source hash mismatch: {digest}")
    spec=importlib.util.spec_from_file_location("frozen_entity_focus_fixtures",path)
    module=importlib.util.module_from_spec(spec)
    sys.modules[spec.name]=module
    spec.loader.exec_module(module)
    return module

def encode_entity(e): return {"name":e.name,"keys":list(e.keys)}
def anchor(a): return None if a is None else {"day":a[0].isoformat(),"inclusive":a[1]}
def partition(rows,a):
    kept=[];removed=[]
    for rank,row in enumerate(rows,1):
        if a is None: kept.append(rank);continue
        try: day=date.fromisoformat(str(row.get("day")))
        except ValueError: day=None
        if day is not None and (day>a[0] or (day==a[0] and not a[1])): removed.append(rank)
        else: kept.append(rank)
    return {"survivors":kept,"removed":removed}
def expected(res):
    return {"entities":[encode_entity(e) for e in res.entities],"carried":res.carried,
            "anchor":anchor(res.anchor),"relational":res.relational,"margin":res.margin,
            "matched":[encode_entity(e) for e in res.matched],
            "kept_ranks":[rank for rank,_ in res.kept],
            "removed":[{"rank":rank,"reason":reason} for rank,_,reason in res.removed],
            "similar":[{"rank":rank,"kind":kind,"shown":shown,"asked":asked} for rank,(kind,shown,asked) in sorted(res.similar.items())],
            "corrected_by":[{"rank":rank,"day":day} for rank,day in sorted(res.corrected_by.items())],
            "absent":[{"entity":encode_entity(e),"spellings":names} for e,names in res.absent.items()],
            "exact":[{"rank":rank,"names":names} for rank,names in sorted(res.exact.items())]}

def main():
    p=argparse.ArgumentParser();p.add_argument('--source',type=Path,required=True)
    p.add_argument('--output',type=Path,default=Path(__file__).with_name('entity-fixtures.jsonl'))
    a=p.parse_args();ef=load(a.source);rnd=random.Random(177180160)
    names=["Alder Brannock","Alder Brennock","Alder Pennick","Bryony Castellow","Caspian Juxon",
           "Caspian Juxton","Dr Patel","Marit","Søren Łukas","William Hart","Janet Bell","Rosa Bank","ΌΣ Μάρκος","Ός Μάρκος"]
    queries=["What is Alder Brannock's favourite tea?","What is Alder Branock's favourite tea?",
             "What did Alder Brannock and Bryony Castellow discuss?","Where did Caspian Juxon go?",
             "What did Dr Patel say?","What did Marit say?","What is Søren Łukas doing?",
             "What did William Hart buy?","What did Will Hart buy?","What did Janet Bell say?",
             "What did Rosa Bank read?","What did ΌΣ Μάρκος say?","Do Alder Brannock's sister and Bryony Castellow know each other?",
             "Is that written in my diary?","And his email?","How has the week been?",
             "As of January 20, 2030, what did Alder Brannock drink?",
             "Before January 20, 2030, what did Alder Brannock drink?",
             "Until 2030-01-20, what did Caspian Juxon write?",
             "What is the tea? As of 2030-01-20, did Alder Brannock change it?"]
    texts=["Alder Brannock likes teal tea.","Alder Brennock likes red tea.","Alder Pennick made a boat.",
           "Alder J. Brannock likes teal tea.","Brannock Alder visited.","Alder's tea is teal.",
           "with alder brannock at the lake.","Caspian Juxton likes red tea.","Caspian Juxon likes teal tea.",
           "Bryony Castellow fixed the boat.","Tom Hardy fixed the boat.","Harbour Lights was busy.",
           "Dr Patel sent a letter.","Jan Bell liked the boat.","Will Hart sent a letter.",
           "William Hart sent a letter.","Soren Lukas sent a letter.","Rosa Bank went home.",
           "along Rosa Bank, the market was busy.","No names here, just a report.",
           "Correction: Alder Brannock likes blue tea, not teal tea.",
           "I misremembered: Caspian Juxon chose a blue boat, not the red boat.",
           "**Bryony Castellow:** went to the market.","Met Tom Hardy yesterday.","Alder Brannocks boat was teal.",
           "I saw him again.","New Year preparations went well.","First National Bank closed early.","Ός Μάρκος spoke.","ΌΣ Μάρκος spoke."]
    headers=["","### 09:12 — Tea","### 8:05 — Correction","[2030-01-20] # Log","Correction: tea","## Harbour Lights"]
    days=["2030-01-10","2030-01-20","2030-01-21","2030-02-01","bad-day","20300120",None]
    scores=[0.9,0.8,0.7,0.69,0.649999999,0.65,"bad","nan","inf",None,False," 0.5 "]
    margins=[0.05,0.0,1.0,"0.05","bad",None,False,"nan",0.050000001]
    cases=[]
    hand=[
      (queries[0],[{"day":"2030-01-10","header":"","text":texts[1],"score":0.81},{"day":"2030-01-11","header":"","text":texts[0],"score":0.8}],None,0.05,[]),
      (queries[0],[{"day":"2030-01-10","header":"","text":texts[0],"score":0.8},{"day":"2030-01-21","header":"Correction: tea","text":texts[20],"score":0.9}],None,0.05,[]),
      (queries[3],[{"day":"2030-01-10","header":"","text":texts[7],"score":0.8}],None,0.05,[]),
      (queries[5],[{"day":"bad-day","header":"","text":"Marit said hello.","score":"bad"}],None,"nan",[]),
      (queries[15],[{"day":"2030-01-21","header":"","text":texts[0],"score":0.9},{"day":"2030-01-20","header":"","text":texts[1],"score":0.8}],None,0.05,[]),
      (queries[16],[{"day":"2030-01-20","header":"","text":texts[0],"score":0.9},{"day":"bad-day","header":"","text":texts[1],"score":0.8}],None,0.05,[]),
      ("And his email?",[{"day":"2030-01-10","header":"","text":texts[0],"score":0.8}],None,0.05,[queries[0]]),
      ("How has the week been?",[{"day":"2030-01-10","header":"","text":texts[0],"score":0.8}],None,0.05,[queries[0]]),
    ]
    for query,rows,limit,margin,earlier in hand: cases.append((query,rows,limit,margin,earlier))
    cases.append(("What did ΌΣ Μάρκος say?",[{"day":"2030-01-10","header":"","text":"Ός Μάρκος spoke.","score":0.9}],None,0.05,[]))
    for score in ["٠.٩", "۰.۹", "०.९", "0_._9", "9e-1", "9_e-1", "Infinity", "-Infinity", "nan", "1_0.5", " 0.9 ", "９.０"]:
        cases.append((queries[0],[{"day":"2030-01-10","header":"","text":texts[0],"score":1.0},{"day":"2030-01-11","header":"","text":"No names here, just a report.","score":score}],None,0.05,[]))
    for margin in ["٠.١", "۰.۱", "०.१", "0_._1", "1e-1", "1_e-1", "Infinity", "-Infinity", "nan", "0_._5", " 0.1 ", "１.０", "0.0_5"]:
        cases.append((queries[0],[{"day":"2030-01-10","header":"","text":texts[0],"score":1.0},{"day":"2030-01-11","header":"","text":"No names here, just a report.","score":0.9}],None,margin,[]))
    for _ in range(750):
        query=rnd.choice(queries);count=rnd.randrange(0,7)
        rows=[{"day":rnd.choice(days),"header":rnd.choice(headers),"text":rnd.choice(texts),"score":rnd.choice(scores)} for _ in range(count)]
        limit=rnd.choice([None,0,1,2,3,4,8]);margin=rnd.choice(margins)
        earlier=rnd.choice([[],[queries[0]],[queries[3]],[queries[0],queries[3]]]) if query in queries[12:15] else []
        cases.append((query,rows,limit,margin,earlier))
    rows=[{"kind":"meta","source_sha256":SOURCE_HASH,"python":sys.version.split()[0],"unicode":ef.unicodedata.unidata_version}]
    for i,(query,candidates,limit,margin,earlier) in enumerate(cases):
        result=ef.focus_candidates(query,candidates,limit=limit,earlier=earlier,margin=margin)
        rows.append({"kind":"focus","id":i,"message":query,"rows":candidates,"limit":limit,"margin":margin,
                     "earlier":earlier,"partition":partition(candidates,result.anchor),"expected":expected(result)})
    helper_cases=[]
    for name in names:
        target=ef.Entity(name,ef._keys(name.split()))
        for text in texts:
            helper_cases.append({"kind":"classify","entity":encode_entity(target),
                                 "text":text,"found":list(ef.classify(text,target)),
                                 "people":[list(k) for k in ef.identified_people(text,[target])],
                                 "asked":[encode_entity(e) for e in ef.asked_entities(text)]})
    rows.extend(helper_cases)
    for value in ["٠.١", "۰.۱", "०.१", "0_._1", "1e-1", "1_e-1", "Infinity", "-Infinity", "nan", "0_._5", " 0.1 ", "１.０", "0.0_5"]:
        rows.append({"kind":"margin","input":value,"expected":ef.parse_margin(value)})
    a.output.write_text('\n'.join(json.dumps(row,ensure_ascii=False,separators=(',',':')) for row in rows)+'\n')
    print(f'generated {len(cases)} focus and {len(helper_cases)} helper cases')

if __name__=='__main__':main()
