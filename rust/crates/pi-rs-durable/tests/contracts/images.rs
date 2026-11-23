use std::{
    error::Error,
    io::Cursor,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use bytes::Bytes;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use pi_rs_ai::{ImageContent, ImageContentKind, InputContent, Model};
use pi_rs_chord::context::Context;
use pi_rs_durable::{
    env::{FileSystem, LocalExecutionEnv},
    images::{
        DEFAULT_IMAGE_LIMITS, FittedImage, FittedImages, ImageLimits, ImageProcessingError,
        ImageProcessor, ImageResize, ImageSize, NativeImageProcessor, PreparedImage, base64_length,
        exif_orientation, fit_image, fit_images, fit_tool_result, from_base64, image_limits,
        to_base64,
    },
    tools::{
        DiagnosticSeverity, ReadToolInput, ReadToolOptions, ToolDiagnostic, ToolError, ToolResult,
        create_read_tool,
        image::{detect_supported_image_mime_type, image_dimensions},
    },
};
use serde_json::json;

fn encode(image: DynamicImage, format: ImageFormat) -> image::ImageResult<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format)?;
    Ok(bytes.into_inner())
}

fn fixture(width: u32, height: u32, format: ImageFormat) -> image::ImageResult<Vec<u8>> {
    encode(
        DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([37, 71, 101]))),
        format,
    )
}

fn block(bytes: &[u8], mime_type: &str) -> InputContent {
    InputContent::Image(ImageContent {
        kind: ImageContentKind::Image,
        data: to_base64(bytes),
        mime_type: mime_type.into(),
    })
}

fn dimensions(data: &str) -> Result<(u32, u32), Box<dyn Error>> {
    let decoded = image::load_from_memory(&from_base64(data)?)?;
    Ok((decoded.width(), decoded.height()))
}

fn tiff(orientation: u8, little: bool) -> Vec<u8> {
    let u16 = if little {
        u16::to_le_bytes
    } else {
        u16::to_be_bytes
    };
    let u32 = if little {
        u32::to_le_bytes
    } else {
        u32::to_be_bytes
    };
    let mut bytes = if little {
        b"II".to_vec()
    } else {
        b"MM".to_vec()
    };
    bytes.extend(u16(42));
    bytes.extend(u32(8));
    bytes.extend(u16(1));
    bytes.extend(u16(0x0112));
    bytes.extend(u16(3));
    bytes.extend(u32(1));
    bytes.extend(u16(orientation.into()));
    bytes.extend([0; 6]);
    bytes
}

fn jpeg_exif(jpeg: &[u8], orientation: u8, little: bool) -> Vec<u8> {
    let tiff = tiff(orientation, little);
    let mut exif = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xe1".to_vec();
    exif.extend(((tiff.len() + 8) as u16).to_be_bytes());
    exif.extend(b"Exif\0\0");
    exif.extend(tiff);
    exif.extend(&jpeg[2..]);
    exif
}

fn webp_exif(orientation: u8, prefixed: bool) -> Vec<u8> {
    let mut body = b"WEBPVP8 \x03\0\0\0\x01\x02\x03\0EXIF".to_vec();
    let tiff = tiff(orientation, true);
    body.extend(((tiff.len() + if prefixed { 6 } else { 0 }) as u32).to_le_bytes());
    if prefixed {
        body.extend(b"Exif\0\0");
    }
    body.extend(tiff);
    let mut riff = b"RIFF".to_vec();
    riff.extend((body.len() as u32).to_le_bytes());
    riff.extend(body);
    riff
}

