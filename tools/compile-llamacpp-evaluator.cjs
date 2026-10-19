'use strict';
// Compile the checksummed frozen source into Rust's resumable instruction table.
// This tool is not loaded in production; the deployed host has no estimator source.
const fs = require('node:fs');
const Module = require('node:module');
const parserModule = new Module('frozen-acorn');
parserModule._compile(process.binding('natives')['internal/deps/acorn/acorn/dist/acorn'], 'frozen-acorn.cjs');
const acorn = parserModule.exports;
const reference = process.argv[2] || (() => { throw Error('pass the checksum-frozen core reference path'); })();
const rs = require('node:path').join(__dirname, '../crates/llamacpp-autoconfig/src');
const frozen = fs.readFileSync(reference, 'utf8');
const crypto = require('node:crypto');
if (crypto.createHash('sha256').update(frozen).digest('hex') !== '66b436f5e2fda9719470fbc18bd92d7b1b52eb6e881885167cdbbebd02eaa040') throw Error('frozen source checksum');
let source = frozen.slice(0, frozen.indexOf('// ── Rust confirmation'));
source += frozen.slice(frozen.indexOf('const isObj ='), frozen.indexOf('// ' + String.fromCharCode(96) + 'undefined' + String.fromCharCode(96)));
source += String.raw`
function nativeConfirm(op, args, candidate) {
  const port = __strict(op, args);
  const text = __stringify(candidate);
  if (port.text !== undefined && text !== undefined && port.text === text) return { decision: "answer", value: candidate };
  let ok = false;
  try { ok = !!(op === 1 ? suggestionIsConservative(candidate, port.reply) : op === 2 ? inputsAreConservative(candidate, port.reply) : footprintIsConservative(candidate, port.reply)); } catch { ok = false; }
  return ok ? { decision: "answer", value: candidate } : { decision: "refuse", reason: port.refused ? "impl_refused" : "impl_mismatch" };
}
function nativeSuggest(args) { return nativeConfirm(1, args, suggestJs(args)); }
function nativeInputs(args) { return nativeConfirm(2, args, estimateInputsJs(args)); }
function nativeFootprint(args) { return nativeConfirm(3, args, estimateFootprintJs(args)); }
`;
const ast = acorn.parse(source, {ecmaVersion:2022});
const functions = [];
const code = [];
let unique = 0;
let compilingFunction = '';
const emit = (op, n = 0, text = '') => { code.push({op,n,text}); return code.length-1; };
const host = (op, n = 0, data = null) => emit('Host', n, JSON.stringify({op,data}));
const lit = value => host('literal', 0, value);
const load = name => {emit('Env'); host('load',1,name);};
const store = (name, decl=false) => {emit('Env'); host(decl?'declare':'store',2,name);};
const enter = () => {emit('Env');host('scope',1);emit('UseEnv');};
const leave = () => {emit('Env');host('parent',1);emit('UseEnv');};
const cloneScope = () => {emit('Env');host('cloneScope',1);emit('UseEnv');};
const patch = (i, target=code.length) => {code[i].n=target;};
const branch = (kind='truthy', invert=false) => emit(invert?'Unless':'If',0,kind);
const memberKey = e => e.computed ? expr(e.property) : lit(e.property.name);

function linearTree(e, params) {
 switch(e.type) {
 case 'Identifier': {
   const index=params.indexOf(e.name);
   return index >= 0 ? ['arg', index] : e.name==='undefined' ? ['undefined'] : ['load',e.name];
 }
 case 'Literal': if(e.regex)throw Error('linear regex');return ['literal',e.value];
 case 'BinaryExpression':return ['binary',e.operator,linearTree(e.left,params),linearTree(e.right,params)];
 case 'UnaryExpression':return ['unary',e.operator,linearTree(e.argument,params)];
 case 'MemberExpression':return ['get',linearTree(e.object,params),e.computed?linearTree(e.property,params):['literal',e.property.name]];
 case 'CallExpression':
   if(e.arguments.some(a=>a.type==='SpreadElement'))throw Error('linear spread');
   if(e.callee.type==='MemberExpression')return ['method',linearTree(e.callee.object,params),e.callee.computed?linearTree(e.callee.property,params):['literal',e.callee.property.name],e.arguments.map(a=>linearTree(a,params))];
   return ['call',linearTree(e.callee,params),e.arguments.map(a=>linearTree(a,params))];
 default:throw Error('control flow or allocation stays in Rust');
 }
}
function linearClosure(e) {
 if(e.type!=='ArrowFunctionExpression'||e.params.length>3||e.params.some(p=>p.type!=='Identifier'))return null;
 try {return linearTree(e.body,e.params.map(p=>p.name));} catch{return null;}
}


