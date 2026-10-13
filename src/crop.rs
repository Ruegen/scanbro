use std::io::Cursor;
use std::sync::Arc;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageReader, RgbImage};

use crate::error::AppResult;

/// Pixels darker than this count as ink or paper edge, not empty feeder.
const CONTENT_LUMA: u8 = 236;
/// Detection image long side. Speckles shrink; the box scales back.
const DETECT_MAX: u32 = 480;
/// Keep this much paper around the ink, in inches.
const PAD_INCH: f32 = 0.12;
/// Treat the page as left-aligned / top-fed below this inset.
const ORIGIN_ALIGN_FRAC: f32 = 0.08;
/// Skip a re-encode when the trim would barely change the image.
const SKIP_MARGIN_FRAC: f32 = 0.015;
const JPEG_QUALITY: u8 = 92;

/// Trim empty feeder around a scan. No-op on blank pages or a failed decode.
pub fn trim_to_page(jpeg: Arc<Vec<u8>>, dpi: u16) -> Arc<Vec<u8>> {
    if jpeg.is_empty() {
        return jpeg;
    }
    match trim_inner(jpeg.as_slice(), dpi) {
        Ok(Some(cropped)) => Arc::new(cropped),
        _ => jpeg,
    }
}

fn trim_inner(bytes: &[u8], dpi: u16) -> AppResult<Option<Vec<u8>>> {
    let img = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()?
        .decode()?;
    let rgb = img.to_rgb8();
    let (width, height) = rgb.dimensions();
    if width < 16 || height < 16 {
        return Ok(None);
    }
    let Some((left, top, right, bottom)) = content_box(&rgb) else {
        return Ok(None);
    };

    let pad = ((f32::from(dpi.max(72)) * PAD_INCH).round() as i64).max(8);
    let mut x0 = i64::from(left) - pad;
    let mut y0 = i64::from(top) - pad;
    let x1 = i64::from(right) + pad;
    let y1 = i64::from(bottom) + pad;

    // Feeder is left-aligned and top-fed: keep the paper's left and top margins.
    if (left as f32) < width as f32 * ORIGIN_ALIGN_FRAC {
        x0 = 0;
    }
    if (top as f32) < height as f32 * ORIGIN_ALIGN_FRAC {
        y0 = 0;
    }

    let x0 = x0.clamp(0, i64::from(width) - 1) as u32;
    let y0 = y0.clamp(0, i64::from(height) - 1) as u32;
    let x1 = x1.clamp(i64::from(x0) + 1, i64::from(width)) as u32;
    let y1 = y1.clamp(i64::from(y0) + 1, i64::from(height)) as u32;
    let crop_w = x1 - x0;
    let crop_h = y1 - y0;

    let margin_x = (width - crop_w) as f32 / width as f32;
    let margin_y = (height - crop_h) as f32 / height as f32;
    if margin_x < SKIP_MARGIN_FRAC && margin_y < SKIP_MARGIN_FRAC {
        return Ok(None);
    }

    let cropped = image::imageops::crop_imm(&rgb, x0, y0, crop_w, crop_h).to_image();
    Ok(Some(encode_jpeg(&cropped)?))
}

fn content_box(rgb: &RgbImage) -> Option<(u32, u32, u32, u32)> {
    let (width, height) = rgb.dimensions();
    let long = width.max(height).max(1);
    let scale = (long as f32 / DETECT_MAX as f32).max(1.0);
    let dw = ((width as f32 / scale).round() as u32).max(1);
    let dh = ((height as f32 / scale).round() as u32).max(1);
    let small = if dw == width && dh == height {
        DynamicImage::ImageRgb8(rgb.clone()).to_luma8()
    } else {
        let resized = image::imageops::resize(rgb, dw, dh, FilterType::Triangle);
        DynamicImage::ImageRgb8(resized).to_luma8()
    };

    let min_row = ((dw as f32) * 0.004).round().max(2.0) as u32;
    let min_col = ((dh as f32) * 0.004).round().max(2.0) as u32;

    let mut row_hit = vec![false; dh as usize];
    let mut col_hit = vec![false; dw as usize];
    for y in 0..dh {
        let mut dark = 0u32;
        for x in 0..dw {
            if small.get_pixel(x, y)[0] < CONTENT_LUMA {
                dark += 1;
                col_hit[x as usize] = true;
            }
        }
        if dark >= min_row {
            row_hit[y as usize] = true;
        }
    }
    for x in 0..dw {
        if !col_hit[x as usize] {
            continue;
        }
        let mut dark = 0u32;
        for y in 0..dh {
            if small.get_pixel(x, y)[0] < CONTENT_LUMA {
                dark += 1;
            }
        }
        col_hit[x as usize] = dark >= min_col;
    }

    let top = row_hit.iter().position(|&h| h)? as u32;
    let bottom = row_hit.iter().rposition(|&h| h)? as u32 + 1;
    let left = col_hit.iter().position(|&h| h)? as u32;
    let right = col_hit.iter().rposition(|&h| h)? as u32 + 1;

    Some((
        ((left as f32) * scale).floor() as u32,
        ((top as f32) * scale).floor() as u32,
        ((right as f32) * scale).ceil().min(width as f32) as u32,
        ((bottom as f32) * scale).ceil().min(height as f32) as u32,
    ))
}

fn encode_jpeg(rgb: &RgbImage) -> AppResult<Vec<u8>> {
    let (w, h) = rgb.dimensions();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY).write_image(
        rgb.as_raw(),
        w,
        h,
        ExtendedColorType::Rgb8,
    )?;
    Ok(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn jpeg_with_rect(
        width: u32,
        height: u32,
        x: u32,
        y: u32,
        rw: u32,
        rh: u32,
    ) -> Arc<Vec<u8>> {
        let mut img = RgbImage::from_pixel(width, height, Rgb([255, 255, 255]));
        for py in y..y + rh {
            for px in x..x + rw {
                img.put_pixel(px, py, Rgb([20, 20, 20]));
            }
        }
        Arc::new(encode_jpeg(&img).expect("jpeg"))
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap()
    }

    #[test]
    fn empty_bytes_unchanged() {
        let src = Arc::new(Vec::new());
        assert!(trim_to_page(src, 300).is_empty());
    }

    #[test]
    fn blank_page_unchanged() {
        let img = RgbImage::from_pixel(200, 280, Rgb([255, 255, 255]));
        let src = Arc::new(encode_jpeg(&img).unwrap());
        let out = trim_to_page(Arc::clone(&src), 100);
        assert_eq!(out.as_slice(), src.as_slice());
    }

    #[test]
    fn clips_feeder_around_a4_like_page() {
        // 8.5" x 14" bed at 100 dpi; ink on an A4-like sheet, unused feeder on the
        // right and below (same leftover Scanbro currently saves).
        let src = jpeg_with_rect(850, 1400, 40, 30, 720, 1050);
        let out = trim_to_page(src, 100);
        let (w, h) = dims(&out);
        assert!(w < 800, "width {w} should drop unused feeder");
        assert!(h < 1150, "height {h} should clip to the page, not 14\"");
        assert!(w > 720, "width {w} should keep the page");
        assert!(h > 1050, "height {h} should keep the page");
    }

    #[test]
    fn full_bleed_skips_reencode() {
        let src = jpeg_with_rect(400, 500, 0, 0, 400, 500);
        let out = trim_to_page(Arc::clone(&src), 100);
        let (w, h) = dims(&out);
        assert_eq!((w, h), (400, 500));
    }
}