#[test]
fn image_signatures_and_dimensions_cover_provider_formats() -> Result<(), Box<dyn Error>> {
    let png = fixture(7, 5, ImageFormat::Png)?;
    let jpeg = fixture(5, 7, ImageFormat::Jpeg)?;
    let bmp = fixture(7, 5, ImageFormat::Bmp)?;
    let gif = b"GIF89a\x2c\x01\xc8\x00";
    for (bytes, mime, expected) in [
        (
            png.as_slice(),
            "image/png",
            Some(ImageSize {
                width: 7,
                height: 5,
            }),
        ),
        (
            jpeg.as_slice(),
            "image/jpeg",
            Some(ImageSize {
                width: 5,
                height: 7,
            }),
        ),
        (bmp.as_slice(), "image/bmp", None),
        (
            gif.as_slice(),
            "image/gif",
            Some(ImageSize {
                width: 300,
                height: 200,
            }),
        ),
    ] {
        assert_eq!(detect_supported_image_mime_type(bytes), Some(mime));
        assert_eq!(image_dimensions(bytes, mime), expected);
    }
    assert_eq!(
        detect_supported_image_mime_type(b"GIF87a"),
        Some("image/gif")
    );
    assert_eq!(detect_supported_image_mime_type(b"GIF"), None);
    assert_eq!(detect_supported_image_mime_type(b"\xff\xd8\xff\xf7"), None);
    assert_eq!(detect_supported_image_mime_type(b"BM not a bitmap"), None);
    assert_eq!(image_dimensions(b"\xff\xd8\xff", "image/jpeg"), None);
    for (kind, data) in [
        (
            *b"VP8 ",
            vec![0, 0, 0, 0x9d, 0x01, 0x2a, 0x2c, 0x01, 0xc8, 0x00],
        ),
        (
            *b"VP8L",
            [vec![0x2f], (299_u32 | (199 << 14)).to_le_bytes().to_vec()].concat(),
        ),
        (*b"VP8X", vec![0, 0, 0, 0, 0x2b, 0x01, 0, 0xc7, 0, 0]),
    ] {
        let mut bits = b"RIFF\0\0\0\0WEBP".to_vec();
        bits.extend(kind);
        bits.extend([0; 4]);
        bits.extend(data);
        bits.resize(32, 0);
        assert_eq!(detect_supported_image_mime_type(&bits), Some("image/webp"));
        assert_eq!(
            image_dimensions(&bits, "image/webp"),
            Some(ImageSize {
                width: 300,
                height: 200
            })
        );
    }
    Ok(())
}

#[tokio::test]
async fn jpeg_is_oriented_for_all_eight_exif_values() -> Result<(), Box<dyn Error>> {
    let colors = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 255],
        [0, 0, 0],
        [255, 255, 0],
    ];
    let stored = RgbImage::from_fn(48, 32, |x, y| Rgb(colors[((y / 16) * 3 + x / 16) as usize]));
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, 95).encode_image(&stored)?;
    let processor = NativeImageProcessor::inline();
    for (orientation, mapping) in [
        (1, [0, 1, 2, 3, 4, 5]),
        (2, [2, 1, 0, 5, 4, 3]),
        (3, [5, 4, 3, 2, 1, 0]),
        (4, [3, 4, 5, 0, 1, 2]),
        (5, [0, 3, 1, 4, 2, 5]),
        (6, [3, 0, 4, 1, 5, 2]),
        (7, [5, 2, 4, 1, 3, 0]),
        (8, [2, 5, 1, 4, 0, 3]),
    ] {
        let bytes = jpeg_exif(&encoded, orientation, orientation.is_multiple_of(2));
        assert_eq!(exif_orientation(&bytes), orientation);
        let result = processor
            .prepare(bytes.into(), "image/jpeg", DEFAULT_IMAGE_LIMITS)
            .await?
            .ok_or("valid JPEG omitted")?;
        assert_eq!(result.mime_type, "image/jpeg");
        assert!(result.resized.is_none());
        let decoded = image::load_from_memory(&from_base64(&result.data)?)?.into_rgb8();
        assert_eq!(
            decoded.dimensions(),
            if orientation >= 5 { (32, 48) } else { (48, 32) }
        );
        let columns = decoded.width() / 16;
        for (index, color) in mapping.into_iter().enumerate() {
            let index = index as u32;
            let pixel = decoded
                .get_pixel(index % columns * 16 + 8, index / columns * 16 + 8)
                .0;
            assert_eq!(
                pixel.map(|channel| if channel > 127 { 255 } else { 0 }),
                colors[color]
            );
        }
    }
    assert_eq!(exif_orientation(&encoded), 1);
    assert_eq!(exif_orientation(&jpeg_exif(&encoded, 9, false)), 1);
    assert_eq!(exif_orientation(&jpeg_exif(&encoded, 6, false)[..24]), 1);
    assert_eq!(exif_orientation(&webp_exif(6, false)), 6);
    assert_eq!(exif_orientation(&webp_exif(3, true)), 3);
    Ok(())
}

