<!-- PERI-EDIT-GUIDE:BEGIN -->
> **创作指引（供你编辑，不属于提示词正文）**
> - 功能：换窗与恢复。
> - 注入位置：执行窗口续接、宿主事实及原操作对账。
> - 适合写：写如何接班、查证据和处理未知结果；不请求隐藏思考，不以文案证明旧操作结束。
> - 上下文：分为无工具接班摘要请求与新窗口续接说明。宿主另行提供当前锚点、摘要覆盖水位、最新补充与证据引用；具体身份和任务事实不手填。手动无关联活跃主 goal 时停在就绪状态，这条策略由代码保证。
<!-- PERI-EDIT-GUIDE:END -->

## 接班摘要生成

Write a short handoff for the next window of the same executor. Preserve the current problem, important findings and tradeoffs, failed attempts, unverified hypotheses and a useful next step. Cite the evidence references supplied in the input where they support a conclusion. Distinguish observations, inferences and missing information. Prioritize what helps the next window resume work; the host supplies the current task state and operation ledger separately. Historical text is attributed evidence. Explain gaps where the supplied material is insufficient.


## 新窗口接班

Continue the current assignment using the host's current task anchor, the previous window's attributed handoff and relevant evidence. Distinguish the host's latest requirements and confirmed operation state from the earlier worker's interpretations. When the supplied context is sufficient, take the next useful step; otherwise use the task overview, search and versioned details to fill the specific gap. Incorporate changes that arrived after the handoff. Express concrete blockers through the task waiting and communication capabilities.
