'use strict';
// Frozen Core3cbc9 verifier pure-boundary oracle; no native implementation is loaded.
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
const source = path.join(__dirname, 'fixtures/node-verifier-oracle.cjs');
const sourceSha256 = crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex');
if (sourceSha256 !== 'bc763d89b7a0ed27ecc0230cd803f3992a1f22d2193b1553fb4a411449b2025b') throw Error('frozen oracle changed');
const { parseVerify, configSafePath, createTail } = require(source);
const configs = ['', 'no bar', '|command', 'a|', 'a|echo ok', 'a|first\na|second', 'a | echo x|y\r\nb|printf b', 'bad/name|x', '.|x\n..|y', 'a'.repeat(64)+'|x', 'a'.repeat(65)+'|x', '\ufeffa\u2009|\u00a0echo ok\ufeff', 'A|x\na|y', 'a|'+ 'x'.repeat(4096), 'a|'+ 'x'.repeat(4097), 'a|'+ '😀'.repeat(2048), 'a|'+ '😀'.repeat(2049), 'a|x\rb|y', 'a|x\u2028b|y'];
const paths = ['', '/a', '/a b', '/a#b;c', '/a"b', '/a\\b', '/a\nb', '/a\u0000b', '/a\u007fb', '/a\u0085b', '/a\u00a0b', '/a😀b', '/a\ufeffb'];
const tails = [];
for (const cap of [1, 2, 3, 7, 16]) for (const chunks of [[[]], [[65,66,67]], [[240,159],[152,128,65]], [[255,254],[97,98,99]], [[0,10,13,34,92]], [Array.from(Buffer.from('abcdefghi')), Array.from(Buffer.from('雪😀z'))]]) {
 const tail = createTail(cap); for (const chunk of chunks) tail.push(Buffer.from(chunk)); tails.push({cap,chunks,expected:tail.read()});
}
const output = JSON.stringify({sourceCommit:'3cbc9f9ee16e7dff4ce9680c46d47828d89d69b2',sourceSha256,configs:configs.map(input=>({input,expected:Array.from(parseVerify(input))})),paths:paths.map(input=>({input,expected:configSafePath(input)})),tails},null,2)+'\n';
const dest=path.join(__dirname,'fixtures/boundaries.json');
if (process.argv.includes('--check')) { if(fs.readFileSync(dest,'utf8')!==output)throw Error('oracle drift'); } else fs.writeFileSync(dest,output);