#[tokio::test]
async fn image_processor_preserves_valid_bytes_then_converts_and_shrinks_to_budget()
-> Result<(), Box<dyn Error>> {
    let png = Bytes::from(fixture(8, 6, ImageFormat::Png)?);
    let inline = NativeImageProcessor::inline();
    let worker = NativeImageProcessor::default();
    let (inline_result, worker_result) = tokio::join!(
        inline.prepare(png.clone(), "image/png", DEFAULT_IMAGE_LIMITS),
        worker.prepare(png.clone(), "image/png", DEFAULT_IMAGE_LIMITS)
    );
    let inline_result = inline_result?.ok_or("valid PNG omitted")?;
    assert_eq!(inline_result.data, to_base64(&png));
    assert_eq!(worker_result?, Some(inline_result));

    let long_png = fixture(600, 2, ImageFormat::Png)?;
    let long = worker
        .prepare(
            long_png.into(),
            "image/png",
            ImageLimits {
                max_width: 200,
                ..DEFAULT_IMAGE_LIMITS
            },
        )
        .await?
        .ok_or("thin PNG omitted")?;
    assert_eq!(
        long.resized,
        Some(ImageResize {
            from: ImageSize {
                width: 600,
                height: 2
            },
            to: ImageSize {
                width: 200,
                height: 1
            }
        })
    );
    assert_eq!(dimensions(&long.data)?, (200, 1));
    let tie = fit_image(
        fixture(9, 9, ImageFormat::Png)?,
        "image/png",
        ImageLimits {
            max_width: 8,
            ..DEFAULT_IMAGE_LIMITS
        },
        &inline,
    )
    .await;
    let FittedImage::Fitted { notes, .. } = tie else {
        return Err("tie-sized image was omitted".into());
    };
    assert_eq!(
        notes,
        ["Resized from 9x9 to 8x8. Multiply coordinates by 1.13 to map them to the original."]
    );
    let bmp = fixture(4, 4, ImageFormat::Bmp)?;
    let converted = worker
        .prepare(bmp.into(), "image/bmp", DEFAULT_IMAGE_LIMITS)
        .await?
        .ok_or("BMP omitted")?;
    assert_eq!(converted.mime_type, "image/png");
    assert_eq!(converted.converted_from.as_deref(), Some("image/bmp"));
    assert!(
        inline
            .prepare(
                png.clone(),
                "image/png",
                ImageLimits {
                    max_bytes: 8,
                    ..DEFAULT_IMAGE_LIMITS
                }
            )
            .await?
            .is_none()
    );
    assert!(
        inline
            .prepare(png.slice(..36), "image/png", DEFAULT_IMAGE_LIMITS)
            .await?
            .is_none()
    );

    let mut seed = 1_u32;
    let mut noise = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    };
    let noisy = Bytes::from(encode(
        DynamicImage::ImageRgb8(RgbImage::from_fn(128, 128, |_, _| {
            Rgb([noise(), noise(), noise()])
        })),
        ImageFormat::Png,
    )?);
    let same = inline
        .prepare(
            noisy.clone(),
            "image/png",
            ImageLimits {
                max_bytes: 40_000,
                ..DEFAULT_IMAGE_LIMITS
            },
        )
        .await?
        .ok_or("full-size JPEG omitted")?;
    assert_eq!(same.mime_type, "image/jpeg");
    assert!(same.resized.is_none());
    let tight = inline
        .prepare(
            noisy,
            "image/png",
            ImageLimits {
                max_bytes: 1_500,
                ..DEFAULT_IMAGE_LIMITS
            },
        )
        .await?
        .ok_or("shrunken image omitted")?;
    assert!(tight.data.len() <= 1_500);
    assert!(
        tight
            .resized
            .is_some_and(|size| size.to.width < 128 && size.to.height < 128)
    );
    Ok(())
}

struct FailingProcessor;

#[async_trait]
impl ImageProcessor for FailingProcessor {
    async fn prepare(
        &self,
        _: Bytes,
        _: &str,
        _: ImageLimits,
    ) -> Result<Option<PreparedImage>, ImageProcessingError> {
        Err(ImageProcessingError::Processor("worker exited".into()))
    }
}

fn model(width: u64) -> serde_json::Result<Model> {
    serde_json::from_value(json!({
        "id":"text-only", "name":"Fixture", "provider":"fixture", "api":"openai-responses", "baseUrl":"https://example.invalid",
        "input":["text"], "inputLimits":{"images":{"resize":{"maxWidth":width}}}, "reasoning":false, "contextWindow":1024, "maxTokens":128,
        "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
    }))
}

