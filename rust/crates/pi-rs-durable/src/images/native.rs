use async_trait::async_trait;
use bytes::Bytes;
use image::{
    DynamicImage, ImageEncoder,
    codecs::{jpeg::JpegEncoder, png::PngEncoder},
    imageops::FilterType,
    metadata::Orientation,
};

use super::{
    INLINE_IMAGE_TYPES, ImageLimits, ImageProcessingError, ImageProcessor, ImageResize, ImageSize,
    PreparedImage, base64_length, exif_orientation, to_base64,
};

const JPEG_QUALITIES: [u8; 4] = [80, 70, 55, 40];

/// The portable image crate handles codecs; the normal processor keeps CPU work off the async runtime.
#[derive(Clone, Copy, Debug)]
pub struct NativeImageProcessor {
    worker: bool,
}

impl Default for NativeImageProcessor {
    fn default() -> Self {
        Self { worker: true }
    }
}

impl NativeImageProcessor {
    /// Prepare the image on the calling thread, without a Tokio runtime.
    pub const fn inline() -> Self {
        Self { worker: false }
    }
}

#[async_trait]
impl ImageProcessor for NativeImageProcessor {
    async fn prepare(
        &self,
        bytes: Bytes,
        mime_type: &str,
        limits: ImageLimits,
    ) -> Result<Option<PreparedImage>, ImageProcessingError> {
        if !self.worker {
            return Ok(prepare(&bytes, mime_type, limits));
        }
        let mime_type = mime_type.to_owned();
        let runtime = tokio::runtime::Handle::try_current()?;
        Ok(runtime
            .spawn_blocking(move || prepare(&bytes, &mime_type, limits))
            .await?)
    }
}

fn prepare(bytes: &[u8], mime_type: &str, limits: ImageLimits) -> Option<PreparedImage> {
    // Validate the pixels even when the encoded bytes can be reused.
    let mut image = image::load_from_memory(bytes).ok()?;
    let orientation = exif_orientation(bytes);
    let width = u64::from(image.width());
    let height = u64::from(image.height());
    if INLINE_IMAGE_TYPES.contains(&mime_type)
        && orientation == 1
        && width <= limits.max_width
        && height <= limits.max_height
        && base64_length(bytes.len() as u64) <= limits.max_bytes
    {
        return Some(PreparedImage {
            data: to_base64(bytes),
            mime_type: mime_type.to_owned(),
            converted_from: None,
            resized: None,
        });
    }
    if let Some(orientation) = Orientation::from_exif(orientation) {
        image.apply_orientation(orientation);
    }
    let from = ImageSize {
        width: image.width(),
        height: image.height(),
    };
    let scale = 1.0_f64
        .min(limits.max_width as f64 / f64::from(from.width))
        .min(limits.max_height as f64 / f64::from(from.height));
    let mut to = ImageSize {
        width: (f64::from(from.width) * scale).round().max(1.0) as u32,
        height: (f64::from(from.height) * scale).round().max(1.0) as u32,
    };
    let mut encoded = Vec::new();
    loop {
        let scaled;
        let source = if from == to {
            &image
        } else {
            scaled = image.resize_exact(to.width, to.height, FilterType::Lanczos3);
            &scaled
        };
        if let Some(mime) = encode_within(
            source,
            mime_type == "image/jpeg",
            limits.max_bytes,
            &mut encoded,
        ) {
            return Some(PreparedImage {
                data: to_base64(&encoded),
                mime_type: mime.to_owned(),
                converted_from: (mime != mime_type).then(|| mime_type.to_owned()),
                resized: (from != to).then_some(ImageResize { from, to }),
            });
        }
        if to.width == 1 && to.height == 1 {
            return None;
        }
        to = ImageSize {
            width: ((u64::from(to.width) * 3 / 4) as u32).max(1),
            height: ((u64::from(to.height) * 3 / 4) as u32).max(1),
        };
    }
}

fn encode_within(
    image: &DynamicImage,
    jpeg_first: bool,
    max_bytes: u64,
    encoded: &mut Vec<u8>,
) -> Option<&'static str> {
    let fits = |bytes: &[u8]| base64_length(bytes.len() as u64) <= max_bytes;
    let png = |encoded: &mut Vec<u8>| {
        encoded.clear();
        PngEncoder::new(&mut *encoded)
            .write_image(
                image.as_bytes(),
                image.width(),
                image.height(),
                image.color().into(),
            )
            .is_ok()
            && fits(encoded)
    };
    if !jpeg_first && png(encoded) {
        return Some("image/png");
    }
    for quality in JPEG_QUALITIES {
        encoded.clear();
        if JpegEncoder::new_with_quality(&mut *encoded, quality)
            .encode_image(image)
            .is_ok()
            && fits(encoded)
        {
            return Some("image/jpeg");
        }
    }
    (jpeg_first && png(encoded)).then_some("image/png")
}
