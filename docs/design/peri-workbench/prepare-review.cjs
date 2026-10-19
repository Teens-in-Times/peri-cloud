/* Build a review from an explicit user export. Never changes the source export. */
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const input=process.argv[2];if(!input)throw new Error('Usage: node prepare-review.cjs <user-export.json>');
const root=__dirname,context={window:{}};vm.createContext(context);
for(const f of ['data.js','integration.js','layout.js'])vm.runInContext(fs.readFileSync(path.join(root,f),'utf8'),context);
const DATA=context.window.PERI_DATA,Layout=context.window.PeriLayout,copy=v=>JSON.parse(JSON.stringify(v));
const raw=fs.readFileSync(input,'utf8');if(Buffer.byteLength(raw)>2*1024*1024)throw new Error('Export exceeds 2 MiB');
const workspace=JSON.parse(raw);if(workspace.format!=='peri-design-workbench'||workspace.version!==2)throw new Error('Unsupported export');
const source=workspace.boards.find(b=>b.id===workspace.ui.board);if(!source)throw new Error('Active board missing');
let board=copy(source);board.id='review-20261019';board.title='方案整理 · 2026-10-19';board.subtitle='基于你导出的修改：保留多用户、跨入口会话和插话方向。总览看主线，领域与分区展开细节。';
const byId=id=>board.nodes.find(n=>n.id===id),auth=board.nodes.find(n=>n.title==='OAuth');if(!auth)throw new Error('User OAuth addition missing');
const changes=[];
function edit(id,patch,reason){const node=byId(id);if(!node)throw new Error('Missing node '+id);Object.assign(node,patch);if(reason)changes.push(reason);return node;}
function add(id,title,summary,details,lenses,group){const node={id,title,summary,details,lenses,status:'proposal',area:'cloud',kind:'module',integration:'new',baselineRefs:[],source:'ours',x:0,y:0,w:218,h:118};board.nodes.push(node);changes.push('新增 '+title+'：'+group);return node;}
function edge(id,from,to,label,kind='flow'){board.edges.push({id,from,to,label,style:'dashed',kind,baselineEdgeRef:''});}
const originalAuth=auth.details;
edit(auth.id,{title:'身份认证 / OAuth',summary:'登录与账号映射，得到可信 principal',details:'【保留的原稿意图】\n'+originalAuth+'\n\n【职责拆分】\nOAuth/登录解决身份确认；数据隔离由用户空间和每次授权检查执行。QQ 等已关联身份查询现有账号映射，不要求每条消息重新 OAuth。保留设备 OAuth 的人类同意、PKCE 和网页同源/CSRF。',area:'cloud',status:'proposal',integration:'extend',baselineRefs:['identity'],source:'arch',lenses:['identity','chat','overview','baseline','additions']},'把 OAuth 的身份确认与用户空间隔离拆开，保留原稿的多用户方向。');
edit('base-channels',{title:'消息适配器 · QQ / Web / 微信 / 飞书',summary:'QQBot / OneBot / Web 已有；微信、飞书规划',details:byId('base-channels').details+'\n\n【整理补充】\n不同入口通过内部用户与会话 ID 共享可见目录和状态；微信、飞书仍是规划，不标为已接入。群聊与私聊可见范围分别检查。消息入口不自行持有另一套 Agent loop。',status:'proposal',integration:'extend'},'保留微信/飞书规划与跨入口会话，区分已接入和拟接入。');
edit('base-gateway',{title:'归一化 Gateway',summary:'归一消息、校验来源、解析用户与会话',details:byId('base-gateway').details+'\n\n消息正文不决定 owner；通过真实平台身份和已验证关联解析用户，再检查会话可见范围。用户输入、控制回应、时间触发和任务完成保留不同来源。',status:'proposal',integration:'extend'});
edit('t-account',{title:'用户空间与资源授权',summary:'按用户隔离人格、记忆、会话与设备',details:byId('t-account').details+'\n\n【多用户方案】\n同一云部署服务多个真实用户，开发者是默认管理员。每个用户拥有自己的助手配置、会话、记忆、Skill/MCP 可用范围与设备池；部署实例不等于共享全部个人数据。共享助手和共享资源需要另行显式授权。',status:'proposal',integration:'extend'},'把用户空间作为明确边界，商业化预留由归属与授权模型承接。');
add('r-config','Agent 配置与能力目录','人格、角色提示词、Skill 和 MCP 引用','配置按用户和角色版本化；主助手与执行 Agent 可使用不同角色规则，但仍受当前用户授权与能力表约束。配置变更在明确快照边界生效，不在运行中偷偷改变任务范围。',['identity','memory','chat','additions'],'用户空间');
add('r-session-hub','跨入口会话目录','会话列表、状态、选择与可见范围','各入口可访问同一用户授权的会话目录，明确选择 conversation_id；聊天入口、模型上下文和设备执行绑定分别标识。私聊资料不能因共享列表恢复进群聊。并发消息有顺序和去重。',['chat','identity','additions'],'入口与会话');
add('r-boundary','回合边界与插话仲裁','用户消息在安全边界进入模型','入站队列保存消息身份和顺序；通常在工具批次结束、下一次模型或工具派发前消费。普通聊天、新任务、问题回答和取消分别处理。后台执行不占主会话整个生命周期；已经派发的操作保留真实状态。',['chat','execution','additions'],'主助手与异步工作衔接');
add('r-mcp','MCP / Connector 能力注册','云端能力发现、凭据引用与范围','按用户启用能力，云端持有凭据引用与连接配置；这些不进入模型工具参数。实际调用仍走原权限体系，加载 Skill 和注册 MCP 都不自行授予操作许可。当前完整云端注册产品尚未交付。',['identity','execution','memory','additions'],'用户配置与能力');
edit('t-events',{title:'会话事件队列',summary:'用户输入、计划触发与后台结果分来源',details:byId('t-events').details+'\n\n持久输入携带 event_id、会话/任务目标、来源、版本和顺序。消息受理与模型 transcript 消费分开，主会话串行处理；普通 trace 不逐条唤醒模型。',status:'proposal'});
edit('t-host',{summary:'持久受理、后台派发与前台容量',details:byId('t-host').details+'\n\n会话事件队列负责待处理输入，Runtime 持有实际运行和恢复。主助手派发成功后返回；为正常聊天留出容量。'});
edit('t-worker',{title:'云端执行 Agent × N',summary:'按设备绑定，独立任务上下文',details:byId('t-worker').details+'\n\n多个工作者继续驻留云端，具体进程池策略待定；本地不运行推理 Agent。任务可暂停恢复，不靠进程永远活着保存身份。'});
edit('t-executor',{title:'本地执行器 × N',summary:'Windows / Linux · 工具与执行日志'});
edit('t-cache',{title:'上下文组装与预算',summary:'授权材料、稳定前缀与有界任务增量'});
edit('t-models',{title:'账号模型路由',summary:'当前账号模型 + 未来角色路由',details:byId('t-models').details+'\n\n模型配置与并发容量分别管理；主助手/工作者可分别选型，但不能承诺仅靠拆进程提高缓存命中。'});
edit('base-limits',{title:'容量与运行预算',summary:'全局 / 用户 / 前台 / 后台配额',details:byId('base-limits').details+'\n\n【拟扩展】\n保留前台聊天容量，后台工作有独立并发和预算；不能让所有模型槽位被子任务占满。',status:'proposal',integration:'extend'});
edit('t-outbox',{title:'消息投影与持久发件箱',summary:'交流结果、审批与主动联系统一投送'});
edit('base-agent',{title:'共享 Agent 运行库',summary:'主助手与工作者复用原循环',details:byId('base-agent').details+'\n\n这里表示库与中间件依赖，不是第三个协调 Agent。实际对话和任务由各自角色实例推进。'});
const owner=byId('t-owner');edit('base-journal',{details:byId('base-journal').details+'\n\n【部署与所有权约束】\n'+owner.details},'移除“唯一持久状态所有者”独立功能框，将约束并入 CloudJournal，减少把部署约束误读成额外服务。');
board.nodes=board.nodes.filter(n=>n.id!=='t-owner');
const remove=new Set(['base-edge-2','base-edge-12','edge-26','edge-34','plan-chat-dispatch']);
board.edges=board.edges.filter(e=>!remove.has(e.id)&&e.from!=='t-owner'&&e.to!=='t-owner'&&e.from!==auth.id&&e.to!==auth.id);
for(const e of board.edges){if(e.from==='base-agent'||e.to==='base-agent')e.kind='dependency';}
edge('r-user-channel','t-user','base-channels','消息 / 控制');edge('r-login','t-user',auth.id,'登录 / 授权');edge('r-identity','base-gateway',auth.id,'解析已关联身份');edge('r-principal',auth.id,'t-account','可信用户');
edge('r-scope','t-account','r-session-hub','可见会话');edge('r-conversation','base-gateway','r-session-hub','路由与归属');edge('r-input','r-session-hub','t-events','有序消息');edge('r-admit','t-events','t-host','持久受理');edge('r-turn','t-host','r-boundary','主回合');edge('r-inject','r-boundary','t-main','边界消费');
edge('r-config-owner','t-account','r-config','用户配置');edge('r-main-config','r-config','t-main','角色版本','dependency');edge('r-worker-config','r-config','t-worker','执行角色','dependency');edge('r-capability-owner','t-account','r-mcp','启用范围');edge('r-capability','r-mcp','t-policy','注册实际工具');
edge('r-main-model','t-models','t-main','推理出口','dependency');edge('r-worker-model','t-models','t-worker','推理出口','dependency');edge('r-schedule-event','base-schedule-adapter','t-events','计划触发');edge('r-journal-task','base-journal','t-host','状态与恢复','data');
for(const e of board.edges)e.views=Object.keys(DATA.lenses).filter(l=>l!=='overview');
function summary(id,from,to,label){edge('summary-'+id,from,to,label,'summary');board.edges.at(-1).views=['overview'];}
summary('message','t-user','base-channels','消息 / 控制');summary('chat','base-channels','t-main','经网关、会话队列与边界');summary('delegate','t-main','t-host','异步派发，立即返回');summary('worker','t-host','t-worker','持久任务');summary('execute','t-worker','t-executor','经权限链与认证运输');summary('direct','t-main','t-executor','简单操作也经同一权限链');summary('login','t-user',auth.id,'登录 / 身份映射');summary('scope',auth.id,'t-account','独立用户空间');summary('context','t-account','t-main','授权配置与资料');summary('trace','t-worker','t-trace','自动采集');summary('state','t-trace','t-state','最新活动与引用');summary('inspect','t-state','t-main','有界进度 / 按需查看');summary('reply','t-main','base-channels','经持久发件箱投送');
board=Layout.overview(Layout.arrange(board,DATA.lenses));
workspace.boards=workspace.boards.filter(b=>b.id!==board.id);workspace.boards.push(board);if(workspace.boards.length>16)throw new Error('Too many boards');
workspace.workspaceId='peri-review-20261019';workspace.ui={...workspace.ui,board:board.id,lens:'overview',page:'topology',views:{},theme:workspace.ui.theme};
for(const [id,title,body]of [['d20','多用户部署与用户空间','保留原稿：同一部署服务多个真实用户，用户空间隔离配置、记忆、会话与设备。OAuth 身份确认与资源授权分别表达。'],['d21','跨入口共享会话与边界插话','保留原稿：QQ/Web 和未来微信/飞书共享授权会话目录，通过有序消息队列在安全边界消费。群聊与私聊的可见范围不合并。']])if(!workspace.decisions.some(d=>d.id===id))workspace.decisions.push({id,title,body,status:'direction'});
const mainExtra='\n\n## 多用户与跨入口补充（2026-10-19 讨论稿）\n\n同一部署可服务多个用户。只根据宿主可信身份读取当前用户的助手配置、会话、记忆、Skill/MCP 范围和设备池；OAuth 登录本身不证明某项资源授权。\n\n多个入口可以访问同一用户授权的会话目录，私聊与群聊保持不同可见范围。输入队列中的用户消息、控制回应和任务结果保留来源与顺序，在宿主规定的边界消费，不能改写正在执行操作的归属。\n';
workspace.prompts.main+=mainExtra;
workspace.prompts.frames+='\n\n## 跨入口事件与用户空间（2026-10-19 讨论稿）\n\n类型化输入携带可信 principal、conversation、event_id、来源、序号及上下文/约束版本。用户空间负责资源授权，入口负责身份归一化；会话目录、事件队列、回合边界仲裁和 Runtime 分别处理归属、待处理输入、安全消费与运行所有权。\n';
const json=JSON.stringify(workspace),jsonPath=path.join(root,'review-20261019.json');fs.writeFileSync(jsonPath,json+'\n');
const template=fs.readFileSync(path.join(root,'index.html'),'utf8'),encoded=json.replace(/</g,'\\u003c');
const html=template.replace(/(<script id="workbench-snapshot"[^>]*>)[\s\S]*?(<\/script>)/,()=>'<script id="workbench-snapshot" type="application/json">'+encoded+'</script>').replace(/<script id="workbench-reset">[\s\S]*?<\/script>/,'');
if(html===template)throw new Error('Snapshot marker missing');fs.writeFileSync(path.join(root,'review-20261019.html'),html);
fs.writeFileSync(path.join(root,'review-notes-20261019.md'),'# 方案整理 · 2026-10-19\n\n基于用户提供的 JSON 中当前激活的画板。原导出不修改；整理版保留所有原画板，另加一块整理图。\n\n## 保留的用户修改\n\n- 多个真实用户共享同一云部署，开发者是默认管理员。\n- OAuth / 用户隔离意图。\n- QQBot、OneBot、Web，以及规划中的微信、飞书。\n- 跨入口共享授权会话目录和状态，通过队列在安全边界插话。\n\n## 本次调整\n\n'+changes.map(s=>'- '+s).join('\n')+'\n- 基础库依赖从执行主链中区分，调度和消息统一经过类型化事件入口。\n- 总览用 10 个模块汇总主链；完整视图保留详细模块与关系，新增分区聚焦和相关连线突出。\n\n所有新增能力仍是方案，未修改生产行为。\n');
console.log(JSON.stringify({sourceBoard:source.id,boards:workspace.boards.length,reviewNodes:board.nodes.length,reviewEdges:board.edges.length,overviewNodes:board.nodes.filter(n=>n.lenses.includes('overview')).length,json:jsonPath,htmlBytes:Buffer.byteLength(html)},null,2));
