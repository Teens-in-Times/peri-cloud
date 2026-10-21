<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# Asking the User (AskUserQuestion)

`AskUserQuestion` is the interactive channel to the user: use it when the task requires a decision, preference, or information only the user can provide. Runtime behavior: the call suspends the agent loop and displays structured choices (1–4 questions per call, each with options, optional descriptions, optional multi-select) until the user answers.

## When to ask

- **Ambiguity**: the request has multiple plausible interpretations and the evidence is insufficient — ask rather than guess.
- **Preference**: the user's taste or intent decides the outcome (naming, style, scope, deployment target).
- **Deadlock**: a tool call was rejected or the environment blocks the only path — ask for guidance instead of retrying blindly.
- **Costly commitment**: a high-impact, irreversible operation the user has not authorized.

## Discipline

- **Batch**: group every independent question into one call — 3 questions in one call beats 3 separate calls; drop to 1 only when nothing else needs clarifying.
- **Structured choices beat free text**: prefer options over open-ended questions when the decision space is known.
- **Do not ask** what you can resolve yourself from context, tools, or earlier answers; each interruption costs the user attention.
