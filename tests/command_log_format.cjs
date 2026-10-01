const fs=require('node:fs'),vm=require('node:vm'),assert=require('node:assert/strict');
const box={window:{}};vm.runInNewContext(fs.readFileSync('web/tool-log.js','utf8'),box);
const format=v=>JSON.parse(JSON.stringify(box.window.WebTermLog.formatOutput(v)));
let count=0;function check(name,actual,expected){assert.deepEqual(actual,expected,name);count++;}
check('current result',format({content:[],structuredContent:{text:'new',exit_code:0},isError:false}),{text:'new',exit_code:0});
const old={output:'old\n界',exit_code:0,nested:{a:1,b:2}};
check('historical duplicate',format({content:[{type:'text',text:JSON.stringify(old)}],structuredContent:old,isError:false}),{text:'old\n界',exit_code:0,nested:{a:1,b:2}});
check('original unchanged',old,{output:'old\n界',exit_code:0,nested:{a:1,b:2}});
check('reordered JSON duplicate',format({content:[{type:'text',text:'{"exit_code":0,"output":"old\\n界","nested":{"b":2,"a":1}}'}],structuredContent:old}),{text:'old\n界',exit_code:0,nested:{a:1,b:2}});
check('error preserved',format({content:[{type:'text',text:'Error: failure'}],isError:true}),{content:[{type:'text',text:'Error: failure'}],isError:true});
check('plain object preserved',format({text:'plain'}),{text:'plain'});
check('null preserved',format(null),null);
check('distinct diagnostic preserved',format({structuredContent:{output:'a'},content:[{type:'text',text:'diagnostic'}]}),{structuredContent:{text:'a'},content:[{type:'text',text:'diagnostic'}]});
check('native image preserved',format({structuredContent:{width:3},content:[{type:'image',data:'native'}]}),{structuredContent:{width:3},content:[{type:'image',data:'native'}]});
check('array/object distinction',format({structuredContent:{v:[1]},content:[{type:'text',text:'{"v":{"0":1}}'}]}),{structuredContent:{v:[1]},content:[{type:'text',text:'{"v":{"0":1}}'}]});
console.log(JSON.stringify({passed:count,failed:0}));
