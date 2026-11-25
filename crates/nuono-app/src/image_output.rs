//! 渲染图片输出格式与压缩质量配置（由图片编码器与渲染缓存共同使用）。

use nuono_error::{NuonoError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageOutputFormat {
    #[default]
    Png,
    Jpeg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageOutputSettings {
    pub format: ImageOutputFormat,
    pub jpeg_quality: u8,
}

impl Default for ImageOutputSettings {
    fn default() -> Self {
        Self {
            format: ImageOutputFormat::Png,
            jpeg_quality: 80,
        }
    }
}

impl ImageOutputSettings {
    /// 校验配置合法性（即使当前格式选为 PNG 也会校验已保存的 JPEG 质量参数，防止后续切换到 JPEG 时生效非法值）。
    pub fn validate(&self) -> Result<()> {
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err(NuonoError::ConfigValidation(
                "图片输出 JPEG 质量必须在 1 到 100 之间".into(),
            ));
        }
        Ok(())
    }

    /// 生成用于渲染缓存键的格式标识标签（当编码算法变更时应升级其中的版本号；PNG 模式的缓存标签不受 JPEG 质量参数影响）。
    pub fn cache_tag(&self) -> String {
        match self.format {
            ImageOutputFormat::Png => "image-output-v2-png".into(),
            ImageOutputFormat::Jpeg => {
                format!("image-output-v1-jpeg-q{}", self.jpeg_quality)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_preserve_png_and_a_valid_saved_jpeg_quality() {
        let settings = ImageOutputSettings::default();
        assert_eq!(ImageOutputFormat::default(), ImageOutputFormat::Png);
        assert_eq!(settings.format, ImageOutputFormat::Png);
        assert_eq!(settings.jpeg_quality, 80);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn missing_settings_fields_use_defaults() {
        assert_eq!(
            serde_json::from_value::<ImageOutputSettings>(json!({})).unwrap(),
            ImageOutputSettings::default()
        );
        assert_eq!(
            serde_json::from_value::<ImageOutputSettings>(json!({"format": "jpeg"})).unwrap(),
            ImageOutputSettings {
                format: ImageOutputFormat::Jpeg,
                jpeg_quality: 80,
            }
        );
        assert_eq!(
            serde_json::from_value::<ImageOutputSettings>(json!({"jpeg_quality": 90})).unwrap(),
            ImageOutputSettings {
                format: ImageOutputFormat::Png,
                jpeg_quality: 90,
            }
        );
    }

    #[test]
    fn format_serde_uses_only_the_supported_lowercase_names() {
        for (format, name) in [
            (ImageOutputFormat::Png, "png"),
            (ImageOutputFormat::Jpeg, "jpeg"),
        ] {
            assert_eq!(serde_json::to_value(format).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<ImageOutputFormat>(json!(name)).unwrap(),
                format
            );
        }
        for unsupported in ["PNG", "JPEG", "jpg", "webp"] {
            assert!(serde_json::from_value::<ImageOutputFormat>(json!(unsupported)).is_err());
        }
    }

    #[test]
    fn quality_boundaries_are_validated_for_every_format() {
        for format in [ImageOutputFormat::Png, ImageOutputFormat::Jpeg] {
            for jpeg_quality in [1, 80, 100] {
                assert!(
                    ImageOutputSettings {
                        format,
                        jpeg_quality
                    }
                    .validate()
                    .is_ok()
                );
            }
            for jpeg_quality in [0, 101, u8::MAX] {
                let error = ImageOutputSettings {
                    format,
                    jpeg_quality,
                }
                .validate()
                .unwrap_err();
                assert!(matches!(
                    error,
                    NuonoError::ConfigValidation(message)
                        if message == "图片输出 JPEG 质量必须在 1 到 100 之间"
                ));
            }
        }
    }

    #[test]
    fn cache_tags_include_encoding_version_and_only_effective_settings() {
        let mut settings = ImageOutputSettings::default();
        assert_eq!(settings.cache_tag(), "image-output-v2-png");
        settings.jpeg_quality = 95;
        assert_eq!(settings.cache_tag(), "image-output-v2-png");
        settings.format = ImageOutputFormat::Jpeg;
        assert_eq!(settings.cache_tag(), "image-output-v1-jpeg-q95");
        settings.jpeg_quality = 80;
        assert_eq!(settings.cache_tag(), "image-output-v1-jpeg-q80");
    }
}
