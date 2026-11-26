
/// rename 只有两端都在 workflow journal 内才是宿主产物：从允许范围外 rename
/// 进 journal 时，源路径的消失仍必须报告。
#[test]
fn rename_into_the_workflow_journal_does_not_hide_the_missing_source() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    let canonical = tmp
        .path()
        .canonicalize()
        .expect("the temporary directory must be canonicalizable");
    let intent = peri_acp_types::workflow::WorkflowWriteIntent::Write {
        repo_root: canonical.to_string_lossy().into_owned(),
        cwd: canonical.to_string_lossy().into_owned(),
        path_allowlist: vec!["src".into()],
        head_may_change: false,
        commit_required: None,
    };
    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");

    let run_dir = tmp.path().join(".claude/workflow-runs/run-1");
    std::fs::create_dir_all(&run_dir).expect("the run directory must be creatable");
    run_git(&[
        "mv",
        "tracked.txt",
        ".claude/workflow-runs/run-1/tracked.txt",
    ]);

    let failure = baseline
        .verify_postcondition(Some(&intent))
        .expect_err("allowlist 外的源文件消失必须报告");
    assert!(
        failure.is_violation(),
        "allowlist 越界是可归因违例: {failure}"
    );
    assert!(
        failure.to_string().contains("tracked.txt"),
        "报告必须带源路径: {failure}"
    );
}

/// 非 ASCII 工作目录下 journal 路径也必须被识别：`-z` 避免 git 的 C-quote。
#[test]
fn workflow_journal_is_excluded_under_a_non_ascii_nested_cwd() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let nested = tmp.path().join("子目录");
    std::fs::create_dir_all(&nested).expect("the nested working directory must be creatable");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    let baseline = GitBaseline::capture(&nested)
        .expect("the capture fixture result is required for this assertion");

    let run_dir = nested.join(".claude/workflow-runs/run-1");
    std::fs::create_dir_all(&run_dir).expect("the run directory must be creatable");
    std::fs::write(run_dir.join("script.js"), "return null")
        .expect("the test fixture file must be writable");
    baseline
        .verify_unchanged()
        .expect("非 ASCII cwd 下的自身 journal 不构成外部变化");
}

/// 两个不同的非 UTF-8 路径若有损解码会折叠成同一个键，从而漏掉变化（false green）。
/// rename 的原始路径是独立字段，解析层必须把两端都留成各自的字节键。
#[test]
fn parse_keeps_distinct_non_utf8_paths_and_rename_endpoints() {
    let untracked = b"? swap-\xff.bin\0? swap-\xfe.bin\0";
    let records = crate::journal::git::parse_status_paths(untracked)
        .expect("non-UTF-8 status must remain parseable");
    assert_eq!(
        records.len(),
        2,
        "distinct byte paths must not collapse into one key"
    );
    assert!(records.contains_key(b"swap-\xff.bin".as_slice()));
    assert!(records.contains_key(b"swap-\xfe.bin".as_slice()));

    let renamed = b"2 R. N... 100644 100644 100644 aaaa bbbb R100 new-\xff.txt\0old-\xfe.txt\0";
    let records = crate::journal::git::parse_status_paths(renamed)
        .expect("rename records must remain parseable");
    assert_eq!(records.len(), 2, "rename source must be its own record key");
    for endpoint in [b"new-\xff.txt".as_slice(), b"old-\xfe.txt".as_slice()] {
        let record = records
            .get(endpoint)
            .unwrap_or_else(|| panic!("missing rename endpoint {endpoint:?}"));
        assert_eq!(
            record.first(),
            Some(&b'2'),
            "rename record type must survive"
        );
    }
}

/// 采到的 status 含非 UTF-8 路径时，对账必须按字节工作，不能退化成
/// observation incomplete；报告中的路径允许有损展示。
#[test]
fn non_utf8_status_path_does_not_become_an_incomplete_observation() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    let mut baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    // macOS APFS 拒绝创建非 UTF-8 文件名，这里直接注入等价的 porcelain 记录。
    baseline.status_porcelain_v2 = b"? dirty-\xff.txt\0".to_vec();

    let failure = baseline
        .verify_unchanged()
        .expect_err("注入的未跟踪记录是真实差异");
    assert!(
        matches!(
            failure,
            crate::journal::PostconditionFailure::Unattributed(_)
        ),
        "非 UTF-8 路径不得被读成 observation incomplete: {failure}"
    );
    let message = failure.to_string();
    assert!(
        message.contains("dirty-"),
        "报告必须带可定位路径（展示可有损）: {message}"
    );
}

