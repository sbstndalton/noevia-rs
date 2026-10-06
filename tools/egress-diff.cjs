#!/usr/bin/env node
'use strict';
// Differential corpus for crates/egress.
//
// Loads noevia's own hostAllowed / parseTarget (apps/web/server/code-egress.cjs), isPrivateIp
// (ssrf.cjs) and Node's net.isIP, evaluates a fixed table of synthetic hosts, targets and
// addresses plus a seeded fuzz set, and prints the verdicts as JSON. The Rust test
// crates/egress/tests/differential.rs requires 100% agreement with the committed output.
//
//   NOEVIA_CHECKOUT=<noevia checkout> node tools/egress-diff.cjs \
//     > crates/egress/tests/fixtures/egress-diff.json
//
// Nothing here touches the network: the modules are only required, never started.

const path = require('node:path');
const net = require('node:net');
const { execFileSync } = require('node:child_process');

const checkout = process.env.NOEVIA_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CHECKOUT to a noevia checkout (the repo root).\n');
  process.exit(2);
}
const server = path.join(path.resolve(checkout), 'apps', 'web', 'server');
const { hostAllowed, parseTarget } = require(path.join(server, 'code-egress.cjs'));
const { isPrivateIp } = require(path.join(server, 'ssrf.cjs'));

let reference = null;
try {
  reference = execFileSync('git', ['-C', checkout, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
} catch { /* not a git checkout: leave it null */ }

// ---------------------------------------------------------------------------------------------
// Addresses: every IPv4 range boundary (lo-1, lo, hi, hi+1), as itself, IPv4-mapped and
// IPv4-compatible; IPv6 prefixes on both sides of each rule; malformed and non-IP strings.

const v4 = (n) => [n >>> 24, (n >>> 16) & 255, (n >>> 8) & 255, n & 255].join('.');
const v4n = (s) => s.split('.').reduce((acc, p) => acc * 256 + Number(p), 0);
const v4Ranges = [
  ['0.0.0.0', '0.255.255.255'], ['10.0.0.0', '10.255.255.255'], ['100.64.0.0', '100.127.255.255'],
  ['127.0.0.0', '127.255.255.255'], ['169.254.0.0', '169.254.255.255'], ['172.16.0.0', '172.31.255.255'],
  ['192.0.0.0', '192.0.255.255'], ['192.168.0.0', '192.168.255.255'], ['198.18.0.0', '198.19.255.255'],
  ['224.0.0.0', '255.255.255.255'],
  // Special-purpose ranges the JS does not single out (documentation, 6to4 relay).
  ['192.0.2.0', '192.0.2.255'], ['198.51.100.0', '198.51.100.255'], ['203.0.113.0', '203.0.113.255'],
  ['192.88.99.0', '192.88.99.255'], ['240.0.0.0', '255.255.255.254'],
];
const v4Boundaries = new Set(['8.8.8.8', '1.1.1.1', '93.184.216.34', '151.101.1.1']);
for (const [lo, hi] of v4Ranges) {
  for (const n of [v4n(lo) - 1, v4n(lo), v4n(hi), v4n(hi) + 1]) if (n >= 0 && n <= 0xffffffff) v4Boundaries.add(v4(n));
}
const addresses = new Set();
for (const a of v4Boundaries) {
  addresses.add(a);
  addresses.add(`::ffff:${a}`);
  addresses.add(`::FFFF:${a}`);
  addresses.add(`::${a}`);
  addresses.add(`0:0:0:0:0:ffff:${a}`);
  addresses.add(`64:ff9b::${a}`);
}
for (const a of [
  // loopback / unspecified, every spelling
  '::', '::1', '::0', '0::', '0::0', '0::1', '0:0:0:0:0:0:0:0', '0:0:0:0:0:0:0:1', '::0:1', '::1%lo',
  // link-local fe80::/10 and site-local, ULA fc00::/7, multicast ff00::/8
  'fe80::', 'fe80::1', 'FE80::1', 'fe80::1%eth0', 'fe80::1%25', 'fe8f::1', 'fe90::1', 'febf::1', 'fec0::1',
  'feff::1', 'fbff::1', 'fc00::', 'fc00::1', 'FC00::1', 'fcff::1', 'fd00::1', 'fdff:ffff::1', 'fe00::1',
  'ff00::', 'ff02::1', 'ff05::1:3', 'ffff::1',
  // 2000::/3 global unicast and its edges
  '1fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff', '2000::', '2000::1', '2001::1', '2001:0::1', '2001:db8::1',
  '2001:4860:4860::8888', '2002:7f00:1::', '2002:a00:1::1', '2606:4700:4700::1111', '2606:4700:4700::1111%1',
  '2a00:1450:4001::1', '3fff:ffff:ffff:ffff:ffff:ffff:ffff:ffff', '4000::', '4000::1', '5f00::1', '8000::1', 'e000::1',
  '02001::1', '2001:0db8:0000:0000:0000:0000:0000:0001', '2001:DB8::1',
  // NAT64, discard, IPv4-mapped/compat in hex, deprecated forms
  '64:ff9b::', '64:ff9b::808:808', '64:ff9b::7f00:1', '64:ff9b:1::1', '100::', '100::1', '::ffff:0:0',
  '::ffff:7f00:1', '::ffff:808:808', '::ffff:0808:0808', '::ffff:ffff:ffff', '::ffff', '::fffe:1.2.3.4',
  '::ffff:0:1.2.3.4', '0:0:0:0:0:ffff:7f00:1', '::ffff:1.2.3.4%a.b', '::ffff:8.8.8.8%1', '::8.8.8.8%1',
  '::1.2.3.4', '::0.0.0.0', '::255.255.255.255', '::ffff:255.255.255.255',
  // grammar edges
  '1:2:3:4:5:6:7:8', '1:2:3:4:5:6:7::', '::2:3:4:5:6:7:8', '1::2:3:4:5:6:7:8', '1:2:3:4:5:6::1.2.3.4',
  '1:2:3:4:5:6:1.2.3.4', '1:2:3:4:5::1.2.3.4', '1::1.2.3.4', '2001:db8::1.2.3.4', '2001:db8:1.2.3.4::',
  ':::', ':1::', '::1:', '1:2', '1:::2', '1::2::3', '12345::1', 'g::1', '::%', '::%-.:', '::%eth0%1',
  '::% eth0', '::ffff:1.2.3', '::ffff:01.2.3.4', '::ffff:256.1.1.1', '[::1]', '::1 ', ' ::1',
  // not IPv4 literals
  '', ' ', 'example.com', 'localhost', '01.2.3.4', '1.2.3.04', '1.2.3', '1.2.3.4.5', '256.1.1.1',
  '1.2.3.256', '0x7f.0.0.1', '127.1', '2130706433', '1.2.3.4 ', ' 1.2.3.4', '1.2.3.-1', '1..2.3',
  '١.٢.٣.٤', '1.2.3.4\n', '0.0.0.00', '127.0.0.1%1', 'metadata.google.internal',
]) addresses.add(a);

// Seeded fuzz over the tokens the grammar cares about, plus random well-formed addresses.
let seed = 0x9e3779b9;
const rand = () => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) / 0x100000000; };
const pick = (xs) => xs[Math.floor(rand() * xs.length)];
const toks = ['0', '1', '9', 'a', 'f', 'F', 'ff', 'ffff', 'fe80', 'fc', 'fd00', '2001', '64', '12345', ':', ':', '::',
  '.', '255', '256', '01', '%', 'eth0', 'a.b', '-', 'g', '1.2.3.4', '10.0.0.1', '::ffff:', '0', '00000'];
