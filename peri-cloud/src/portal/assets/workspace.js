'use strict';

// Account-scoped projections. Rendering never creates sessions or changes policy.
window.WorkspaceView = (() => {
  const get = id => document.getElementById(id);
  const node = (tag, className, text = '') => {
    const result = document.createElement(tag); result.className = className;
    result.textContent = text; return result;
  };
  let account = null;
  let hooks = null;
  let generation = 0;
  let focusedSession = null;
  let deviceChecks = new Map();
  let scheduleSessions = [];
  const gatewayName = id => (account.gateways || []).find(g => g.adapter === id)?.label || '聊天入口';
  const command = (label, task, className = 'quiet small') => {
    const button = node('button', className, label); button.type = 'button';
    button.addEventListener('click', () => task(button)); return button;
  };
  function clear() {
    scheduleSessions = []; get('schedule-list').replaceChildren(); get('schedule-create').onclick = null; get('schedule-policy').onclick = null;
    generation += 1; account = null; focusedSession=null; deviceChecks.clear();
    for (const id of ['devices','channels','claims','conversation-activity','interactions','topology-entries','topology-devices']) get(id).replaceChildren();
    for (const id of ['device-count','channel-count','session-count','pending-count']) get(id).textContent = '0';
    for (const id of ['service-name','service-origin','service-version','topology-model','greeting','usage-summary','model-current']) get(id).textContent = '';
    for (const id of ['admin-panel','admin-nav','model-panel','model-nav','settings-error']) get(id).hidden = true;
    get('settings-error').textContent = '';
  }
  function render(data, handlers) {
    if (account && account.principal.id !== data.principal.id) clear();
    const devicesChanged = !account || JSON.stringify([account.devices,account.device_management,account.device_connections]) !== JSON.stringify([data.devices,data.device_management,data.device_connections]);
    account = data; hooks = handlers;
    get('service-name').textContent = data.service?.name || '云端主控';
    get('service-origin').textContent = data.service?.origin || window.location.origin;
    get('service-version').textContent = data.service?.version ? '主控版本 ' + data.service.version : '';
    const entries = get('topology-entries'); entries.replaceChildren();
    for (const gateway of data.gateways || []) {
      const row = node('div', 'topology-entry'); row.append(node('strong','',gateway.label), node('span','subtle',gateway.adapter === '__peri_web' ? '使用当前网页登录身份' : gateway.linked_identities ? gateway.linked_identities + ' 个已关联身份' : '已配置 · 你尚未关联'));
      const states={ready:'网关会话已就绪',disconnected:'连接未就绪',bridge_authenticated:'桥接连接已认证',browser_authenticated:'使用当前网页登录',unobserved:'尚无连接观测'};
      row.append(node('span','subtle',states[gateway.connection_state]||'尚无连接观测'));
      if(gateway.observed_at)row.append(node('span','subtle','最近就绪：'+new Date(gateway.observed_at*1000).toLocaleString()));
      entries.append(row);
    }
    if (!entries.children.length) entries.append(node('p','subtle','尚未关联聊天入口'));
    const devices = get('topology-devices'); devices.replaceChildren();
    for (const device of data.devices.filter(d => !d.revoked)) devices.append(node('div','topology-entry',device.name));
    if (!devices.children.length) devices.append(node('p','subtle','尚未登记执行设备'));
    get('topology-model').textContent ||= '模型在主控调用';
    if (devicesChanged) renderDevices();
  }
  function renameDevice(device) {
    const root = node('div', '');
    const label = node('label', '', '设备名称');
    const input = document.createElement('input');
    input.value = device.name; input.maxLength = 128; input.required = true;
    label.append(input); root.append(label);
    hooks.confirm('重命名设备', '云端名称用于聊天中的设备选择；执行器本地名称不会覆盖它。', root, async () => {
      const name = input.value.trim();
      if (!name) throw new Error('请输入设备名称。');
      await hooks.api('/api/devices/' + device.id + '/rename', { name });
    }, undefined, '保存');
  }

  function deleteDevice(device) {
    hooks.confirm('删除设备记录？', '删除后这台设备不再出现在列表中；已保存的对话结果仍保留。设备上的执行器需要重新登录才能重新接入。', device.name,
      () => hooks.api('/api/devices/' + device.id + '/delete', {}), undefined, '确认删除', '取消', true);
  }

  function renderDevices() {
    const root = get('devices');
    const expanded = new Set([...root.querySelectorAll('details[open]')].map(detail => detail.parentElement.dataset.device + ':' + detail.className));
    root.replaceChildren();
    if (!account.devices.length) root.append(node('p','empty','还没有登记执行设备。'));
    for (const device of account.devices) {
      const card = node('article','device-card'); card.dataset.device = device.id;
      const heading = node('div','section-heading'); heading.append(node('h3','',device.name), node('span','pill',device.revoked ? '已撤销' : '已登记'));
      const platform = device.platform === 'windows' ? 'Windows' : device.platform === 'linux' ? 'Linux' : device.platform;
      card.append(heading,node('p','subtle',platform + ' · ' + (device.is_local ? '管理员提供的执行器' : '我的执行器')));
      const status = node('p','connection-status'); status.setAttribute('role','status');
      const checked = deviceChecks.get(device.id);
      const reach = account.device_connections?.[device.id];
      if (device.revoked) status.textContent = '访问已撤销';
      else if (checked?.pending) status.textContent = '正在验证连接…';
      else if (checked) {
        const time = new Date(checked.time).toLocaleTimeString('zh-CN', {hour12:false});
        status.textContent = checked.ok ? '已验证连接 · ' + time : '无法验证连接 · ' + time;
        status.dataset.verified = String(checked.ok);
        const routes = {wireguard:'WireGuard',loopback:'回环 / SSH'};
        card.append(node('p','subtle',checked.ok ? '本次通路：' + (routes[checked.transport] || '已认证通路') : checked.error));
      } else if (reach?.state === 'online') status.textContent = '最近通路探测通过 · ' + (reach.observed_at ? new Date(reach.observed_at*1000).toLocaleTimeString() : '时间未提供');
      else if (reach?.state === 'offline') status.textContent = '暂时无法连接';
      else if (reach?.state === 'not_configured') status.textContent = '未接入云端连接';
      else status.textContent = '连接待检查';
      const actions = node('div','device-actions');
      if (!device.revoked) {
        actions.append(command('检查连接', button => hooks.action(button, async () => {
          const owner = account.principal.id;
          const started = generation;
          deviceChecks.set(device.id, {pending:true});
          try {
            const result = await hooks.api('/api/instances/' + device.id + '/inspect', {});
            if (!account || generation !== started || account.principal.id !== owner) return;
            if (result.device_id !== device.id) throw new Error('设备身份不一致，请重新检查。');
            const management=account.device_management?.[device.id];
            if(management){management.check=result.check;management.recommendation=null;}
            deviceChecks.set(device.id,{ok:result.check.authenticated,time:result.check.observed_at*1000,transport:result.check.details?.transport,error:'认证或工具目录检查未通过，请检查执行器连接。'});
          } catch (error) {
            if (!account || generation !== started || account.principal.id !== owner) return;
            const management=account.device_management?.[device.id];
            if(management){management.recommendation=null;if(management.check)management.check.fresh=false;}
            deviceChecks.set(device.id,{ok:false,time:Date.now(),error:error.message});
          }
          renderDevices();
        })));
        actions.append(command('新建网页工作区',button=>hooks.action(button,()=>hooks.openBrowserWorkspace(device))));
        actions.append(command('浏览 / 默认目录', () => hooks.openInstanceWorkspace(device)));
        if (!device.is_local) {
          actions.append(command('重命名', () => renameDevice(device)));
          actions.append(command('撤销', () => hooks.confirm('撤销设备连接？','撤销后需要重新登录才能继续使用。',device.name, () => hooks.api('/api/devices/' + device.id + '/revoke', {})), 'quiet danger small'));
        }
      }
      if (device.revoked && !device.is_local) actions.append(command('删除记录', () => deleteDevice(device), 'quiet danger small'));
      if (actions.firstElementChild) actions.firstElementChild.disabled = checked?.pending === true;
      const defaults = node('details','device-defaults'); defaults.append(node('summary','','新会话默认目录'),node('div','workspace-path',device.default_workspace));
      const identity = node('details','device-identity'); identity.append(node('summary','','设备标识'),node('div','workspace-path',device.id));
      for (const detail of [defaults,identity]) detail.open = expanded.has(device.id + ':' + detail.className);
      card.append(status,actions,defaults,identity); root.append(card);
      if (!device.revoked) renderManagement(card,device,account.device_management?.[device.id],expanded);
    }
  }
  function bookmarkForm(device, initial = null) {
    const data=account.device_management?.[device.id];
    const picker=hooks.directoryPicker(device,initial,data?.known_workspaces || []);
    const root=node('div',''); const label=node('label','','书签名称');
    const input=document.createElement('input');input.maxLength=32;input.required=true;
    input.value=(initial || device.default_workspace).split(/[\\/]/).filter(Boolean).pop()?.slice(0,32) || '常用工作区';
    label.append(input);root.append(label,picker.root);
    hooks.confirm('保存工作区书签','保存前由这台设备确认目录。书签是快捷入口，使用时仍会重新验证。',root,()=>hooks.api('/api/instances/'+device.id+'/bookmarks',{
      action:'save',workspace:picker.value() || device.default_workspace,label:input.value.trim()
    }),undefined,'保存书签');picker.load();
  }
  function renameBookmark(device, bookmark) {
    const root=node('label','','书签名称');const input=document.createElement('input');input.maxLength=32;input.required=true;input.value=bookmark.label;root.append(input);
    hooks.confirm('重命名书签',device.name,root,()=>hooks.api('/api/instances/'+device.id+'/bookmarks',{
      action:'rename',id:bookmark.id,revision:bookmark.revision,label:input.value.trim()
    }),undefined,'保存名称');
  }
  function renderManagement(card,device,data,expanded) {
    const root=node('section','device-management');root.dataset.device=device.id;root.setAttribute('aria-label','设备状态与工作区');
    if (!data) {root.append(node('p','subtle','设备能力与占用状态未载入。'));card.append(root);return;}
    const lease=data.lease || {};
    const sources={reported:'设备上报的连接租约有效',expired:'设备上报的连接租约已过期',configured:'已配置连接',none:'尚未配置连接'};
    root.append(node('p','subtle',sources[lease.source] || '连接租约尚无观测'));
    if(lease.reported_at)root.append(node('p','subtle','最近上报：'+new Date(lease.reported_at*1000).toLocaleString()+' · 租约截止：'+new Date(lease.expires_at*1000).toLocaleString()));
    const check=data.check;
    if(check) {
      const detail=check.details;
      root.append(node('p','connection-status',(check.authenticated ? '身份检查通过' : '身份或能力检查失败')+' · '+new Date(check.observed_at*1000).toLocaleString()+(check.fresh ? '' : ' · 历史观测，请重新检查')));
      if(detail) {
        if(detail.version)root.append(node('p','subtle','执行器 '+detail.version+' · 协议 '+detail.protocol_version));
        root.append(node('p','subtle',detail.tools ? '工具目录：'+detail.tools.join('、') : '执行器未提供设备工具目录，工作区绑定时再核验。'));
        if(detail.features?.length){const declared=node('details','device-capabilities');declared.append(node('summary','','协议能力标识'),node('p','workspace-path',detail.features.join('、')));root.append(declared);}
      }
    } else root.append(node('p','subtle','尚无能力检查。点击「检查连接」读取身份与工具目录，不创建任务。'));
    root.append(node('p','subtle','目录声明不等于工作区可执行；工具权限、目录和资源占用在提交时重新核验。'));
    const work=data.work || {};const states=work.task_states || {};
    const labels={queued:'等待调度',running:'执行中',waiting_capacity:'等待执行额度或设备资源',waiting_parent:'等待主助手答复',waiting_approval:'等待审批',waiting_device:'等待设备',waiting_external:'等待外部条件',waiting_operations:'等待原操作结算',recovery_required:'需要核对原执行结果',paused:'已暂停',pause_requested:'正在暂停',cancel_requested:'正在取消',rotate_requested:'正在交接窗口',resume_requested:'等待恢复',window_ready:'新窗口就绪，等待继续'};
    if(work.mutation_lease)root.append(node('p','subtle',work.mutation_lease==='reserved' ? '设备修改资源已由执行任务预占。' : '设备修改资源正在使用。'));
    if(work.mutation_owner_session){const owner=node('details','device-resource-owner');owner.append(node('summary','','占用会话'),node('div','workspace-path',work.mutation_owner_session));root.append(owner);}
    if(work.unconfirmed_operations)root.append(node('p','load-error',work.unconfirmed_operations+' 项原操作结果待确认。先回原会话核对，不迁移或重复提交。'));
    for(const [state,count] of Object.entries(states))root.append(node('p','subtle',(labels[state] || '任务状态：'+state)+' · '+count+' 项'));
    if(!work.mutation_lease && !work.unconfirmed_operations)root.append(node('p','subtle','当前未观察到设备修改占用。'));
    if(data.observed_at)root.append(node('p','subtle','占用观测：'+new Date(data.observed_at*1000).toLocaleString()));
    if(data.recommendation)root.append(node('p','pill','新任务候选：近期认证与目录检查通过，当前未占用修改资源。'));
    root.append(node('p','subtle','建议仅用于新任务；已提交操作仍由原设备核对和结算。'));
    const bookmarks=node('details','device-bookmarks');bookmarks.open=expanded.has(device.id+':device-bookmarks');bookmarks.append(node('summary','','工作区书签'));
    bookmarks.append(command('添加书签',()=>bookmarkForm(device)));
    for(const bookmark of data.bookmarks || []) {
      const row=node('div','workspace-bookmark');row.append(node('strong','',bookmark.label),node('div','workspace-path',bookmark.workspace));
      const actions=node('div','device-actions');actions.append(
        command('打开独立工作区',()=>hooks.openBrowserWorkspace(device,bookmark.workspace)),
        command('改名',()=>renameBookmark(device,bookmark)),
        command('移除',()=>hooks.confirm('移除工作区书签','只移除快捷入口，目录、会话与任务保留。',bookmark.label,()=>hooks.api('/api/instances/'+device.id+'/bookmarks',{action:'remove',id:bookmark.id,revision:bookmark.revision})), 'quiet danger small')
      );row.append(actions);bookmarks.append(row);
    }
    if(!data.bookmarks?.length)bookmarks.append(node('p','subtle','暂无工作区书签。'));
    const recent=node('details','device-recents');recent.open=expanded.has(device.id+':device-recents');recent.append(node('summary','','最近使用的目录'));
    for(const path of data.known_workspaces || []) {const row=node('div','');row.append(node('div','workspace-path',path),command('存为书签',()=>bookmarkForm(device,path)));recent.append(row);}
    if(!data.known_workspaces?.length)recent.append(node('p','subtle','暂无已确认绑定的历史目录。'));
    root.append(bookmarks,recent);card.append(root);
  }
  function failure(root, title, response) {
    if (response.status === 'rejected') root.append(node('p','load-error',title + '未载入：' + response.reason.message));
  }
  function sessions(workspacesResult, activitiesResult, interactionsResult) {
    const workspaces = workspacesResult.status === 'fulfilled' ? workspacesResult.value : [];
    const activities = activitiesResult.status === 'fulfilled' ? activitiesResult.value : [];
    scheduleSessions = [...activities, ...workspaces.filter(w=>!activities.some(a=>a.session_id===w.session_id))];
    const interactions = interactionsResult.status === 'fulfilled' ? interactionsResult.value : [];
    const root = get('conversation-activity'); root.replaceChildren();
    failure(root,'会话状态',activitiesResult); failure(root,'工作区操作',workspacesResult); failure(root,'待办',interactionsResult);
    get('pending-count').textContent = interactionsResult.status === 'fulfilled' ? interactions.length : '?';
    const ids = [...new Set([...activities.map(s=>s.session_id),...workspaces.map(s=>s.session_id)])];
    get('session-count').textContent = activitiesResult.status === 'fulfilled' ? activities.length : '—';
    for (const id of ids) {
      const activity = activities.find(s=>s.session_id===id);
      const selection = workspaces.find(s=>s.session_id===id);
      const info = activity || selection;
      const card = node('article','conversation-card'); card.dataset.session = id;
      card.dataset.phase = activity?.phase || 'unknown';
      const header = node('div','section-heading');
      header.append(node('h3','',gatewayName(info.adapter)),node('span','phase-label',activity?.label || '状态暂不可用'));
      card.append(header);
      const source=node('details','conversation-source');source.append(node('summary','',info.adapter==='__peri_web'?'网页独立工作区':'入口与会话标识'),node('p','workspace-path',info.adapter+' · '+info.conversation_id+' · '+id));card.append(source);
      const binding = node('dl','session-binding');
      for (const [label,value] of [['执行设备',info.device_name],['工作区',info.workspace],['工具权限',activity?.permission || '未载入']]) binding.append(node('dt','',label),node('dd','',value));
      if(activity?.next_model)binding.append(node('dt','','后续任务模型'),node('dd','',activity.next_model));
      if(activity?.turn_id)binding.append(node('dt','','本轮选择模型'),node('dd','',activity.turn_model||'未记录（旧任务或尚未开始）'));
      card.append(binding);
      const actions = node('div','device-actions');
      const submit=command('配置并提交任务',()=>hooks.openTaskForm(info));
      submit.disabled=activity?.phase!=='idle'||selection?.busy===true;
      actions.append(submit);
      if(activity?.turn_id && ['running','waiting_approval','waiting_answer','cancelling','needs_attention'].includes(activity.phase)){
        const cancel=command('请求取消任务',()=>hooks.cancelTask(activity));cancel.disabled=activity.phase==='cancelling';actions.append(cancel);
      }
      if (selection) {
        const change = command('切换这条会话的工作区', () => hooks.openWorkspace(selection));
        change.disabled = selection.busy || ['running','waiting_approval','waiting_answer','cancelling','needs_attention'].includes(activity?.phase);
        actions.append(change);
      }
      if (account.service?.model_gateway) actions.append(command('选择这条会话的模型',button=>hooks.action(button,()=>hooks.openModelSelection(info))));
      if (activity?.has_result) actions.append(command('查看已保存结果', button => hooks.action(button, () => hooks.openSavedResult(id))));
      card.append(actions);
      for (const interaction of interactions.filter(i=>i.session_id===id)) card.append(interactionItem(interaction));
      root.append(card);
    }
    if (!ids.length && activitiesResult.status === 'fulfilled' && workspacesResult.status === 'fulfilled') root.append(node('p','empty','还没有会话。到“执行设备”打开网页工作区，或在 QQ 发送“菜单”选择设备。'));
    const requested=new URLSearchParams(window.location.search).get('session');
    const selected=[...root.querySelectorAll('[data-session]')].find(card=>card.dataset.session===requested);
    if(selected){selected.classList.add('selected-session');if(focusedSession!==requested){selected.scrollIntoView?.({block:'start'});focusedSession=requested;}}
    const orphans = interactions.filter(i=>!ids.includes(i.session_id));
    const urgent = get('interactions'); urgent.replaceChildren(...orphans.map(interactionItem));
    get('approval-panel').hidden = orphans.length === 0;
  }
  function interactionItem(card) {
    const row = node('div','session-interaction');
    const count = card.context.items?.length || 0;
    row.append(node('span','',card.context.kind === 'Approval' ? '等待确认 ' + count + ' 项操作' : '等待你的回答'),command('查看并处理', () => hooks.openInteraction(card)));
    return row;
  }
  function schedules(result) {
    const root=get('schedule-list'); root.replaceChildren();
    get('schedule-create').onclick=()=>createSchedule();
    get('schedule-policy').onclick=()=>contactPolicy();
    get('schedule-create').disabled=!scheduleSessions.length;
    if (result.status==='rejected') { failure(root,'定时任务',result); return; }
    const owner=account?.principal.id;
    for (const task of result.value) {
      const card=node('article','conversation-card');
      card.append(node('h3','',task.title),node('p','subtle',task.enabled?'已启用':'暂停或已结束'));
      const timing=task.timing.kind==='once'?task.timing.at:task.timing.kind==='cron'?task.timing.expression+' · '+task.timing.timezone:task.timing.kind==='interval'?'固定间隔 '+task.timing.every_seconds+' 秒':'延迟 '+task.timing.seconds+' 秒';
      card.append(node('p','',timing),node('p','subtle','下次：'+(task.next_fire?new Date(task.next_fire*1000).toLocaleString():'无')),
        node('p','subtle',(task.mode==='conversation'?'主动聊天 · 无需执行器在线':'设备任务 · 沿用会话审批')+' · '+(task.notification==='push'?'可主动通知':'工作台收件箱，此入口不支持主动推送')));
      const details=node('details',''); details.append(node('summary','','完整任务与目标'),node('pre','workspace-path',task.prompt+'\n绑定 '+(task.binding==='chat'?'跟随此聊天的当前会话':task.binding==='device'?'冻结设备会话':'旧版冻结会话')+'\n创建会话 '+task.session_id+'\n设备 '+(task.device_id||'无设备约束')+'\n目录 '+(task.workspace||'无')+'\n任务 '+task.id+' · 版本 '+task.revision)); card.append(details);
      const actions=node('div','device-actions');
      actions.append(command('编辑内容与时间',()=>createSchedule(task)));
      for (const [label,operation] of [[task.enabled?'暂停':'恢复',task.enabled?'pause':'resume'],['删除','delete']]) {
        actions.append(command(label,()=>hooks.confirm(label+'定时任务？','后续触发将按这次修改生效；已开始的任务仍需单独取消。',task.title+'\n'+task.prompt+'\n'+task.session_id,()=>hooks.api('/api/schedules/'+task.id+'/actions',{confirmed:true,revision:task.revision,action:operation}))));
      }
      actions.append(command('触发与送达记录',button=>hooks.action(button,async()=>{
        const rows=await hooks.api('/api/schedules/'+task.id+'/runs');
        if(account?.principal.id!==owner)return;
        const content=node('div','');
        for(const run of rows){
          const row=node('div','item');row.append(node('p','',new Date(run.scheduled_at*1000).toLocaleString()+' · 执行 '+run.state+' · 投递 '+run.delivery_state),node('p','subtle',run.reason||''));
          if(run.turn_id)row.append(command('查看本次结果',()=>hooks.openSavedResult(run.session_id||task.session_id,1,run.turn_id)));
          content.append(row);
        }
        if(!rows.length)content.append(node('p','','尚未触发。'));
        hooks.confirm('触发记录',task.title,content,async()=>{},undefined,'关闭');
      })));
      card.append(actions);root.append(card);
    }
    if(!result.value.length)root.append(node('p','empty','还没有定时任务。也可以在个人 QQ 中说“每天下午两点来问候我”。'));
  }
  function createSchedule(task=null) {
    const root=node('div','');
    const field=(label,tag,value)=>{const wrap=node('label','',label);const input=document.createElement(tag);input.value=value??'';wrap.append(input);root.append(wrap);return input;};
    const target=field('聊天目标（设备任务冻结当前选择）','select');
    for(const session of scheduleSessions){const option=node('option','',gatewayName(session.adapter)+' · '+session.device_name+' · '+(session.workspace||'云端对话'));option.value=session.session_id;target.append(option);}
    if(task){if(![...target.options].some(o=>o.value===task.session_id)){const option=node('option','','原授权目标 '+task.session_id);option.value=task.session_id;target.append(option);}target.value=task.session_id;target.disabled=true;}
    const title=field('名称','input',task?.title||'下午的问候');title.maxLength=128;
    const prompt=field('希望助手做什么','textarea',task?.prompt||'自然地向我说下午好，关心一下我今天的心情。简短、可爱一点，问一个轻松的问题。');prompt.rows=4;prompt.maxLength=8192;
    const kind=field('时间安排','select');for(const [value,label] of [['cron','周期 cron'],['once','仅一次'],['after','从现在延迟'],['interval','固定间隔']]){const option=node('option','',label);option.value=value;kind.append(option);}kind.value=task?.timing.kind||'cron';
    const time=field('五段 cron / RFC3339 时间 / 延迟或间隔秒数','input',task?.timing.kind==='once'?task.timing.at:task?.timing.kind==='interval'?task.timing.every_seconds:task?.timing.kind==='after'?task.timing.seconds:task?.timing.expression||'0 14 * * *');
    const timezone=field('cron 使用 IANA 时区；固定间隔填写 UTC 锚点（RFC3339）','input',task?.timing.kind==='interval'?new Date(task.timing.anchor*1000).toISOString():task?.timing.timezone||Intl.DateTimeFormat().resolvedOptions().timeZone||'Asia/Shanghai');
    kind.onchange=()=>{if(kind.value==='after')time.value='3600';if(kind.value==='interval'){time.value='3600';timezone.value=new Date(Date.now()+60000).toISOString();}if(kind.value==='once')time.value=new Date(Date.now()+3600000).toISOString();if(kind.value==='cron'){time.value='0 14 * * *';timezone.value=Intl.DateTimeFormat().resolvedOptions().timeZone||'Asia/Shanghai';}};
    const mode=field('任务类型','select');for(const [value,label] of [['conversation','主动聊天（无需电脑在线）'],['task','操作设备（沿用工具审批）']]){const option=node('option','',label);option.value=value;mode.append(option);}mode.value=task?.mode||'conversation';mode.disabled=!!task;
    const notify=field('通知方式','select');for(const [value,label] of [['always','按明确约定发送'],['when_useful','在联系窗口内，由助手判断是否需要发送']]){const option=node('option','',label);option.value=value;notify.append(option);}notify.value=task?.notify||'always';
    const enabled=task?field('保存后启用','input'):null;if(enabled){enabled.type='checkbox';enabled.checked=task.enabled;}
    const request_id=crypto.randomUUID();
    hooks.confirm(task?'编辑定时任务':'安排主动聊天或任务','主动聊天跟随原聊天；设备任务保留冻结目标。明确约定的时间不受自动关怀窗口限制，关闭主动联系会阻止聊天问候。',root,async()=>{
      if(!title.value.trim()||!prompt.value.trim())throw new Error('请填写名称和任务内容。');
      let timing;
      if(kind.value==='once')timing={kind:'once',at:time.value.trim()};
      else if(kind.value==='after')timing={kind:'after',seconds:Number(time.value)};
      else if(kind.value==='interval'){const anchorText=timezone.value.trim();const anchor=Date.parse(anchorText);if(!/[tT].*(?:[zZ]|[+-]\d{2}:\d{2})$/.test(anchorText)||!Number.isFinite(anchor))throw new Error('请填写带时区的固定间隔锚点。');timing={kind:'interval',anchor:Math.floor(anchor/1000),every_seconds:Number(time.value)};}
      else timing={kind:'cron',expression:time.value.trim(),timezone:timezone.value.trim()};
      const content={request_id,title:title.value.trim(),prompt:prompt.value.trim(),timing,notify:notify.value};
      if(task)await hooks.api('/api/schedules/'+task.id+'/edit',{confirmed:true,edit:{...content,revision:task.revision,enabled:enabled.checked}});
      else await hooks.api('/api/schedules',{confirmed:true,schedule:{...content,session_id:target.value,mode:mode.value}});
    },undefined,task?'保存修改':'创建任务');
  }
  async function contactPolicy() {
    const owner=account?.principal.id;
    const policy=await hooks.api('/api/schedules/contact-policy');
    if(account?.principal.id!==owner)return;
    const root=node('div','');
    const field=(label,type,value)=>{const wrap=node('label','',label);const input=document.createElement('input');input.type=type;if(type==='checkbox')input.checked=value;else input.value=value;wrap.append(input);root.append(wrap);return input;};
    const enabled=field('允许主动聊天','checkbox',policy.enabled);
    const timezone=field('自动关怀窗口的 IANA 时区','text',policy.timezone);
    const start=field('窗口开始小时（0–23）','number',policy.start_hour);start.min=0;start.max=23;
    const end=field('窗口结束小时（1–24，可跨夜）','number',policy.end_hour);end.min=1;end.max=24;
    const limit=field('每日后台任务上限（0–100）','number',policy.daily_limit);limit.min=0;limit.max=100;
    hooks.confirm('主动联系设置','窗口用于“由助手判断”的关怀；你明确约定的问候按约定时间发送。关闭主动聊天后，未发送的问候也会重新检查并被抑制。',root,()=>hooks.api('/api/schedules/contact-policy',{confirmed:true,policy:{...policy,enabled:enabled.checked,timezone:timezone.value.trim(),start_hour:Number(start.value),end_hour:Number(end.value),daily_limit:Number(limit.value)}}),undefined,'保存设置');
  }
  return {render,sessions,schedules,clear};
})();