/// 真实文件系统上的非 UTF-8 文件名：采样不失败，两个不同路径的 rename 两端都能被区分。
/// macOS 的 APFS 拒绝此类文件名（EILSEQ），因此只在 Linux 验证。
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_paths_are_sampled_and_reconciled_by_bytes() {
    use std::os::unix::ffi::OsStrExt;

    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    let first = std::ffi::OsStr::from_bytes(b"swap-\xff.bin");
    std::fs::write(tmp.path().join(first), b"payload")
        .expect("the test fixture file must be writable");
    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    baseline
        .verify_unchanged()
        .expect("非 UTF-8 路径属于基线，既不是变化也不是观测不完整");

    let second = std::ffi::OsStr::from_bytes(b"swap-\xfe.bin");
    std::fs::rename(tmp.path().join(first), tmp.path().join(second))
        .expect("the test fixture must be renamable");
    let failure = baseline
        .verify_unchanged()
        .expect_err("两个不同非 UTF-8 路径之间的 rename 必须被观察到");
    assert!(
        matches!(
            failure,
            crate::journal::PostconditionFailure::Unattributed(_)
        ),
        "非 UTF-8 路径不得被读成 observation incomplete: {failure}"
    );
    let message = failure.to_string();
    assert_eq!(
        message.matches("swap-").count(),
        2,
        "rename 的两端必须各自作为独立字节路径参与对账: {message}"
    );
}

/// 符号链接指向的同一目录必须得到同一分类：损坏的仓库是“无法观测”，
/// 不能因为按字面路径找不到 `.git` marker 而读成“没有仓库范围”。
#[cfg(unix)]
#[test]
fn broken_repository_behind_a_symlink_is_an_incomplete_observation() {
    let tmp = TempDir::new().expect("the temporary test directory must be available");
    let repo = tmp.path().join("repo");
    let sub = repo.join("sub");
    std::fs::create_dir_all(&sub).expect("the test fixture directory must be creatable");
    std::fs::write(repo.join(".git"), "gitdir: /nonexistent/peri-issue-522\n")
        .expect("the test fixture file must be writable");
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&sub, &alias).expect("the test fixture symlink must be creatable");

    let observation = GitBaseline::capture_for_intent(&alias, None)
        .expect("sampling failure must not fail start");
    assert!(
        matches!(observation, GitObservation::Incomplete(_)),
        "同一目录经符号链接访问仍是采样失败: {observation:?}"
    );
}

#[test]
fn test_write_postcondition_ignores_unchanged_preexisting_dirty_path() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("allowed.txt"), "base\n")
        .expect("the test fixture file must be writable");
    std::fs::write(tmp.path().join("outside.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "."]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    std::fs::write(tmp.path().join("outside.txt"), "pre-existing dirty\n")
        .expect("the test fixture file must be writable");

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    let intent = peri_acp_types::workflow::WorkflowWriteIntent::Write {
        repo_root: tmp.path().to_string_lossy().into_owned(),
        cwd: tmp.path().to_string_lossy().into_owned(),
        path_allowlist: vec!["allowed.txt".into()],
        head_may_change: false,
        commit_required: None,
    };
    std::fs::write(tmp.path().join("allowed.txt"), "changed\n")
        .expect("the test fixture file must be writable");

    baseline
        .verify_postcondition(Some(&intent))
        .expect("the verify postcondition fixture result is required for this assertion");
}

#[test]
fn test_write_postcondition_allows_only_allowlisted_change() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("allowed.txt"), "base\n")
        .expect("the test fixture file must be writable");
    std::fs::write(tmp.path().join("outside.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "."]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    let intent = peri_acp_types::workflow::WorkflowWriteIntent::Write {
        repo_root: tmp.path().to_string_lossy().into_owned(),
        cwd: tmp.path().to_string_lossy().into_owned(),
        path_allowlist: vec!["allowed.txt".into()],
        head_may_change: false,
        commit_required: None,
    };
    baseline
        .validate_write_intent(&intent)
        .expect("the validate write intent fixture result is required for this assertion");
    std::fs::write(tmp.path().join("allowed.txt"), "changed\n")
        .expect("the test fixture file must be writable");
    baseline
        .verify_postcondition(Some(&intent))
        .expect("the verify postcondition fixture result is required for this assertion");
    std::fs::write(tmp.path().join("outside.txt"), "escaped\n")
        .expect("the test fixture file must be writable");
    let error = baseline.verify_postcondition(Some(&intent)).unwrap_err();
    assert!(error.is_violation(), "allowlist 越界必须是可归因违例");
    let message = error.to_string();
    assert!(message.contains("escaped"));
    assert!(
        message.contains("outside.txt"),
        "越界报告必须带路径: {message}"
    );
}

