/* Personal fact management. The owner is captured from the authenticated page. */
window.MemoryView = (() => {
  const byId = id => document.getElementById(id);
  let io = null, owner = null, epoch = 0, sequence = 0;
  let scopeKey = 'user', scopes = [], scopeSelection = {kind:'user'};
  let page = null, query = '', offset = 0, previous = [], editing = null, requestId = null;
  const current = (who, version) => owner === who && epoch === version && io?.owner() === who;
  const stamp = value => value ? new Date(value * 1000).toLocaleString() : '永久';
  const sources = {web:'你在网页保存',qq:'你在 QQ 确认',agent:'对话工具保存',legacy_import:'旧版原文导入（原时间未知）'};
  function resetEditor() {
    editing = null; requestId = crypto.randomUUID(); byId('memory-form').reset();
    byId('memory-edit-title').textContent = '添加一条记忆'; byId('memory-cancel-edit').hidden = true;
  }
  function clear() {
    owner = null; epoch++; sequence++; page = null; io = null;
    scopeKey = 'user'; scopes = []; scopeSelection = {kind:'user'};
    byId('memory-scope').replaceChildren(...[['user','我的资料'],['fleet','我的设备池']].map(([key,label])=>{const option=document.createElement('option');option.value=key;option.textContent=label;return option;}));
    query = ''; offset = 0; previous = []; editing = null; requestId = null;
    byId('memory-list').replaceChildren(); byId('memory-summary').textContent = '';
    byId('memory-search').reset(); byId('memory-form').reset(); byId('memory-error').textContent = '';
    byId('memory-edit-title').textContent = '添加一条记忆'; byId('memory-cancel-edit').hidden = true;
  }
  function edit(item) {
    editing = {id:item.id,revision:item.revision,scope:item.scope};
    const fields = byId('memory-form').elements;
    fields.content.value = item.content; fields.kind.value = item.kind; fields.pinned.checked = item.pinned;
    if (item.valid_until) {
      const date = new Date(item.valid_until * 1000);
      fields.valid_until.value = new Date(date - date.getTimezoneOffset()*60000).toISOString().slice(0,16);
    } else fields.valid_until.value = '';
    byId('memory-edit-title').textContent = '更正记忆 · v' + item.revision;
    byId('memory-cancel-edit').hidden = false; fields.content.focus();
  }
  function render() {
    const readOnly = !!scopes.find(s=>s.key===scopeKey)?.read_only;
    byId('memory-save').disabled = readOnly;
    byId('memory-summary').textContent = page.total + ' / 128 条（账号总计） · ' + page.used_bytes + ' / ' + page.max_bytes + ' 字节 · 版本 ' + page.generation;
    byId('memory-list').replaceChildren();
    if (!page.items.length) byId('memory-list').textContent = query ? '没有匹配的记忆。' : '还没有记忆。可以添加事实，或在私聊说“记住……”。';
    for (const item of page.items) {
      const card = document.createElement('article'); card.className = 'item';
      const details = document.createElement('div'); details.className = 'item-content';
      const title = document.createElement('strong');
      title.textContent = (item.pinned ? '置顶 · ' : '') + (item.kind === 'decision' ? '决定' : '事实') + (item.valid_until && item.valid_until <= Date.now()/1000 ? ' · 已过期' : '');
      const body = document.createElement('pre'); body.className = 'details'; body.textContent = item.content;
      const meta = document.createElement('p'); meta.className = 'subtle';
      meta.textContent = '范围：' + (scopes.find(s=>s.key===scopeKey)?.label || '我的资料') + ' · ' + (sources[item.source.kind] || item.source.kind) + ' · v' + item.revision + '\n有效至：' + stamp(item.valid_until) + '\nID：' + item.id +
        (item.source.session_id ? '\n来源会话：' + item.source.session_id : '') + (item.source.turn_id ? '\n来源任务：' + item.source.turn_id : '');
      const buttons = document.createElement('div'); buttons.className = 'actions';
      const change = document.createElement('button'); change.type = 'button'; change.className = 'quiet small'; change.textContent = '更正'; change.onclick = () => edit(item);
      change.disabled = readOnly;
      const forget = document.createElement('button'); forget.type = 'button'; forget.className = 'quiet small'; forget.textContent = '忘记';
      forget.onclick = () => {
        const who=owner, version=epoch, key=scopeKey;
        io.confirm('忘记这条记忆？','正文与检索索引会被移除，旧模型上下文将失效。聊天记录和备份仍独立保留。',item.content,async()=>{
          if (!current(who,version) || key!==scopeKey) return;
          await io.api('/api/memories/'+item.id+'/forget',{revision:item.revision,scope:item.scope});
          if (!current(who,version)) return;
          if (editing?.id===item.id) resetEditor();
          await refresh();
        });
      };
      buttons.append(change,forget); details.append(title,body,meta); card.append(details,buttons); byId('memory-list').append(card);
    }
    byId('memory-prev').disabled = previous.length === 0;
    byId('memory-next').disabled = !page.has_more;
  }
  async function refresh() {
    const who=owner, version=epoch, ticket=++sequence;
    const result = await io.api('/api/memories?query='+encodeURIComponent(query)+'&offset='+offset+'&scope='+encodeURIComponent(scopeKey));
    if (!current(who,version) || ticket!==sequence) return;
    scopes = result.scopes || [{key:'user',label:'我的资料',scope:{kind:'user'}}];
    scopeSelection = scopes.find(s=>s.key===scopeKey)?.scope || {kind:'user'};
    byId('memory-scope').replaceChildren(...scopes.map(s=>{const option=document.createElement('option');option.value=s.key;option.textContent=s.label;return option;}));
    byId('memory-scope').value=scopeKey;
    page = {...result.page,max_bytes:result.max_bytes}; byId('memory-error').textContent = ''; render();
  }
  async function load(who, bridge) {
    if (owner!==who) { clear(); io=bridge; owner=who; resetEditor(); }
    else io=bridge;
    try { await refresh(); }
    catch(error) { if (io?.owner()===who && owner===who) byId('memory-error').textContent = error.message; }
  }
  byId('memory-form').addEventListener('submit',event=>{
    event.preventDefault(); const fields=event.currentTarget.elements, who=owner,version=epoch;
    if (!page || !io) return;
    const value={content:fields.content.value,kind:fields.kind.value,pinned:fields.pinned.checked,valid_until:fields.valid_until.value ? Math.floor(new Date(fields.valid_until.value).getTime()/1000) : null};
    const target=editing ? {url:'/api/memories/'+editing.id+'/edit',data:{revision:editing.revision,scope:editing.scope,value}} : {url:'/api/memories',data:{request_id:requestId,generation:page.generation,scope:scopeSelection,value}};
    io.action(byId('memory-save'),async()=>{
      await io.api(target.url,target.data);
      if (!current(who,version)) return;
      resetEditor(); await refresh();
    });
  });
  byId('memory-scope').onchange=()=>{if(!io)return;epoch++;sequence++;scopeKey=byId('memory-scope').value;query='';offset=0;previous=[];page=null;byId('memory-list').replaceChildren();byId('memory-search').reset();resetEditor();io.action(byId('memory-reload'),refresh);};
  byId('memory-cancel-edit').onclick=resetEditor;
  byId('memory-reload').onclick=()=>io && io.action(byId('memory-reload'),refresh);
  byId('memory-search').onsubmit=event=>{event.preventDefault(); if(!io)return;query=event.currentTarget.elements.query.value;offset=0;previous=[];io.action(byId('memory-search-button'),refresh);};
  byId('memory-prev').onclick=()=>{if(!io||!previous.length)return;offset=previous.pop();io.action(byId('memory-prev'),refresh);};
  byId('memory-next').onclick=()=>{if(!io||!page?.has_more)return;previous.push(offset);offset=page.next_offset;io.action(byId('memory-next'),refresh);};
  return {load,clear};
})();