for (let i = 0; i < 600; i++) {
  let s = '';
  const n = 1 + Math.floor(rand() * 9);
  for (let j = 0; j < n; j++) s += pick(toks);
  addresses.add(s);
}
const hex = () => Math.floor(rand() * 0x10000).toString(16);
const firstGroups = ['0', '1', '64', '100', '1fff', '2000', '2001', '2400', '3fff', '4000', 'fc00', 'fd12', 'fe80', 'febf', 'fec0', 'ff02', 'e000'];
for (let i = 0; i < 300; i++) {
  const g = [pick(firstGroups)];
  for (let k = 1; k < 8; k++) g.push(rand() < 0.3 ? '0' : hex());
  let s = g.join(':');
  if (rand() < 0.5) s = s.replace(/(^|:)0(:0)+(:|$)/, '::');
  if (rand() < 0.2) s = s.toUpperCase();
  if (rand() < 0.1) s += '%' + pick(['eth0', '1', 'a.b', '']);
  addresses.add(s);
}
for (let i = 0; i < 200; i++) {
  addresses.add([0, 0, 0, 0].map(() => pick([0, 1, 10, 100, 127, 169, 172, 192, 198, 224, 255, Math.floor(rand() * 256)])).join('.'));
}

// ---------------------------------------------------------------------------------------------
// Hosts against grant lists: exact, subdomain, lookalikes, case, dots, IDN, IP literals.