#[test]
fn test_write_intent_rejects_parent_traversal() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .status()
        .expect("the status fixture result is required for this assertion")
        .success());
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    assert!(std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(tmp.path())
        .status()
        .expect("the status fixture result is required for this assertion")
        .success());
    assert!(std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Peri Test",
            "-c",
            "user.email=peri-test@example.invalid",
            "commit",
            "-qm",
            "baseline"
        ])
        .current_dir(tmp.path())
        .status()
        .expect("the status fixture result is required for this assertion")
        .success());
    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    let intent = peri_acp_types::workflow::WorkflowWriteIntent::Write {
        repo_root: tmp.path().to_string_lossy().into_owned(),
        cwd: tmp.path().to_string_lossy().into_owned(),
        path_allowlist: vec!["../outside".into()],
        head_may_change: false,
        commit_required: None,
    };
    assert!(baseline.validate_write_intent(&intent).is_err());
}

// ─── extract_long_texts / write_output 测试 ────────────────────

/// 顶层 return 值是字符串：固定 label `return`，占位符与文件必须同源。
#[test]
fn test_extract_long_texts_top_level_string_uses_return_label() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let long_text = "r".repeat(250);
    let mut value = serde_json::json!(long_text.clone());

    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);

    assert_eq!(extracted, vec!["return"]);
    assert_eq!(value, serde_json::json!("${return}"));
    assert_eq!(
        std::fs::read_to_string(store.run_dir("r1").join("outputs").join("return.txt"))
            .expect("the test fixture text file must be readable"),
        long_text
    );
}

/// 字符串数组元素也外置：label 沿用 `[i]` hint，短字符串保持原样。
#[test]
fn test_extract_long_texts_string_array_elements() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let long_text = "e".repeat(250);
    let mut value = serde_json::json!([long_text.clone(), "short"]);

    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);

    assert_eq!(extracted, vec!["[0]"]);
    assert_eq!(value[0], "${[0]}");
    assert_eq!(value[1], "short");
    assert_eq!(
        std::fs::read_to_string(store.run_dir("r1").join("outputs").join("[0].txt"))
            .expect("the test fixture text file must be readable"),
        long_text
    );
}

/// [回归测试] label 归一：分隔符/控制字符替换、连续点折叠、空 label 回落、
/// 重名去重（同一 label 不得覆盖）。
#[test]
fn test_output_label_sanitizes_and_dedups() {
    let mut used = std::collections::HashSet::new();
    assert_eq!(
        crate::journal::output::output_label("a.b[0]", &mut used),
        "a.b[0]"
    );
    assert_eq!(
        crate::journal::output::output_label("raw/crates", &mut used),
        "raw_crates"
    );
    assert_eq!(
        crate::journal::output::output_label("raw\\crates", &mut used),
        "raw_crates-2"
    );
    assert_eq!(crate::journal::output::output_label("..", &mut used), "._");
    assert_eq!(
        crate::journal::output::output_label("", &mut used),
        "unnamed"
    );
}

/// [回归测试] 占位符与文件名同源：label 含 `/`（非法路径字符）时，state.json 写入的
/// `${label}` 必须指向真实存在的 `outputs/<label>.txt`；两个会归一到同一名字的
/// label 不得互相覆盖（2026-10-11 issue：引用断裂 / 内容被覆盖）。
#[test]
fn test_extract_long_texts_label_matches_output_file() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let first = "a".repeat(250);
    let second = "b".repeat(260);
    let mut value = serde_json::json!({"raw/crates": first.clone(), "raw_crates": second.clone()});
    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);
    let mut labels = extracted.clone();
    labels.sort();
    assert_eq!(labels, vec!["raw_crates", "raw_crates-2"]);

    // 每个占位符都必须解析到真实文件，两份内容都不丢。
    let outputs = store.run_dir("r1").join("outputs");
    let mut resolved: Vec<(String, String)> = ["raw/crates", "raw_crates"]
        .into_iter()
        .map(|key| {
            let placeholder = value[key].as_str().expect("长文本应替换为占位符");
            let name = placeholder
                .strip_prefix("${")
                .and_then(|rest| rest.strip_suffix('}'))
                .expect("占位符形态必须为 ${label}");
            let content = std::fs::read_to_string(outputs.join(format!("{name}.txt")))
                .unwrap_or_else(|error| panic!("占位符 {placeholder} 未指向真实文件: {error}"));
            (name.to_string(), content)
        })
        .collect();
    resolved.sort();
    assert_eq!(
        resolved,
        vec![
            ("raw_crates".to_string(), first),
            ("raw_crates-2".to_string(), second),
        ]
    );
}