#[tokio::test]
async fn fit_preserves_order_corrects_mime_and_omits_broken_images() -> Result<(), Box<dyn Error>> {
    let png = fixture(8, 6, ImageFormat::Png)?;
    let jpeg = fixture(7, 5, ImageFormat::Jpeg)?;
    let processor = NativeImageProcessor::inline();
    assert_eq!(fit_image(png.clone(), "image/png", DEFAULT_IMAGE_LIMITS, &FailingProcessor).await,
        FittedImage::Omitted { note: "An image (image/png) cannot be prepared for the model and was left out: worker exited.".into() });
    let unchanged = [
        InputContent::Text("before".into()),
        block(&png, "image/png"),
    ];
    assert_eq!(
        fit_images(&unchanged, DEFAULT_IMAGE_LIMITS, &processor).await,
        FittedImages::Unchanged
    );
    let malformed = InputContent::Image(ImageContent {
        kind: ImageContentKind::Image,
        data: "%%%".into(),
        mime_type: "image/png".into(),
    });
    let content = [
        unchanged[0].clone(),
        block(&jpeg, "image/png"),
        block(&png[..36], "image/png"),
        malformed,
        InputContent::Text("after".into()),
    ];
    let FittedImages::Fitted { content, notes } =
        fit_images(&content, DEFAULT_IMAGE_LIMITS, &processor).await
    else {
        return Err("invalid and mistyped images passed unchanged".into());
    };
    assert_eq!(
        content,
        [
            unchanged[0].clone(),
            block(&jpeg, "image/jpeg"),
            InputContent::Text("after".into())
        ]
    );
    assert_eq!(notes.len(), 2);
    assert_eq!(
        notes[0],
        "An image (image/png) cannot be prepared for the model and was left out."
    );
    assert!(
        notes[1].starts_with(
            "An image (image/png) cannot be prepared for the model and was left out: "
        )
    );
    assert_eq!(from_base64("Z h \r\n")?, b"f");
    assert!(from_base64("Zg=").is_err());
    for bytes in [0_u64, 1, 2, 3, 4, 8191, 8192, 8193] {
        let data = vec![254; bytes as usize];
        let encoded = to_base64(&data);
        assert_eq!(encoded.len() as u64, base64_length(bytes));
        assert_eq!(from_base64(&encoded)?, data);
    }
    Ok(())
}

#[tokio::test]
async fn fit_hook_uses_current_model_only_for_top_level_image_results() -> Result<(), Box<dyn Error>>
{
    let png = fixture(8, 6, ImageFormat::Png)?;
    let processor = NativeImageProcessor::inline();
    let mut result = ToolResult::<serde_json::Value> {
        output: Some(vec![
            InputContent::Text("here".into()),
            block(&png, "image/png"),
        ]),
        diagnostics: Some(vec![ToolDiagnostic::new(
            DiagnosticSeverity::Info,
            None,
            "tool diagnostic",
        )]),
        structured_output: Some(json!({"untouched":"program data"})),
        ..ToolResult::default()
    };
    let original = result.clone();
    assert!(
        !fit_tool_result(true, &mut result, &processor, || async {
            Err(ToolError::Message(
                "nested hook unexpectedly looked up model".into(),
            ))
        })
        .await?
    );
    assert_eq!(result, original);
    assert!(
        fit_tool_result(false, &mut result, &processor, || async {
            Ok(Some(model(4)?))
        })
        .await?
    );
    let Some([InputContent::Text(text), InputContent::Image(image)]) = result.output.as_deref()
    else {
        return Err("rewritten output lost content ordering".into());
    };
    assert_eq!(text.text, "here");
    assert_eq!(dimensions(&image.data)?, (4, 3));
    assert_eq!(result.structured_output, original.structured_output);
    let details = result.diagnostics.ok_or("no fit diagnostic")?;
    assert_eq!(details[0].message, "tool diagnostic");
    assert_eq!(
        details[1],
        ToolDiagnostic::new(
            DiagnosticSeverity::Info,
            Some("image"),
            "Resized from 8x6 to 4x3. Multiply coordinates by 2.00 to map them to the original."
        )
    );
    assert_eq!(
        image_limits(Some(&model(4)?)),
        ImageLimits {
            max_width: 4,
            ..DEFAULT_IMAGE_LIMITS
        }
    );
    Ok(())
}

