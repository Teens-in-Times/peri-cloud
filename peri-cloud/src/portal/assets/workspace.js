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
  let deviceChecks = new Map();
  const gatewayName = id => (account.gateways || []).find(g => g.adapter === id)?.label || '聊天入口';
  const command = (label, task, className = 'quiet small') => {
    const button = node('button', className, label); button.type = 'button';
    button.addEventListener('click', () => task(button)); return button;
  };
  function clear() {
    generation += 1; account = null; deviceChecks.clear();
    for (const id of ['devices','channels','claims','conversation-activity','interactions','topology-entries','topology-devices']) get(id).replaceChildren();
    for (const id of ['device-count','channel-count','session-count','pending-count']) get(id).textContent = '0';
    for (const id of ['service-name','service-origin','service-version','topology-model','greeting','usage-summary','model-current']) get(id).textContent = '';
    for (const id of ['admin-panel','admin-nav','model-panel','model-nav','settings-error']) get(id).hidden = true;
    get('settings-error').textContent = '';
  }
  function render(data, handlers) {
    if (account && account.principal.id !== data.principal.id) clear();
    const devicesChanged = !account || JSON.stringify(account.devices) !== JSON.stringify(data.devices);
    account = data; hooks = handlers;
    get('service-name').textContent = data.service?.name || '云端主控';
    get('service-origin').textContent = data.service?.origin || window.location.origin;
    get('service-version').textContent = data.service?.version ? '主控版本 ' + data.service.version : '';
    const entries = get('topology-entries'); entries.replaceChildren();
    for (const gateway of data.gateways || []) {
      const row = node('div', 'topology-entry'); row.append(node('strong','',gateway.label), node('span','subtle',gateway.linked_identities ? gateway.linked_identities + ' 个已关联身份' : '已配置 · 你尚未关联'));
      entries.append(row);
    }
    if (!entries.children.length) entries.append(node('p','subtle','尚未关联聊天入口'));
    const devices = get('topology-devices'); devices.replaceChildren();
    for (const device of data.devices.filter(d => !d.revoked)) devices.append(node('div','topology-entry',device.name));
    if (!devices.children.length) devices.append(node('p','subtle','尚未登记执行设备'));
    get('topology-model').textContent ||= '模型在主控调用';
    if (devicesChanged) renderDevices();
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
      if (device.revoked) status.textContent = '访问已撤销';
      else if (!checked) status.textContent = '连接待检查';
      else if (checked.pending) status.textContent = '正在验证连接…';
      else {
        const time = new Date(checked.time).toLocaleTimeString('zh-CN', {hour12:false});
        status.textContent = checked.ok ? '已验证连接 · ' + time : '无法验证连接 · ' + time;
        status.dataset.verified = String(checked.ok);
        const routes = {wireguard:'WireGuard',loopback:'回环 / SSH'};
        card.append(node('p','subtle',checked.ok ? '本次通路：' + (routes[checked.transport] || '已认证通路') : checked.error));
      }
      const actions = node('div','device-actions');
      if (!device.revoked) {
        actions.append(command('检查连接', button => hooks.action(button, async () => {
          const owner = account.principal.id;
          const started = generation;
          deviceChecks.set(device.id, {pending:true});
          try {
            const result = await hooks.api('/api/instances/' + device.id + '/connection-check', {});
            if (!account || generation !== started || account.principal.id !== owner) return;
            if (result.device_id !== device.id) throw new Error('设备身份不一致，请重新检查。');
            deviceChecks.set(device.id,{ok:true,time:Date.now(),transport:result.transport});
          } catch (error) {
            if (!account || generation !== started || account.principal.id !== owner) return;
            deviceChecks.set(device.id,{ok:false,time:Date.now(),error:error.message});
          }
          renderDevices();
        })));
        actions.append(command('浏览 / 默认目录', () => hooks.openInstanceWorkspace(device)));
        if (!device.is_local) actions.append(command('撤销', () => hooks.confirm('撤销设备连接？','撤销后需要重新登录才能继续使用。',device.name, () => hooks.api('/api/devices/' + device.id + '/revoke', {})), 'quiet danger small'));
      }
      if (actions.firstElementChild) actions.firstElementChild.disabled = checked?.pending === true;
      const defaults = node('details','device-defaults'); defaults.append(node('summary','','新会话默认目录'),node('div','workspace-path',device.default_workspace));
      const identity = node('details','device-identity'); identity.append(node('summary','','设备标识'),node('div','workspace-path',device.id));
      for (const detail of [defaults,identity]) detail.open = expanded.has(device.id + ':' + detail.className);
      card.append(status,actions,defaults,identity); root.append(card);
    }
  }
  function failure(root, title, response) {
    if (response.status === 'rejected') root.append(node('p','load-error',title + '未载入：' + response.reason.message));
  }
  function sessions(workspacesResult, activitiesResult, interactionsResult) {
    const workspaces = workspacesResult.status === 'fulfilled' ? workspacesResult.value : [];
    const activities = activitiesResult.status === 'fulfilled' ? activitiesResult.value : [];
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
      card.append(header,node('p','conversation-source',info.adapter + ' · ' + info.conversation_id));
      const binding = node('dl','session-binding');
      for (const [label,value] of [['执行设备',info.device_name],['工作区',info.workspace],['工具权限',activity?.permission || '未载入']]) binding.append(node('dt','',label),node('dd','',value));
      card.append(binding);
      const actions = node('div','device-actions');
      if (selection) {
        const change = command('切换这条会话的工作区', () => hooks.openWorkspace(selection));
        change.disabled = selection.busy || ['running','waiting_approval','waiting_answer','cancelling','needs_attention'].includes(activity?.phase);
        actions.append(change);
      }
      if (activity?.has_result) actions.append(command('查看已保存结果', button => hooks.action(button, () => hooks.openSavedResult(id))));
      card.append(actions);
      for (const interaction of interactions.filter(i=>i.session_id===id)) card.append(interactionItem(interaction));
      root.append(card);
    }
    if (!ids.length && activitiesResult.status === 'fulfilled' && workspacesResult.status === 'fulfilled') root.append(node('p','empty','还没有选定设备的聊天。在 QQ 发送“菜单”，选择设备后，会话会出现在这里。'));
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
  return {render,sessions,clear};
})();
