"""Refresh editable prompt copies without overwriting user-authored changes."""
from pathlib import Path
import argparse, hashlib, json, re, subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs/prompt-review"
BEGIN = "<!-- PERI-EDIT-GUIDE:BEGIN -->"
END = "<!-- PERI-EDIT-GUIDE:END -->"
STR = re.compile(r'//[^\n]*|/\*[\s\S]*?\*/|r(#{0,8})"[\s\S]*?"\1|"(?:\\[\s\S]|[^"\\])*"')

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def read(path):
    return (ROOT / path).read_text("utf-8")

def literals(text):
    values = []
    for match in STR.finditer(text):
        token = match.group()
        if token.startswith(("//", "/*")):
            continue
        if token.startswith("r"):
            at = token.index('"')
            value = token[at + 1:len(token) - at]
        else:
            raw = re.sub(r"\\\r?\n\s*", "", token[1:-1])
            raw = raw.replace(r"\0", r"\u0000").replace("\n", r"\n").replace("\t", r"\t")
            value = json.loads('"' + raw + '"')
        values.append((match.start(), match.end(), value))
    return values

def functions(text, name):
    masked = STR.sub(lambda m: "".join("\n" if c == "\n" else " " for c in m.group()), text)
    for match in re.finditer(r"\bfn\s+" + re.escape(name) + r"\b[^;{]*\{", masked):
        pos, depth = match.end(), 1
        while pos < len(text) and depth:
            depth += (masked[pos] == "{") - (masked[pos] == "}")
            pos += 1
        if depth:
            raise ValueError("Cannot extract function " + name)
        yield match.start(), text[match.start():pos]

def constant(source, name):
    text = read(source)
    match = re.search(r"\bconst\s+" + name + r"\s*:\s*&[\w\s]*str\s*=", text)
    if not match:
        raise ValueError(name)
    suffix = text[match.end():]
    value = literals(suffix)[0][2]
    if suffix.lstrip().startswith("include_str!"):
        return (ROOT / source).parent.joinpath(value).read_text("utf-8")
    return value

def selected(source):
    text = read(source).split("#[cfg(test)]")[0]
    result = []
    for start, _, value in literals(text):
        if len(value) < 35 or not any(c.isspace() for c in value):
            continue
        if re.match(r"(?is)^(SELECT|INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|PRAGMA|WITH)\b", value.strip()):
            continue
        line = text.count("\n", 0, start) + 1
        result.append(f"### {source}:{line}\n\n~~~text\n{value}\n~~~")
    return "\n\n".join(result)

def tool_contract(source):
    text = read(source).split("#[cfg(test)]")[0]
    chunks = ["# 工具说明与参数契约\n\n正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。\n"]
    for method in ("description", "parameters", "prompt_declaration", "prompt_sections", "prompt_contribution"):
        for start, body in functions(text, method):
            line = text.count("\n", 0, start) + 1
            chunks.append(f"## {method} · {source}:{line}\n")
            if method == "description":
                chunks.extend("~~~text\n" + v + "\n~~~\n" for _, _, v in literals(body))
            else:
                chunks.append("~~~rust\n" + body + "\n~~~\n")
    return "\n".join(chunks)

def strip_guide(text):
    if text.count(BEGIN)!=text.count(END):
        raise ValueError("Unbalanced edit guide markers; preserve the draft and fix its markers first")
    return re.sub(re.escape(BEGIN) + r"[\s\S]*?" + re.escape(END) + r"\n*", "", text)

def prompt_blocks(name):
    return re.findall(r"~~~text\n([\s\S]*?)\n~~~", read("docs/prompt-review/" + name))

def titled_blocks(titles, blocks):
    if len(titles) != len(blocks):
        raise ValueError("Prompt section count changed; review authoring labels")
    return "\n\n".join("## " + title + "\n\n" + body for title, body in zip(titles, blocks))

