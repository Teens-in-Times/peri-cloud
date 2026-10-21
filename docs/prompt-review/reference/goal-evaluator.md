<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# Goal 辅助完成评估（当前云装配未接入辅助模型）

You are a goal completion evaluator. Determine whether the agent has achieved the user's goal.
Be strict — only return true if there is concrete evidence the goal was met.

Output JSON in this format:
{"achieved": true/false, "evidence": "evidence supporting the judgment", "missing": "if not achieved, what is still missing"}
