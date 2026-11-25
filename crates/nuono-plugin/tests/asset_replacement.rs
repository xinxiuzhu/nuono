use nuono_plugin::{AssetLimits, PluginAssets, PluginHttpClient};

#[tokio::test]
async fn explicit_byte_replacement_invalidates_cache_and_failed_writes_preserve_existing_assets() {
    let root = std::env::temp_dir().join(format!("nuono-asset-replace-{}", uuid::Uuid::new_v4()));
    let assets = PluginAssets::with_limits(
        &root,
        PluginHttpClient::new(),
        AssetLimits {
            max_file_bytes: 8,
            ..Default::default()
        },
    )
    .unwrap();
    assets.install("shared/image", b"original").await.unwrap();
    let cached = assets.read("shared/image").await.unwrap();
    assets.install("shared/image", b"preserve").await.unwrap();
    assert_eq!(
        assets.read("shared/image").await.unwrap().as_ref(),
        b"original"
    );
    assets.replace("shared/image", b"restored").await.unwrap();
    assert_eq!(cached.as_ref(), b"original");
    assert_eq!(
        assets.read("shared/image").await.unwrap().as_ref(),
        b"restored"
    );
    for invalid in [&b""[..], &b"too large"[..]] {
        assert!(assets.replace("shared/image", invalid).await.is_err());
        assert_eq!(
            assets.read("shared/image").await.unwrap().as_ref(),
            b"restored"
        );
    }
    assert!(assets.replace("../outside", b"invalid").await.is_err());
    let directory = assets.path("shared/directory").unwrap();
    tokio::fs::create_dir_all(&directory).await.unwrap();
    tokio::fs::write(directory.join("preserved"), b"yes")
        .await
        .unwrap();
    assert!(
        assets
            .replace("shared/directory", b"invalid")
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(directory.join("preserved")).await.unwrap(),
        b"yes"
    );
    tokio::fs::remove_dir_all(root).await.unwrap();
}
