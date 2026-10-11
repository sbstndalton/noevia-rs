'use strict';
// Interpose only the literal actual corpus server spawn; preserve all options and lifecycle.
const cp=require('node:child_process'),fs=require('node:fs');
const server=__SERVER__,guard=__GUARD__,preload=__PRELOAD__,entry=__ENTRY__;
const spawn=cp.spawn;
cp.spawn=function(command,args,options) {
  if(command===process.execPath && Array.isArray(args) && args.length===3 && args[0]==='--require' && args[1]===guard && args[2]===server) {
    return spawn(command,[args[0],args[1],'--require',preload,args[2]],options);
  }
  if(Array.isArray(args) && args.includes(server)) throw Error('actual corpus server spawn changed; fixture seam refuses');
  return spawn(command,args,options);
};
process.argv=[process.execPath,entry,...__ARGS__];
process.once('exit',()=>fs.rmSync(__TEMP__,{recursive:true,force:true}));
require(entry);
