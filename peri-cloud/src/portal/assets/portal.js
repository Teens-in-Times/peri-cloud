'use strict';

const byId = id => document.getElementById(id);
const views = ['loading', 'entry', 'consent', 'account'];
let csrfCookie = 'peri_csrf';
let activeAccount = null;
let refreshInFlight = false;
let pendingDecision = null;
let authorization = null;
let visibleView = 'loading';
let canStopService = false;
const errors = {
  unauthorized: '登录信息无效或已过期，请重新登录。',
  forbidden: '当前账户无法完成这项操作。',
  origin_rejected: '访问地址与云服务配置不一致，请使用正确的登录地址。',
  identity_conflict: '账户已初始化，或这台设备已经属于其他账户。',
  stale_request: '这个请求已过期或已经处理，请刷新后查看。',
  rate_limited: '操作过于频繁，请稍后再试。',
  invalid_request: '请检查填写的信息。',
  identity_unavailable: '云服务暂时无法处理请求，请稍后再试。'
};

function show(view) {
  for (const id of views) byId(id).hidden = id !== view;
  byId('workspace-nav').hidden = view !== 'account';
  document.body.classList.toggle('workspace-mode', view === 'account');
  if (view !== visibleView) { window.scrollTo(0, 0); visibleView = view; }
}
function notice(message) {
  for (const id of ['notice', 'confirmation-error']) {
    byId(id).textContent = message; byId(id).hidden = !message;
  }
}
function csrf() {
  const pair = document.cookie.split(';').map(part => part.trim()).find(part => part.startsWith(csrfCookie + '='));
  return pair ? pair.slice(csrfCookie.length + 1) : '';
}

async function api(path, body) {
  const options = { credentials: 'same-origin', cache: 'no-store', headers: {} };
  if (body !== undefined) {
    options.method = 'POST';
    options.headers['Content-Type'] = 'application/json';
    options.headers['X-Peri-CSRF'] = csrf();
    options.body = JSON.stringify(body);
  }
  let response;
  try { response = await fetch(path, options); }
  catch { throw new Error('暂时无法连接云服务，请检查网络。'); }
  const result = await response.json().catch(() => ({}));
  if (!response.ok) {
    if (response.status === 401 && !['/api/login', '/api/setup'].includes(path)) {
      activeAccount = null;
      byId('logout').hidden = true;
      show('entry');
    }
    throw new Error(errors[result.error] || '请求未完成，请稍后再试。');
  }
  return result;
}

async function action(button, task) {
  if (button.disabled) return;
  button.disabled = true;
  notice('');
  try { await task(); }
  catch (error) { notice(error.message); }
  finally { button.disabled = false; }
}

function item(title, detail, buttonText, callback, danger = false) {
  const row = document.createElement('div'); row.className = 'item';
  const avatar = document.createElement('span'); avatar.className = 'item-avatar';
  avatar.textContent = Array.from(title)[0] || '·'; avatar.setAttribute('aria-hidden', 'true');
  const content = document.createElement('div'); content.className = 'item-content';
  const heading = document.createElement('div'); heading.className = 'item-title';
  const name = document.createElement('span'); name.textContent = title; heading.append(name);
  if (buttonText) {
    const button = document.createElement('button'); button.type = 'button';
    button.className = 'quiet small' + (danger ? ' danger' : '');
    button.textContent = buttonText; button.addEventListener('click', () => callback(button)); heading.append(button);
  }
  const info = document.createElement('div'); info.className = 'item-info'; info.textContent = detail;
  content.append(heading, info); row.append(avatar, content); return row;
}

function list(id, rows, empty, render) {
  const root = byId(id); root.replaceChildren();
  if (!rows.length) { const element = document.createElement('div'); element.className = 'empty'; element.textContent = empty; root.append(element); }
  else for (const row of rows) root.append(render(row));
}

function confirm(title, text, details, onAllow, onDeny, allowLabel = '确认', denyLabel = '取消', refreshAfter = true) {
  byId('confirmation-title').textContent = title;
  byId('confirmation-text').textContent = text;
  const body = byId('confirmation-details'); body.replaceChildren();
  if (details instanceof Node) body.append(details); else body.textContent = details;
  byId('confirmation-allow').textContent = allowLabel;
  byId('confirmation-deny').textContent = denyLabel;
  pendingDecision = { onAllow, onDeny, refreshAfter };
  byId('confirmation').showModal();
}

