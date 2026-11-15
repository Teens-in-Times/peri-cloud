use super::*;

#[test]
fn entry_from_dto_extracts_name_description() {
    let dto: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "name": "a", "description": "A skill", "license": "MIT" },
        "resources": [{ "uri": "skill://a/SKILL.md", "digest": "sha256:abc" }],
    }))
    .unwrap();
    let entry = entry_from_dto(dto).unwrap();
    assert_eq!(entry.uri, "skill://a/SKILL.md");
    // name/description 门闩通过后，frontmatter 原样保留（verbatim，逐字段
    // 比对用）——附加字段 license 也在其中
    assert_eq!(
        entry.frontmatter,
        serde_json::json!({ "name": "a", "description": "A skill", "license": "MIT" })
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!(
        entry.resources,
        Some(vec![SkillResource {
            uri: "skill://a/SKILL.md".to_string(),
            digest: "sha256:abc".to_string(),
        }])
    );
}

#[test]
fn entry_from_dto_resources_omitted_vs_empty_distinguished() {
    // 省略 → None（动态生成技能，规范 MAY）
    let omitted: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "name": "a", "description": "A" },
    }))
    .unwrap();
    let entry = entry_from_dto(omitted).unwrap();
    assert_eq!(entry.resources, None, "省略 resources → None（动态技能）");

    // 显式空数组 → Some(空)（present 但完整性违规，由 verify_and_build 拒绝）
    let empty: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "name": "a", "description": "A" },
        "resources": [],
    }))
    .unwrap();
    let entry = entry_from_dto(empty).unwrap();
    assert_eq!(
        entry.resources,
        Some(vec![]),
        "显式空数组 → Some(空)（与省略区分）"
    );

    // 非空 → Some(非空)
    let full: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "name": "a", "description": "A" },
        "resources": [{ "uri": "skill://a/SKILL.md", "digest": "sha256:abc" }],
    }))
    .unwrap();
    let entry = entry_from_dto(full).unwrap();
    assert_eq!(entry.resources.as_ref().unwrap().len(), 1);
}

#[test]
fn entry_from_dto_missing_name_reports_field_and_category() {
    let dto: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "description": "no name" },
    }))
    .unwrap();
    let defect = entry_from_dto(dto).unwrap_err();
    assert_eq!(defect.field, "name");
    assert_eq!(defect.category, "missing");
}

/// M8：非字符串 name/description 同样在 discovery 边界隔离（类别 wrong-type），
/// 诊断只带字段与类别，不回显字段正文。
#[test]
fn entry_from_dto_non_string_field_is_isolated() {
    let dto: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "name": 7, "description": { "nested": "value" } },
    }))
    .unwrap();
    let defect = entry_from_dto(dto).unwrap_err();
    assert_eq!(defect.field, "name");
    assert_eq!(defect.category, "wrong-type");

    let dto: SkillListEntryDto = serde_json::from_value(serde_json::json!({
        "uri": "skill://a/SKILL.md",
        "frontmatter": { "name": "a", "description": 7 },
    }))
    .unwrap();
    let defect = entry_from_dto(dto).unwrap_err();
    assert_eq!(defect.field, "description");
    assert_eq!(defect.category, "wrong-type");
}

#[test]
fn skill_list_response_parses_pagination_and_defaults() {
    let json = serde_json::json!({
        "skills": [
            { "uri": "skill://a/SKILL.md", "frontmatter": { "name": "a", "description": "A" } },
            {
                "uri": "skill://b/SKILL.md",
                "frontmatter": { "name": "b", "description": "B" },
                "resources": [{ "uri": "skill://b/SKILL.md", "digest": "sha256:abcd" }]
            }
        ],
        "nextCursor": "page-2"
    });
    let parsed: SkillListResponse = serde_json::from_value(json).unwrap();
    assert_eq!(parsed.next_cursor.as_deref(), Some("page-2"));
    assert_eq!(parsed.skills.len(), 2);
    // resources 可省略（动态生成技能）：缺省 → None
    let e0 = entry_from_dto(parsed.skills[0].clone()).unwrap();
    assert!(e0.resources.is_none(), "省略 resources → None");
    let e1 = entry_from_dto(parsed.skills[1].clone()).unwrap();
    assert_eq!(e1.resources.as_ref().unwrap().len(), 1);
}

/// [回归测试] 只带 `_meta` 的 result 不得被解析成工具结果。
///
/// rmcp 3.5.1 把 `_meta` 当 `CallToolResult` 的已知字段，导致 mcp-use 的
/// `skills/list` 响应在 untagged `ServerResult` 中先命中 `CallToolResult`；
/// 3.9.0 修复后仍须保证该响应不会被当成携带空技能目录的结果。
#[test]
fn skills_list_meta_only_result_is_not_a_tool_result() {
    let json = serde_json::json!({
        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "mcp-use", "version": "2.12.4"}},
        "resultType": "complete"
    });
    let parsed: ServerResult = serde_json::from_value(json).unwrap();
    assert!(!matches!(parsed, ServerResult::CallToolResult(_)));
    // 该响应不携带 skills 载荷：归一化必须失败，而不是冒充空目录。
    assert!(skill_result_payload(&parsed).is_none());
}

