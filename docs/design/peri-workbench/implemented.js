/* Current implementation snapshot. The reviewed design and Grok evidence stay intact. */
window.PeriImplemented = (() => {
  'use strict';
  function create(original) {
    const board=JSON.parse(JSON.stringify(original));
    board.subtitle='2026-10-19 · 已落地的云端主/执行 Agent、持久输入队列与上下文装配；按当前源码标记边界。';
    const updates={
      gateway:['Gateway','来源准入、身份与会话路由','平台身份与逻辑会话解耦。普通输入归一化后持久排队；控制面沿原认证路径执行。账号级门保护选择，进行中轮次不被换入口重绑。','ingress'],
      runtime:['CloudRuntime','前台 / 后台容量，受理与取消','原会话继续串行；主助手和执行任务都复用 Runtime。前后台使用独立全局/账号并发预算，部署持有任务 future，取消不冒充副作用已撤销。','runtime'],
      agent:['主助手 Agent','聊天、小操作直做、大任务异步委派','保留原 run_react_loop、PermissionMiddleware 与 Todo。统一装配上下文；可委派后继续聊天，按需查看任务/trace/独立会话，并向工作者插话。普通消息只展示主助手的 AI 正文。','core'],
      journal:['CloudJournal / SQLite','schema 21：操作指令版本与验收证据','唯一宿主拥有状态目录。任务、窗口、运行轮次、检查点、原始回执、指令水位与审批分别持久化；产物和验收属于任务记录。未知执行或投送不盲目重试。','coreplan'],
      broker:['提问 / 审批 Broker','主助手独立审查，原权限链执行','执行任务的审批进入主助手独立、有界、无工具的审查回合，不占普通聊天；具体参数、设备、任务与指令版本绑定。删除或可能替换内容先通知再执行；原 OAuth 人工授权和主会话审批保持原路径。','worker'],
      limits:['额度与并发','前后台独立容量，时限与每日用量','主助手默认全局 4 / 每人 1 活跃回合；执行任务默认全局 2 / 每人 1。沿用每日任务、模型请求和回合时限，不等于精确 token 或金额计费。','coreplan'],
      delivery:['结果投影与持久发件箱','主助手、定时回合与任务事件统一投送','工作者事件先回根会话，由主助手组织 AI 正文。发送仍由原发件箱管理，复核账号、路由和隐私/上下文代次；平台接受不代表已读，未知投送不重试。','worker'],
      portal:['账号工作台与菜单','显式会话、SOUL、任务和原控制面','网页可新建独立逻辑会话、按 ID 续接本人会话，管理个人 SOUL；聊天按逻辑会话展示。原绑定、目录、模型/记忆/计划、审批详情保留。','coreplan']
    };
    for(const [id,[title,summary,details,source]]of Object.entries(updates))Object.assign(board.nodes.find(n=>n.id===id),{title,summary,details,source});
    const add=(id,title,summary,details,source,lenses='chat execution')=>board.nodes.push({id,title,summary,details,source,lenses:lenses.split(' '),status:'implemented',area:'cloud',kind:'module',x:0,y:0,w:218,h:118});
    add('normalizer','MessageNormalizer','统一事件身份，保留原始输入','各平台完成认证后进入统一消息校验；事件 ID、原文指纹与路由归属分别保留。命令别名在持久接收后解释。','ingress');
    add('logical-session','逻辑会话选择','账号归属，入口仅作选择','同一 OAuth 身份可跨入口选择本人会话，或显式新建独立会话。request_id 去重；进行中轮次保留原回复与审批来源。执行任务历史不能切换为普通聊天。','sessions','chat identity');
    add('inbox','持久输入 FIFO','当前轮次结束后按序处理','gateway_queue 绑定目标逻辑会话、原始回复引用和消息来源。运行中输入排队，边界重新认证后受理；未知执行保留原轮次，不盲目重做。','queue');
    add('chat-projection','共享聊天投影','按逻辑会话展示用户与 AI 正文','Web 聊天历史按 session 展示，跨入口恢复可见同一会话。执行器结果、内部思考和工作者事件不直接伪装成用户消息。','projection','chat');
    add('worker-service','WorkerService','云端异步任务宿主','委派先持久受理，主助手随即继续聊天。宿主驱动 queued/running/waiting_parent/terminal 状态，复用原 Runtime 和推理循环。观察游标避免不可投送旧任务饿死新任务。','tasks');
    add('worker','执行 Agent','独立上下文与冻结设备','主/执行 Agent 都在云端，首版复用同一宿主进程。执行 Agent 独立 session/history/turn，冻结 device/workspace；本地只执行工具。没有递归委派工具，审批仍走原权限链。','worker');
    add('task-tools','主/执行任务工具','主助手保留 Todo，工作者维护计划','主：delegate_task/list_tasks/inspect_task/message_task/control_task。执行：inspect_task/update_task/message_parent。生命周期由宿主维护，Agent 的 note/work_plan 属于带来源的声明。','tasktools');
    add('task-ledger','任务账本与回执','版本、受理和未知结果分开','agent_tasks 保存根会话、子会话、真实轮次、请求指纹、版本及结果。CAS 防止覆盖并发修改；完成不等于独立验收通过，重启不重做未知副作用。','tasks');
    add('task-messages','双向任务消息','submitted → consumed → applied','主助手可插话、追问与补充约束。执行 Agent 可 message_parent(wait_for_reply=true) 异步等待；裁剪预览不当作完整指令，必要时先 inspect_task 读取正文。','tasktools');
    add('task-trace','自动 trace 与深层查看','工具预览、可见说明、live 历史','宿主记录工具开始/结束、裁剪输入输出和等待审批。可见说明按定稿迭代落库，排除内部思考；inspect_task 可分页查看 trace、消息、运行中或已结算会话，尊重历史排除与隐私版本。','traces');
    add('task-event','任务事件与观察刷新','事实变化 + 可配置对话轮次','问题与终态经持久队列进入根会话，作为类型提醒。主助手在模型边界读取有界状态，变化时更新，无变化默认每 3 个本会话轮次刷新；工具迭代不计轮次，游标随会话恢复。旧观察帧退出有效上下文，执行者无需定期汇报。回复仍走原发件箱与隐私检查。','worker');
    add('device-lease','设备修改占用','首版按整台设备协调写操作','后台任务预占设备，主助手仍能聊天、读取状态和补充指令。Write/Edit/Bash 在原授权后检查占用；结算后释放，未知执行保留占用。它不是第二套审批，也不是按文件锁。','resources');
    add('execution-session','可复用执行会话','身份 / 会话 / 任务 / 窗口分离','同一执行会话串行承接独立任务；同账号跨主会话须显式复用，设备、规范工作区、隐私范围及来源代次仍须一致。不会静默继承其他范围的历史。','tasks');
    add('execution-continuity','检查点与窗口续接','持久等待、暂停、换窗与查询恢复','主会话保留摘要压缩，执行窗口按预算轮换。宿主事实、工作笔记与历史引用生成接班材料；原调用回执保留身份。未知原生操作只查询，不能因超时或重启再提交。旧操作运行中独立换窗仍待完善。','tasks');
    add('task-evidence','有界证据检索','分页、筛选、完整详情与版本','inspect_task 分层查看窗口、操作、审批、消息与可见会话；trace 有界保留并可筛选，必要正文按字符分页且校验内容版本。明确预览与截断，不采集或暴露隐藏思考。','tasktools');
    add('task-deliverables','产物、验证与验收','执行完成不等于验收通过','产物位置、设备和版本绑定实际原生操作；验证状态来自真实回执。主助手依据最新要求接受或退回；新增要求使旧验收过期，历史保留。证据是操作时快照，不代替持续文件监控。','tasks');
    add('context-assembler','ContextAssembler','主/执行共用可组合装配','基础提示词、角色、SOUL、渠道规则、已有记忆协议、外置 Markdown 和原生工具可按配置排序；支持扩展 ContextPart。顺序不改变原权限链，动态记忆帧和工作区规则继续按原契约刷新。','assemblycontext','chat memory');
    add('context-files','SOUL 与外置文件','部署者来源 + 账号私人文件','全局 SOUL.md、配置的 Markdown 与 users/<principal>/SOUL.md 每轮发现读取。私人文件不进入群聊；路径、编码、大小和越界受限。网页沿原同源/CSRF 认证管理个人 SOUL。','contextfiles','chat memory identity');
    const connect=(from,to,label,kind='flow')=>board.edges.push({id:'implemented-'+board.edges.length,from,to,label,kind,style:'solid'});
    board.edges=board.edges.filter(e=>!((e.from==='gateway'&&e.to==='runtime')||(e.from==='agent'&&e.to==='delivery')));
    for(const [from,to,label,kind] of [
      ['gateway','normalizer','平台输入'],['identity','logical-session','可信账号','control'],['normalizer','logical-session','原始事件'],
      ['logical-session','inbox','选定根会话'],['inbox','runtime','边界受理'],['journal','inbox','恢复与去重','data'],
      ['agent','worker-service','异步委派'],['worker-service','task-ledger','宿主状态','data'],['worker-service','runtime','后台独立额度'],
      ['runtime','worker','独立执行回合'],['worker','permission','原工具审批'],['worker','models','云端推理'],
      ['agent','task-tools','主任务工具'],['worker','task-tools','执行任务工具'],['task-tools','task-ledger','CAS 与状态'],
      ['task-tools','task-messages','双向消息'],['task-messages','worker','补充 / 唤醒','control'],['task-messages','task-event','问题回流'],
      ['worker','task-trace','自动观察'],['task-trace','agent','按需 inspect_task','data'],['task-ledger','task-event','终态事件'],
      ['task-event','inbox','有类型的后台输入'],['agent','delivery','主助手结算正文'],['journal','chat-projection','共享会话视图','data'],
      ['chat-projection','portal','聊天历史'],['context-files','context-assembler','按身份发现'],['context-assembler','agent','主角色上下文'],
      ['context-assembler','worker','执行角色上下文'],['memory','context-assembler','已有记忆协议'],['gateway','context-assembler','渠道展示规则'],
      ['worker-service','device-lease','预占设备','control'],['permission','device-lease','授权后的修改协调'],['device-lease','remote','冻结绑定执行'],
      ['journal','task-ledger','schema 21 持久状态','data'],
      ['worker-service','execution-session','显式复用'],['execution-session','execution-continuity','物理窗口与运行轮次'],
      ['execution-continuity','task-ledger','检查点与恢复事实','data'],['task-trace','task-evidence','预览与原证据引用','data'],
      ['task-evidence','agent','按需检索'],['worker','broker','审批 / 持久问题'],['broker','delivery','删除前确认通知'],['broker','permission','一次性原权限决定'],
      ['worker','task-deliverables','登记真实回执'],['task-evidence','task-deliverables','操作证据与版本','data'],['task-deliverables','agent','主助手验收 / 补充要求']
    ])connect(from,to,label,kind);
    return board;
  }
  return {create};
})();
