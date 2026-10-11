use super::*;

#[tokio::test]
async fn discovery_is_sorted_paged_and_contains_only_directories() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for name in ["zeta", "Alpha", "目录 with spaces"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    std::fs::write(root.path().join("not-a-folder.txt"), "content").unwrap();
    let executor = Executor::open(state.path(), "fixture").await.unwrap();
    let mut query = DirectoryQuery {
        path: root.path().to_str().unwrap().into(),
        offset: 0,
        limit: 2,
    };
    let first = executor.browse_directories(query.clone()).await.unwrap();
    assert_eq!(
        first.path,
        root.path().canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        first
            .directories
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "zeta"]
    );
    assert_eq!(first.next_offset, Some(2));
    assert!(!first.roots.is_empty());
    query.offset = 2;
    let last = executor.browse_directories(query).await.unwrap();
    assert_eq!(last.directories[0].name, "目录 with spaces");
    assert_eq!(last.next_offset, None);
    assert_eq!(last.parent, first.parent);
    executor.shutdown().await.unwrap();
}

#[tokio::test]
async fn roots_are_independent_of_old_paths_and_invalid_queries_are_rejected() {
    let state = tempfile::tempdir().unwrap();
    let executor = Executor::open(state.path(), "fixture").await.unwrap();
    let roots = executor
        .browse_directories(DirectoryQuery {
            path: String::new(),
            offset: 0,
            limit: 1,
        })
        .await
        .unwrap();
    assert!(!roots.roots.is_empty());
    assert!(roots.directories.is_empty());
    for (path, limit, offset) in [
        ("relative", 1, 0),
        ("", 0, 0),
        ("", 101, 0),
        ("", 1, 100001),
        ("\n", 1, 0),
    ] {
        assert!(executor
            .browse_directories(DirectoryQuery {
                path: path.into(),
                limit,
                offset
            })
            .await
            .is_err());
    }
    executor.shutdown().await.unwrap();
}
