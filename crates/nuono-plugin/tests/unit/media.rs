use super::*;

#[tokio::test]
async fn reusable_images_keep_the_normal_wire_type_and_validate_the_source() {
    let image = ImageSource::bytes(b"\x89PNG\r\n\x1a\n".to_vec())
        .into_reusable_segment()
        .await
        .unwrap();
    let json = image.to_json();
    assert_eq!(json["type"], "image");
    assert_eq!(json["data"]["reuse"], "persistent");
    assert_eq!(json["data"]["file"], "base64://iVBORw0KGgo=");
    assert_eq!(image.into_json(), json);
    assert!(
        ImageSource::bytes(b"not an image".to_vec())
            .into_reusable_segment()
            .await
            .is_err()
    );
}