ARCHITECTURE = """# 项目架构参考

用户 ↔ 云端主助手 ↔ 云端执行者 ↔ 设备执行器

主助手负责聊天、理解意图和统筹任务；小操作可直接调用设备执行器，复杂任务交给执行者，派活后仍能继续聊天。执行者可以复用会话、换窗续接，并与主助手双向沟通。设备端只负责执行工具。

宿主提供任务与操作记录、权限审批、个人记忆、调度和消息渠道。个人记忆与任务历史分开；主助手保留摘要式压缩，执行者根据任务事实和交接材料继续工作。

## 创作与阅读方式

每节先给出创作指引，再给出可修改的提示词正文。指引供你安排内容，不属于模型正文；这里的副本不会自动改变运行配置。可集中修改本合集，也可只改对应分文件，优先选择一种方式。

角色提示词写意图理解、策略取舍、工作方法和表达方式；人格放 SOUL，项目工程习惯放项目指令。任务事实、身份、权限、工具参数和当前进度由宿主提供，不需要在各段中手工重复。

## 上下文如何装配

默认基座顺序：base → role → soul → channel → memory_protocol → files → extensions → tools。

这只是基座的拼接顺序，不能理解为所有信息都放在一条永久 system 中，也不表示后面的文本拥有更高权限。

| 层次 / 段 | 来自哪里、在什么位置出现 | 创作重点 |
| --- | --- | --- |
| base | 共有 system 基座；主助手和执行者共用 | 共同定位、对用户的态度、证据使用原则 |
| role | 按 main / worker 选择一份角色正文 | 主助手的统筹策略；执行者的任务落实方法 |
| soul | 助手级 SOUL；私人路由还可加入本人 SOUL | 性格、称呼、语气、关系和偏好 |
| channel | QQ / OneBot / Web 等渠道适配说明 | 同一人格在不同渠道中的表达习惯 |
| memory_protocol | 共有 system 中的记忆使用原则 | 何时回忆、如何判断时效与相关性、何时写入 |
| files | 管理员明确配置的上下文资料 | 需要共同参考的背景；不等同于 cwd 项目指令 |
| extensions | 共有 system 中的扩展能力使用原则 | 何时发现、加载 Skill / MCP，如何使用其资料 |
| tools | 中间件提供的工具目录及 API 定义 | 工具使用时机与参数含义；调用能力由宿主注册 |
| 项目指令 | 目标设备绑定工作区的文件快照，通过中间件加入 | 项目目标、代码约定、构建测试、工程边界 |
| 设备环境 | 绑定设备的环境说明；跨设备准备也会返回目标环境 | 如何选择工具、核实版本、使用目标 shell |
| 任务 / 审批 / 记忆事实 | 带来源的消息帧或工具结果，随当前任务与授权变化 | 阅读与判断方法；事实本身由宿主填充 |
| Skill / MCP 正文 | 先发现目录，相关时再按需加载 | 专门领域的操作知识，不预先堆入所有正文 |
| 历史归档引用 | 上下文中的收起回执；使用 ReadContextArchive 有界回查 | 只在需要原始证据时检索，与长期个人记忆分开 |

审批使用独立的有界上下文视图，主要包含当前授权记忆、审批事实和协调工具。审批轨迹保留在宿主记录中，不作为普通聊天的连续正文；主助手仍可按需查询相关任务与证据。

基座中的配置性文本适合保持稳定。任务进度、最新补充、设备观测和加载的资料属于动态上下文，避免把它们复制进角色正文。项目和技能目录按工作区绑定加载；文件发生变化后，不应假定既有会话已自动重新读取。

## 助手、项目与技能的归属

| 内容 | 放置位置（逻辑路径） | 作用范围 |
| --- | --- | --- |
| 共有助手 SOUL | context.directory/SOUL.md | 助手级人格，不读取 cwd/SOUL.md |
| 本人 SOUL | context.directory/users/<principal>/SOUL.md | 对应账号的私人路由 |
| 项目主指令 | cwd/AGENTS.md；不可用时回退 cwd/CLAUDE.md，再到 .claude/AGENTS.md | 当前设备与工作区；候选主文件不是叠加读取 |
| 项目补充 | cwd/CLAUDE.local.md | 与选中的项目主指令一起提供 |
| 项目 Skill | cwd/.agents/skills/**/SKILL.md 或 .claude/skills/**/SKILL.md | 当前项目；同相对目录优先 .agents |
| 账号 Skill | context.directory/users/<principal>/skills/<name>/SKILL.md | 本人可用的全局技能 |
| 管理员 Skill | context.directory/skills/<name>/SKILL.md | 明确安装的公共技能 |
| MCP | 账号明确配置的连接及其发现接口 | 依授权提供工具和资源；项目 .mcp.json 不自动导入 |

同名 Skill 的生效顺序为项目 → 账号 → 管理员，由宿主合并后呈现目录。正文只需说明如何选用技能，不必反复解释目录遮蔽算法。表中的 principal、cwd 和 context.directory 是位置说明，不是需要填写到提示词里的私人机器路径。

## 各种请求如何获得上下文

- 主助手：共有基座 + main 角色 + 对应人格、渠道与项目资料 + 会话历史 / 摘要 + 相关记忆与有界任务动态。
- 执行者：共有基座 + worker 角色 + 当前任务的目标、约束、验收条件及补充指令 + 当前窗口历史。复用会话时，旧任务只作为背景。
- 执行窗口续接：宿主提供完整的活跃要求、持久状态及交接材料；执行者按需要检索历史证据，不必为了接班先反复抄录状态。
- 审批：在现有主助手会话中呈现待决调用及任务事实，结合审批指引作出判断；不另设审批人格。
- 主会话压缩：独立摘要请求使用摘要 system、摘要任务模板和待压缩会话，工具列表为空；摘要续写规则只在需要时使用。
- 调度：依任务类型提供主动问候或执行任务的说明、原有授权内容和事件来源；不把定时事件误当作用户刚发送的一句话。

## 宏与宿主填充位置

| 写法 | 用途与适用位置 |
| --- | --- |
| {{assistant.role}} | base / role 模板中的角色值：main 或 worker |
| {{channel.constraints}} | base / role 模板中需要引用渠道说明时使用；渠道也有独立段，避免重复大量正文 |
| {summary_target_tokens} | 摘要 user 模板的输出预算变量，不是通用双花括号宏 |
| {objective}、{urgency} | Goal 引导帧中的目标与接续说明 |
| {behavior}、{notify}、{} | 特定动态模板中的宿主填充位；含义见对应节指引，不是通用宏 |
| {{你希望增加的宏名}} | 创作时可以先写上，并注明想放什么；接入前仅为设计占位 |

通用双花括号宏只在宿主拥有的 base / role 模板中做一层替换；SOUL、项目文件、技能正文和工具结果不继续当模板求值。未知宏需要接入后才能运行。任务快照、记忆条目和设备资料目前通过独立上下文提供，不要把设想的宏误认为已经有值。
"""

