'use strict';
// Node oracle for the JSON.parse error text the MTP artifact check reports as its reason when the
// model source's revision reply is not JSON (core mtp-artifact.cjs: readCappedJson -> JSON.parse).
// Run with the Node line production uses (22):
//   node json-parse-errors-node.cjs > json-parse-errors-node.json
// Synthetic inputs only.
const inputs = [
  '', ' ', '\n\t ', 'hello', 'h', '<html>', '<!DOCTYPE html><html><body>Not Found</body></html>',
  'undefined', 'NaN', 'Infinity', '[object Object]', '-Infinity', 'nul', 'nulx', 'tru', 'trux', 'falsey', 'nulL',
  '{', '{ ', '{"a"', '{"a" ', '{"a":', '{"a":1', '{"a":1,', '{"a":1,}', '{"a":1 "b":2}', '{a:1}', "{'a':1}", '{"a"1}', '{"a":1]',
  '[', '[1', '[1,', '[1,]', '[1 2]', '[,]', '[1}', ']', '}', ',', ':', '{}x', '[] []', '1 2', '"a" "b"', '{} ', '{}\n\nx',
  '"abc', '"a\\', '"a\\q"', '"a\\u12"', '"a\\u12G4"', '"a\u0001b"', '"tab\there"', '"line\nbreak"',
  '-', '-a', '01', '00', '1.', '1.a', '1e', '1e+', '1E-x', '.5', '+1', '0x10', '1_000',
  '{"sha":"abc"', '{"sha":"' + 'a'.repeat(40) + '"', '{"sha": "abc", }', '{"sha": "' + 'a'.repeat(30) + '" "x"}',
  '﻿{"a":1}', '{"é":1,x}', '{"a":"é"x}', 'é', '{"a":\n1,\n"b"\n2}', '{"a":1}\r\nx', '[1,\r\n2,\r\nx]',
  'x'.repeat(19), 'x'.repeat(20), 'x'.repeat(21), 'x'.repeat(22), '{"aaaaaaaaaaaaaaaaaa":x}', '{"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa":1,x"bbbbbbbbbbbbbb":2}',
  '{"a":1}' + ' '.repeat(5) + 'zzzzzzzzzzzzzzzzzzzz', '[' + '1,'.repeat(30) + 'x]', '[' + '1,'.repeat(30) + ']', '[1,2,3,4,5,6,7,8,9,x]', '{"k":tru}', '{"k":-}', '[01]', '{"k":"\\x41"}',
  ' ', '{"a":1,"a":2}x', '[[[[', '{"a":{"b":', '😀', '"😀"x', '{"😀😀😀😀😀😀😀😀😀😀😀":x}',
];
inputs.push('"\\u12', '"a\\u00', '{"a":1,"b"}', '{"aaaaaaaaaaaaaa": x , "bbbbbbbbbbbbbbbbbbbb":1}', '\r\n\r\nx', '\rx', '[1,\n\r2x]', '[1, x]', ' \n x', '  NaN', 'Na', '{"k":"\\uD83D"}x', 'abcédefghijklmnopqrstuvwxyz');
const cases = [];
for (const input of inputs) {
  try { JSON.parse(input); cases.push({ input, units: [...Array(input.length).keys()].map((k) => input.charCodeAt(k)), ok: true }); }
  catch (e) { cases.push({ input, inputUnits: [...Array(input.length).keys()].map((k) => input.charCodeAt(k)), ...(e.message.isWellFormed() ? { message: e.message } : {}), units: [...Array(e.message.length).keys()].map((k) => e.message.charCodeAt(k)) }); }
}
process.stdout.write(JSON.stringify({ node: process.version, cases }, null, 1) + '\n');
