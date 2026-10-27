"""Freeze integer-generation acceptance from shipped CPython 3.12 / Unicode 15.
Only writes generation-cases.json; existing schema/archive/float oracles are untouched.
"""
import json
from pathlib import Path
import sys
import unicodedata

assert sys.version_info[:2] == (3, 12)
assert unicodedata.unidata_version == '15.0.0'
assert sys.get_int_max_str_digits() == 4300
zeros = [code for code in range(sys.maxunicode + 1)
         if unicodedata.decimal(chr(code), None) == 0]
assert len(zeros) == 68
unsupported = [0x10D40, 0x116D0, 0x116DA, 0x11BF0,
               0x16130, 0x16D70, 0x1CCF0, 0x1E5F1]
spaces = [chr(code) for code in range(sys.maxunicode + 1) if chr(code).isspace()]
raws = [chr(zero + digit) for zero in zeros + unsupported for digit in range(10)]
raws += [space + '12' + space for space in spaces]
raws += ['', '+', '-', '_1', '1_', '1__2', '+_1', '1 2', '+1_2', '-1_2',
         '-0', '+000', '9' * 1000, '-' + '1' + '0' * 1000]
raws += ['1' + '0' * 4299, '9' * 4299, '9' * 4300,
         '1' + '0' * 4300, '0' * 4301, '-' + '1' + '0' * 4299]
rows = []
accepted_spaces = []
for raw in raws:
    try:
        expected = str(int(raw) + 1)
    except ValueError:
        expected = None
    rows.append([raw, expected])
for space in spaces:
    try:
        int(space + '0' + space)
    except ValueError:
        continue
    accepted_spaces.append(ord(space))
fixture = {'python_version': sys.version.split()[0],
           'unicode_version': unicodedata.unidata_version,
           'integer_digits_limit': sys.get_int_max_str_digits(),
           'decimal_zeros': zeros, 'integer_whitespace': accepted_spaces,
           'unsupported_zeros': unsupported, 'cases': rows}
Path(__file__).with_name('generation-cases.json').write_text(
    json.dumps(fixture, ensure_ascii=True, separators=(',', ':')) + '\n')
print(f'{len(zeros)} decimal blocks, {len(accepted_spaces)} integer whitespace characters, {len(rows)} cases')