const hostCases = [
  ['example.com', ['example.com']], ['api.example.com', ['example.com']], ['a.b.example.com', ['example.com']],
  ['notexample.com', ['example.com']], ['evil-example.com', ['example.com']], ['example.com.evil.com', ['example.com']],
  ['example.co', ['example.com']], ['example.com', ['example.co']], ['example.comm', ['example.com']],
  ['EXAMPLE.COM', ['example.com']], ['Api.Example.Com', ['EXAMPLE.com']], ['example.com', ['Example.Com.']],
  ['example.com.', ['example.com']], ['example.com..', ['example.com']], ['example.com', ['.example.com']],
  ['example.com', ['..example.com']], ['a.example.com', ['..example.com']], ['example.com', ['example.com..']],
  ['.example.com', ['example.com']], ['..example.com', ['example.com']], ['', ['example.com']],
  ['.', ['.']], ['x', ['.']], ['example.com', []], ['example.com', ['']], ['example.com', ['', 'example.com']],
  ['example.com', ['.']], ['example.com', ['..']], ['example.com', ['...']], ['.example.com', ['...']],
  ['com', ['example.com']], ['example.com', ['com']], ['example.com', ['*.example.com']],
  ['sub.example.com', ['*.example.com']], ['registry.npmjs.org', ['npmjs.org', 'pypi.org']],
  ['files.pythonhosted.org', ['pypi.org', 'pythonhosted.org']], ['pypi.org.attacker.net', ['pypi.org']],
  ['github.com', ['api.github.com']], ['api.github.com', ['github.com']], ['xgithub.com', ['github.com']],
  ['127.0.0.1', ['127.0.0.1']], ['127.0.0.1', ['0.0.1']], ['10.0.0.1', ['1']], ['::1', ['::1']],
  ['fe80::1', ['::1']], ['example.com:443', ['example.com']], ['ex ample.com', ['ample.com']],
  ['example.com\n', ['example.com']], ['example.com', ['example.com\n']], ['example.com ', ['example.com']],
  ['xn--bcher-kva.example', ['bücher.example']], ['bücher.example', ['BÜCHER.example']],
  ['BÜCHER.EXAMPLE', ['bücher.example']], ['www.bücher.example', ['xn--bcher-kva.example']],
  ['İ.example', ['i̇.example']], ['i.example', ['İ.example']], ['ΣΑΣ.gr', ['σας.gr']], ['ΣΑΣ.gr', ['σασ.gr']],
  ['straße.de', ['STRASSE.de']], ['ǅ.example', ['ǆ.example']], ['ﬀ.example', ['ff.example']],
  ['examp1e.com', ['example.com']], ['exаmple.com', ['example.com']], ['example.com', ['exаmple.com']],
];

// ---------------------------------------------------------------------------------------------
// Proxy targets: bare, ported, bracketed IPv6, bad ports, junk.

const targetRaws = [
  'example.com', 'example.com:443', 'example.com:80', 'Example.COM:8080', 'EXAMPLE.com', 'example.com.',
  'example.com.:443', 'example.com:', 'example.com:0', 'example.com:00', 'example.com:1', 'example.com:65535',
  'example.com:65536', 'example.com:99999', 'example.com:00080', 'example.com:0000000000000000443',
  'example.com:99999999999999999999999', 'example.com:+443', 'example.com:-1', 'example.com: 443',
  'example.com:443 ', 'example.com:4 43', 'example.com:44a', 'example.com:0x1bb', 'example.com:٤٤٣',
  'example.com:443:443', ':443', ':', '', ' ', ' example.com', 'example.com/', 'example.com/path',
  'example.com:443/path', 'example.com?x', 'example.com#x', 'user@example.com', 'user:pw@example.com:443',
  'a b', 'a\tb', 'a\nb', 'example.com\n', 'http://example.com', '127.0.0.1', '127.0.0.1:80', '10.0.0.1:22',
  '[::1]', '[::1]:443', '[::1]:80', '[::1]:', '[::1]:0', '[::1]:65536', '[]', '[]:443', '[::1', '::1',
  '::1:443', '[::1]x', '[::1]:44a', '[::1]]:443', '[[::1]]:443', '[FE80::1%25eth0]:443', '[fe80::1%eth0]',
  '[2606:4700:4700::1111]:443', '[::ffff:127.0.0.1]:443', '[evil.com]:443', '[example.com]', '[a b]:80',
  'bücher.example:443', 'BÜCHER.example', 'İ.example', 'ΣΑΣ.gr:443', 'xn--bcher-kva.example:443', 'a[b]:443',
  'a]:443', 'a@b', '.', '..:80', 'a..b:443',
];
const targetCases = [];
for (const raw of targetRaws) for (const port of [80, 443]) targetCases.push([raw, port]);
for (const port of [0, 1, 22, 65535]) targetCases.push(['example.com', port], ['[::1]', port], ['example.com:443', port]);

// ---------------------------------------------------------------------------------------------

const cases = [];
for (const input of addresses) {
  cases.push({ fn: 'isIP', input, expect: net.isIP(input) });
  cases.push({ fn: 'isPrivateIp', input, expect: isPrivateIp(input) });
}
for (const [host, domains] of hostCases) cases.push({ fn: 'hostAllowed', host, domains, expect: hostAllowed(host, domains) });
for (const [raw, defaultPort] of targetCases) cases.push({ fn: 'parseTarget', raw, defaultPort, expect: parseTarget(raw, defaultPort) });

process.stdout.write(JSON.stringify({ reference, node: process.version, count: cases.length, cases }, null, 1) + '\n');