for (const [id, field] of [['confirmation-allow', 'onAllow'], ['confirmation-deny', 'onDeny']]) {
  byId(id).addEventListener('click', () => action(byId(id), async () => {
    const decision = pendingDecision;
    const task = decision && decision[field];
    if (task) await task();
    byId('confirmation').close(); pendingDecision = null;
    if (!decision || decision.refreshAfter) await refresh();
  }));
}
byId('confirmation').addEventListener('cancel', () => { pendingDecision = null; });
for (const link of byId('workspace-nav').querySelectorAll('a')) {
  link.addEventListener('click', () => {
    for (const sibling of byId('workspace-nav').querySelectorAll('a')) sibling.classList.toggle('active', sibling === link);
    if (link.id === 'service-nav') byId('service-controls').open = true;
  });
}

function renderAccount(data) {
  activeAccount = data;
  byId('logout').hidden = false;
  byId('service-controls').hidden = !canStopService;
  byId('service-nav').hidden = !canStopService;
  if (authorization) {
    byId('consent-device').textContent = authorization.device_name;
    byId('consent-owner').textContent = data.principal.display_name;
    show('consent'); return;
  }
  byId('greeting').textContent = data.principal.display_name + '，你的工作台';
  byId('device-count').textContent = data.devices.filter(device => !device.revoked).length;
  byId('channel-count').textContent = data.channels.length;
  byId('pair-title').textContent = data.channels.length ? '让对话继续。' : '把聊天，接到工作台。';
  byId('pair-description').textContent = data.channels.length ? '聊天已经连接。你也可以关联其他账号，共用这个工作台。' : '关联你的 QQ 账号。之后，把想做的事直接告诉 Agent。';
  byId('pair-create').textContent = data.channels.length ? '关联其他账号' : '生成关联口令';
  list('devices', data.devices, '还没有关联的执行设备。', device => item(
    device.name + (device.revoked ? ' · 已撤销' : ''),
    (device.platform === 'windows' ? 'Windows' : 'Linux') + ' · ' + device.default_workspace,
    device.revoked ? '' : '撤销', () => confirm('撤销设备连接？', '撤销后，该执行器需要重新登录。', device.name, () => api('/api/devices/' + device.id + '/revoke', {})), true
  ));
  list('channels', data.channels, '还没有关联的聊天账号。', channel => item(
    channel.adapter_instance_id, shortIdentity(channel.external_user_id), '移除',
    () => confirm('移除聊天账号？', '这个聊天账号将无法再使用你的 Agent。电脑登录仍然保留。', channel.adapter_instance_id + '\n' + channel.external_user_id, () => api('/api/channels/revoke', channel)), true
  ));
  list('claims', data.pending_pair_claims, '目前没有等待确认的请求。', claim => item(
    '新的聊天账号关联', claim.identity.adapter_instance_id + ' · ' + shortIdentity(claim.identity.external_user_id), '查看并确认',
    () => confirm('确认关联这个聊天账号？', '关联后，这个账号可以选择并使用你的执行设备。请核对是不是你刚刚发送口令的账号。', claim.identity.adapter_instance_id + '\n' + claim.identity.external_user_id,
      () => api('/api/pairing/' + claim.claim_id + '/confirm', { allow: true }),
      () => api('/api/pairing/' + claim.claim_id + '/confirm', { allow: false }), '确认关联', '拒绝关联')
  ));
  show('account');
}

async function refresh() {
  if (refreshInFlight) return;
  refreshInFlight = true;
  try {
    renderAccount(await api('/api/account'));
    const cards = await api('/api/interactions');
    byId('pending-count').textContent = cards.length;
    list('interactions', cards, '目前没有等待审批的操作。', card => item(
      card.context.kind === 'Approval' ? '确认 ' + card.context.items.length + ' 项操作' : 'Agent 需要你的回答',
      card.device_name + ' · ' + card.workspace, '查看', () => openInteraction(card)
    ));
    const requested = new URLSearchParams(window.location.search).get('approval');
    const card = cards.find(card => card.request_id === requested);
    if (card && !byId('confirmation').open && !authorization) {
      window.history.replaceState(null, '', '/'); openInteraction(card);
    }
  }
  finally { refreshInFlight = false; }
}

function shortIdentity(value) {
  const chars = Array.from(value); return chars.length > 18 ? chars.slice(0, 8).join('') + '…' + chars.slice(-6).join('') : value;
}

