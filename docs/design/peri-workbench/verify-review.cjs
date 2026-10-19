const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto');
const {TextEncoder,TextDecoder}=require('node:util');
const {JSDOM,VirtualConsole}=require(path.resolve(__dirname,'../../../scripts/portal-ui/node_modules/jsdom'));
const html=fs.readFileSync(path.join(__dirname,'review-20261019.html'),'utf8');
function mount(source){const errors=[],vc=new VirtualConsole();vc.on('jsdomError',e=>errors.push(e.message));const dom=new JSDOM(source,{url:'http://127.0.0.1/peri-workbench/review-20261019.html',runScripts:'dangerously',virtualConsole:vc,beforeParse(w){w.TextEncoder=TextEncoder;w.TextDecoder=TextDecoder;w.ResizeObserver=class{observe(){}};Object.defineProperty(w.HTMLElement.prototype,'getBoundingClientRect',{value:()=>({left:0,top:0,width:1200,height:720,right:1200,bottom:720})});w.URL.createObjectURL=()=> 'blob:test';w.URL.revokeObjectURL=()=>{};}});assert.equal(errors.length,0,errors.join('\n'));assert.ok(dom.window.PeriWorkbench);return {dom,w:dom.window,d:dom.window.document,app:dom.window.PeriWorkbench,errors};}
const c=mount(html),$=id=>c.d.getElementById(id),active=()=>{const s=c.app.getState();return s.boards.find(b=>b.id===s.ui.board);};
const canonical=value=>JSON.stringify(value,(key,v)=>v&&typeof v==='object'&&!Array.isArray(v)?Object.fromEntries(Object.entries(v).sort(([a],[b])=>a.localeCompare(b))):v);
const state=c.app.getState(),review=active(),checks=[];
assert.equal(review.id,'review-20261019');assert.equal(review.nodes.length,45);assert.equal(state.boards.length,10);assert.equal(c.app.canvas.visible().length,10);
assert.equal(c.d.querySelectorAll('[data-edge]').length,review.edges.filter(e=>e.kind==='summary').length);
assert.ok(c.app.canvas.view().z>=.8,'overview fits readable main labels at 1200px');
assert.equal(review.nodes.find(n=>n.id==='t-worker').area,'cloud');assert.equal(review.nodes.find(n=>n.id==='t-executor').area,'local');
for(const id of ['r-session-hub','r-config','r-boundary','r-mcp'])assert.ok(review.nodes.some(n=>n.id===id));
assert.equal(review.nodes.some(n=>n.id==='t-owner'),false);assert.ok(review.nodes.find(n=>n.id==='base-journal').details.includes('所有权约束'));
checks.push('10-node overview; detailed graph keeps 45 modules; cloud/local boundary and added responsibilities');
for(const [lens,layout]of Object.entries(review.layouts)){
  const nodes=review.nodes.filter(n=>layout.positions[n.id]);for(let i=0;i<nodes.length;i++)for(let j=i+1;j<nodes.length;j++){const a=layout.positions[nodes[i].id],b=layout.positions[nodes[j].id];assert.equal(a.x<b.x+218&&a.x+218>b.x&&a.y<b.y+118&&a.y+118>b.y,false,lens+' overlap '+nodes[i].id+' '+nodes[j].id);}
  for(const group of layout.groups)for(const id of group.nodeIds)assert.ok(review.nodes.some(n=>n.id===id));
}
const ids=new Set(review.nodes.map(n=>n.id));for(const e of review.edges){assert.ok(ids.has(e.from)&&ids.has(e.to));if(e.kind==='summary')assert.equal(JSON.stringify(e.views),JSON.stringify(['overview']));}
checks.push('no overlapping modules in any domain layout; valid group and edge references');
if(process.argv[2]){const sourceText=fs.readFileSync(process.argv[2],'utf8'),source=JSON.parse(sourceText);for(const b of source.boards)assert.equal(canonical(state.boards.find(x=>x.id===b.id)),canonical(b),'original board preserved '+b.id);
  const original=source.boards.find(b=>b.id===source.ui.board),user=original.nodes.find(n=>n.id==='t-user');for(const key of ['title','summary','details'])assert.equal(review.nodes.find(n=>n.id==='t-user')[key],user[key]);assert.ok(review.nodes.find(n=>n.title==='身份认证 / OAuth').details.includes(original.nodes.find(n=>n.title==='OAuth').details));
  checks.push('all nine source boards preserved exactly; user identity and OAuth intent retained');
}
const groupPicker=$('layout-group');groupPicker.value='overview-chat';groupPicker.dispatchEvent(new c.w.Event('change',{bubbles:true}));assert.ok(c.app.canvas.view().z>1,'focus group enlarges text');
c.app.select('node','t-main');assert.equal($('canvas').dataset.focused,'true');assert.ok(c.d.querySelectorAll('[data-edge][data-related="false"]').length>0);
const before=active(),rawMain=before.nodes.find(n=>n.id==='t-main'),displayMain=before.layouts.overview.positions['t-main'];
c.app.canvas.moveNode('t-main',24,16);const after=active();assert.equal(after.layouts.overview.positions['t-main'].x,displayMain.x+24);assert.equal(after.nodes.find(n=>n.id==='t-main').x,rawMain.x,'view edit preserves full layout');
const reloaded=mount(c.app.exportHTML());assert.equal(reloaded.app.getState().boards.find(b=>b.id==='review-20261019').layouts.overview.positions['t-main'].x,displayMain.x+24);reloaded.dom.window.close();
checks.push('group focus, related-edge emphasis, independent view dragging and standalone round trip');
const beforeArrange=c.app.exportJSON();$('arrange-board').click();assert.equal(c.app.getState().boards.length,11);assert.equal(active().nodes.length,review.nodes.length);assert.equal(active().edges.length,review.edges.length);assert.ok(active().layouts.memory.groups.length);c.app.travel('undo');assert.equal(c.app.exportJSON(),beforeArrange);
const candidate=c.app.getState();candidate.boards.find(b=>b.id==='review-20261019').layouts.overview.groups[0].title='<img src=x onerror=alert(1)>';c.app.importText(JSON.stringify(candidate));assert.equal(c.d.querySelectorAll('img').length,0);assert.ok($('canvas').textContent.includes('<img'));c.app.travel('undo');
checks.push('non-destructive arrange-copy with undo; imported group names escaped');
const allStart=Date.now();$('lens').value='all';$('lens').dispatchEvent(new c.w.Event('change',{bubbles:true}));assert.equal(c.app.canvas.visible().length,45);assert.equal(c.d.querySelectorAll('[data-edge][data-kind="summary"]').length,0);const allDrawMs=Date.now()-allStart;
checks.push('full graph expands detail and excludes overview summary edges');
assert.equal(c.errors.length,0);const lines=html.split('\n').length;assert.ok(lines<1000);assert.ok(Buffer.byteLength(html)<1024*1024);c.dom.window.close();
console.log(JSON.stringify({checks,modules:review.nodes.length,relations:review.edges.length,overview:10,allDrawMs,htmlBytes:Buffer.byteLength(html),lines,sha256:crypto.createHash('sha256').update(html).digest('hex')},null,2));
