/* Compose the future plan with every current Peri module; preserve old edits. */
(() => {
  'use strict';
  const data=window.PERI_DATA,copy=value=>JSON.parse(JSON.stringify(value));
  const baseline=data.boards.find(b=>b.id==='peri'),original=data.boards.find(b=>b.id==='target');
  const previousTarget=copy(original),previousMine=copy(data.boards.find(b=>b.id==='mine'));
  const kinds={reuse:'沿用现有',extend:'扩展现有',new:'新增设计'};
  const mappings={
    user:['t-user','reuse'],runtime:['t-host','extend'],permission:['t-policy','reuse'],
    executors:['t-executor','reuse'],memory:['t-memory','reuse'],delivery:['t-outbox','extend'],
    scheduler:['t-scheduler','reuse'],contact:['t-contact','extend'],broker:['t-broker','extend'],
    identity:['t-account','reuse'],models:['t-models','extend'],candidate:['t-candidate','new']
  };
  const appended=baseline.nodes.filter(n=>!mappings[n.id]);
  const remap=new Map(baseline.nodes.map(n=>[n.id,mappings[n.id]?.[0]||'base-'+n.id]));
  data.integrationKinds=kinds;data.planRevision=2;
  data.lenses.baseline='方案内 · 现有基础';data.lenses.additions='方案内 · 新增与扩展';
  const target=copy(original);target.title='我们的方案 · 现有 + 新增';
  target.subtitle='完整可编辑方案：现有模块、扩展位置与新增设计同图。新增和扩展部分尚未实施。';target.planRevision=2;
  for(const node of target.nodes){node.integration='new';node.baselineRefs=[];node.lenses=[...new Set([...node.lenses,'additions'])];}
  for(const before of baseline.nodes){
    const [id,integration]=mappings[before.id]||[remap.get(before.id),'reuse'];
    let node=target.nodes.find(n=>n.id===id);
    if(!node){const index=appended.findIndex(n=>n.id===before.id);node={...copy(before),id,x:30+(index%5)*280,y:1250+Math.floor(index/5)*220};node.lenses=node.lenses.filter(l=>l!=='overview');target.nodes.push(node);}
    const proposedDetails=node.details;
    node.integration=integration;node.baselineRefs=[before.id];node.status=integration==='reuse'?'implemented':'proposal';
    node.details=integration==='reuse'?before.details:'【当前实现】\n'+before.details+'\n\n【方案中的扩展】\n'+proposedDetails;
    node.lenses=[...new Set([...node.lenses,...before.lenses.filter(l=>l!=='overview'),...(before.status==='implemented'?['baseline']:[]),...(integration!=='reuse'?['additions']:[])])];
    if(integration==='reuse'){node.source=before.source;node.summary='沿用现有 · '+before.summary;}
  }
  const entryPositions={'base-channels':[30,-350],'base-gateway':[310,-350],'base-journal':[870,1250],'base-remote':[1150,-150]};
  for(const [id,[x,y]]of Object.entries(entryPositions)){const n=target.nodes.find(n=>n.id===id);if(n){n.x=x;n.y=y;}}
  target.nodes.find(n=>n.id==='t-user').title='你';target.nodes.find(n=>n.id==='t-user').summary='用户目标、追问与控制';
  target.nodes.find(n=>n.id==='t-policy').title='PermissionMiddleware';target.nodes.find(n=>n.id==='t-executor').title='Windows / Linux 执行器';
  for(const id of ['base-channels','base-gateway','base-remote']){const n=target.nodes.find(n=>n.id===id);n.lenses.push('overview');}
  target.edges=target.edges.filter(e=>!(e.from==='t-user'&&e.to==='t-main')&&!(e.from==='t-policy'&&e.to==='t-executor'));
  for(const edge of target.edges){edge.style='dashed';edge.baselineEdgeRef='';}
  for(const before of baseline.edges){const from=remap.get(before.from),to=remap.get(before.to);let edge=target.edges.find(e=>e.from===from&&e.to===to);
    if(!edge){edge={...copy(before),id:'base-'+before.id,from,to};target.edges.push(edge);}edge.baselineEdgeRef=before.id;edge.style=before.style;}
  target.edges.push({id:'plan-chat-dispatch',from:'t-host',to:'t-main',label:'受理主对话',style:'dashed',baselineEdgeRef:''});
  target.edges.push({id:'plan-main-loop',from:'t-main',to:'base-agent',label:'复用原循环',style:'dashed',baselineEdgeRef:''});
  target.edges.push({id:'plan-worker-loop',from:'t-worker',to:'base-agent',label:'独立上下文复用',style:'dashed',baselineEdgeRef:''});
  const mine=copy(target);mine.id='mine';mine.title='我的草稿';mine.subtitle='现有基础与新增方案已合并；可以直接删改职责、关系和布局。';
  data.boards=data.boards.map(b=>b.id==='target'?target:b.id==='mine'?mine:b);data.previousPlans={target:previousTarget,mine:previousMine};
  const integrationDecision={id:'d19',title:'现有实现与新增方案放在同一图中',status:'confirmed',body:'方案包含全部现有模块和关系，标记沿用、扩展与新增，支持对照当前职责。旧浏览器草稿升级时保留已有修改。'};
  data.decisions.push(integrationDecision);
  function same(a,b){return JSON.stringify(a)===JSON.stringify(b);}
  function mergeRecord(current,previous,next,keepPosition=false){const merged=copy(next);
    for(const key of Object.keys(previous))if(Object.hasOwn(current,key)&&(!same(current[key],previous[key])||keepPosition&&['x','y'].includes(key)))merged[key]=copy(current[key]);return merged;}
  function upgradeBoard(board){
    if(board.planRevision>=2)return {board,changed:false};
    const old=data.previousPlans[board.id]||previousTarget;
    const isPlan=['target','mine'].includes(board.id)||['t-main','t-host','t-worker'].every(id=>board.nodes.some(n=>n.id===id));
    if(!isPlan)return {board,changed:false};
    const next=copy(board.id==='target'?target:mine),oldNodeMap=new Map(old.nodes.map(n=>[n.id,n])),currentNodeMap=new Map(board.nodes.map(n=>[n.id,n]));
    const deletedNodes=new Set(old.nodes.filter(n=>!currentNodeMap.has(n.id)).map(n=>n.id));
    next.nodes=next.nodes.filter(n=>!deletedNodes.has(n.id)).map(n=>{const current=currentNodeMap.get(n.id),previous=oldNodeMap.get(n.id);return current?previous?mergeRecord(current,previous,n,board.strokes.length>0):copy(current):n;});
    for(const n of board.nodes)if(!next.nodes.some(x=>x.id===n.id)&&!oldNodeMap.has(n.id))next.nodes.push({...copy(n),integration:n.integration||'new',baselineRefs:n.baselineRefs||[]});
    const ids=new Set(next.nodes.map(n=>n.id)),oldEdges=new Map(old.edges.map(e=>[e.id,e])),currentEdges=new Map(board.edges.map(e=>[e.id,e]));
    const deletedEdges=new Set(old.edges.filter(e=>!currentEdges.has(e.id)).map(e=>e.id)),deletedPairs=new Set(old.edges.filter(e=>!currentEdges.has(e.id)).map(e=>e.from+'>'+e.to));
    next.edges=next.edges.filter(e=>!deletedEdges.has(e.id)&&!deletedPairs.has(e.from+'>'+e.to)).map(e=>{const current=currentEdges.get(e.id),previous=oldEdges.get(e.id);return current?previous?mergeRecord(current,previous,e):copy(current):e;});
    for(const e of board.edges)if(!next.edges.some(x=>x.id===e.id)&&(!oldEdges.has(e.id)||!same(e,oldEdges.get(e.id))))next.edges.push(copy(e));
    next.edges=next.edges.filter(e=>ids.has(e.from)&&ids.has(e.to));next.id=board.id;next.strokes=copy(board.strokes);
    if(board.title!==old.title)next.title=board.title;if(board.subtitle!==old.subtitle)next.subtitle=board.subtitle;
    if(next.nodes.length>160||next.edges.length>480)return {board,changed:false,blocked:true};return {board:next,changed:true};
  }
  data.upgradeWorkspace=workspace=>{const next=copy(workspace),upgraded=[];let blocked=false;
    next.boards=next.boards.map(b=>{const result=upgradeBoard(b);if(result.changed){upgraded.push(b.id);for(const key of Object.keys(next.ui.views))if(key.startsWith(b.id+':'))delete next.ui.views[key];}blocked ||= Boolean(result.blocked);return result.board;});
    if(upgraded.length&&!next.decisions.some(d=>d.id===integrationDecision.id)&&next.decisions.length<100)next.decisions.push(copy(integrationDecision));return {workspace:next,upgraded,blocked};};
})();
