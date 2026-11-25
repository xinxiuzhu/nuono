use axum::{body::Body, http::header};

use super::*;
use axum::extract::FromRequest;
use image::GenericImageView;

async fn multipart(fields: &[(&str, &[u8], Option<&str>)]) -> Multipart {
    let mut body = Vec::new();
    for (name, bytes, mime) in fields {
        body.extend(
            format!("--persona-test\r\nContent-Disposition: form-data; name=\"{name}\"").as_bytes(),
        );
        if let Some(mime) = mime {
            body.extend(format!("; filename=\"test.bin\"\r\nContent-Type: {mime}").as_bytes());
        }
        body.extend(b"\r\n\r\n");
        body.extend(*bytes);
        body.extend(b"\r\n");
    }
    body.extend(b"--persona-test--\r\n");
    let request = axum::http::Request::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/form-data; boundary=persona-test",
        )
        .body(Body::from(body))
        .unwrap();
    Multipart::from_request(request, &()).await.unwrap()
}

#[tokio::test]
async fn upload_requires_one_file_and_a_unique_positive_client_revision() {
    let bytes = b"example";
    let fields = [
        ("file", bytes.as_slice(), Some("audio/wav")),
        ("revision", b"2".as_slice(), None),
    ];
    let accepted = read_upload(multipart(&fields).await, PersonaAssetKind::ReferenceAudio)
        .await
        .unwrap();
    assert_eq!(accepted.2, 2);
    assert!(
        read_upload(
            multipart(&fields[..1]).await,
            PersonaAssetKind::ReferenceAudio
        )
        .await
        .is_err()
    );
    let mut duplicate = fields.to_vec();
    duplicate.push(fields[1]);
    assert!(
        read_upload(
            multipart(&duplicate).await,
            PersonaAssetKind::ReferenceAudio
        )
        .await
        .is_err()
    );
    duplicate = fields.to_vec();
    duplicate.push(fields[0]);
    assert!(
        read_upload(
            multipart(&duplicate).await,
            PersonaAssetKind::ReferenceAudio
        )
        .await
        .is_err()
    );
    for revision in [b"0".as_slice(), b"-1".as_slice(), b"invalid".as_slice()] {
        let fields = [fields[0], ("revision", revision, None)];
        assert!(
            read_upload(multipart(&fields).await, PersonaAssetKind::ReferenceAudio)
                .await
                .is_err()
        );
    }
}

#[test]
fn reference_audio_rejects_mime_spoofing_and_truncated_containers() {
    for mime in ["audio/wav", "audio/mpeg", "audio/ogg", "audio/flac"] {
        assert!(audio_extension(b"fake audio", mime).is_err());
    }
    let mut wav = b"RIFF".to_vec();
    wav.extend(38u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend([1, 0, 1, 0, 0x80, 0xbb, 0, 0, 0, 0, 0, 0, 2, 0, 16, 0]);
    wav.extend(b"data");
    wav.extend(2u32.to_le_bytes());
    wav.extend([0, 0]);
    assert_eq!(audio_extension(&wav, "audio/wav").unwrap(), "wav");
    assert_eq!(
        audio_extension(&wav, "application/octet-stream").unwrap(),
        "wav"
    );
    assert_eq!(audio_extension(&wav, "").unwrap(), "wav");
    assert!(audio_extension(&wav, "image/png").is_err());
    assert!(audio_extension(&wav, "audio/mpeg").is_err());
    wav.truncate(44);
    assert!(!valid_wav(&wav));
    assert!(audio_extension(b"ID3\0\0\0\0\0\0\0", "audio/mpeg").is_err());
}

#[test]
fn portrait_keeps_aspect_and_avatar_crops_without_trusting_extension() {
    let image = image::DynamicImage::new_rgba8(800, 400);
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    let avatar = normalize_image(&bytes, "image/png", PersonaAssetKind::Avatar).unwrap();
    assert_eq!(
        image::load_from_memory(&avatar).unwrap().dimensions(),
        (512, 512)
    );
    let portrait = normalize_image(&bytes, "image/png", PersonaAssetKind::Portrait).unwrap();
    assert_eq!(
        image::load_from_memory(&portrait).unwrap().dimensions(),
        (800, 400)
    );
    assert!(normalize_image(&bytes, "image/jpeg", PersonaAssetKind::Avatar).is_err());
}