const toolLabels = { Read: '读取文件', Write: '写入文件', Edit: '编辑文件', Glob: '查找文件', Grep: '搜索内容', Bash: '运行命令' };
function operationDetails(items) {
  const root = document.createElement('div'); root.className = 'approval-operations';
  for (const operation of items) {
    const input = operation.tool_input || {};
    const section = document.createElement('section'); section.className = 'approval-operation';
    const heading = document.createElement('div'); heading.className = 'operation-heading';
    heading.textContent = toolLabels[operation.tool_name] || operation.tool_name;
    const target = document.createElement('div'); target.className = 'operation-target';
    const description = operation.tool_name === 'Bash' ? input.command : input.file_path || input.path || input.pattern;
    target.textContent = typeof description === 'string' ? description : '查看完整参数';
    if (operation.tool_name === 'Write' && typeof input.content === 'string') {
      const size = document.createElement('p'); size.className = 'subtle';
      size.textContent = '写入 ' + Array.from(input.content).length + ' 个字符'; section.append(size);
    }
    const parameters = document.createElement('details'); parameters.className = 'tool-parameters';
    const summary = document.createElement('summary'); summary.textContent = '查看完整参数';
    const full = document.createElement('pre'); full.textContent = JSON.stringify(operation.tool_input, null, 2);
    parameters.append(summary, full); section.prepend(heading, target); section.append(parameters); root.append(section);
  }
  return root;
}

function openInteraction(card) {
  if (card.context.kind !== 'Approval') { notice('这项交互暂不支持网页作答。'); return; }
  const details = operationDetails(card.context.items);
  const respond = kind => api('/api/interactions/' + card.request_id + '/respond', { kind });
  confirm('允许这 ' + card.context.items.length + ' 项操作？', card.device_name + ' · ' + card.workspace, details,
    () => respond('allow_once'), () => respond('reject'), '允许一次', '拒绝');
}

async function signIn(login, password) {
  await api('/api/login', { login, password });
  for (const form of [byId('login-form'), byId('setup-form')]) form.reset();
  await refresh();
}

byId('login-form').addEventListener('submit', event => {
  event.preventDefault(); const form = event.currentTarget;
  const fields = Object.fromEntries(new FormData(form));
  action(form.querySelector('button'), () => signIn(fields.login, fields.password));
});
byId('setup-form').addEventListener('submit', event => {
  event.preventDefault(); const form = event.currentTarget;
  const fields = Object.fromEntries(new FormData(form));
  action(form.querySelector('button'), async () => {
    await api('/api/setup', fields);
    byId('setup-form').hidden = true; byId('login-form').hidden = false;
    byId('entry-title').textContent = '登录你的云 Agent';
    await signIn(fields.login, fields.password);
  });
});
byId('logout').addEventListener('click', () => action(byId('logout'), async () => {
  await api('/api/logout', {}); activeAccount = null;
  byId('pair-result').hidden = true; byId('pair-code').textContent = '';
  byId('logout').hidden = true; show('entry');
}));
byId('refresh').addEventListener('click', () => action(byId('refresh'), refresh));
byId('service-stop').addEventListener('click', () => confirm('关闭云服务？', '将停止接收新消息并结束当前任务。', '', async () => {
  await api('/api/service/shutdown', {}); activeAccount = null;
  byId('service-controls').hidden = true;
  notice('已请求关闭云服务，正在结算任务。');
}, undefined, '确认关闭', '取消', false));
byId('pair-create').addEventListener('click', () => action(byId('pair-create'), async () => {
  const pair = await api('/api/pairing/code', {});
  byId('pair-code').textContent = pair.code;
  byId('pair-expiry').textContent = '5 分钟内有效，只能使用一次。重新生成会替换旧口令。';
  byId('pair-result').hidden = false;
}));
byId('pair-copy').addEventListener('click', () => action(byId('pair-copy'), async () => {
  if (!navigator.clipboard) throw new Error('请选中并复制上方口令。');
  await navigator.clipboard.writeText(byId('pair-code').textContent); notice('口令已复制。');
}));
for (const [id, allow] of [['consent-allow', true], ['consent-deny', false]]) {
  byId(id).addEventListener('click', () => action(byId(id), async () => {
    const result = await api('/oauth/authorize', { authorization, allow });
    window.location.assign(result.redirect_uri);
  }));
}

async function start() {
  const state = await api('/api/status'); csrfCookie = state.csrf_cookie;
  canStopService = state.service_control === true;
  byId('setup-form').hidden = state.initialized;
  byId('login-form').hidden = !state.initialized;
  byId('entry-title').textContent = state.initialized ? '登录你的云 Agent' : '创建你的私人账户';
  if (window.location.pathname === '/oauth/authorize') {
    const params = Object.fromEntries(new URLSearchParams(window.location.search));
    delete params.response_type; authorization = params;
  }
  try { await refresh(); }
  catch (error) { show('entry'); if (!error.message.includes('登录信息')) notice(error.message); }
}
start().catch(error => { show('entry'); notice(error.message); });
setInterval(() => {
  if (activeAccount && !document.hidden) refresh().catch(error => notice(error.message));
}, 5000);
