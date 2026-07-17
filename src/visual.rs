use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use image::{
    DynamicImage, GenericImageView, ImageEncoder, ImageFormat, ImageReader, Limits, Rgba, RgbaImage,
};
use thiserror::Error;

pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_ALLOCATION: u64 = MAX_IMAGE_PIXELS * 4;

#[derive(Debug, Error)]
pub enum VisualError {
    #[error("visual baseline could not be read")]
    BaselineRead,
    #[error("visual baseline is not a valid bounded PNG")]
    BaselineDecode,
    #[error("captured screenshot is not a valid bounded PNG")]
    ActualDecode,
    #[error("visual diff could not be encoded")]
    DiffEncode,
}

pub struct Comparison {
    pub changed_pixels: u64,
    pub total_pixels: u64,
    pub dimensions_match: bool,
    pub diff: RgbaImage,
}

impl Comparison {
    pub fn ratio(&self) -> f64 {
        self.changed_pixels as f64 / self.total_pixels as f64
    }
}

pub fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .filter(|pixels| *pixels <= MAX_IMAGE_PIXELS);
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || pixels.is_none()
    {
        return Err(format!(
            "visual image dimensions must not exceed {MAX_IMAGE_DIMENSION} per axis or {MAX_IMAGE_PIXELS} pixels"
        ));
    }
    Ok(())
}

pub fn compare(
    baseline_path: &Path,
    actual_png: &[u8],
    channel_tolerance: u8,
) -> Result<Comparison, VisualError> {
    let baseline_png = read_bounded(baseline_path)?;
    let baseline = decode(&baseline_png).map_err(|_| VisualError::BaselineDecode)?;
    let actual = decode(actual_png).map_err(|_| VisualError::ActualDecode)?;
    let dimensions_match = baseline.dimensions() == actual.dimensions();
    let (width, height) = actual.dimensions();
    let total_pixels = u64::from(width) * u64::from(height);
    if !dimensions_match {
        return Ok(Comparison {
            changed_pixels: total_pixels,
            total_pixels,
            dimensions_match,
            diff: RgbaImage::from_pixel(width, height, Rgba([255, 0, 0, 255])),
        });
    }

    let baseline = baseline.to_rgba8();
    let actual = actual.to_rgba8();
    let mut changed_pixels = 0;
    let mut diff = actual.clone();
    for ((expected, actual), output) in baseline
        .pixels()
        .zip(actual.pixels())
        .zip(diff.pixels_mut())
    {
        if expected
            .0
            .iter()
            .zip(actual.0)
            .any(|(expected, actual)| expected.abs_diff(actual) > channel_tolerance)
        {
            changed_pixels += 1;
            *output = Rgba([255, 0, 0, 255]);
        }
    }
    Ok(Comparison {
        changed_pixels,
        total_pixels,
        dimensions_match,
        diff,
    })
}

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, VisualError> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|_| VisualError::DiffEncode)?;
    Ok(bytes)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, VisualError> {
    let file = File::open(path).map_err(|_| VisualError::BaselineRead)?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| VisualError::BaselineRead)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(VisualError::BaselineDecode);
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<DynamicImage, ()> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOCATION);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| ())?;
    validate_dimensions(image.width(), image.height()).map_err(|_| ())?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use image::{ImageEncoder, codecs::png::PngEncoder};

    use super::*;

    fn png(image: &RgbaImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn compares_per_channel_and_marks_changed_pixels_red() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("baseline.png");
        let expected = RgbaImage::from_pixel(2, 1, Rgba([10, 20, 30, 255]));
        std::fs::write(&baseline, png(&expected)).unwrap();
        let mut actual = expected;
        actual.put_pixel(1, 0, Rgba([13, 20, 30, 255]));

        let within = compare(&baseline, &png(&actual), 3).unwrap();
        assert_eq!(within.changed_pixels, 0);
        let changed = compare(&baseline, &png(&actual), 2).unwrap();
        assert_eq!(changed.changed_pixels, 1);
        assert_eq!(changed.diff.get_pixel(1, 0), &Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn rejects_non_png_and_unbounded_dimensions() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = directory.path().join("baseline.png");
        std::fs::write(&baseline, b"not png").unwrap();
        assert!(matches!(
            compare(&baseline, b"not png", 0),
            Err(VisualError::BaselineDecode)
        ));
        assert!(validate_dimensions(MAX_IMAGE_DIMENSION + 1, 1).is_err());
        assert!(validate_dimensions(4097, 4097).is_err());
    }
}
