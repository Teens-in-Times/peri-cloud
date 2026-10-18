/* Presentation-only grouping; all author content and identities are retained. */
window.PeriLayout = (() => {
  'use strict';
  const W=218,H=118,DX=254,DY=178,copy=v=>JSON.parse(JSON.stringify(v));
  const groups={entry:'入口与会话',dialog:'云端 · 主助手',tasks:'云端 · 异步执行',identity:'云端 · 用户空间',observe:'云端 · 观察与控制',execution:'云端 · 工具与权限',memory:'云端 · 记忆与方法',schedule:'云端 · 时间与联系',local:'本地 · 执行器',foundation:'云端 · 共享基础'};
  const membership={
    entry:['t-user','base-channels','base-gateway','r-session-hub','t-events','r-boundary','base-portal','t-outbox','user','channels','gateway','portal','g-user','g-action','g-send','g-files'],
    dialog:['t-main','main','agent','g-bot','g-run','b-user','b-run'],
    tasks:['t-host','t-worker','t-context','base-goal','runtime','goal','g-worker','g-goal','g-completion'],
    identity:['t-account','r-config','r-mcp','identity','g-team','g-other'],
    observe:['t-state','t-trace','t-inspect','t-inject','t-control','t-artifacts','t-broker','broker','g-state'],
    execution:['t-policy','base-remote','t-guards','base-device-choice','permission','remote','device-choice','g-tools','g-approval','g-apps'],
    memory:['t-memory','base-frames','base-search','base-workspace','t-candidate','t-cache','t-prompts','memory','frames','search','workspace','candidate','g-memory','g-memory-u','g-context','g-skill','b-history','b-capture','b-observation','b-consolidation','b-store','b-inject','b-forget'],
    schedule:['t-scheduler','base-schedule-adapter','t-contact','scheduler','schedule-adapter','contact','g-routine','g-notice','g-unknown','b-scheduler'],
    local:['t-executor','executors','g-local','g-computer','b-tools'],
    foundation:['base-agent','base-journal','base-limits','base-transport','base-operations','t-models','t-owner','journal','limits','transport','operations','models','g-model','g-recovery','b-boundary']
  };
  function groupOf(n){if(n.kind==='note')return 'foundation';if(n.area==='local')return 'local';for(const [key,ids]of Object.entries(membership))if(ids.includes(n.id))return key;
    const text=n.title+' '+n.summary;if(/OAuth|认证|用户空间|租户|账号/.test(text))return 'identity';if(/会话|消息|入口|网关|Gateway/.test(text))return 'entry';if(/记忆|Skill|技能|上下文/.test(text))return 'memory';if(/计划|调度|定时/.test(text))return 'schedule';if(/trace|进度|观察|监控/.test(text))return 'observe';return 'foundation';}
  function included(n,lens){if(lens==='all')return true;if(lens==='baseline')return (n.baselineRefs||[]).length>0&&n.integration!=='new';if(lens==='additions')return ['new','extend'].includes(n.integration);return n.lenses.includes(lens);}
  function pack(nodes){const positions={},sections=[],keys=Object.keys(groups),buckets=keys.map(key=>({key,nodes:nodes.filter(n=>groupOf(n)===key)})).filter(g=>g.nodes.length);
    let y=40;
    for(let row=0;row<buckets.length;row+=3){const cells=buckets.slice(row,row+3),height=Math.max(...cells.map(g=>Math.ceil(g.nodes.length/3)*DY+60));
      cells.forEach((g,col)=>{const x=col*830+30;g.nodes.forEach((n,i)=>positions[n.id]={x:x+(i%3)*DX,y:y+44+Math.floor(i/3)*DY});sections.push({id:g.key,title:groups[g.key],nodeIds:g.nodes.map(n=>n.id)});});y+=height+68;}
    return {positions,groups:sections};
  }
  function arrange(board,lenses){const result=copy(board);result.layouts={};for(const lens of Object.keys(lenses)){const nodes=result.nodes.filter(n=>included(n,lens));if(nodes.length)result.layouts[lens]=pack(nodes);}
    const all=result.layouts.all||pack(result.nodes);result.nodes.forEach(n=>Object.assign(n,all.positions[n.id]));return result;}
  function overview(board){const ids=['t-user','base-channels','t-main','t-host','t-worker','t-account','t-state','t-trace','t-executor'],auth=board.nodes.find(n=>/OAuth|身份认证/.test(n.title));if(auth&&!ids.includes(auth.id))ids.push(auth.id);
    const available=ids.filter(id=>board.nodes.some(n=>n.id===id)),positions={},top=['t-user','base-channels','t-main','t-host','t-worker'],bottom=[auth?.id,'t-account','t-state','t-trace','t-executor'];
    top.forEach((id,i)=>{if(available.includes(id))positions[id]={x:30+i*280,y:90};});bottom.forEach((id,i)=>{if(available.includes(id))positions[id]={x:30+i*280,y:390};});
    for(const n of board.nodes)n.lenses=[...new Set([...n.lenses.filter(l=>l!=='overview'),...(available.includes(n.id)?['overview']:[])])];
    board.layouts.overview={positions,groups:[{id:'overview-chat',title:'前台交流 · 云端主助手',nodeIds:top.slice(0,3).filter(id=>available.includes(id))},{id:'overview-task',title:'云端后台 · 委派后继续聊天',nodeIds:top.slice(3).filter(id=>available.includes(id))},{id:'overview-identity',title:'身份与用户空间',nodeIds:bottom.slice(0,2).filter(id=>available.includes(id))},{id:'overview-observe',title:'状态与自动 trace',nodeIds:bottom.slice(2,4).filter(id=>available.includes(id))},{id:'overview-local',title:'本地 · 只保留执行器',nodeIds:bottom.slice(4).filter(id=>available.includes(id))}]};
    return board;
  }
  return {arrange,overview,groupOf,groups,W,H};
})();
