/* Design projections: a subsystem is a responsibility, not a deployment unit. */
window.PeriHierarchy = (() => {
  'use strict';
  const copy=v=>JSON.parse(JSON.stringify(v));
  function create(input,DATA){
    const source=input.boards.find(b=>b.id==='review-20261019');if(!source)throw new Error('Reviewed design missing');
    const out=copy(input),boards=[],owners=new Map(),auth=source.nodes.find(n=>/OAuth/.test(n.title)).id;
    const spec=[
      ['entry','01 · 入口与用户空间','谁在说话，能看见什么',['base-channels','base-gateway',auth,'t-account','r-session-hub','base-portal'],['t-main','t-outbox']],
      ['dialog','02 · 对话与上下文','输入事件在边界进入对话',['t-events','r-boundary','t-main','r-config','t-cache','t-prompts','t-models'],['t-host','t-memory']],
      ['tasks','03 · 异步任务','派完活，主助手继续聊天',['t-host','t-worker','t-context','t-guards','base-goal'],['t-main','t-state','t-executor']],
      ['observe','04 · 观察与双向控制','状态先看摘要，再逐级深入',['t-state','t-trace','t-inspect','t-inject','t-control','t-artifacts'],['t-main','t-worker']],
      ['execute','05 · 工具与本地执行','简单操作直做，长任务委派',['t-policy','base-remote','base-device-choice','base-transport','t-executor','t-broker','r-mcp'],['t-main','t-worker']],
      ['memory','06 · 记忆与 Skill','工作上下文和长期记忆分开',['t-memory','t-candidate','base-frames','base-search','base-workspace'],['t-main','t-worker']],
      ['schedule','07 · 调度与主动联系','定时触发与投送各有职责',['t-scheduler','base-schedule-adapter','t-contact','t-outbox'],['t-events','base-channels','t-main']],
      ['foundation','08 · 运行基础','共享代码、持久状态、容量和运维',['base-agent','base-journal','base-limits','base-operations'],['t-main','t-host','t-worker']]
    ];
    function node(n,extra={}){return {...copy(n),lenses:['overview','all'],...extra};}
    function projection(raw,id,title,subtitle,parentId,owned,context=[]){
      const own=new Set(owned),ids=new Set([...owned,...context]);
      const b={id,title,subtitle,parentId,nodes:[...owned,...context].map(id=>{const n=raw.nodes.find(n=>n.id===id);if(!n)throw new Error('Missing module '+id);return node(n,own.has(n.id)?{}:{context:true});}),edges:raw.edges.filter(e=>e.kind!=='summary'&&ids.has(e.from)&&ids.has(e.to)&&(own.has(e.from)||own.has(e.to))).map(e=>({...copy(e),views:['overview','all']})),strokes:[],planRevision:2,interfaces:[]};
      for(const n of b.nodes)if(own.has(n.id))owners.set(raw.id+':'+n.id,id);
      boards.push(b);return b;
    }
    function card(id,title,summary,subgraph,x,y,details,source='ours'){
      return {id,title,summary,subgraph,x,y,w:218,h:118,details:details||'职责级抽象；双击进入子图。这里不指定独立进程或微服务。',status:source==='ours'?'proposal':source==='arch'?'implemented':'documented',area:'logical',kind:'module',source,lenses:['overview','all']};
    }
    const root={id:'peri-atlas',title:'Peri · 设计原稿',subtitle:'先前审阅的 45 个设计模块原样保留，含尚未实施的设想。最新已实现状态请看「Peri · 当前实现」。双击卡片展开。',nodes:[],edges:[],strokes:[],planRevision:2};boards.push(root);
    const positions={entry:[400,40],dialog:[760,40],tasks:[760,310],observe:[400,580],execute:[760,580],memory:[400,310],schedule:[40,310],foundation:[40,580]};
    root.nodes.push(node(source.nodes.find(n=>n.id==='t-user'),{x:40,y:40}));owners.set(source.id+':t-user',root.id);
    for(const [key,title,summary,owned,context]of spec){projection(source,'peri-'+key,title,summary,root.id,owned,context);const [x,y]=positions[key];root.nodes.push(card('system-'+key,title.replace(/^\d+ · /,''),summary+' · 进入 '+owned.length+' 个模块','peri-'+key,x,y));}
    const flow=(b,id,from,to,label,kind='flow')=>b.edges.push({id,from,to,label,style:'dashed',kind,views:['overview','all']});
    flow(root,'root-input','t-user','system-entry','消息与身份');flow(root,'root-dialog','system-entry','system-dialog','可信会话输入');flow(root,'root-task','system-dialog','system-tasks','异步委派');flow(root,'root-exec','system-tasks','system-execute','绑定设备执行');flow(root,'root-memory','system-memory','system-dialog','按需授权资料');flow(root,'root-schedule','system-schedule','system-entry','主动联系与事件');flow(root,'root-observe','system-tasks','system-observe','状态 / 自动 trace');
    root.nodes.find(n=>n.id==='system-foundation').details+='\n\n共享运行库供各角色复用；CloudJournal 保存事实。它不是额外的协调 Agent，也不要求每个方框独立部署。';
    const current=window.PeriImplemented.create(input.boards.find(b=>b.id==='peri')),grok=input.boards.find(b=>b.id==='grok'),build=input.boards.find(b=>b.id==='build');
    if(!current||!grok||!build)throw new Error('Reference boards missing');
    function reference(raw,id,title,subtitle,parts){
      const overview={id,title,subtitle,nodes:[],edges:[],strokes:[],planRevision:2};boards.push(overview);
      parts.forEach(([key,label,summary,owned],i)=>{const child=projection(raw,id+'-'+key,label,summary,id,owned);overview.nodes.push(card(id+'-card-'+key,label,summary,child.id,40+(i%2)*360,40+Math.floor(i/2)*260,'职责级索引；子图保留原模块的证据来源和未知范围。',raw.id==='peri'?'arch':'research'));});
      for(let i=1;i<overview.nodes.length;i++)flow(overview,id+'-relation-'+i,overview.nodes[i-1].id,overview.nodes[i].id,'相关职责，非调用顺序','dependency');
    }
    reference(current,'current-atlas','Peri · 当前实现','2026-10-19 · 已实现快照。点入子图查看职责与源码；主/执行 Agent 都在云端，本地只保留执行器。原设计与 Grok 参考另列保留。',[
      ['entry','入口、身份与会话','统一输入、会话选择、持久 FIFO 和聊天投影',['user','channels','gateway','identity','portal','normalizer','logical-session','inbox','chat-projection']],
      ['runtime','云端对话运行','主助手、原循环、持久状态与前后台预算',['runtime','agent','journal','models','limits','goal']],
      ['tasks','异步任务与双向控制','会话复用、窗口续接、消息与有界证据',['worker-service','worker','task-tools','task-ledger','task-messages','task-trace','task-event','device-lease','execution-session','execution-continuity','task-evidence']],
      ['execute','工具与执行器','原权限链、设备绑定与运输',['permission','remote','executors','broker','transport','device-choice','operations']],
      ['knowledge','上下文、记忆与 Skill','统一装配、SOUL 与已有动态来源；后台晋升仍待实现',['context-assembler','context-files','memory','frames','search','workspace','candidate']],
      ['schedule','调度与投送','独立时间模型、联系策略与原持久发件箱',['scheduler','schedule-adapter','contact','delivery']]
    ]);
    const actual=boards.find(b=>b.id==='current-atlas');actual.edges=[];
    for(const [id,from,to,label]of [['input','entry','runtime','FIFO 边界受理'],['context','knowledge','runtime','主/执行上下文'],['delegate','runtime','tasks','主助手异步委派'],['execute','tasks','execute','原审批 + 设备占用'],['return','tasks','entry','类型化结果回流'],['schedule','schedule','runtime','持久定时受理']])actual.edges.push({id:'current-'+id,from:'current-atlas-card-'+from,to:'current-atlas-card-'+to,label,style:'solid',kind:'flow',views:['overview','all']});
    reference(grok,'grok-atlas','Grok Bot · 参考总览','参考既有研究，不新增未经核实的实现结论。公开行为、归档运行时和未知后台分别保留。',[
      ['conversation','对话与事件','Conversation Action 与持久对话',['g-user','g-bot','g-action','g-run','g-send','g-files','g-state','g-completion','g-notice']],
      ['work','委派与工作环境','子任务、工具、审批和云电脑',['g-worker','g-tools','g-computer','g-apps','g-approval','g-local','g-recovery','g-goal','g-other']],
      ['knowledge','上下文与主动性','Skill、记忆、Routine 和共享范围',['g-context','g-memory','g-memory-u','g-skill','g-routine','g-unknown','g-model','g-team']]
    ]);
    const buildBoard=projection(build,'build-atlas','Grok Build · 记忆流水线','CLI 局部参考，不等同于 Grok Bot 后台；保留 Capture / Consolidation / Maintenance 的证据边界。',undefined,build.nodes.map(n=>n.id));
    const rawByBoard=new Map();for(const b of boards)if(b.parentId?.startsWith('peri'))rawByBoard.set(b.id,source);else if(b.parentId==='current-atlas')rawByBoard.set(b.id,current);else if(b.parentId==='grok-atlas')rawByBoard.set(b.id,grok);rawByBoard.set(buildBoard.id,build);
    for(const b of boards){const raw=rawByBoard.get(b.id);if(!raw)continue;
      for(const n of b.nodes)if(n.context)n.origin={board:owners.get(raw.id+':'+n.id),node:n.id};
      const own=new Set(b.nodes.filter(n=>!n.context).map(n=>n.id));
      for(const e of raw.edges.filter(e=>e.kind!=='summary')){const from=own.has(e.from),to=own.has(e.to);if(from===to)continue;const external=from?e.to:e.from,target=owners.get(raw.id+':'+external);if(!target)throw new Error('Missing owner '+external);
        b.interfaces.push({id:e.id,direction:from?'out':'in',from:e.from,to:e.to,label:e.label||'关联',fromTitle:raw.nodes.find(n=>n.id===e.from).title,toTitle:raw.nodes.find(n=>n.id===e.to).title,targetBoard:target,targetNode:external});}
    }
    function arrange(b){if(['peri-atlas','current-atlas','grok-atlas'].includes(b.id))return;
      const internal=b.nodes.filter(n=>!n.context&&n.area!=='local'),context=b.nodes.filter(n=>n.context),local=b.nodes.filter(n=>!n.context&&n.area==='local'),cols=internal.length>=5?3:2;
      internal.forEach((n,i)=>{n.x=40+(i%cols)*280;n.y=80+Math.floor(i/cols)*190;});
      context.forEach((n,i)=>{n.x=940;n.y=80+i*190;});local.forEach((n,i)=>{n.x=940;n.y=80+(context.length+i)*190+(context.length?60:0);});
      const groups=[{id:'owned',title:'本图职责 · 不指定独立部署',nodeIds:internal.map(n=>n.id)}];if(context.length)groups.push({id:'context',title:'接口引用 · 可跳转',nodeIds:context.map(n=>n.id)});if(local.length)groups.push({id:'local',title:'本地 · 执行与工作环境',nodeIds:local.map(n=>n.id)});
      const positions=Object.fromEntries(b.nodes.map(n=>[n.id,{x:n.x,y:n.y}]));b.layouts={overview:{positions:copy(positions),groups:copy(groups)},all:{positions:copy(positions),groups:copy(groups)}};
    }
    boards.forEach(arrange);
    const represented=new Set(boards.flatMap(b=>b.nodes.filter(n=>!n.context).map(n=>n.id)));for(const n of source.nodes)if(!represented.has(n.id))throw new Error('Unassigned design module '+n.id);
    out.boards=boards;out.workspaceId='peri-atlas-core-20261019-v1';out.ui={...out.ui,board:'current-atlas',lens:'overview',views:{},page:'topology',theme:'light'};
    out.decisions.push({id:'d22',title:'职责抽象与子图',status:'confirmed',body:'以职责总览和可进入的子图替代一张完整关系网。抽象卡片不等同于独立进程或微服务；细节模块保留现状标记、用户原稿与证据。跨图关系进入接口清单；外部引用可跳回归属。'});
    return out;
  }
  return {create};
})();
