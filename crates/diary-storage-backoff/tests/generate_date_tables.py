#!/usr/bin/env python3
"""Freeze only CPython date grammar properties; no runtime Python dependency."""
from pathlib import Path
import sys,unicodedata
assert sys.version_info[:3]==(3,12,15) and unicodedata.unidata_version=='15.0.0'
ranges=[];upper=[]
for cp in range(0x110000):
 if 0xd800<=cp<=0xdfff:continue
 c=chr(cp)
 if c.isdigit():
  if ranges and ranges[-1][1]+1==cp:ranges[-1][1]=cp
  else:ranges.append([cp,cp])
 if not c.isascii() and c.upper().isascii():upper.append((cp,c.upper()))
lines=['// Generated CPython3.12.15 / Unicode15 date grammar properties.','pub(super) const DIGITS: &[(u32,u32)] = &[']
lines += [f'    (0x{a:x},0x{b:x}),' for a,b in ranges];lines += ['];','pub(super) const UPPER_ASCII: &[(u32,&str)] = &[']
lines += [f'    (0x{a:x},"{b}"),' for a,b in upper];lines += ['];']
Path(__file__).resolve().parents[1].joinpath('src/email_date/tables.rs').write_text('\n'.join(lines)+'\n')