/// [回归测试] 完整 skills + 带 `size` 的 resources 命中 3.9.0 的
/// `ListSkillsResult`，归一化后仍按 `SkillListResponse` 解析（`size` 被忽略）。
#[test]
fn skills_list_typed_result_with_sizes_normalizes() {
    let json = serde_json::json!({
        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "mcp-use"}},
        "skills": [{
            "uri": "skill://a/SKILL.md",
            "frontmatter": {"name": "a", "description": "A"},
            "resources": [{"uri": "skill://a/SKILL.md", "digest": "sha256:abcd", "size": 42}]
        }]
    });
    let parsed: ServerResult = serde_json::from_value(json).unwrap();
    assert!(matches!(parsed, ServerResult::ListSkillsResult(_)));
    let payload = skill_result_payload(&parsed).expect("typed payload");
    let response: SkillListResponse = serde_json::from_value(payload).unwrap();
    assert_eq!(response.skills.len(), 1);
    let entry = entry_from_dto(response.skills[0].clone()).unwrap();
    assert_eq!(
        entry.resources.unwrap()[0].digest,
        "sha256:abcd",
        "size 不在领域模型内，digest 必须保留"
    );
}

/// [回归测试] 动态技能的 `resources` 在类型化结果中是字符串 `"dynamic"`：
/// 归一化后按缺省 resources 解析，同页静态技能不得因整页解析失败而丢失。
#[test]
fn skills_list_typed_result_with_dynamic_skill_keeps_static_siblings() {
    let json = serde_json::json!({
        "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "mcp-use"}},
        "skills": [
            {
                "uri": "skill://static/SKILL.md",
                "frontmatter": {"name": "static", "description": "S"},
                "resources": [{"uri": "skill://static/SKILL.md", "digest": "sha256:abcd", "size": 42}]
            },
            {
                "uri": "skill://dynamic/SKILL.md",
                "frontmatter": {"name": "dynamic", "description": "D"},
                "resources": "dynamic"
            }
        ]
    });
    let parsed: ServerResult = serde_json::from_value(json).unwrap();
    assert!(matches!(parsed, ServerResult::ListSkillsResult(_)));
    let payload = skill_result_payload(&parsed).expect("typed payload");
    let response: SkillListResponse =
        serde_json::from_value(payload).expect("含动态技能的整页仍可解析");
    assert_eq!(response.skills.len(), 2);
    let static_entry = entry_from_dto(response.skills[0].clone()).unwrap();
    assert_eq!(
        static_entry.resources.expect("静态技能 manifest")[0].digest,
        "sha256:abcd"
    );
    let dynamic_entry = entry_from_dto(response.skills[1].clone()).unwrap();
    assert!(
        dynamic_entry.resources.is_none(),
        "动态技能无 digest：归一化为缺省 resources（保守激活）"
    );
}

/// [回归测试] 省略 `resources`（mcp-use 实际形状）落到 `CustomResult`，
/// 归一化后仍可解析；与类型化分支共用同一领域响应。
#[test]
fn skills_list_custom_result_without_resources_normalizes() {
    let json = serde_json::json!({
        "skills": [{"uri": "skill://a/SKILL.md", "frontmatter": {"name": "a", "description": "A"}}]
    });
    let parsed: ServerResult = serde_json::from_value(json).unwrap();
    assert!(matches!(parsed, ServerResult::CustomResult(_)));
    let payload = skill_result_payload(&parsed).expect("custom payload");
    let response: SkillListResponse = serde_json::from_value(payload).unwrap();
    assert_eq!(response.skills.len(), 1);
    let entry = entry_from_dto(response.skills[0].clone()).unwrap();
    assert!(entry.resources.is_none());
}

/// [回归测试] `skills/get` 的完整条目命中 `GetSkillResult`，归一化后按
/// `SkillGetResponse` 解析（与 list 条目同构）。
#[test]
fn skills_get_typed_result_normalizes() {
    let json = serde_json::json!({
        "skill": {
            "uri": "skill://a/SKILL.md",
            "frontmatter": {"name": "a", "description": "A"},
            "resources": [{"uri": "skill://a/SKILL.md", "digest": "sha256:abcd", "size": 42}]
        }
    });
    let parsed: ServerResult = serde_json::from_value(json).unwrap();
    assert!(matches!(parsed, ServerResult::GetSkillResult(_)));
    let payload = skill_result_payload(&parsed).expect("typed payload");
    let response: SkillGetResponse = serde_json::from_value(payload).unwrap();
    let entry = entry_from_dto(response.skill).unwrap();
    assert_eq!(entry.uri, "skill://a/SKILL.md");
}

/// [回归测试] `skills/get` 的动态技能同样把字符串 `"dynamic"` 归一化为缺省
/// resources；DTO 本身仍拒绝任意字符串。
#[test]
fn skills_get_typed_result_with_dynamic_resources_normalizes() {
    let json = serde_json::json!({
        "skill": {
            "uri": "skill://a/SKILL.md",
            "frontmatter": {"name": "a", "description": "A"},
            "resources": "dynamic"
        }
    });
    let parsed: ServerResult = serde_json::from_value(json).unwrap();
    assert!(matches!(parsed, ServerResult::GetSkillResult(_)));
    let payload = skill_result_payload(&parsed).expect("typed payload");
    let response: SkillGetResponse = serde_json::from_value(payload).unwrap();
    let entry = entry_from_dto(response.skill).unwrap();
    assert!(
        entry.resources.is_none(),
        "动态技能按缺省 resources 保守激活"
    );
}

// ─── entry_to_metadata（W2：发现期只做结构校验，不读正文）─────────────────

const SPEC_MD: &str = "---\nname: a\ndescription: A skill\n---\n\n# A\n";
