use nuono_error::NuonoError;
use std::{error::Error, io};

#[test]
fn anyhow_conversion_preserves_context_and_typed_source() {
    let error = NuonoError::from(
        anyhow::Error::new(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "synthetic IO cause",
        ))
        .context("opening synthetic config"),
    );
    assert_eq!(error.kind(), "Other");
    assert_eq!(error.to_string(), "opening synthetic config");
    let mut cause = error.source();
    let mut original = None;
    while let Some(error) = cause {
        if let Some(error) = error.downcast_ref::<io::Error>() {
            original = Some(error.kind());
            break;
        }
        cause = error.source();
    }
    assert_eq!(original, Some(io::ErrorKind::PermissionDenied));
}

#[cfg(feature = "database")]
#[test]
fn database_conversion_preserves_source_and_classification() {
    let error = NuonoError::from(sea_orm::DbErr::Custom("synthetic database cause".into()));
    assert_eq!(error.kind(), "DatabaseQuery");
    assert!(
        error
            .source()
            .unwrap()
            .downcast_ref::<sea_orm::DbErr>()
            .is_some()
    );
    assert_eq!(
        error.to_string(),
        "数据库查询失败: Custom Error: synthetic database cause"
    );
    assert!(nuono_error::to_api_error(&error).starts_with("[DB] "));
}
