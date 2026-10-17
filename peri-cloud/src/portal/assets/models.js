'use strict';
window.ModelView = (() => {
  const get = id => document.getElementById(id);
  const el = (tag, text = '', className = '') => { const n=document.createElement(tag);n.textContent=text;n.className=className;return n; };
  let owner=null, hooks=null, state=null, generation=0, page=0;
  const command=(label,task)=>{const b=el('button',label,'quiet small');b.type='button';b.addEventListener('click',()=>hooks.action(b,task));return b;};
  const valid=token=>owner && token===generation && hooks.owner()===owner;
  function clear(){generation++;owner=null;state=null;get('aggregate-models').replaceChildren();get('aggregate-models').hidden=true;}
  async function load(account,callbacks){
    if(owner!==account){clear();owner=account;} hooks=callbacks;
    const token=generation;
    const data=await hooks.api('/api/aggregate');if(!valid(token))return;
    state=data;get('aggregate-models').hidden=!data.enabled;
    if(!data.enabled)return;
    get('model-form').hidden=true;get('legacy-model-intro').hidden=true;
    get('model-current').textContent=(data.selection.model||data.default_model)+' · New API';
    get('topology-model').textContent='模型出口：New API · '+(data.selection.model||data.default_model);
    const root=get('aggregate-models');root.replaceChildren();
    root.append(el('p','模型经统一出口调用；上游密钥保存在 New API，管理操作无需调用模型。','subtle'));
    const route=el('p','内部出口：'+data.inference_base,'workspace-path');root.append(route);
    const selectBox=el('section','','model-section');selectBox.append(el('h3','账号默认模型'));
    if(data.catalog_error)selectBox.append(el('p','模型目录暂不可用。可以继续检查下方渠道与管理凭据，管理不依赖推理。','load-error'));
    else {
      const form=modelSelector(data.models,data.selection,data.default_model,async model=>{
        await hooks.api('/api/aggregate/model',{model,revision:data.selection.revision});await load(owner,hooks);
      });selectBox.append(form);
    }
    selectBox.append(el('p','用于未单独选择模型的会话；每条会话也可以在“会话与任务”中单独选择。运行中的任务保持原模型。','subtle'),command('重新读取模型目录',()=>load(owner,hooks)));
    root.append(selectBox);
    if(data.admin){
      const admin=el('section','','model-section');admin.append(el('h3','上游 API 渠道'));
      admin.append(el('p','供应商端点、密钥、模型映射、分组和优先级在此管理。启用只表示配置状态，实际可用性以显式测试为准。','subtle'));
      const actions=el('div','','device-actions');actions.append(command('载入渠道',()=>loadChannels(0)),command('新增 API 渠道',()=>editChannel(null)),command('用量与操作记录',()=>loadUsage()));admin.append(actions);
      const channels=el('div');channels.id='aggregate-channels';admin.append(channels);
      const usage=el('div');usage.id='aggregate-usage';admin.append(usage);root.append(admin);
    }
  }
  function modelSelector(models,selected,defaultModel,save){
    const form=el('form');const label=el('label','模型');const select=el('select');select.required=true;
    for(const [value,text] of [['','使用默认（'+defaultModel+'）'],...models.map(name=>[name,name])]){const option=el('option',text);option.value=value;select.append(option);}
    select.value=selected.model||'';label.append(select);
    const full=el('p',select.selectedOptions[0]?.textContent||selected.model||defaultModel,'workspace-path');
    select.addEventListener('change',()=>{full.textContent=select.selectedOptions[0].textContent;});
    const button=el('button','保存模型选择','primary');button.type='submit';
    form.append(label,full,button);form.addEventListener('submit',event=>{event.preventDefault();hooks.action(button,()=>save(select.value||null));});return form;
  }
  async function sessionModel(session){
    const token=generation;const current=owner;
    const [selection,catalog]=await Promise.all([hooks.api('/api/aggregate/model?session='+encodeURIComponent(session.session_id)),hooks.api('/api/aggregate')]);
    if(!valid(token)||current!==owner)return;
    if(!catalog.enabled)throw new Error('当前使用个人模型配置，请在模型设置中管理。');
    if(!catalog.models)throw new Error('模型目录暂不可用，请检查聚合服务。');
    let selected=selection.model;
    const form=modelSelector(catalog.models,selection,catalog.selection.model||catalog.default_model,async model=>{selected=model;get('confirmation-allow').click();});
    // The dialog's explicit confirm performs the write for this exact session.
    form.querySelector('button').hidden=true;
    const select=form.querySelector('select');select.addEventListener('change',()=>{selected=select.value||null;});
    hooks.confirm('选择这条会话的模型',session.device_name+' · '+session.workspace+'。其他会话不变，当前任务保持原模型。',form,async()=>{
      await hooks.api('/api/aggregate/model?session='+encodeURIComponent(session.session_id),{model:selected,revision:selection.revision});
    },undefined,'保存选择');
  }
  async function loadChannels(targetPage){
    const token=generation;page=targetPage;
    const result=await hooks.api('/api/aggregate/channels?page='+targetPage);if(!valid(token))return;
    const root=get('aggregate-channels');root.replaceChildren();
    if(!result.items.length)root.append(el('p','尚无渠道。','empty'));
    for(const channel of result.items){
      const card=el('article','','device-card');const title=el('div','','section-heading');title.append(el('h4',channel.name),el('span',channel.status===1?'已启用':'已禁用','pill'));card.append(title);
      card.append(el('p','渠道 #'+channel.id+' · '+({1:'OpenAI 兼容',14:'Anthropic'}[channel.type]||'类型 '+channel.type),'subtle'),el('p','供应商端点：'+(channel.base_url||'未展示；请在编辑时指定安全地址'),'workspace-path'),el('p','模型：'+channel.models,'workspace-path'),el('p','分组：'+channel.group+' · 优先级 '+channel.priority+' · 权重 '+channel.weight,'subtle'));
      if(channel.model_mapping && channel.model_mapping!=='{}'){const detail=el('details');detail.append(el('summary','模型名称与上游映射'),el('pre',channel.model_mapping,'saved-result'));card.append(detail);}
      card.append(el('p',channel.test_time?'最近测试：'+new Date(channel.test_time*1000).toLocaleString()+'；记录响应时间 '+channel.response_time+' ms':'尚无测试记录','subtle'));
      const actions=el('div','','device-actions');
      if([1,14].includes(channel.type))actions.append(command('编辑配置 / 更换密钥',()=>editChannel(channel)));
      actions.append(command(channel.status===1?'禁用渠道':'启用渠道',()=>confirmAction(channel,{kind:'status',id:channel.id,revision:channel.revision,enabled:channel.status!==1},channel.status===1?'禁用该渠道？':'启用该渠道？','影响使用这个渠道的新请求；其他渠道配置不变。')),
        command('测试渠道',()=>confirmAction(channel,{kind:'test',id:channel.id,model:null},'测试该渠道？','会实际请求模型并可能消耗上游额度。')),
        command('删除渠道',()=>confirmAction(channel,{kind:'delete',id:channel.id,revision:channel.revision},'删除该渠道？','删除后不能自动恢复密钥。先确认该渠道已经不再需要；可以优先禁用。')));
      card.append(actions);root.append(card);
    }
    const pager=el('div','','device-actions');if(targetPage>0)pager.append(command('上一页',()=>loadChannels(targetPage-1)));
    if((targetPage+1)*30<result.total)pager.append(command('下一页',()=>loadChannels(targetPage+1)));
    root.append(pager);
  }
  function confirmAction(channel,action,title,description){
    const request_id=crypto.randomUUID();
    hooks.confirm(title,description,'渠道 #'+channel.id+' · '+channel.name,async()=>{
      await hooks.api('/api/aggregate/channel-command',{request_id,confirmed:true,action});await loadChannels(page);
    },undefined,'确认执行');
  }
  function editChannel(channel){
    const form=el('form');form.className='channel-form';
    const fields=[['name','渠道名称',channel?.name||''],['type','协议类型',String(channel?.type||1)],['base_url','API 基础地址',channel?.base_url||''],['models','可调用模型（英文逗号分隔）',channel?.models||''],['group','渠道分组（英文逗号分隔）',channel?.group||''],['priority','优先级',String(channel?.priority??10)],['weight','同优先级权重',String(channel?.weight??10)],['model_mapping','模型映射 JSON（内部名称 → 上游名称）',channel?.model_mapping||'{}'],['key',channel?'新密钥（留空保留原密钥）':'API 密钥','']];
    for(const [name,title,value] of fields){const label=el('label',title);let input;
      if(name==='type'){input=el('select');for(const [id,title] of [['1','OpenAI 兼容'],['14','Anthropic']]){const o=el('option',title);o.value=id;input.append(o);}}
      else if(name==='model_mapping'){input=el('textarea');input.rows=4;input.maxLength=16384;}
      else {input=el('input');input.type=name==='key'?'password':['priority','weight'].includes(name)?'number':'text';input.maxLength=name==='key'?8192:name==='models'?8192:1024;}
      input.name=name;input.value=value;input.autocomplete='off';input.required=name!=='key'||!channel;
      if(name==='priority'||name==='weight'){input.min='0';input.max='10000';}
      label.append(input);form.append(label);
    }
    form.append(el('p','地址应填写 New API 所需的基础地址。新渠道使用 HTTPS；现有内部 HTTP 地址可以原样保留。更换端点须重新填写密钥。分组应包含内部调用凭据使用的组。密钥不会回显，也不会进入模型上下文。','subtle'));
    const request_id=crypto.randomUUID();
    form.addEventListener('submit',event=>{event.preventDefault();get('confirmation-allow').click();});
    hooks.confirm(channel?'编辑 API 渠道':'新增 API 渠道','这是共享渠道配置，会影响使用它的所有会话的新请求。仅切换自己的模型，请使用会话模型选择。更换地址时须重填密钥。',form,async()=>{
      if(!form.reportValidity())throw new Error('请补全必要字段。');
      const data=Object.fromEntries(new FormData(form));data.type=Number(data.type);data.priority=Number(data.priority);data.weight=Number(data.weight);data.key=data.key||null;
      const action=channel?{kind:'update',id:channel.id,revision:channel.revision,fields:data}:{kind:'create',fields:data};
      await hooks.api('/api/aggregate/channel-command',{request_id,confirmed:true,action});form.elements.key.value='';await loadChannels(page);
    },undefined,'保存渠道');
  }
  async function loadUsage(){
    const token=generation;const results=await Promise.allSettled([hooks.api('/api/aggregate/usage'),hooks.api('/api/aggregate/audit')]);if(!valid(token))return;
    const root=get('aggregate-usage');root.replaceChildren();root.append(el('h4','内部出口用量与近期操作'));
    if(results[0].status==='fulfilled'){const value=results[0].value;root.append(el('p','已用：'+(value.total_used??'未提供')+' · 剩余：'+(value.unlimited_quota?'无限制（请检查配置）':value.total_available??'未提供')+'（New API 额度单位）','workspace-path'),el('p','此处是网关计量，不是上游真实账单；未提供的数据不作估算。','subtle'));}else root.append(el('p','用量未载入：'+results[0].reason.message,'load-error'));
    if(results[1].status==='fulfilled')for(const record of results[1].value)root.append(el('p',new Date(record.created_at*1000).toLocaleString()+' · '+record.kind+' · '+record.target+' · '+record.state+' · '+record.id,'workspace-path'));
    else root.append(el('p','操作记录未载入：'+results[1].reason.message,'load-error'));
  }
  return {load,clear,sessionModel};
})();
