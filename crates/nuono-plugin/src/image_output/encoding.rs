use anyhow::{Context, Result, ensure};
use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, Rgb, Rgba,
    codecs::{
        jpeg::JpegEncoder,
        png::{CompressionType, FilterType, PngEncoder},
    },
};

use super::{ImageOutputFormat, ImageOutputSettings};

/// 在不改变原图宽高尺寸的前提下对生成的图片进行编码。
/// PNG 模式无损保留整数像素值与 Alpha 透明通道；JPEG 模式会在有损压缩前先将非预乘（straight）Alpha 像素合成到纯白背景上（压缩质量参数不会改变源像素选择、内存布局或色度抽样方式）。
pub fn encode(image: &DynamicImage, settings: &ImageOutputSettings) -> Result<Vec<u8>> {
    settings.validate()?;
    let (width, height) = image.dimensions();
    ensure!(width > 0 && height > 0, "生成图片宽高必须大于 0");
    let pixels = usize::try_from(width)?
        .checked_mul(usize::try_from(height)?)
        .context("生成图片尺寸超出可编码范围")?;
    let source_bytes = pixels
        .checked_mul(usize::from(image.color().bytes_per_pixel()))
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .context("生成图片像素缓冲超出可编码范围")?;
    ensure!(
        source_bytes == image.as_bytes().len(),
        "生成图片像素缓冲与尺寸不一致"
    );

    let mut output = Vec::new();
    match settings.format {
        ImageOutputFormat::Png => {
            // 固定使用 Paeth 滤波器配合 Fast 压缩级别，避免逐行试探最优滤波器的开销。
            PngEncoder::new_with_quality(&mut output, CompressionType::Fast, FilterType::Paeth)
                .write_image(image.as_bytes(), width, height, image.color().into())
                .context("生成图片 PNG 编码失败")?;
        }
        ImageOutputFormat::Jpeg => {
            ensure!(
                width <= u32::from(u16::MAX) && height <= u32::from(u16::MAX),
                "JPEG 宽高不能超过 65535 像素"
            );
            // 即使编码器现在通过借用视图按需读取像素而不再分配第二份完整 RGB 缓冲区，仍保留原有的 RGB 总字节数上限校验。
            pixels
                .checked_mul(3)
                .filter(|bytes| *bytes <= isize::MAX as usize)
                .context("JPEG 像素缓冲超出可编码范围")?;
            let mut encoder = JpegEncoder::new_with_quality(&mut output, settings.jpeg_quality);
            match image {
                DynamicImage::ImageRgb8(rgb) => {
                    encoder.encode(rgb.as_raw(), width, height, ExtendedColorType::Rgb8)
                }
                DynamicImage::ImageRgba8(rgba) => encoder.encode_image(&WhiteBackground(rgba)),
                // `DynamicImage::get_pixel` 对灰度、16 位及浮点图像仍保持原先转为 RGBA8 的行为，
                // 并经由 `WhiteBackground` 视图输出与原先一致的三通道 JPEG 结果。
                _ => encoder.encode_image(&WhiteBackground(image)),
            }
            .context("生成图片 JPEG 编码失败")?;
        }
    }
    Ok(output)
}

/// 在 JPEG 编码器逐像素读取时，按需将非预乘（straight）Alpha 像素合成到纯白背景上（保持与原先整图预转换完全一致的整数四舍五入精度）。
struct WhiteBackground<'a, I>(&'a I);

