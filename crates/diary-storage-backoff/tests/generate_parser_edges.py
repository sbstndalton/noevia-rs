#!/usr/bin/env python3
"""Synthetic parser edges from pinned shipped source; no real HTTP/clock/data."""
import argparse,hashlib,importlib.util,json,locale,math,os,random,sys,time,types,unicodedata,struct
from pathlib import Path
from email.utils import parsedate_to_datetime
if sys.version_info[:3] != (3,12,15) or unicodedata.unidata_version!='15.0.0':
    raise SystemExit('requires CPython3.12.15/Unicode15')
assert sys.get_int_max_str_digits()==4300, 'requires shipped decimal conversion limit'
a=argparse.ArgumentParser();a.add_argument('--source',type=Path,required=True);a=a.parse_args()
sha=hashlib.sha256(a.source.read_bytes()).hexdigest()
assert sha=='d2ba1b187d611be7766e523d4638f7a3b9a4f223e49c4847d8f960897f04f2af'
os.environ['TZ']='UTC';time.tzset();locale.setlocale(locale.LC_ALL,'C')
stub=types.ModuleType('httpx');stub.BaseTransport=object;sys.modules['httpx']=stub
spec=importlib.util.spec_from_file_location('storage_edge_oracle',a.source);m=importlib.util.module_from_spec(spec);sys.modules[spec.name]=m;spec.loader.exec_module(m)
values=[]
for prefix in ['', 'Wed, ', 'Fri,', 'garbage, ', 'Sun ', 'Wed,21 ']:
 for middle in ['21 Oct 2015 07:28:00','Oct 21 2015 07:28','21 October 15 07.28.00','21-Oct-15 07:28:00','21 Oct 07:28:00 2015','21 Oct 07:28:00 GMT 2015']:
  for zone in ['GMT','UTC','UT','Z','EST','EDT','eſt','eﬆ','+0100','-0030','+0060','-0000','-٠٠٠٠','unknown','', '+2400','+2399','+0_100']:
   values.append(prefix+middle+' '+zone)
for day in ['0','29','30','31','+21','٢١','2_1']:
 for month in ['Feb','September','oct','Bad']:
  for year in ['0','1','68','69','99','100','9999','10000','2015,','²']:
   values.append(f'Wed, {day} {month} {year} 07:28:00 GMT')
for clock in ['0:0','23:59:59','24:00:00','07:60:00','07:28:60','07.28','07.28.00','07:28:00,','٠٧:٢٨:٠٠']:
 for suffix in ['GMT','GMT trailing ignored','+0100','']:
  values.append('21 Oct 2015 '+clock+' '+suffix)
values += ['21 Oct 2015 07:28:00+0100','21 Oct 2015 07:28:00-0030','21 Oct 2015 07:28:00 +0000',
 '21 Oct 2015 07:28:00 '+'9'*100, '21 Oct 2015 07:28:00 '+'9'*4301]
values += [None,'',' ','NaN','+NaN','-nan','inf','Infinity','-Infinity','1_2.3_4e+0_2','1__2','1_.0','1._0','1e_2','0x10','−2','١٢.٣','\x1c7\x1f','\U00010d50']
rows=[]
for value in dict.fromkeys(values):
    try:
        numeric=float(value.strip()); numeric_valid=not math.isnan(numeric)
    except (ValueError,TypeError,AttributeError): numeric_valid=False
    try: stamp=parsedate_to_datetime(value).timestamp()
    except (ValueError,TypeError,IndexError,OverflowError,AttributeError): stamp=None
    walls=[stamp-7.125,stamp+1,stamp-5000] if stamp is not None else [1445412420.0]
    for wall in walls:
        m.time.time=lambda w=wall:w
        expected=m.parse_retry_after(value,7.25)
        source='Absent' if not value else 'Numeric' if numeric_valid else 'HttpDate' if stamp is not None else 'Invalid'
        rows.append({'value':value,'wall':wall,'expected':expected,'source':source})
sequences=[]
for base in [-1e12,-1000.0,-0.5,0.0,1e-9,1000.0,1e12-4000]:
 for status,period in [(401,300.0),(429,120.0)]:
  clock=[base];gate=m.StorageGate(clock=lambda:clock[0]);steps=[]
  until=base+period
  actions=[{'op':'sample','mono':base}, {'op':'record','mono':base,'status':status,'header':'120'},
   *[{'op':'sample','mono':sample} for sample in [math.nextafter(until,-math.inf),until,math.nextafter(until,math.inf)]],
   {'op':'record','mono':base+1,'status':200}, {'op':'record','mono':base+2,'status':401},
   {'op':'reset','mono':base+3}, {'op':'record','mono':base+4,'status':401},
   *[{'op':'reset','mono':sample} for sample in [math.nextafter(base+33,-math.inf),base+33,math.nextafter(base+33,math.inf)]]]
  for action in actions:
   clock[0]=action['mono'];m.time.time=lambda:1445412420.0;reset=None
   if action['op']=='record':gate.record(action['status'],action.get('header'),False)
   elif action['op']=='reset':reset=gate.reset_login()
   state=list(gate.state());blocked=None
   try:gate.check()
   except m.StorageBackoff as e:blocked={'kind':e.kind,'retry_after':e.retry_after}
   action['mono_bits']=struct.unpack('>Q',struct.pack('>d',action['mono']))[0]
   steps.append({'action':action,'state':state,'remaining_bits':struct.unpack('>Q',struct.pack('>d',state[1]))[0],'blocked':blocked,'reset':reset})
  sequences.append({'base':base,'status':status,'steps':steps})
out=Path(__file__).with_name('oracles')/'parser-edges.json';out.write_text(json.dumps({'source_sha256':sha,'python':sys.version.split()[0],'unicode':unicodedata.unidata_version,'timezone':'UTC','cases':rows,'sequences':sequences},ensure_ascii=True,indent=2)+'\n')
print(len(rows),'actual shipped-source edge cases')
