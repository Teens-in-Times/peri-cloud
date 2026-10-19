/* Cross-board navigation is explicit; external references are independently editable. */
window.PeriNavigation = (() => {
  'use strict';
  function render(state,navigate){
    const $=id=>document.getElementById(id),board=state.boards.find(b=>b.id===state.ui.board),byId=id=>state.boards.find(b=>b.id===id);
    const ancestors=[],seen=new Set();let cursor=board;
    while(cursor&&!seen.has(cursor.id)){seen.add(cursor.id);ancestors.unshift(cursor);cursor=byId(cursor.parentId);}
    $('breadcrumbs').replaceChildren();for(const [i,b]of ancestors.entries()){if(i){const divider=document.createElement('span');divider.textContent='/';$('breadcrumbs').append(divider);}const button=document.createElement('button');button.textContent=b.title;button.disabled=b.id===board.id;button.onclick=()=>navigate(b.id);$('breadcrumbs').append(button);}
    $('boards').replaceChildren();const roots=state.boards.filter(b=>!b.parentId),placed=new Set();
    function append(b,depth=0){if(placed.has(b.id))return;placed.add(b.id);const button=document.createElement('button');button.dataset.board=b.id;button.dataset.depth=String(depth);button.textContent=b.title;button.setAttribute('aria-pressed',String(b.id===board.id));button.onclick=()=>navigate(b.id);$('boards').append(button);
      if(ancestors.some(a=>a.id===b.id))for(const child of state.boards.filter(c=>c.parentId===b.id))append(child,depth+1);}
    roots.forEach(b=>append(b));
    $('subgraph-guide').textContent=board.nodes.some(n=>n.subgraph)?'职责总览 · 点击卡片内「展开子图」。卡片可改名、连线、涂画；不代表独立进程。':board.parentId?'子图 · 实线与虚线沿用证据标记。外部引用只显示接口，跨图关系列在下方。':'研究局部 · 保留原证据范围。';
    const interfaces=board.interfaces||[];$('interface-panel').hidden=!interfaces.length;$('interface-summary').textContent='跨图接口快照 · '+interfaces.length+' 条关系（点击跳转，图内涂改独立保存）';$('interface-list').replaceChildren();
    for(const link of interfaces){const button=document.createElement('button'),direction=document.createElement('span'),text=document.createElement('span'),label=document.createElement('small');direction.textContent=link.direction==='in'?'入站':'出站';text.textContent=link.fromTitle+' → '+link.toTitle;label.textContent=link.label;button.append(direction,text,label);button.onclick=()=>navigate(link.targetBoard,link.targetNode);$('interface-list').append(button);}
    const select=$('edit-subgraph'),previous=select.value;select.replaceChildren();const empty=document.createElement('option');empty.value='';empty.textContent='普通模块';select.append(empty);
    for(const target of state.boards.filter(b=>b.id!==board.id)){const option=document.createElement('option');option.value=target.id;option.textContent=target.title;select.append(option);}select.value=previous;
  }
  function validate(boards){
    const byId=new Map(boards.map(b=>[b.id,b]));
    for(const b of boards){if(b.parentId&&!byId.has(b.parentId))throw new Error('子图的父图不存在。');const visited=new Set([b.id]);let parent=byId.get(b.parentId);while(parent){if(visited.has(parent.id))throw new Error('子图目录不能循环。');visited.add(parent.id);parent=byId.get(parent.parentId);}
      for(const n of b.nodes){if(n.subgraph&&!byId.has(n.subgraph))throw new Error('模块关联的子图不存在。');if(n.origin&&!byId.get(n.origin.board)?.nodes.some(item=>item.id===n.origin.node))throw new Error('外部引用的归属模块不存在。');}
      for(const link of b.interfaces||[])if(!byId.get(link.targetBoard)?.nodes.some(n=>n.id===link.targetNode))throw new Error('跨图接口的目标不存在。');
    }
  }
  return {render,validate};
})();