impl<I: GenericImageView<Pixel = Rgba<u8>>> GenericImageView for WhiteBackground<'_, I> {
    type Pixel = Rgb<u8>;

    fn dimensions(&self) -> (u32, u32) {
        self.0.dimensions()
    }

    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        let [r, g, b, alpha] = self.0.get_pixel(x, y).0;
        let alpha = u32::from(alpha);
        Rgb([r, g, b]
            .map(|channel| ((u32::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Luma, LumaA, Rgba, RgbaImage};

    fn settings(format: ImageOutputFormat, jpeg_quality: u8) -> ImageOutputSettings {
        ImageOutputSettings {
            format,
            jpeg_quality,
        }
    }

    /// 保留旧版整图预转换的独立实现作为对照基准，用于验证借用视图优化不会改变任何编码输出字节。
    fn legacy_jpeg(image: &DynamicImage, quality: u8) -> Vec<u8> {
        let (width, height) = image.dimensions();
        let mut rgb = Vec::new();
        for (_, _, pixel) in image.pixels() {
            let [r, g, b, alpha] = pixel.0;
            let alpha = u32::from(alpha);
            for channel in [r, g, b] {
                let value = (u32::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
                rgb.push(value as u8);
            }
        }
        let mut output = Vec::new();
        JpegEncoder::new_with_quality(&mut output, quality)
            .encode(&rgb, width, height, ExtendedColorType::Rgb8)
            .unwrap();
        output
    }

    #[test]
    fn borrowed_jpeg_matches_legacy_bytes_for_every_dynamic_image_type_and_quality() {
        let values8 = [0_u8, 1, 2, 63, 127, 128, 129, 253, 254, 255];
        let values16 = [
            0_u16, 1, 127, 128, 129, 255, 256, 257, 32767, 32768, 65534, 65535,
        ];
        let values_float = [
            f32::NEG_INFINITY,
            -0.5,
            0.0,
            1.0 / 512.0,
            1.0 / 255.0,
            0.5,
            127.5 / 255.0,
            0.999,
            1.0,
            1.5,
            f32::INFINITY,
            f32::NAN,
        ];
        let sample8 = |x: u32, y: u32, channel: usize| {
            values8[(x as usize * 3 + y as usize * 7 + channel * 11) % values8.len()]
        };
        let sample16 = |x: u32, y: u32, channel: usize| {
            values16[(x as usize * 3 + y as usize * 7 + channel * 11) % values16.len()]
        };
        let sample_float = |x: u32, y: u32, channel: usize| {
            values_float[(x as usize * 3 + y as usize * 7 + channel * 11) % values_float.len()]
        };
        // 宽高均非 8 的整数倍，用于覆盖 JPEG 编码器对 MCU 边缘块的填充逻辑。
        let images = [
            (
                "luma8",
                DynamicImage::ImageLuma8(ImageBuffer::from_fn(13, 11, |x, y| {
                    Luma([sample8(x, y, 0)])
                })),
            ),
            (
                "lumaa8",
                DynamicImage::ImageLumaA8(ImageBuffer::from_fn(13, 11, |x, y| {
                    LumaA([sample8(x, y, 0), sample8(x, y, 1)])
                })),
            ),
            (
                "rgb8",
                DynamicImage::ImageRgb8(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgb(std::array::from_fn(|channel| sample8(x, y, channel)))
                })),
            ),
            (
                "rgba8",
                DynamicImage::ImageRgba8(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgba(std::array::from_fn(|channel| sample8(x, y, channel)))
                })),
            ),
            (
                "luma16",
                DynamicImage::ImageLuma16(ImageBuffer::from_fn(13, 11, |x, y| {
                    Luma([sample16(x, y, 0)])
                })),
            ),
            (
                "lumaa16",
                DynamicImage::ImageLumaA16(ImageBuffer::from_fn(13, 11, |x, y| {
                    LumaA([sample16(x, y, 0), sample16(x, y, 1)])
                })),
            ),
            (
                "rgb16",
                DynamicImage::ImageRgb16(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgb(std::array::from_fn(|channel| sample16(x, y, channel)))
                })),
            ),
            (
                "rgba16",
                DynamicImage::ImageRgba16(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgba(std::array::from_fn(|channel| sample16(x, y, channel)))
                })),
            ),
            (
                "rgb32f",
                DynamicImage::ImageRgb32F(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgb(std::array::from_fn(|channel| sample_float(x, y, channel)))
                })),
            ),
            (
                "rgba32f",
                DynamicImage::ImageRgba32F(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgba(std::array::from_fn(|channel| sample_float(x, y, channel)))
                })),
            ),
        ];
        for (name, image) in images {
            for quality in [1, 20, 80, 95, 100] {
                let actual = encode(&image, &settings(ImageOutputFormat::Jpeg, quality)).unwrap();
                assert_eq!(
                    actual,
                    legacy_jpeg(&image, quality),
                    "{name} at quality {quality}"
                );
                assert_eq!(
                    image::load_from_memory(&actual).unwrap().dimensions(),
                    image.dimensions()
                );
            }
        }
    }

    #[test]
    fn borrowed_jpeg_preserves_every_alpha_value_and_white_compositing_rounding() {
        let channels = [0_u8, 1, 2, 127, 128, 254, 255];
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(256, 7, |x, y| {
            Rgba([
                channels[y as usize],
                x as u8,
                channels[6 - y as usize],
                x as u8,
            ])
        }));
        for quality in [1, 20, 80, 95, 100] {
            assert_eq!(
                encode(&image, &settings(ImageOutputFormat::Jpeg, quality)).unwrap(),
                legacy_jpeg(&image, quality),
                "alpha 0–255 at quality {quality}"
            );
        }
    }

    #[test]
    fn borrowed_jpeg_does_not_skip_source_layout_validation() {
        let padded = DynamicImage::ImageRgb8(ImageBuffer::from_raw(1, 1, vec![42_u8; 6]).unwrap());
        assert!(encode(&padded, &settings(ImageOutputFormat::Jpeg, 80)).is_err());
    }

    #[test]
    fn png_preserves_all_eight_integer_color_types_dimensions_and_pixel_samples() {
        let values8 = [0_u8, 1, 2, 127, 128, 129, 254, 255];
        let values16 = [0_u16, 1, 128, 257, 32767, 32768, 65534, 65535];
        let sample8 = |x: u32, y: u32, channel: usize| {
            values8[(x as usize * 3 + y as usize * 5 + channel * 7) % values8.len()]
        };
        let sample16 = |x: u32, y: u32, channel: usize| {
            values16[(x as usize * 3 + y as usize * 5 + channel * 7) % values16.len()]
        };
        // 宽高均非 8 的整数倍；带 Alpha 通道的测试样本同时覆盖带非零 RGB 值的全透明与半透明像素。
        let images = [
            (
                "luma8",
                DynamicImage::ImageLuma8(ImageBuffer::from_fn(13, 11, |x, y| {
                    Luma([sample8(x, y, 0)])
                })),
            ),
            (
                "lumaa8",
                DynamicImage::ImageLumaA8(ImageBuffer::from_fn(13, 11, |x, y| {
                    LumaA([sample8(x, y, 0), sample8(x, y, 1)])
                })),
            ),
            (
                "rgb8",
                DynamicImage::ImageRgb8(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgb(std::array::from_fn(|channel| sample8(x, y, channel)))
                })),
            ),
            (
                "rgba8",
                DynamicImage::ImageRgba8(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgba(std::array::from_fn(|channel| sample8(x, y, channel)))
                })),
            ),
            (
                "luma16",
                DynamicImage::ImageLuma16(ImageBuffer::from_fn(13, 11, |x, y| {
                    Luma([sample16(x, y, 0)])
                })),
            ),
            (
                "lumaa16",
                DynamicImage::ImageLumaA16(ImageBuffer::from_fn(13, 11, |x, y| {
                    LumaA([sample16(x, y, 0), sample16(x, y, 1)])
                })),
            ),
            (
                "rgb16",
                DynamicImage::ImageRgb16(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgb(std::array::from_fn(|channel| sample16(x, y, channel)))
                })),
            ),
            (
                "rgba16",
                DynamicImage::ImageRgba16(ImageBuffer::from_fn(13, 11, |x, y| {
                    Rgba(std::array::from_fn(|channel| sample16(x, y, channel)))
                })),
            ),
        ];
        for (name, original) in images {
            let bytes = encode(&original, &ImageOutputSettings::default()).unwrap();
            assert_eq!(
                image::guess_format(&bytes).unwrap(),
                ImageFormat::Png,
                "{name}"
            );
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!(decoded.color(), original.color(), "{name} color type");
            assert_eq!(decoded.dimensions(), (13, 11), "{name} dimensions");
            // 直接比对底层原生整数样本字节，而非调用 `DynamicImage::pixels()`（后者会先转为 8 位 RGBA8，从而掩盖 16 位通道低字节的精度损失）。
            assert_eq!(
                decoded.as_bytes(),
                original.as_bytes(),
                "{name} pixel samples"
            );
        }
    }

    #[test]
    fn png_preserves_rgba_pixels_and_ignores_unused_jpeg_quality() {
        let original = RgbaImage::from_fn(9, 7, |x, y| {
            Rgba([(x * 23) as u8, (y * 31) as u8, 170, ((x + y) * 17) as u8])
        });
        let image = DynamicImage::ImageRgba8(original.clone());
        let low = encode(&image, &settings(ImageOutputFormat::Png, 1)).unwrap();
        let high = encode(&image, &settings(ImageOutputFormat::Png, 100)).unwrap();
        assert_eq!(low, high);
        assert_eq!(image::guess_format(&low).unwrap(), ImageFormat::Png);
        let decoded = image::load_from_memory(&low).unwrap();
        assert_eq!(decoded.dimensions(), image.dimensions());
        assert_eq!(decoded.to_rgba8(), original);
    }

    #[test]
    fn png_preserves_sixteen_bit_rgba_samples() {
        let original = ImageBuffer::from_fn(3, 2, |x, y| {
            Rgba([x as u16 * 10001, y as u16 * 30001, 12345_u16, 45678_u16])
        });
        let image = DynamicImage::ImageRgba16(original.clone());
        let bytes = encode(&image, &ImageOutputSettings::default()).unwrap();
        assert_eq!(
            image::load_from_memory(&bytes).unwrap().to_rgba16(),
            original
        );
    }

    #[test]
    fn jpeg_quality_changes_size_and_preserves_dimensions_without_resampling() {
        let original = RgbaImage::from_fn(192, 128, |x, y| {
            let noise = x
                .wrapping_mul(747796405)
                .wrapping_add(y.wrapping_mul(2891336453));
            Rgba([(noise >> 16) as u8, (noise >> 8) as u8, noise as u8, 255])
        });
        let image = DynamicImage::ImageRgba8(original);
        let low = encode(&image, &settings(ImageOutputFormat::Jpeg, 20)).unwrap();
        let high = encode(&image, &settings(ImageOutputFormat::Jpeg, 95)).unwrap();
        assert!(
            high.len() > low.len(),
            "the explicit quality must reach the JPEG encoder"
        );
        for bytes in [&low, &high] {
            assert_eq!(image::guess_format(bytes).unwrap(), ImageFormat::Jpeg);
            assert_eq!(
                image::load_from_memory(bytes).unwrap().dimensions(),
                image.dimensions()
            );
        }
    }

    #[test]
    fn jpeg_composites_fully_and_partially_transparent_pixels_onto_white() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(32, 16, |x, _| {
            if x < 16 {
                Rgba([20, 40, 60, 0])
            } else {
                Rgba([0, 0, 0, 128])
            }
        }));
        let bytes = encode(&image, &settings(ImageOutputFormat::Jpeg, 100)).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (32, 16));
        for channel in decoded.get_pixel(4, 8).0 {
            assert!(channel >= 253, "transparent pixels should become white");
        }
        for channel in decoded.get_pixel(24, 8).0 {
            assert!(
                channel.abs_diff(127) <= 2,
                "partial alpha must blend with white"
            );
        }
    }

    #[test]
    fn invalid_quality_empty_dimensions_and_jpeg_dimension_overflow_are_errors() {
        let image = DynamicImage::new_rgba8(2, 2);
        for format in [ImageOutputFormat::Png, ImageOutputFormat::Jpeg] {
            for quality in [0, 101, 255] {
                assert!(encode(&image, &settings(format, quality)).is_err());
            }
            assert!(encode(&DynamicImage::new_rgba8(0, 1), &settings(format, 80)).is_err());
        }
        let too_wide = DynamicImage::new_rgba8(u32::from(u16::MAX) + 1, 1);
        assert!(encode(&too_wide, &settings(ImageOutputFormat::Jpeg, 80)).is_err());
    }
}
