const fs=require('node:fs');
const path=require('node:path');
const assert=require('node:assert/strict');
const {JSDOM}=require('jsdom');
const assets=path.join(__dirname,'../peri-cloud/src/portal/assets');
const dom=new JSDOM(fs.readFileSync(path.join(assets,'index.html'),'utf8'),{url:'http://127.0.0.1:8765',runScripts:'outside-only'});
const w=dom.window, id=name=>w.document.getElementById(name);
const settle=()=>new Promise(resolve=>setTimeout(resolve,20));
w.eval(fs.readFileSync(path.join(assets,'memory.js'),'utf8')+'\nwindow.memoryFixture=MemoryView;');
const view=w.memoryFixture, requests=[], errors=[];
const malicious='<img src=x onerror=alert(1)>';
let principal='owner', confirmation=null, failure=null, pending=null;
const scopes=[{key:'user',label:'我的资料',scope:{kind:'user'}},{key:'fleet',label:'我的设备池',scope:{kind:'fleet'}},{key:'device_env:device-1',label:'PRIVATE_DEVICE_NAME',scope:{kind:'device_env',device_id:'device-1'}},{key:'device_env:retired',label:'已撤销设备',scope:{kind:'device_env',device_id:'retired'},read_only:true}];
let page={generation:7,total:1,used_bytes:50,has_more:false,next_offset:null,items:[{id:'item-1',revision:3,scope:{kind:'user'},content:malicious,kind:'fact',pinned:true,valid_until:null,source:{kind:'qq',session_id:'session-1'},updated_at:1}]};
const io={
 owner:()=>principal,
 api:async(url,body)=>{
  requests.push({url,body});
  if(failure&&body){const error=failure;failure=null;throw new Error(error);}
  if(body)return {};
  if(pending)return pending;
  return {page,scopes:principal==='owner'?scopes:scopes.slice(0,2),max_bytes:65536,max_items:128};
 },
 action:async(_button,job)=>{try{await job();}catch(error){errors.push(error.message);}},
 confirm:(title,description,details,allow)=>{confirmation={title,description,details,allow};}
};
(async()=>{
 await view.load(principal,io);
 assert(id('memory-list').textContent.includes(malicious));
 assert.equal(id('memory-list').querySelectorAll('img').length,0);
 assert(id('memory-list').textContent.includes('session-1'));
 const form=id('memory-form');
 form.elements.content.value='东京部署主控';form.elements.kind.value='decision';form.elements.pinned.checked=true;
 form.dispatchEvent(new w.Event('submit',{cancelable:true}));await settle();
 const add=requests.find(r=>r.url==='/api/memories'&&r.body);
 assert.equal(add.body.generation,7);assert.match(add.body.request_id,/^[a-f0-9-]{36}$/);
 assert.deepEqual(Object.keys(add.body).sort(),['generation','request_id','scope','value']);
 assert.equal(add.body.value.content,'东京部署主控');assert.equal(add.body.value.kind,'decision');
 assert.equal(add.body.value.valid_until,null);assert.equal(form.elements.content.value,'');
 assert.equal(add.body.scope.kind,'user');
 id('memory-list').querySelector('button').click();
 assert.equal(form.elements.content.value,malicious);
 form.elements.content.value='更正后的事实';failure='memory_conflict';
 form.dispatchEvent(new w.Event('submit',{cancelable:true}));await settle();
 assert.equal(errors.pop(),'memory_conflict');
 assert.equal(form.elements.content.value,'更正后的事实','a conflict must preserve the user draft');
 const edit=requests.find(r=>r.url.endsWith('/edit'));
 assert.equal(edit.body.revision,3);assert.equal(edit.body.value.content,'更正后的事实');
 assert.equal(Object.hasOwn(edit.body,'owner'),false);
 id('memory-cancel-edit').click();
 assert.equal(form.elements.content.value,'');
 id('memory-list').querySelectorAll('button')[1].click();
 assert.equal(requests.filter(r=>r.url.endsWith('/forget')).length,0);
 assert.equal(confirmation.details,malicious);assert(confirmation.description.includes('聊天记录'));
 await confirmation.allow();
 assert.equal(requests.filter(r=>r.url.endsWith('/forget'))[0].body.revision,3);
 id('memory-search').elements.query.value='小蓝 /srv/a_b';
 id('memory-search').dispatchEvent(new w.Event('submit',{cancelable:true}));await settle();
 assert(requests.at(-1).url.includes(encodeURIComponent('小蓝 /srv/a_b')));
 id('memory-scope').value='fleet';id('memory-scope').dispatchEvent(new w.Event('change'));await settle();
 assert(requests.at(-1).url.includes('scope=fleet'));
 form.elements.content.value='设备池职责';form.dispatchEvent(new w.Event('submit',{cancelable:true}));await settle();
 assert.equal(requests.findLast(r=>r.url==='/api/memories'&&r.body).body.scope.kind,'fleet');
 id('memory-scope').value='device_env:device-1';id('memory-scope').dispatchEvent(new w.Event('change'));await settle();
 form.elements.content.value='Python 环境';form.dispatchEvent(new w.Event('submit',{cancelable:true}));await settle();
 assert.equal(requests.findLast(r=>r.url==='/api/memories'&&r.body).body.scope.device_id,'device-1');
 id('memory-scope').value='device_env:retired';id('memory-scope').dispatchEvent(new w.Event('change'));await settle();
 assert.equal(id('memory-save').disabled,true);assert.equal(id('memory-list').querySelector('button').disabled,true);
 assert.equal(id('memory-list').querySelectorAll('button')[1].disabled,false);
 let resolve;
 pending=new Promise(done=>{resolve=done;});
 const late=view.load('owner',io);
 principal=null;view.clear();
 resolve({page,max_bytes:65536});await late;
 assert.equal(id('memory-list').textContent,'');
 assert.equal(id('memory-summary').textContent,'');
 assert.equal(form.elements.content.value,'');
 assert(!id('memory-scope').textContent.includes('PRIVATE_DEVICE_NAME'));
 principal='guest';pending=null;page={generation:0,total:0,used_bytes:0,items:[],has_more:false};
 await view.load('guest',io);
 assert(!id('memory-list').textContent.includes(malicious));
 console.log('Memory UI: CRUD payloads, confirmation, conflict drafts, escaping and logout isolation passed.');
})().catch(error=>{console.error(error);process.exitCode=1;});