#[test]
fn test_extract_long_texts_empty() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let mut value = serde_json::json!({});
    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);
    assert!(extracted.is_empty());
    assert_eq!(value, serde_json::json!({}));
}

#[test]
fn test_extract_long_texts_short_strings() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let mut value = serde_json::json!({"k": "short"});
    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);
    assert!(extracted.is_empty());
    assert_eq!(value, serde_json::json!({"k": "short"}));

    // 顶层短字符串同样不外置。
    let mut top_level = serde_json::json!("short");
    let extracted = crate::journal::output::extract_long_texts(&mut top_level, "r1", &store, 200);
    assert!(extracted.is_empty());
    assert_eq!(top_level, serde_json::json!("short"));
}

#[test]
fn test_extract_long_texts_top_level() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let long_str = "a".repeat(250);
    let mut value = serde_json::json!({"result": long_str.clone()});
    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0], "result");
    // 原位置替换为 ${label}
    assert_eq!(
        value["result"]
            .as_str()
            .expect("the as str fixture result is required for this assertion"),
        "${result}"
    );
    // 验证文件写入
    let content = std::fs::read_to_string(store.run_dir("r1").join("outputs").join("result.txt"))
        .expect("the test fixture text file must be readable");
    assert_eq!(content, long_str);
}

#[test]
fn test_extract_long_texts_array() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let long1 = "b".repeat(300);
    let long2 = "c".repeat(300);
    let mut value = serde_json::json!({"items": [
        {"result": long1.clone()},
        {"result": long2.clone()}
    ]});
    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);
    assert_eq!(extracted.len(), 2);
    // 使用索引标签，无覆盖
    assert_eq!(extracted[0], "items[0].result");
    assert_eq!(extracted[1], "items[1].result");
    // 原位置替换
    assert_eq!(
        value["items"][0]["result"]
            .as_str()
            .expect("the as str fixture result is required for this assertion"),
        "${items[0].result}"
    );
    assert_eq!(
        value["items"][1]["result"]
            .as_str()
            .expect("the as str fixture result is required for this assertion"),
        "${items[1].result}"
    );
    // 验证两个文件均正确写入，无覆盖
    let content0 = std::fs::read_to_string(
        store
            .run_dir("r1")
            .join("outputs")
            .join("items[0].result.txt"),
    )
    .expect("the test fixture text file must be readable");
    assert_eq!(content0, long1);
    let content1 = std::fs::read_to_string(
        store
            .run_dir("r1")
            .join("outputs")
            .join("items[1].result.txt"),
    )
    .expect("the test fixture text file must be readable");
    assert_eq!(content1, long2);
}

#[test]
fn test_extract_long_texts_nested() {
    let (_tmp, store) = make_store();
    store
        .init_run("r1", "script")
        .expect("the init run fixture result is required for this assertion");
    let inner_long = "d".repeat(250);
    let mut value = serde_json::json!({
        "outer": {
            "inner": inner_long.clone()
        }
    });
    let extracted = crate::journal::output::extract_long_texts(&mut value, "r1", &store, 200);
    assert_eq!(extracted.len(), 1);
    // 点号分隔标签
    assert_eq!(extracted[0], "outer.inner");
    assert_eq!(
        value["outer"]["inner"]
            .as_str()
            .expect("the as str fixture result is required for this assertion"),
        "${outer.inner}"
    );
    let content =
        std::fs::read_to_string(store.run_dir("r1").join("outputs").join("outer.inner.txt"))
            .expect("the test fixture text file must be readable");
    assert_eq!(content, inner_long);
}