function closureDescriptor(e,entry) {const first=e.params.findIndex(p=>p.type==='AssignmentPattern');return {entry,arity:first<0?e.params.length:first,name:e.id?.name||''};}
function functionId(e, name='') {
 const id = functions.length;
 functions.push({node:e,name,pc:0});
 return id;
}
function bind(p) {
 if(p.type==='Identifier') {store(p.name,true);emit('Pop');return;}
 if(p.type==='AssignmentPattern') {
   emit('Dup'); const skip=branch('undefined',true); emit('Pop');expr(p.right);patch(skip);bind(p.left);return;
 }
 if(p.type==='ObjectPattern'){
   host('destructure',1,p.properties[0]?.key.name || '');
   for(const prop of p.properties){
     emit('Dup');if(prop.computed)expr(prop.key);else lit(prop.key.name??prop.key.value);
     host('get',2);bind(prop.value);
   }
   emit('Pop');return;
 }
 throw Error('bind '+p.type);
}
function expr(e){
 if(!e){lit(undefined);return;}
 switch(e.type){
 case 'Literal': host(e.regex?'regexp':'literal',0,e.regex?{pattern:e.regex.pattern,flags:e.regex.flags}:e.value);return;
 case 'Identifier': if(e.name==='undefined')host('undefined');else load(e.name);return;
 case 'ArrayExpression': host('array');for(const x of e.elements){if(!x){host('hole',1);continue;}expr(x.type==='SpreadElement'?x.argument:x);host(x.type==='SpreadElement'?'arraySpread':'append',2);}return;
 case 'ObjectExpression':
   host('object');
   for(const p of e.properties){
    if(p.type==='SpreadElement'){expr(p.argument);host('spread',2);continue;}
    if(p.computed)expr(p.key);else lit(p.key.name??p.key.value);
    expr(p.value);host('define',3);
   }return;
 case 'MemberExpression':expr(e.object);memberKey(e);host('get',2);return;
 case 'CallExpression': case 'NewExpression':
   if(e.callee.type==='MemberExpression' && e.type!=='NewExpression'){expr(e.callee.object);emit('Dup');memberKey(e.callee);host('get',2);emit('Swap');}
   else {expr(e.callee);host('undefined');}
   for(const a of e.arguments){if(a.type==='SpreadElement')throw Error('spread call');expr(a);}
   host(e.type==='NewExpression'?'construct':'call',e.arguments.length+2);return;
 case 'BinaryExpression':expr(e.left);expr(e.right);host('binary',2,e.operator);return;
 case 'UnaryExpression':expr(e.argument);host('unary',1,e.operator);return;
 case 'LogicalExpression': {
   expr(e.left);emit('Dup');const stop=branch(e.operator==='??'?'nullish':'truthy',e.operator!=='||');
   emit('Pop');expr(e.right);patch(stop);return;
 }
 case 'ConditionalExpression': {
   expr(e.test);const otherwise=branch('truthy',true);expr(e.consequent);const end=emit('Jump');patch(otherwise);expr(e.alternate);patch(end);return;
 }
 case 'ArrowFunctionExpression':case 'FunctionExpression': {const tree=linearClosure(e);if(tree){emit('Env');host('closureLinear',1,{arity:e.params.length,tree});return;}const id=functionId(e);emit('Env');host('closure',1,closureDescriptor(e,id));return;}
 case 'TemplateLiteral':
   lit(e.quasis[0].value.cooked);
   for(let i=0;i<e.expressions.length;i++){expr(e.expressions[i]);host('template',2,e.quasis[i+1].value.cooked);}return;
 case 'AssignmentExpression': {
   if(e.left.type==='Identifier'){
     if(e.operator!=='=')load(e.left.name);expr(e.right);if(e.operator!=='=')host('binary',2,e.operator.slice(0,-1));store(e.left.name);return;
   }
   if(e.left.type==='MemberExpression'){
     expr(e.left.object);memberKey(e.left);
     if(e.operator!=='=')throw Error('compound member assignment is not in the frozen program');
     expr(e.right);if(e.operator!=='=')host('binary',2,e.operator.slice(0,-1));host('set',3);return;
   }
   throw Error('assignment');
 }
 case 'UpdateExpression':
   if(e.argument.type!=='Identifier')throw Error('update member');
   load(e.argument.name);host('increment',1,e.operator==='++'?1:-1);store(e.argument.name);return;
 default:throw Error('expression '+e.type+' '+source.slice(e.start,e.end));
 }
}
function stmt(s){
 switch(s.type){
 case 'VariableDeclaration':for(const d of s.declarations){expr(d.init);bind(d.id);}return;
 case 'FunctionDeclaration':{const id=functionId(s,s.id.name);emit('Env');host('closure',1,closureDescriptor(s,id));store(s.id.name,true);emit('Pop');return;}
 case 'ExpressionStatement':expr(s.expression);emit('Pop');return;
 case 'BlockStatement':enter();for(const st of s.body)stmt(st);leave();return;
 case 'ReturnStatement':expr(s.argument);emit('Return');return;
 case 'IfStatement': {
  expr(s.test);const no=branch('truthy',true);stmt(s.consequent);
  if(s.alternate){const end=emit('Jump');patch(no);stmt(s.alternate);patch(end);}else patch(no);return;
 }
 case 'ForStatement': {
   let sum;
   if(compilingFunction==='kvCacheBytes'){for(const name of ['pattern','headSeq','period','reps','kvHeads','localElem','window','globalElem','ctx'])load(name);sum=emit('KvSum');}
   enter();
   if(s.init)s.init.type==='VariableDeclaration'?stmt(s.init):(expr(s.init),emit('Pop'));
   const start=code.length;let end;
   if(s.test){expr(s.test);end=branch('truthy',true);}
   stmt(s.body);cloneScope();if(s.update){expr(s.update);emit('Pop');}emit('Jump',start);if(end!==undefined)patch(end);leave();
   if(sum!==undefined){const done=emit('Jump');patch(sum);store('total');emit('Pop');patch(done);}
   return;
 }
 case 'ForOfStatement': {
  enter();
  const iter='@iterator'+(++unique), next='@next'+unique;
  expr(s.right);host('iterator',1);store(iter,true);emit('Pop');
  const start=code.length;load(iter);host('next',1);store(next,true);emit('Pop');
  load(next);lit('done');host('get',2);const end=branch('truthy');
  enter();load(next);lit('value');host('get',2);bind(s.left.declarations[0].id);
  // IteratorStep/Value errors do not close; abrupt loop bodies do. Preserve the
  // original thrown completion even if reading/calling return also throws.
  const bodyCatch=emit('Catch');
  stmt(s.body);emit('Uncatch');leave();emit('Jump',start);
  patch(bodyCatch);
  const error='@loopError'+unique, actual='@actualIterator'+unique;
  store(error,true);emit('Pop');
  const closeCatch=emit('Catch');
  load(iter);lit('iterator');host('get',2);store(actual,true);emit('Pop');
  load(actual);lit('return');host('get',2);emit('Dup');
  const absent=branch('nullish');
  load(actual);host('call',2);emit('Pop');const called=emit('Jump');
  patch(absent);emit('Pop');patch(called);emit('Uncatch');
  const closed=emit('Jump');patch(closeCatch);emit('Pop');patch(closed);
  load(error);host('throw',1);emit('Pop');
  leave();emit('Jump',start);
  patch(end);leave();return;
 }
 case 'TryStatement': {
   const mark=emit('Catch');stmt(s.block);emit('Uncatch');const end=emit('Jump');patch(mark);
   if(s.handler.param)bind(s.handler.param);else emit('Pop');stmt(s.handler.body);patch(end);
   if(s.finalizer)throw Error('finally');return;
 }
 case 'EmptyStatement':return;
 default:throw Error('statement '+s.type);
 }
}
functions.push({node:{params:[],body:{type:'BlockStatement',body:ast.body}},name:'@initialize',pc:0});
for(let i=0;i<functions.length;i++){
 const f=functions[i];f.pc=code.length;compilingFunction=f.name;
 f.node.params.forEach((p,index)=>{load('@args');lit(index);host('get',2);bind(p);});
 let booleanPeriod;
 if(f.name==='periodOf'){load('seq');booleanPeriod=emit('BooleanPeriod');}
 if(f.node.body.type==='BlockStatement'){for(const statement of f.node.body.body)stmt(statement);}else{expr(f.node.body);emit('Return');}
 if(i===0){emit('Env');emit('Return');}else{host('undefined');emit('Return');}
 if(booleanPeriod!==undefined){patch(booleanPeriod);emit('Return');}
}
const q = x => JSON.stringify(x).replace(/\\u([0-9a-fA-F]{4})/g,(_,h)=>'\\u{'+h+'}');
const out = '// Generated from frozen source SHA-256 66b436f5e2fda9719470fbc18bd92d7b1b52eb6e881885167cdbbebd02eaa040.\n'
 + 'const ENTRIES: &[usize] = &['+functions.map(f=>f.pc).join(',')+'];\n'
 + 'const PROGRAM: &[Instruction] = &[\n'+code.map((i,pc)=>(functions.filter(f=>f.pc===pc).map(f=>'// '+(f.name||'callback')+' (function '+functions.indexOf(f)+')').join('\n')+'\n')+'Instruction::'+i.op+(i.op==='Host'?'('+i.n+','+q(i.text)+')':i.op==='If'||i.op==='Unless'?'('+i.n+','+q(i.text)+')':i.op==='Jump'||i.op==='Catch'||i.op==='BooleanPeriod'||i.op==='KvSum'?'('+i.n+')':'')+',').join('\n')+'\n];\n';
fs.writeFileSync(rs+'/evaluator_program.rs',out);
require('node:child_process').execFileSync('rustfmt', [rs+'/evaluator_program.rs']);

console.log(JSON.stringify({functions:functions.length,instructions:code.length,bytes:out.length}));
