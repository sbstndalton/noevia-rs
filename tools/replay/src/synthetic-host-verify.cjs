'use strict';
// Execute both the actual serve source and the emitted wrapper against one observed spawn seam.
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm'),assert=require('node:assert/strict'),{EventEmitter}=require('node:events');
const core=__CORE__,wrapper=__WRAPPER__,entry=path.join(core,'tools/contract-corpus/serve.cjs');
const server=path.join(core,'server/index.cjs'),guard=path.join(core,'tools/contract-corpus/net-guard.cjs');
const actualMocks=require(path.join(core,'tools/contract-corpus/mocks.cjs'));
const actualServe=fs.readFileSync(entry,'utf8').replace(/^#![^\n]*\n/,'');
async function execute(wrapped) {
  const calls=[],stops=[],exits=[],handlers=new Map();let closed=0;
  const child=new EventEmitter();child.stderr=new EventEmitter();child.kill=signal=>{stops.push(signal);return true;};
  const cp={spawn:(command,args,options)=>{calls.push({command,args,options});return child;}};
  const fakeMocks={inference:{address:()=>({port:19001})},diary:{address:()=>({port:19002})},close:()=>{closed++;}};
  const processStub={env:{PATH:process.env.PATH,UI_DATA_DIR:'/synthetic/host-qualification',UI_PORT:'18021',PRIVATE_TEST_VALUE:'must-not-inherit',NOEVIA_RUST_AUTH:'1',NOEVIA_RUST_AUTH_CONFIRMED:'1',NOEVIA_RUST_REASONING_SETTINGS:'1',NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED:'1'},execPath:process.execPath,argv:[],on:(name,fn)=>handlers.set(name,fn),once:(name,fn)=>handlers.set(name,fn),exit:code=>exits.push(code)};
  const context={process:processStub,console};
  context.require=name=> {
    if(name==='node:child_process')return cp;
    if(name===entry)return vm.runInNewContext('(function(){'+actualServe+'\n})();',context,{filename:entry});
    if(name==='./mocks.cjs')return {...actualMocks,startMocks:async()=>fakeMocks};
    return require(name);
  };
  vm.runInNewContext(wrapped?fs.readFileSync(wrapper,'utf8'):actualServe,context,{filename:wrapped?wrapper:entry});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(calls.length,1);
  assert.equal(calls[0].command,process.execPath);
  assert.equal(calls[0].options.env.PRIVATE_TEST_VALUE,undefined);
  assert.equal(calls[0].options.env.NOEVIA_RUST_REASONING_SETTINGS_CONFIRMED,'1');
  handlers.get('SIGTERM')();assert.deepEqual(stops,['SIGTERM']);
  child.emit('exit',3);assert.equal(closed,1);assert.deepEqual(exits,[3]);
  return {calls,cp};
}
(async()=>{
  const original=await execute(false),wrapped=await execute(true);
  assert.equal(JSON.stringify(original.calls[0].args),JSON.stringify(['--require',guard,server]));
  const injected=wrapped.calls[0].args;
  assert.equal(JSON.stringify(injected),JSON.stringify(['--require',guard,'--require',path.join(path.dirname(wrapper),'host-input.cjs'),server]),'FIXTURE_SPAWN_PRELOAD_REQUIRED');
  assert.equal(JSON.stringify(original.calls[0].options),JSON.stringify(wrapped.calls[0].options));
  assert.throws(()=>wrapped.cp.spawn(process.execPath,['--require',guard,server,'unexpected'],{}),/spawn changed/);
  const options={env:{KEEP:'same'},stdio:'inherit'};
  wrapped.cp.spawn('unrelated',['other'],options);assert.equal(wrapped.calls[1].options,options);
  const budgetModule=require(path.join(core,'server/inference-budget.cjs')),actualFactory=budgetModule.createInferenceBudget;
  require(path.join(path.dirname(wrapper),'host-input.cjs'));
  const env={INFERENCE_MEMORY_BUDGET_GIB:'16'};
  const expected=actualFactory({env,readMeminfo:()=> 'MemTotal:        8388608 kB\n'}).get();
  const actual=budgetModule.createInferenceBudget({env,readMeminfo:()=>{throw Error('caller/host reader must be replaced only in fixture');}}).get();
  assert.deepEqual(actual,expected);assert.equal(actual.hostRamGib,8);
  console.log('actual pinned serve spawn/options/confirmation/signals/reap and actual budget factory input seam verified');
})().catch(e=>{console.error(e.message);process.exitCode=1;});