fn read_args(path: &str) -> ReadToolInput {
    ReadToolInput {
        path: path.into(),
        offset: None,
        limit: None,
    }
}

#[tokio::test]
async fn read_emits_image_blocks_and_separate_model_details() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    let ctx = Context::background();
    let png = fixture(8, 6, ImageFormat::Png)?;
    let bmp = fixture(4, 4, ImageFormat::Bmp)?;
    env.write_file("picture.txt", &png, &ctx).await?;
    env.write_file("bitmap", &bmp, &ctx).await?;
    let tool = create_read_tool(ReadToolOptions::default());
    let default = tool
        .execute_with_model(read_args("picture.txt"), Some(&env), &ctx, || async {
            Ok(Some(model(20)?))
        })
        .await?;
    assert_eq!(default.output, Some(vec![block(&png, "image/png")]));
    assert_eq!(
        default.structured_output,
        Some(json!({"type":"image", "data":to_base64(&png), "mimeType":"image/png"}))
    );
    assert_eq!(
        default
            .details
            .and_then(|d| d.image_unsupported_by_model)
            .as_deref(),
        Some("text-only")
    );
    assert_eq!(
        default.diagnostics.ok_or("missing image diagnostic")?[0].message,
        "Read image file [image/png]."
    );
    let too_wide = tool
        .execute_with_model(read_args("picture.txt"), Some(&env), &ctx, || async {
            Ok(Some(model(4)?))
        })
        .await?;
    assert_eq!(too_wide.is_error, Some(true));
    assert!(
        too_wide.diagnostics.ok_or("missing size diagnostic")?[0]
            .message
            .contains("of 8x6, larger than 4x2000")
    );
    let no_processor = tool.execute(read_args("bitmap"), Some(&env), &ctx).await?;
    assert_eq!(no_processor.is_error, Some(true));
    assert!(
        no_processor.diagnostics.ok_or("missing BMP diagnostic")?[0]
            .message
            .contains("needs converting")
    );
    assert!(!tool.spec().description.contains("bmp"));
    let processor = NativeImageProcessor::inline();
    let tool = create_read_tool(ReadToolOptions {
        images: Some(&processor),
    });
    assert!(tool.spec().description.contains("bmp"));
    let converted = tool.execute(read_args("bitmap"), Some(&env), &ctx).await?;
    assert_eq!(
        converted.structured_output.ok_or("missing image")?["mimeType"],
        "image/png"
    );
    Ok(())
}

#[tokio::test]
async fn read_retries_when_an_image_grows_and_detects_distant_apng_chunks()
-> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let env = LocalExecutionEnv::new(dir.path().to_string_lossy());
    let ctx = Context::background();
    let mut png = fixture(8, 6, ImageFormat::Png)?;
    env.write_file("growing", &png, &ctx).await?;
    env.write_file("busy", &png, &ctx).await?;
    let tool = create_read_tool(ReadToolOptions::default());
    let calls = AtomicUsize::new(0);
    let grown = tool
        .execute_with_model(read_args("growing"), Some(&env), &ctx, || async {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                env.append_file("growing", b"more", &ctx).await?;
            }
            Ok(None)
        })
        .await?;
    png.extend(b"more");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(grown.output, Some(vec![block(&png, "image/png")]));
    let error = tool
        .execute_with_model(read_args("busy"), Some(&env), &ctx, || async {
            env.append_file("busy", b"more", &ctx).await?;
            Ok(None)
        })
        .await
        .err()
        .ok_or("second image mutation did not fail")?;
    assert_eq!(error.to_string(), "busy changed while it was read");

    let mut animated = b"\x89PNG\r\n\x1a\n".to_vec();
    for (tag, size) in [
        (b"IHDR", 13_u32),
        (b"iCCP", 200_000),
        (b"acTL", 8),
        (b"IDAT", 10),
    ] {
        animated.extend(size.to_be_bytes());
        animated.extend(tag);
        animated.resize(animated.len() + size as usize + 4, 0);
    }
    assert_eq!(detect_supported_image_mime_type(&animated), None);
    env.write_file("animated.png", &animated, &ctx).await?;
    let result = tool
        .execute_with_model(read_args("animated.png"), Some(&env), &ctx, || async {
            Err(ToolError::Message("APNG detected as inline PNG".into()))
        })
        .await?;
    assert!(
        result
            .structured_output
            .is_some_and(|value| value.is_string())
    );
    Ok(())
}
