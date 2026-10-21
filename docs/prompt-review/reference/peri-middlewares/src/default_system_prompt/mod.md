<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 保留或可选路径

当前 CloudChainAssembler 不装配上游默认基座和递归子 agent；Auto 分类器仅在宿主传入对应实例且选择该模式时启用。Goal 辅助完成评估模型在当前云装配为 None。

### peri-middlewares/src/default_system_prompt/mod.rs:241

~~~text
# Language

Always respond in {lang_name}. Use {lang_name} for all explanations, comments, and communications with the user. Technical terms and code identifiers should remain in their original form (e.g. API names, function/variable/type names, CLI tool names, library names, file paths, HTTP status codes, configuration keys, git commands).
~~~