PURPOSE = {
    "00": ("装配与宏说明", "供人设计，不发给模型", "定义宏名、值来源和适用范围；目前仅 base/role 支持 assistant.role、channel.constraints。其他宏先标记待接入。"),
    "01": ("宿主默认基座", "稳定 system 的 base 段", "写助手共有行为和能力边界；不写本机路径、任务进度或私人资料。"),
    "02": ("主助手角色", "主助手 system 的 role 段", "写如何聊天、何时直接做小活、何时委派、如何关注执行任务和呈现结果。"),
    "03": ("执行者角色", "执行 agent system 的 role 段", "写如何落实当前任务、处理补充要求、查证据和留下工作笔记；不写主助手日常聊天人格。"),
    "04": ("主会话审批事件", "现有主 agent 的事件帧与有界控制回合", "写任务意图、必要性与范围判断；通过 decide_approval 提交决定，格式、版本、通知与权限由宿主保证。"),
    "05": ("记忆协议", "稳定 system 的 memory_protocol 段", "写何时检索、何时经用户明确要求写入、如何看待过期资料；不在这里填实际记忆。"),
    "06": ("渠道表达", "渠道适配段", "写个人 QQ、官方 QQ 与 Web 的可读表达；不改变账号或投递权限。"),
    "07": ("任务动态与双向消息", "宿主生成的观察、启动、等待和消息帧", "写如何理解任务状态及消息来源；动态 task_id、最新要求、回执由宿主注入。"),
    "08": ("换窗与恢复", "执行窗口续接、宿主事实及原操作对账", "写如何接班、查证据和处理未知结果；不请求隐藏思考，不以文案证明旧操作结束。"),
    "09": ("摘要系统规则", "工具能力为空的独立摘要请求", "写事实提取、信息优先级与取舍；工具隔离、输出预算及摘要包装由宿主实现。"),
    "10": ("摘要任务模板", "主会话摘要请求的 user 模板", "写目标、约束、当前状态、决策依据、失败与下一步的取舍；保留 summary_target_tokens 现有变量。"),
    "11": ("摘要续写", "摘要被截断或无正文时的有界恢复", "写如何补齐缺失事实和简短收尾；工具能力与恢复次数由代码限定。"),
    "12": ("长程目标与主动联系", "Goal 接续和持久调度触发", "写接续、主动问候与静默策略；保留真实用户要求、时区和事件身份的宿主注入位置。"),
    "13": ("Skill、工作区与全局资料", "动态能力说明和按需正文入口", "写何时发现与加载技能；有效目录、遮蔽、AGENTS/CLAUDE 选择由宿主决定。SOUL 属于助手级。"),
    "14": ("扩展能力加载协议", "稳定 system 的 extensions 段", "写 Skill/MCP 的发现、资料信任及设备环境使用原则；不在这里配置 endpoint、token 或 SSH。"),
    "15": ("设备环境快照", "动态 system 后缀或 prepare_device 返回值", "写如何使用目标设备事实与处理未探测项；发现 PATH 可执行文件不等于版本已验证。"),
    "16": ("助手人格 SOUL 创作位置", "助手级 operator/user SOUL，不读取 cwd/SOUL.md", "创作称呼、性格、关系、语气与相处习惯；不放项目构建命令、身份凭据或绕过权限的规则。"),
    "17": ("项目指令创作位置", "认证 cwd 的 AGENTS.md，缺失时 CLAUDE.md；local 为补充", "写项目目标、目录规则、构建测试、工程约束；不写助手人格，不声称扩大工具权限。"),
    "18": ("宿主承接契约", "供人审阅，不注入模型", "理解已下沉的流程、验证结果与限制；不把实现规则重新写成角色提示词。"),
}
def guide(entry):
    name = entry["file"]
    prefix = Path(name).name[:2]
    if "/" not in name and prefix in PURPOSE:
        purpose, place, content = PURPOSE[prefix]
    elif name.startswith("tools/"):
        purpose, place, content = "工具使用契约", "工具目录的 description/schema", "写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。"
    elif name.startswith("reference/"):
        purpose, place, content = "上游或历史设计参考", "未必启用，不能直接当作当前运行规则", "批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。"
    elif name.startswith(("tool-text/", "middleware/")):
        purpose, place, content = "工具或中间件说明", "受实际工具与中间件装配开关控制", "写使用时机、输入输出和限制；保持与实际能力一致。"
    else:
        purpose, place, content = "提示词编辑导航或合订稿", "供人编辑，不直接载入模型", "优先修改合订稿或对应分文件中的一份，避免两份同时改出冲突。"
    detail = {
        "04": "任务帧提供当前要求、待决调用及风险信息。这里写判断依据，不设计独立助手人格或 JSON 排版规则。",
        "05": "这里写记忆的使用方法；实际个人资料通过授权记忆帧及检索结果出现，任务 trace 不整体充当个人记忆。",
        "06": "分别编辑官方 QQ 与个人 QQ 的表达习惯；Web 没有单列默认文案，需要时可提出。消息投递和排版转换属于渠道管道。",
        "07": "主助手读取有界任务摘要与审批事件；执行者获得目标、约束、验收条件及最新补充。双向沟通使用 message_task / message_parent，等待使用 wait_task。下方是观察证据时的固定指引，不是完整动态任务帧。",
        "08": "分为无工具接班摘要请求与新窗口续接说明。宿主另行提供当前锚点、摘要覆盖水位、最新补充与证据引用；具体身份和任务事实不手填。手动无关联活跃主 goal 时停在就绪状态，这条策略由代码保证。",
        "09": "与摘要任务模板配合使用；负责总原则，不混入主助手人格、当前任务事实或工具黑名单。",
        "10": "与摘要 system 一起构成独立压缩请求；保留 {summary_target_tokens}，可改信息取舍与组织方式。",
        "11": "三个小节分别用于继续未完成摘要、在剩余预算内收尾、重新生成简洁摘要，并非每次都一起发送。",
        "12": "各小节按触发场景分别使用。Goal 帧的 {objective}/{urgency} 是目标和接续说明；调度帧中三个 {} 依次是事件标识、UTC 时间、用户授权任务，{behavior}/{notify} 是所选行为与通知策略。",
        "13": "这里写选用技能的方法。生效目录由项目、账号和管理员技能合并得到；完整 SKILL.md 与 MCP 资料按需加载。具体根目录和作用域见开头装配参考。",
        "15": "末尾 {} 是宿主序列化的环境快照，包含设备、工作区绑定、平台、架构、shell、观测时间与应用信息；未探测版本可能为空，不在这里填写自己电脑的实际配置。",
        "16": "这是留给你创作的空白位置。可分别写助手共有的人格和本人偏好；两者的归属见开头表格，不作为项目 cwd 文件。",
        "17": "这是留给项目 AGENTS.md 的创作位置；只有主文件不可用才回退 CLAUDE.md，CLAUDE.local.md 可单独补充。这里的内容约束项目工作，不替换助手 SOUL。",
    }.get(prefix)
    extra = f"> - 上下文：{detail}\n" if detail else ""
    return f"{BEGIN}\n> **创作指引（供你编辑，不属于提示词正文）**\n> - 功能：{purpose}。\n> - 注入位置：{place}。\n> - 适合写：{content}\n{extra}{END}\n\n"

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--clean-to", type=Path, help="Copy drafts without guides to a NEW output directory; never deploy automatically")
    args = parser.parse_args()
    if args.clean_to:
        dest = args.clean_to.resolve()
        if dest.exists():
            raise SystemExit("Output already exists; refusing to overwrite")
        if dest == OUT.resolve() or OUT.resolve() in dest.parents:
            raise SystemExit("Output must be outside the editable folder")
        dest.mkdir(parents=True)
        count = 0
        for source in sorted(OUT.glob("*.md")):
            if source.name not in ("README.md", "PROMPTS-EDIT.md") and not re.match(r"(?:0\d|1[0-7])-", source.name):
                continue
            target = dest / source.relative_to(OUT)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(strip_guide(source.read_text("utf-8")), encoding="utf-8", newline="\n")
            count += 1
        print(json.dumps({"clean_documents": count, "destination": str(dest)}))
        return
    manifest = json.loads((OUT / "source-map.json").read_text("utf-8"))
    entries = manifest["files"]
    by_name = {e["file"]: e for e in entries}
    def put(name, text, sources=(), kind="active-protocol"):
        entry = by_name.get(name)
        path = OUT / name
        if path.exists() and entry and (entry.get("user_edited") or digest(path) != entry["export_sha256"]):
            print("preserved user edit: " + name)
            entry["user_edited"] = True
            return
        if entry is None:
            entry = {"file": name, "kind": kind, "sources": []}
            entries.append(entry)
            by_name[name] = entry
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text.rstrip() + "\n", encoding="utf-8", newline="\n")
        entry["sources"] = [{"path": s, "sha256": digest(ROOT / s)} for s in sources]
        entry["export_sha256"] = digest(path)
    archive_tool_source = "peri-cloud/src/archive/tool.rs"
    put("tools/peri-cloud--archive-tool.md", tool_contract(archive_tool_source), [archive_tool_source], "active-tool-contract")
    task_tool_source="peri-cloud/src/tasks/tools.rs"
    put("tools/peri-cloud--tasks-tools.md",tool_contract(task_tool_source),[task_tool_source],"active-tool-contract")
    put("01-host-base.md", read("peri-cloud/prompts/base.md"), ["peri-cloud/prompts/base.md"], "active-default")
    for name, source in [("02-main.md", "peri-cloud/prompts/main.md"), ("03-worker.md", "peri-cloud/prompts/worker.md")]:
        put(name, read(source), [source], "active-role")
    for name, source, symbol in [("04-approval.md", "peri-cloud/src/tasks/approvals.rs", "REVIEW_RULES"), ("05-memory-protocol.md", "peri-cloud/src/memory/frames.rs", "RULES")]:
        runtime = "peri-cloud/prompts/approval.md" if name.startswith("04") else "peri-cloud/prompts/memory.md"
        put(name, constant(source, symbol), [source, runtime])
    channels = []
    for title, source in (("官方 QQ", "peri-cloud/src/qq/api.rs"), ("个人 QQ", "peri-cloud/src/onebot/mod.rs")):
        for _, body in functions(read(source), "context_constraints"):
            channels.append("## " + title + "\n\n" + "\n".join(v for _, _, v in literals(body)))
    put("06-channels.md", "\n\n".join(channels), ["peri-cloud/src/qq/api.rs", "peri-cloud/src/onebot/mod.rs"])
    for name, source in [("09-summary-system.md", "peri-agent/src/agent/compact_v2/descriptions/summary_system_prompt.md"), ("10-summary-user.md", "peri-agent/src/agent/compact_v2/descriptions/summary_user_prompt.md")]:
        put(name, read(source), [source])
    summary_source = "peri-agent/src/agent/compact_v2/full.rs"
    put("11-summary-continuation.md", titled_blocks(["续写未完成的摘要", "收尾", "重新生成"], [constant(summary_source, symbol) for symbol in ["SUMMARY_RECOVERY_PROMPT", "SUMMARY_FINAL_PROMPT", "SUMMARY_REGENERATION_PROMPT"]]), [summary_source])
    put("14-capability-loading.md", read("peri-cloud/prompts/extensions.md"), ["peri-cloud/prompts/extensions.md"])
    put("16-assistant-soul.md", "# 助手 SOUL\n\n", [], "review-authoring-slot")
    put("17-project-instructions.md", "# 项目指令\n\n", [], "review-authoring-slot")
    # Editorial copies contain prose only. Engineering references remain separate.
    put("00-assembly-and-macros.md", ARCHITECTURE, ["peri-cloud/src/context/mod.rs", "peri-cloud/src/workspace/mod.rs", "peri-executor/src/workspace.rs"], "review-guide")
    task_source = "peri-cloud/src/tasks/middleware.rs"
    task_text = next(value for _, _, value in literals(read(task_source)) if value.startswith("Worker notes and results"))
    put("07-task-frames.md", task_text, [task_source])
    recovery_source = "peri-cloud/src/tasks/recovery.rs"
    handoff_source="peri-cloud/prompts/window-handoff.md"
    resume_source="peri-cloud/prompts/window-resume.md"
    handoff = "## 接班摘要生成\n\n"+read(handoff_source)+"\n\n## 新窗口接班\n\n"+read(resume_source)
    put("08-window-handoff.md", handoff, [recovery_source,handoff_source,resume_source])
    schedule_name = "12-goal-and-schedule.md"
    blocks = prompt_blocks(schedule_name)
    if blocks:
        titles = ["目标接续", "目标仍未结束", "目标终态提醒", "目标引导帧", "主动问候", "执行定时任务", "静默策略", "按时发送消息", "调度事件帧"]
        put(schedule_name, titled_blocks(titles, blocks))
    skills_source = "peri-cloud/src/workspace/skills.rs"
    skills = next(value for _, _, value in literals(read(skills_source)) if value.startswith("# Skills\n"))
    put("13-workspace-and-skills.md", skills.removeprefix("# Skills\n").strip(), [skills_source])
    environment_source = "peri-cloud/src/workspace/environment.rs"
    environment = next(value for _, _, value in literals(read(environment_source)) if value.startswith("Target device environment"))
    put("15-environment.md", environment, [environment_source])
    put("README.md", "# Peri 提示词创作包\n\n打开 [提示词合集](PROMPTS-EDIT.md) 即可开始修改，也可只改对应分文件，优先选择一种编辑方式。\n\n开头为架构与上下文装配参考，包括注入层次、SOUL / 项目 / Skill 的归属及宏位置。各节保留功能、注入位置和创作建议，随后为待编辑正文；助手 SOUL 与项目指令留有创作位置。\n\n创作指引位于 PERI-EDIT-GUIDE 标记之间，不属于模型正文。需要的宏可直接标在正文中；创作完成后再核对接入并清理指引。这些副本不会自动部署。\n", [], "review-index")
    for entry in entries:
        path = OUT / entry["file"]
        if "/" in entry["file"] or entry["file"] in ("PROMPTS-EDIT.md", "18-host-contracts.md"):
            continue
        text = path.read_text("utf-8")
        text = strip_guide(text)
        if path.name[:2] in PURPOSE and path.name[:2] != "00":
            text = guide(entry) + text
        path.write_text(text, encoding="utf-8", newline="\n")
        entry["export_sha256"] = digest(path)
    intro = (OUT / "00-assembly-and-macros.md").read_text("utf-8")
    intro = re.sub(r"(?m)^(#{1,5}) ", r"#\1 ", intro)
    combined = "# Peri 提示词创作稿\n\n" + intro
    for path in sorted(OUT.glob("[0-9][0-9]-*.md")):
        prefix = path.name[:2]
        if prefix in ("00", "18"):
            continue
        title = PURPOSE[prefix][0]
        body = strip_guide(path.read_text("utf-8")).strip()
        body = re.sub(r"^# [^\n]+\n*", "", body, count=1)
        body = re.sub(r"(?m)^(#{1,5}) ", r"#\1 ", body)
        combined += f"\n\n## {title}\n\n" + guide(by_name[path.name]) + body + "\n"
    put("PROMPTS-EDIT.md", combined, [], "review-combined")
    e = by_name["PROMPTS-EDIT.md"]
    p = OUT / e["file"]
    e["export_sha256"] = digest(p)
    manifest["source_commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    manifest["source_snapshot"] = "working tree, source hashes recorded per document"
    (OUT / "source-map.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"documents": len(entries), "authoring_collection": "PROMPTS-EDIT.md"}, ensure_ascii=False))

if __name__ == "__main__":
    main()
