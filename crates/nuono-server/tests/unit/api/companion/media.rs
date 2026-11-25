use super::*;

#[tokio::test]
async fn historical_bytes_and_pending_files_count_against_the_card_budget() {
    let root = tempfile::tempdir().unwrap();
    assert!(ensure_asset_budget(root.path(), 1).await.is_ok());
    let file = tokio::fs::File::create(root.path().join("old.png"))
        .await
        .unwrap();
    file.set_len(MAX_CARD_ASSET_BYTES - 1).await.unwrap();
    assert!(ensure_asset_budget(root.path(), 1).await.is_ok());
    assert_eq!(
        ensure_asset_budget(root.path(), 2).await.unwrap_err().0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    file.set_len(0).await.unwrap();
    for index in 0..MAX_CARD_ASSET_FILES - 2 {
        tokio::fs::write(root.path().join(format!("{index}.tmp")), [])
            .await
            .unwrap();
    }
    assert!(ensure_asset_budget(root.path(), 1).await.is_ok());
    tokio::fs::write(root.path().join("last.png"), [])
        .await
        .unwrap();
    assert_eq!(
        ensure_asset_budget(root.path(), 1).await.unwrap_err().0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[cfg(unix)]
#[tokio::test]
async fn budget_inspection_does_not_follow_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("linked.png")).unwrap();
    assert_eq!(
        ensure_asset_budget(root.path(), 1).await.unwrap_err().0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
