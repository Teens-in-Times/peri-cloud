use super::*;

#[tokio::test]
async fn main_requires_an_authorized_explicit_device_and_can_call_it_without_preparing() {
    let mut f = Fixture::new_for(vec![], true).await;
    f.pair().await;
    // Start a device-free conversation, then register an executor without switching it.
    for device in &f.devices {
        f.c()
            .identity
            .revoke_device(&f.c().browser, device.id)
            .await
            .unwrap();
    }
    f.c()
        .gateway
        .receive(f.message("cloud-start", "/new"))
        .await
        .unwrap();
    f.add_device(2).await;
    let device = f.devices[2].id;
    let foreign = Uuid::new_v4();
    let mut step = 0;
    let mut invocation = Value::Null;
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let reply = match step {
            0 => {
                let tools = request["tools"].as_array().unwrap();
                for name in [
                    "Read",
                    "Write",
                    "Edit",
                    "Glob",
                    "Grep",
                    "Bash",
                    "GetExecution",
                    "CancelExecution",
                ] {
                    let schema = &tools
                        .iter()
                        .find(|t| t["function"]["name"] == name)
                        .unwrap()["function"]["parameters"];
                    assert!(schema["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("device_id")));
                }
                assert!(!tools
                    .iter()
                    .any(|t| t["function"]["name"] == "execute_device"));
                call(
                    "Write",
                    json!({"file_path":"missing.txt","content":"no"}),
                    "missing-device",
                )
            }
            1 => {
                assert!(request["messages"]
                    .to_string()
                    .contains("device_id is required"));
                call(
                    "Write",
                    json!({"device_id":foreign,"file_path":"foreign.txt","content":"no"}),
                    "foreign-device",
                )
            }
            2 => call(
                "Write",
                json!({"device_id":device,"_device_target":{},"file_path":"forged.txt","content":"no"}),
                "forged-target",
            ),
            3 => call(
                "Write",
                json!({"device_id":device,"file_path":"explicit.txt","content":"explicit device"}),
                "valid-device",
            ),
            4 => call("inspect_device_operations", json!({}), "operations"),
            5 => {
                let entries = tool_value(request, "operations");
                let op = &entries["operations"][0];
                assert_eq!(entries["operations"].as_array().unwrap().len(), 1);
                assert_eq!(op["device_id"], device.to_string());
                invocation = op["invocation_id"].clone();
                call(
                    "GetExecution",
                    json!({"device_id":device,"task_id":op["invocation_id"]}),
                    "original-control",
                )
            }
            6 => {
                assert!(request["messages"].to_string().contains("explicit device"));
                let calls: Vec<_> = ["cancel-first", "cancel-duplicate"].into_iter().enumerate().map(|(index, id)| json!({
                    "index":index, "id":id, "type":"function", "function":{"name":"CancelExecution", "arguments":json!({"device_id":device,"task_id":invocation}).to_string()}
                })).collect();
                let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":calls},"finish_reason":"tool_calls"}]});
                format!("data: {delta}\n\ndata: [DONE]\n\n")
            }
            7 => answer("finished on the explicitly chosen device"),
            _ => panic!("unexpected step {step}"),
        };
        step += 1;
        reply
    }));
    let receipt = f
        .c()
        .gateway
        .receive(f.message("explicit-cloud-operation", "write on device one"))
        .await
        .unwrap();
    let parent = receipt.session_id.unwrap();
    let turn = receipt.turn_id.unwrap();
    let card = super::device_tests::next_card(&f).await;
    assert_eq!(card.device_id, Some(device));
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    assert_eq!(f.submissions[2].load(Ordering::SeqCst), 0);
    f.c()
        .gateway
        .respond_browser(
            &f.c().browser,
            card.request_id,
            InteractionAction::AllowOnce {},
        )
        .await
        .unwrap();
    f.settled(parent, turn).await;
    assert_eq!(f.submissions[2].load(Ordering::SeqCst), 1);
    assert_eq!(
        f.cancellations[2].load(Ordering::SeqCst),
        1,
        "duplicate controls query the original receipt instead of posting cancellation twice"
    );
    assert_eq!(
        std::fs::read_to_string(f.workspaces[2].path().join("explicit.txt")).unwrap(),
        "explicit device"
    );
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, parent)
            .await
            .unwrap()
            .frozen
            .device_id,
        None
    );
    f.finish().await;
}

#[tokio::test]
async fn workers_have_no_device_argument_and_cannot_override_their_execution_session() {
    let f = Fixture::new_for(vec![], true).await;
    let device = f.devices[0].id;
    let other = f.devices[1].id;
    let mut main_step = 0;
    let mut worker_step = 0;
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let worker = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "message_parent");
        if worker {
            let tools = request["tools"].as_array().unwrap();
            for name in ["Read", "Write", "Edit", "Glob", "Grep", "Bash"] {
                let schema = &tools
                    .iter()
                    .find(|t| t["function"]["name"] == name)
                    .unwrap()["function"]["parameters"];
                assert!(schema["properties"].get("device_id").is_none());
                assert!(schema["properties"].get("workspace").is_none());
            }
            let reply = match worker_step {
                0 => call(
                    "Write",
                    json!({"device_id":other,"file_path":"escape.txt","content":"no"}),
                    "escape-device",
                ),
                1 => {
                    assert!(request["messages"]
                        .to_string()
                        .contains("host-bound device"));
                    tool_write()
                }
                2 => answer("worker finished in its bound workspace"),
                _ => panic!("unexpected worker step {worker_step}"),
            };
            worker_step += 1;
            reply
        } else {
            let reply = match main_step {
                0 => call(
                    "delegate_task",
                    json!({"device_id":device,"goal":"write agent.txt"}),
                    "delegate",
                ),
                1 => answer("delegated"),
                _ => answer("worker result received"),
            };
            main_step += 1;
            reply
        }
    }));
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), device, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "delegate the write".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    pump(&f, 6).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
    assert_eq!(f.submissions[1].load(Ordering::SeqCst), 0);
    assert!(!f.workspaces[1].path().join("escape.txt").exists());
    f.finish().await;
}