/// [回归测试] 文件写失败时不能把仍未持久化的原文替换成不可读取的引用。
#[test]
fn test_extract_long_texts_preserves_content_when_output_write_fails() {
    let (_tmp, store) = make_store();
    store
        .init_run("failed-output", "script")
        .expect("the init run fixture result is required for this assertion");
    let outputs = store.run_dir("failed-output").join("outputs");
    std::fs::create_dir_all(outputs.join("blocked.txt"))
        .expect("the test fixture directory must be creatable");
    let original = "retained".repeat(40);
    let saved = "persisted".repeat(40);
    let mut value = serde_json::json!({"blocked": original, "saved": saved});
    let extracted =
        crate::journal::output::extract_long_texts(&mut value, "failed-output", &store, 200);
    assert_eq!(extracted, vec!["saved"]);
    assert_eq!(value["blocked"], original, "失败分支必须保留可用正文");
    assert_eq!(value["saved"], "${saved}");
    assert_eq!(
        std::fs::read_to_string(outputs.join("saved.txt"))
            .expect("the test fixture text file must be readable"),
        saved
    );
}

#[test]
fn attempt_tracker_makes_engine_retries_visible_and_preserves_recovery() {
    let mut tracker = AttemptTracker::default();
    tracker.record_dispatch("run-1", 7, "task-1");
    tracker.record_dispatch("run-1", 7, "task-2");
    // 同一 task 的重复通知不得重复计数。
    tracker.record_dispatch("run-1", 7, "task-2");
    let mut entry = JournalEntry {
        key: "key-7".into(),
        seq: 7,
        result: AgentRunResult::Skipped,
        attempt: Some(WorkflowAttempt {
            run_id: "run-1".into(),
            agent_id: None,
            journal_seq: 7,
            attempt_index: None,
            recovered_from: None,
            consumed: true,
            disposition: AttemptDisposition::Produced,
            execution: None,
        }),
    };
    tracker.annotate(&mut entry);
    let attempt = entry.attempt.as_ref().expect("attempt must remain present");
    assert_eq!(attempt.attempt_index, Some(2), "两次派发必须计为两次尝试");
    assert_eq!(
        attempt.agent_id,
        Some(7),
        "host 可证明的 agent identity 应回填"
    );

    let mut recovered = JournalEntry {
        key: "key-8".into(),
        seq: 8,
        result: AgentRunResult::Skipped,
        attempt: Some(WorkflowAttempt {
            run_id: "run-1".into(),
            agent_id: Some(8),
            journal_seq: 8,
            attempt_index: Some(3),
            recovered_from: None,
            consumed: true,
            disposition: AttemptDisposition::Recovered,
            execution: None,
        }),
    };
    tracker.annotate(&mut recovered);
    assert_eq!(
        recovered
            .attempt
            .as_ref()
            .and_then(|attempt| attempt.attempt_index),
        Some(3),
        "缓存恢复不得把旧尝试序号重置为本 run 的派发数"
    );
}

#[test]
fn no_write_intent_still_samples_git_baseline_inside_a_repository() {
    let tmp = TempDir::new().expect("the temporary test directory must be available");
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .status()
        .expect("the status fixture result is required for this assertion")
        .success());
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    assert!(std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(tmp.path())
        .status()
        .expect("the status fixture result is required for this assertion")
        .success());
    assert!(std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Peri Test",
            "-c",
            "user.email=peri-test@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ])
        .current_dir(tmp.path())
        .status()
        .expect("the status fixture result is required for this assertion")
        .success());
    let canonical = tmp
        .path()
        .canonicalize()
        .expect("the temporary directory must be canonicalizable");

    for intent in [
        None,
        Some(&peri_acp_types::workflow::WorkflowWriteIntent::ReadOnly),
    ] {
        let observation = GitBaseline::capture_for_intent(tmp.path(), intent)
            .expect("sampling without a write intent must not fail start");
        match observation {
            GitObservation::Captured(baseline) => {
                assert_eq!(baseline.repo_root, canonical);
                assert_eq!(baseline.cwd, canonical);
            }
            other => panic!("inside a repository the baseline must be sampled: {other:?}"),
        }
    }
}

#[test]
fn broken_git_repository_is_incomplete_not_not_applicable() {
    let tmp = TempDir::new().expect("the temporary test directory must be available");
    std::fs::write(
        tmp.path().join(".git"),
        "gitdir: /nonexistent/peri-issue-522\n",
    )
    .expect("the test fixture file must be writable");

    let observation = GitBaseline::capture_for_intent(tmp.path(), None)
        .expect("sampling failure must not fail start");
    assert!(
        observation.scope().is_none(),
        "采样失败没有可持久化的观测范围"
    );
    assert!(
        matches!(observation, GitObservation::Incomplete(_)),
        "损坏的仓库是采样失败，不得读作没有可观测范围: {observation:?}"
    );
}
