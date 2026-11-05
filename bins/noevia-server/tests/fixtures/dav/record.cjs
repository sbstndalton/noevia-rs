'use strict';
// Extend the retired handler's synthetic oracle with empty-header client cases.
// Set NOEVIA_DAV_ORACLE_CORE to the paired Core checkout; Node 22 is required.
const fs=require('node:fs'),path=require('node:path'),Module=require('node:module');
const file=path.join(process.env.NOEVIA_DAV_ORACLE_CORE,'tests/oracles/dav/record.cjs');
let source=fs.readFileSync(file,'utf8');
if(!source.includes('const cases=['))throw Error('Update the fixture recorder for this oracle format');
source=source.replace('const cases=[',`const cases=[
 ['empty preconditions','PUT','/dav/alice/new.md','New',{'if-match':'','if-none-match':''}],
 ['empty encoding','PUT','/dav/alice/new.md','New',{'content-encoding':''}],
 ['empty mkdir conditions','MKCOL','/dav/alice/folder','',{'if':'','if-match':'','if-none-match':'','content-encoding':''}],`);
const moduleCopy=new Module(file);moduleCopy.filename=file;moduleCopy.paths=Module._nodeModulePaths(path.dirname(file));moduleCopy._compile(source,file);
