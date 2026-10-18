/* Native SVG board; no external runtime or network dependencies. */
window.PeriCanvas = (() => {
  'use strict';
  const NS='http://www.w3.org/2000/svg', XHTML='http://www.w3.org/1999/xhtml', W=218, H=118;
  const areaNames={cloud:'云端',local:'本地执行器 / 工作环境',external:'用户 / 外部入口',logical:'逻辑对象',unknown:'部署未知'};
  const el=(name,attrs={})=>{const e=document.createElementNS(NS,name);for(const [k,v] of Object.entries(attrs))e.setAttribute(k,String(v));return e;};
  const uid=prefix=>prefix+'-'+(globalThis.crypto?.randomUUID?.()||Math.random().toString(36).slice(2));
  function hitsSegment(a,b,n,padding=7){
    const left=n.x-padding,right=n.x+W+padding,top=n.y-padding,bottom=n.y+H+padding;
    if(a.x===b.x)return a.x>left&&a.x<right&&Math.max(a.y,b.y)>top&&Math.min(a.y,b.y)<bottom;
    if(a.y===b.y)return a.y>top&&a.y<bottom&&Math.max(a.x,b.x)>left&&Math.min(a.x,b.x)<right;
    return true;
  }
  function detour(start,end,obstacles){
    const xs=[...new Set([start.x,end.x,...obstacles.flatMap(n=>[n.x-22,n.x+W+22])])].sort((a,b)=>a-b);
    const ys=[...new Set([start.y,end.y,...obstacles.flatMap(n=>[n.y-22,n.y+H+22])])].sort((a,b)=>a-b);
    const key=(x,y,d)=>x+','+y+','+d,score=new Map(),parents=new Map(),values=new Map(),blocked=new Map();
    const sx=xs.indexOf(start.x),sy=ys.indexOf(start.y),ex=xs.indexOf(end.x),ey=ys.indexOf(end.y);
    const first={x:sx,y:sy,d:0,g:0,f:Math.abs(end.x-start.x)+Math.abs(end.y-start.y)};
    const queue=[first],firstKey=key(sx,sy,0);score.set(firstKey,0);values.set(firstKey,first);
    let steps=0;
    while(queue.length&&steps++<9000){
      queue.sort((a,b)=>b.f-a.f);const current=queue.pop(),currentKey=key(current.x,current.y,current.d);
      if(current.g!==score.get(currentKey))continue;
      if(current.x===ex&&current.y===ey){const points=[];let k=currentKey;while(k){const p=values.get(k);points.push({x:xs[p.x],y:ys[p.y]});k=parents.get(k);}return points.reverse();}
      for(const [dx,dy,d]of [[1,0,1],[-1,0,1],[0,1,2],[0,-1,2]]){
        const x=current.x+dx,y=current.y+dy;if(x<0||y<0||x>=xs.length||y>=ys.length)continue;
        const a={x:xs[current.x],y:ys[current.y]},b={x:xs[x],y:ys[y]},segment=[a.x,a.y,b.x,b.y].join(',');
        let hit=blocked.get(segment);if(hit===undefined){hit=obstacles.some(n=>hitsSegment(a,b,n));blocked.set(segment,hit);}if(hit)continue;
        const g=current.g+Math.abs(a.x-b.x)+Math.abs(a.y-b.y)+(current.d&&d!==current.d?18:0),k=key(x,y,d);
        if(g>=(score.get(k)??Infinity))continue;
        const next={x,y,d,g,f:g+Math.abs(end.x-b.x)+Math.abs(end.y-b.y)};score.set(k,g);parents.set(k,currentKey);values.set(k,next);queue.push(next);
      }
    }
    return null;
  }
  function route(a,b,index,obstacles=[]){
    const ax=a.x+W/2,ay=a.y+H/2,bx=b.x+W/2,by=b.y+H/2;let s,t,points;
    if(Math.abs(bx-ax)>Math.abs(by-ay)*1.2){
      const sign=bx>ax?1:-1;s={x:ax+sign*W/2,y:ay};t={x:bx-sign*W/2,y:by};
      if(sign*(t.x-s.x)>35){const m=(s.x+t.x)/2;points=[s,{x:m,y:s.y},{x:m,y:t.y},t];}
      else{const rail=Math.max(a.y,b.y)+H+28+(index%3)*14;points=[s,{x:s.x+sign*26,y:s.y},{x:s.x+sign*26,y:rail},{x:t.x-sign*26,y:rail},{x:t.x-sign*26,y:t.y},t];}
    }else{
      const sign=by>ay?1:-1;s={x:ax,y:ay+sign*H/2};t={x:bx,y:by-sign*H/2};
      if(sign*(t.y-s.y)>30){const m=(s.y+t.y)/2;points=[s,{x:s.x,y:m},{x:t.x,y:m},t];}
      else{const rail=Math.max(a.x,b.x)+W+26+(index%3)*14;points=[s,{x:s.x,y:s.y+sign*22},{x:rail,y:s.y+sign*22},{x:rail,y:t.y-sign*22},{x:t.x,y:t.y-sign*22},t];}
    }
    if(obstacles.some(n=>n.id!==a.id&&n.id!==b.id&&points.slice(1).some((p,i)=>hitsSegment(points[i],p,n)))){
      const s0=points[0],t0=points.at(-1),sp={x:s0.x+(s0.x===a.x?-20:s0.x===a.x+W?20:0),y:s0.y+(s0.y===a.y?-20:s0.y===a.y+H?20:0)},tp={x:t0.x+(t0.x===b.x?-20:t0.x===b.x+W?20:0),y:t0.y+(t0.y===b.y?-20:t0.y===b.y+H?20:0)};
      const routed=detour(sp,tp,obstacles);if(routed)points=[s0,...routed,t0];
    }
    const path=points.map((p,i)=>(i?'L':'M')+p.x+','+p.y).join(' '),lengths=points.slice(1).map((p,i)=>Math.hypot(p.x-points[i].x,p.y-points[i].y));
    let half=lengths.reduce((a,b)=>a+b,0)/2,label=points[0];
    for(let i=0;i<lengths.length;i++){if(half<=lengths[i]){const k=lengths[i]?half/lengths[i]:0;label={x:points[i].x+(points[i+1].x-points[i].x)*k,y:points[i].y+(points[i+1].y-points[i].y)*k};break;}half-=lengths[i];}
    return {path,label,points};
  }
  function create(options){
    const svg=options.svg,stage=options.stage,get=options.get,selection=options.selection,notify=options.notify;
    let mode='select',linking=null,drag=null,routeKey='',cache=new Map();
    const board=()=>get().boards.find(b=>b.id===get().ui.board);
    const lens=()=>get().ui.lens;
    const key=()=>get().ui.board+':'+lens();
    const view=()=>get().ui.views[key()]||(get().ui.views[key()]={x:0,y:0,z:.85});
    const layout=()=>board().layouts?.[lens()];
    const position=n=>({...n,...layout()?.positions?.[n.id]});
    const setPosition=(id,x,y)=>{const n=board().nodes.find(n=>n.id===id),p=layout()?.positions?.[id]||n;if(p){p.x=x;p.y=y;}};
    const visible=()=>board().nodes.filter(n=>lens()==='baseline'?(n.baselineRefs||[]).some(id=>window.PERI_DATA.boards[0].nodes.some(b=>b.id===id&&b.status==='implemented')):lens()==='additions'?['extend','new'].includes(n.integration):lens()==='all'||n.lenses.includes(lens())||n.kind==='note'&&n.lenses.includes('all')).map(position);
    const visibleStrokes=()=>board().strokes.filter(s=>lens()==='all'||s.lens==='all'||s.lens===lens());
    const size=()=>{const r=stage.getBoundingClientRect();return {w:r.width||1050,h:r.height||640};};
    const point=e=>{const r=stage.getBoundingClientRect(),v=view();return {x:Math.round((v.x+(e.clientX-r.left)/v.z)*10)/10,y:Math.round((v.y+(e.clientY-r.top)/v.z)*10)/10};};
    function draw(){
      const b=board(),v=view(),{w,h}=size(),nodes=visible(),ids=new Set(nodes.map(n=>n.id));
      svg.setAttribute('viewBox',`${v.x} ${v.y} ${w/v.z} ${h/v.z}`);svg.replaceChildren();
      const title=el('title');title.textContent=b.title+'，当前显示 '+nodes.length+' 个模块';svg.append(title);
      const defs=el('defs'),marker=el('marker',{id:'board-arrow',viewBox:'0 0 10 10',refX:9,refY:5,markerWidth:8,markerHeight:8,orient:'auto-start-reverse',markerUnits:'userSpaceOnUse'});marker.append(el('path',{d:'M 0 0 L 10 5 L 0 10 z',fill:'var(--muted)'}));defs.append(marker);svg.append(defs);
      const regions=el('g'),edges=el('g'),nodeLayer=el('g'),ink=el('g');svg.append(regions,edges,nodeLayer,ink);
      if(layout()?.groups?.length){for(const group of layout().groups){const members=nodes.filter(n=>group.nodeIds.includes(n.id));if(!members.length)continue;const x=Math.min(...members.map(n=>n.x))-18,y=Math.min(...members.map(n=>n.y))-46,right=Math.max(...members.map(n=>n.x))+W+18,bottom=Math.max(...members.map(n=>n.y))+H+20;regions.append(el('rect',{x,y,width:right-x,height:bottom-y,rx:12,class:'group-region'}));const label=el('text',{x:x+12,y:y+25,class:'group-heading'});label.textContent=group.title;regions.append(label);}}
      else for(const area of ['cloud','local']){const group=nodes.filter(n=>n.area===area);if(!group.length)continue;const x=Math.min(...group.map(n=>n.x))-20,y=Math.min(...group.map(n=>n.y))-37,right=Math.max(...group.map(n=>n.x))+W+20,bottom=Math.max(...group.map(n=>n.y))+H+20;
        regions.append(el('rect',{x,y,width:right-x,height:bottom-y,rx:12,fill:'none',stroke:'var(--line)','stroke-dasharray':'4 6'}));const label=el('text',{x:x+10,y:y+20,fill:'var(--muted)','font-size':12});label.textContent=areaNames[area];regions.append(label);}
      const signature=b.id+'|'+lens()+'|'+Boolean(drag?.kind==='node')+'|'+nodes.map(n=>[n.id,n.x,n.y].join(',')).join(';');if(signature!==routeKey){routeKey=signature;cache.clear();}
      const displayedEdges=b.edges.filter(e=>(!e.views||e.views.includes(lens()))&&ids.has(e.from)&&ids.has(e.to)),focus=selection(),related=new Set();if(focus?.type==='node'){related.add(focus.id);for(const e of displayedEdges)if(e.from===focus.id||e.to===focus.id){related.add(e.from);related.add(e.to);}}else if(focus?.type==='edge'){const e=b.edges.find(e=>e.id===focus.id);if(e){related.add(e.from);related.add(e.to);}}svg.dataset.focused=String(related.size>0);
      b.edges.forEach((e,i)=>{if(e.views&&!e.views.includes(lens())||!ids.has(e.from)||!ids.has(e.to))return;const a=nodes.find(n=>n.id===e.from),t=nodes.find(n=>n.id===e.to),cacheId=e.id+e.from+e.to;
        let r=cache.get(cacheId);if(!r){r=route(a,t,i,drag?.kind==='node'?[]:nodes);cache.set(cacheId,r);}
        const group=el('g',{'data-edge':e.id,'data-kind':e.kind||'flow','data-related':focus?.type==='edge'?focus.id===e.id:focus?.type==='node'?(e.from===focus.id||e.to===focus.id):true}),hit=el('path',{d:r.path,class:'edge-hit'}),line=el('path',{d:r.path,class:'edge-line'+(selection()?.type==='edge'&&selection().id===e.id?' selected':''),'marker-end':'url(#board-arrow)'});
        if(e.style==='dashed')line.setAttribute('stroke-dasharray','6 5');group.append(hit,line);if(e.label){const text=el('text',{x:r.label.x,y:r.label.y-7,class:'edge-label'});text.textContent=e.label;group.append(text);}edges.append(group);});
      for(const n of nodes){const group=el('g',{'data-node':n.id,'data-related':!related.size||related.has(n.id),'data-status':n.status,'data-kind':n.kind,'data-selected':selection()?.type==='node'&&selection().id===n.id,'data-linking':linking===n.id,class:'graph-node',transform:`translate(${n.x} ${n.y})`});
        const rect=el('rect',{width:W,height:H,rx:n.kind==='note'?3:8}),fo=el('foreignObject',{x:13,y:10,width:W-26,height:H-20}),wrap=document.createElementNS(XHTML,'div');wrap.className='node-content';
        const meta=document.createElementNS(XHTML,'div');meta.className='node-meta';meta.textContent=window.PERI_DATA.statuses[n.status]+' · '+(window.PERI_DATA.integrationKinds[n.integration]||areaNames[n.area]||'逻辑对象');
        const title=document.createElementNS(XHTML,'div');title.className='node-name';title.textContent=n.title;
        const summary=document.createElementNS(XHTML,'div');summary.className='node-summary';summary.textContent=n.summary;wrap.append(meta,title,summary);fo.append(wrap);group.append(rect,fo);nodeLayer.append(group);}
      const colors={amber:'var(--accent)',blue:'var(--blue)',red:'var(--red)',ink:'var(--ink)'};
      for(const s of visibleStrokes()){const d=s.points.map((p,i)=>(i?'L':'M')+p.x+','+p.y).join(' '),group=el('g',{'data-stroke':s.id});group.append(el('path',{d,class:'drawing-hit'}),el('path',{d,class:'drawing-line'+(selection()?.type==='stroke'&&selection().id===s.id?' drawing-selected':''),stroke:colors[s.color]||colors.amber,'stroke-width':s.width}));ink.append(group);}
      stage.dataset.mode=mode;document.getElementById('zoom').textContent=Math.round(v.z*100)+'%';
      document.getElementById('graph-count').textContent=`${nodes.length} / ${b.nodes.length} 个模块 · ${b.edges.length} 条关系 · ${b.strokes.length} 笔涂画`;
    }
    function fit(save=true){const nodes=visible(),strokes=visibleStrokes(),points=strokes.flatMap(s=>s.points);if(!nodes.length&&!points.length){draw();return;}
      const minX=Math.min(...nodes.map(n=>n.x),...points.map(p=>p.x))-55,minY=Math.min(...nodes.map(n=>n.y),...points.map(p=>p.y))-55,maxX=Math.max(...nodes.map(n=>n.x+W),...points.map(p=>p.x))+55,maxY=Math.max(...nodes.map(n=>n.y+H),...points.map(p=>p.y))+55,{w,h}=size();
      const z=Math.max(.25,Math.min(1.1,(w-24)/(maxX-minX),(h-24)/(maxY-minY)));get().ui.views[key()]={x:minX-(w/z-(maxX-minX))/2,y:minY-(h/z-(maxY-minY))/2,z};draw();if(save)options.save();}
    function zoom(factor,at){const v=view(),{w,h}=size(),sx=at?.x??w/2,sy=at?.y??h/2,x=v.x+sx/v.z,y=v.y+sy/v.z,z=Math.max(.25,Math.min(2.5,v.z*factor));v.z=z;v.x=x-sx/z;v.y=y-sy/z;draw();options.save();}
    function setMode(value){mode=value;linking=null;document.querySelectorAll('[data-mode]').forEach(b=>{if(b.tagName==='BUTTON')b.setAttribute('aria-pressed',String(b.dataset.mode===value));});document.querySelectorAll('.pen-options').forEach(e=>e.hidden=value!=='pen');draw();}
    function center(){const v=view(),{w,h}=size();return {x:Math.round((v.x+w/(2*v.z)-W/2)/8)*8,y:Math.round((v.y+h/(2*v.z)-H/2)/8)*8};}
    function focusNode(id){const n=visible().find(n=>n.id===id)||board().nodes.find(n=>n.id===id);if(!n)return;const v=view(),{w,h}=size();v.z=Math.max(.85,v.z);v.x=n.x+W/2-w/(2*v.z);v.y=n.y+H/2-h/(2*v.z);draw();options.save();}
    function connect(id){if(!linking){linking=id;options.select('node',id);notify('已选起点，再点终点。');return;}if(linking===id){linking=null;draw();return;}
      const from=linking;linking=null;if(board().edges.length>=480){notify('单图最多 480 条关系。');return;}if(board().edges.some(e=>e.from===from&&e.to===id)){notify('已有同方向关系。');draw();return;}
      const before=options.snapshot(),e={id:uid('edge'),from,to:id,label:'',style:'dashed'};board().edges.push(e);options.select('edge',e.id);options.commit(before,'已连线，可填写关系。');}
    function distance(p,a,b){const dx=b.x-a.x,dy=b.y-a.y,l=dx*dx+dy*dy,t=l?Math.max(0,Math.min(1,((p.x-a.x)*dx+(p.y-a.y)*dy)/l)):0;return Math.hypot(p.x-a.x-t*dx,p.y-a.y-t*dy);}
    function erase(p){const ids=new Set(visibleStrokes().filter(s=>s.points.some((q,i)=>i?distance(p,s.points[i-1],q)<12/view().z:Math.hypot(p.x-q.x,p.y-q.y)<12/view().z)).map(s=>s.id));if(ids.size){board().strokes=board().strokes.filter(s=>!ids.has(s.id));drag.moved=true;draw();}}
    svg.addEventListener('pointerdown',e=>{if(e.button!==0)return;const node=e.target.closest?.('[data-node]'),edge=e.target.closest?.('[data-edge]'),stroke=e.target.closest?.('[data-stroke]');
      if(mode==='pen'){if(board().strokes.length>=500){notify('涂画较多，请先导出或擦除部分画线。');return;}const before=options.snapshot(),s={id:uid('stroke'),lens:lens(),color:document.getElementById('pen-color').value,width:Number(document.getElementById('pen-width').value),points:[point(e)]};board().strokes.push(s);drag={kind:'pen',id:s.id,before,moved:true};options.select(null);}
      else if(mode==='erase'){drag={kind:'erase',before:options.snapshot(),moved:false};erase(point(e));}
      else if(node){const id=node.dataset.node;if(mode==='connect'){connect(id);return;}const n=visible().find(n=>n.id===id);drag={kind:'node',id,x:n.x,y:n.y,startX:e.clientX,startY:e.clientY,before:options.snapshot(),moved:false};options.select('node',id);}
      else if(edge){options.select('edge',edge.dataset.edge);return;}else if(stroke){options.select('stroke',stroke.dataset.stroke);return;}
      else{options.select(null);const v=view();drag={kind:'pan',x:v.x,y:v.y,startX:e.clientX,startY:e.clientY,moved:false};}
      svg.setPointerCapture?.(e.pointerId);e.preventDefault();draw();});
    svg.addEventListener('pointermove',e=>{if(!drag)return;if(drag.kind==='pen'){const s=board().strokes.find(s=>s.id===drag.id),p=point(e),last=s.points.at(-1);if(s.points.length<2000&&Math.hypot(p.x-last.x,p.y-last.y)>1.5/view().z)s.points.push(p);draw();return;}if(drag.kind==='erase'){erase(point(e));return;}
      const dx=e.clientX-drag.startX,dy=e.clientY-drag.startY,v=view();if(Math.abs(dx)+Math.abs(dy)>3)drag.moved=true;if(drag.kind==='node'){const n=board().nodes.find(n=>n.id===drag.id);if(n){setPosition(n.id,Math.round((drag.x+dx/v.z)/8)*8,Math.round((drag.y+dy/v.z)/8)*8);}}else{v.x=drag.x-dx/v.z;v.y=drag.y-dy/v.z;}stage.dataset.dragging='true';draw();});
    function end(cancel=false){if(!drag)return;const d=drag;drag=null;stage.dataset.dragging='false';if(cancel&&d.before){options.restore(d.before);return;}if(d.kind==='pen'){const s=board().strokes.find(s=>s.id===d.id);if(s?.points.length===1)s.points.push({x:s.points[0].x+.1,y:s.points[0].y+.1});}if(d.before&&d.moved)options.commit(d.before);else if(d.moved)options.save();draw();}
    svg.addEventListener('pointerup',()=>end());svg.addEventListener('pointercancel',()=>end(true));
    svg.addEventListener('dblclick',e=>{const n=e.target.closest?.('[data-node]');if(n){options.select('node',n.dataset.node);document.getElementById('edit-title').focus();}});
    stage.addEventListener('wheel',e=>{e.preventDefault();if(e.ctrlKey||e.metaKey){const r=stage.getBoundingClientRect();zoom(Math.exp(-e.deltaY*.004),{x:e.clientX-r.left,y:e.clientY-r.top});}else{const v=view();v.x+=e.deltaX/v.z;v.y+=e.deltaY/v.z;draw();options.save();}},{passive:false});
    if(typeof ResizeObserver!=='undefined')new ResizeObserver(()=>draw()).observe(stage);
    function focusGroup(id){const group=layout()?.groups?.find(g=>g.id===id),members=visible().filter(n=>group?.nodeIds.includes(n.id));if(!members.length)return;const {w,h}=size(),minX=Math.min(...members.map(n=>n.x))-45,minY=Math.min(...members.map(n=>n.y))-65,maxX=Math.max(...members.map(n=>n.x))+W+45,maxY=Math.max(...members.map(n=>n.y))+H+45,z=Math.min(1.25,(w-24)/(maxX-minX),(h-24)/(maxY-minY));get().ui.views[key()]={x:minX-(w/z-(maxX-minX))/2,y:minY-(h/z-(maxY-minY))/2,z:Math.max(.25,z)};draw();options.save();}
    function moveNode(id,dx,dy){const n=visible().find(n=>n.id===id);if(n)setPosition(id,n.x+dx,n.y+dy);}
    return {draw,fit,zoom,setMode,center,focusNode,focusGroup,moveNode,visible,view,resetLink:()=>{linking=null;},route,hitsSegment};
  }
  return {create,route,hitsSegment,W,H};
})();
