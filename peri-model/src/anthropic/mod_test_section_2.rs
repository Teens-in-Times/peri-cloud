#[test]
fn request_contract_without_cache_uses_plain_top_level_system() {
    let body = request_functions::body_for_test(&config().without_cache(), &request());
    assert!(body["system"].is_string());
    assert!(!body["system"]
        .as_str()
        .expect("system text")
        .contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY));
    assert!(body["messages"][0]["content"][0]
        .get("cache_control")
        .is_none());
}

#[test]
fn system_cache_boundary_four_state_matrix_and_fallback_are_explicit() {
    let cases = [
        (
            format!("STATIC{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}\n\nDYNAMIC"),
            json!([
                { "type": "text", "text": "STATIC", "cache_control": { "type": "ephemeral" } },
                { "type": "text", "text": "DYNAMIC" },
            ]),
        ),
        (
            format!("STATIC{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}"),
            json!([
                { "type": "text", "text": "STATIC", "cache_control": { "type": "ephemeral" } },
            ]),
        ),
        (
            format!("{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}\n\nDYNAMIC"),
            json!([{ "type": "text", "text": "DYNAMIC" }]),
        ),
        (
            "LEGACY-WITHOUT-BOUNDARY".to_string(),
            json!([
                { "type": "text", "text": "LEGACY-WITHOUT-BOUNDARY", "cache_control": { "type": "ephemeral" } },
            ]),
        ),
    ];
    for (system_text, expected) in cases {
        let body = request_functions::body_for_test(
            &config(),
            &ModelRequest::new(vec![
                ModelMessage::system_text(system_text),
                ModelMessage::user_text("go"),
            ]),
        );
        assert_eq!(body["system"], expected);
        assert!(!serde_json::to_string(&body["system"])
            .expect("system serializes")
            .contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY));
    }

    let empty = request_functions::body_for_test(
        &config(),
        &ModelRequest::new(vec![
            ModelMessage::system_text(""),
            ModelMessage::user_text("go"),
        ]),
    );
    assert!(empty.get("system").is_none());
}

#[test]
fn repeated_system_cache_boundary_fails_closed_without_wire_leak() {
    let system = format!(
        "STATIC{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}DYNAMIC{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}TAIL"
    );
    let body = request_functions::body_for_test(
        &config(),
        &ModelRequest::new(vec![
            ModelMessage::system_text(system),
            ModelMessage::user_text("go"),
        ]),
    );
    let blocks = body["system"].as_array().expect("system blocks");

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["text"], "STATICDYNAMICTAIL");
    assert!(blocks[0].get("cache_control").is_none());
    assert!(!serde_json::to_string(blocks)
        .expect("blocks serialize")
        .contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY));
}

#[test]
fn duplicate_boundary_plus_dynamic_contribution_remains_uncached_and_byte_preserving() {
    let base = format!("A{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}B{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}C");
    let combined =
        prompt_cache::combine_system_prompt_with_dynamic(Some(&base), "REQUEST").unwrap();
    let body = request_functions::body_for_test(
        &config(),
        &ModelRequest::new(vec![
            ModelMessage::system_text(combined),
            ModelMessage::user_text("go"),
        ]),
    );
    let blocks = body["system"].as_array().expect("system blocks");
    assert_eq!(
        blocks,
        &[json!({ "type": "text", "text": "ABC\n\nREQUEST" })]
    );
    assert!(blocks[0].get("cache_control").is_none());
}

#[test]
fn system_cache_prefix_is_stable_and_dynamic_order_is_preserved() {
    let body = |dynamic: &str| {
        request_functions::body_for_test(
            &config(),
            &ModelRequest::new(vec![
                ModelMessage::system_text(format!(
                    "BASE-STATIC{SYSTEM_PROMPT_DYNAMIC_BOUNDARY}\n\n{dynamic}"
                )),
                ModelMessage::system_text("REQUEST-MIDDLEWARE"),
                ModelMessage::user_text("go"),
            ]),
        )
    };
    let first = body("BASE-DYNAMIC-A");
    let second = body("BASE-DYNAMIC-B");
    let first_system = first["system"].as_array().expect("system blocks");
    let second_system = second["system"].as_array().expect("system blocks");

    assert_eq!(
        serde_json::to_vec(&first_system[0]).expect("static block serializes"),
        serde_json::to_vec(&second_system[0]).expect("static block serializes"),
        "动态 suffix 改变时 cached block 必须逐字节稳定"
    );
    assert_eq!(
        first_system[1]["text"],
        "BASE-DYNAMIC-A\n\nREQUEST-MIDDLEWARE"
    );
    assert_eq!(
        second_system[1]["text"],
        "BASE-DYNAMIC-B\n\nREQUEST-MIDDLEWARE"
    );
    assert!(first_system[1].get("cache_control").is_none());
    assert!(second_system[1].get("cache_control").is_none());
}

/// [回归测试] 只读输出预算接口与 wire 解析同源（H6）。
///
/// Compact 摘要器据此沿用 provider 已解析的单次输出上限：既不在请求里覆写成
/// 独立常量，也不代填任意值。未声明请求预算时 wire 仍落到同一已解析上限。
#[test]
fn output_token_limit_matches_the_resolved_wire_budget() {
    let model = AnthropicModel::new(
        config()
            .with_max_tokens(4_096)
            .with_extended_thinking(1_024, "high"),
    );
    assert_eq!(model.output_token_limit(), Some(4_096));
    let body = request_functions::body_for_test(
        &model.config,
        &ModelRequest::new(vec![ModelMessage::user_text("summarize")]),
    );
    assert_eq!(body["max_tokens"], 4_096);
    // 摘要路径只沿用上限，不改写 thinking 配置。
    assert_eq!(
        body["thinking"],
        json!({ "type": "enabled", "budget_tokens": 1_024 })
    );
    assert_eq!(body["output_config"], json!({ "effort": "high" }));
}

/// [回归测试] Anthropic 工具图片保持在 `tool_result.content` 原生位置。
#[test]
fn tool_images_stay_native_inside_tool_result() {
    let request = ModelRequest::new(vec![
        ModelMessage::assistant(
            vec![],
            vec![ToolCall::new("call-image", "Read", JsonObject::default())],
        ),
        ModelMessage::tool_result(ToolResult {
            id: None,
            tool_call_id: "call-image".into(),
            name: "Read".into(),
            content: vec![
                ContentBlock::text("Image read"),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: MediaType::new("image/png"),
                        data: "AQID".into(),
                    },
                },
            ],
            is_error: false,
        }),
    ]);

    let body = request_functions::body_for_test(&config(), &request);
    let blocks = body["messages"][1]["content"]
        .as_array()
        .expect("content array");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "tool_result");
    let content = blocks[0]["content"].as_array().expect("tool content array");
    assert_eq!(content[0], json!({ "type": "text", "text": "Image read" }));
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[1]["source"]["data"], "AQID");
}
