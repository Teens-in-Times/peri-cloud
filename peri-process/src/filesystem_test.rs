use super::*;

#[test]
fn identity_survives_content_changes_and_moves() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let moved = root.path().join("moved");
    std::fs::create_dir(&first).unwrap();
    let identity = directory_identity(&first).unwrap();
    std::fs::write(first.join("file"), "content").unwrap();
    assert_eq!(directory_identity(&first).unwrap(), identity);
    std::fs::rename(&first, &moved).unwrap();
    assert_eq!(directory_identity(&moved).unwrap(), identity);
    std::fs::create_dir(&first).unwrap();
    assert_ne!(directory_identity(&first).unwrap(), identity);
}

#[test]
fn missing_path_and_regular_file_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        directory_identity(&root.path().join("absent"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    let file = root.path().join("file");
    std::fs::write(&file, "content").unwrap();
    assert_eq!(
        directory_identity(&file).unwrap_err().kind(),
        io::ErrorKind::NotADirectory
    );
}
