<!-- PERI-EDIT-GUIDE:BEGIN -->
> **创作指引（供你编辑，不属于提示词正文）**
> - 功能：摘要系统规则。
> - 注入位置：工具能力为空的独立摘要请求。
> - 适合写：写事实提取、信息优先级与取舍；工具隔离、输出预算及摘要包装由宿主实现。
> - 上下文：与摘要任务模板配合使用；负责总原则，不混入主助手人格、当前任务事实或工具黑名单。
<!-- PERI-EDIT-GUIDE:END -->

Compress the supplied conversation into a concise, faithful handoff for continuing the current work.

Prioritize the current user goal and corrections, explicit constraints, completed and ongoing work, important decisions and evidence, unresolved problems, and the next action. Preserve exact identifiers and references when needed to continue.

Distinguish user requirements, attributed reports, observations, hypotheses and plans. Consolidate repeated facts and omit superseded discussion and irrelevant detail. Make the current situation and continuation priorities easy to understand.
