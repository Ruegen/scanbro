use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageReader};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref};

use crate::error::{AppError, AppResult};

/// Typical mailbox limit. Email PDF is recompressed until it fits.
pub const EMAIL_MAX_BYTES: usize = 5 * 1024 * 1024;

pub fn pages_to_pdf(pages: &[Arc<Vec<u8>>], dpi: u16) -> AppResult<Vec<u8>> {
    let images: Vec<&Arc<Vec<u8>>> = pages.iter().filter(|p| !p.is_empty()).collect();
    if images.is_empty() {
        return Err(AppError::Pdf("no scanned pages to save".into()));
    }
    let dpi = f32::from(dpi.max(72));

    let mut pdf = Pdf::new();
    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let mut next = 3i32;
    let mut kid_ids = Vec::new();

    for jpeg in images {
        let (px_w, px_h) = jpeg_dimensions(jpeg)?;
        let page_w = px_w as f32 * 72.0 / dpi;
        let page_h = px_h as f32 * 72.0 / dpi;

        let page_id = Ref::new(next);
        next += 1;
        let content_id = Ref::new(next);
        next += 1;
        let img_id = Ref::new(next);
        next += 1;

        let mut image = pdf.image_xobject(img_id, jpeg.as_slice());
        image.filter(Filter::DctDecode);
        image.width(px_w as i32);
        image.height(px_h as i32);
        image.bits_per_component(8);
        image.color_space().device_rgb();
        image.finish();

        let mut content = Content::new();
        content.save_state();
        content.transform([page_w, 0.0, 0.0, page_h, 0.0, 0.0]);
        content.x_object(Name(b"Im0"));
        content.restore_state();
        pdf.stream(content_id, &content.finish());

        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, page_w, page_h));
        page.parent(pages_id);
        page.contents(content_id);
        page.resources().x_objects().pair(Name(b"Im0"), img_id);
        page.finish();

        kid_ids.push(page_id);
    }

    let count = i32::try_from(kid_ids.len()).unwrap_or(0);
    pdf.pages(pages_id).kids(kid_ids).count(count);
    pdf.catalog(catalog_id).pages(pages_id);
    Ok(pdf.finish())
}

pub fn write_pdf(pages: &[Arc<Vec<u8>>], path: &Path, dpi: u16) -> AppResult<()> {
    let bytes = pages_to_pdf(pages, dpi)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

pub fn write_email_pdf(pages: &[Arc<Vec<u8>>], path: &Path, dpi: u16) -> AppResult<usize> {
    let bytes = pages_to_email_pdf(pages, dpi)?;
    let len = bytes.len();
    std::fs::write(path, bytes)?;
    Ok(len)
}

pub fn pages_to_email_pdf(pages: &[Arc<Vec<u8>>], dpi: u16) -> AppResult<Vec<u8>> {
    let full = pages_to_pdf(pages, dpi)?;
    if full.len() <= EMAIL_MAX_BYTES {
        return Ok(full);
    }

    let decoded: Vec<DynamicImage> = pages
        .iter()
        .filter(|p| !p.is_empty())
        .map(|jpeg| {
            ImageReader::new(Cursor::new(jpeg.as_slice()))
                .with_guessed_format()?
                .decode()
        })
        .collect::<Result<_, _>>()?;
    if decoded.is_empty() {
        return Err(AppError::Pdf("no scanned pages to save".into()));
    }

    let mut scale = ((EMAIL_MAX_BYTES as f32 / full.len() as f32).sqrt() * 0.92).clamp(0.15, 0.95);
    let mut quality = 78u8;
    let mut best = full;

    for _ in 0..10 {
        let jpegs = encode_pages(&decoded, scale, quality)?;
        let out_dpi = ((f32::from(dpi) * scale).round() as u16).max(72);
        let pdf = pages_to_pdf(&jpegs, out_dpi)?;
        if pdf.len() < best.len() {
            best = pdf;
        }
        if best.len() <= EMAIL_MAX_BYTES {
            return Ok(best);
        }
        let overshoot = best.len() as f32 / EMAIL_MAX_BYTES as f32;
        scale = (scale * (1.0 / overshoot).sqrt() * 0.9).clamp(0.12, 0.95);
        quality = quality.saturating_sub(8).max(38);
    }

    if best.len() <= EMAIL_MAX_BYTES {
        Ok(best)
    } else {
        Err(AppError::Pdf(format!(
            "Couldn't get this PDF under 5 MB (still {:.1} MB).",
            best.len() as f64 / 1_048_576.0
        )))
    }
}

fn encode_pages(pages: &[DynamicImage], scale: f32, quality: u8) -> AppResult<Vec<Arc<Vec<u8>>>> {
    let mut out = Vec::with_capacity(pages.len());
    for page in pages {
        let rgb = page.to_rgb8();
        let (w, h) = rgb.dimensions();
        let nw = ((w as f32) * scale).round().max(1.0) as u32;
        let nh = ((h as f32) * scale).round().max(1.0) as u32;
        let pixels = if nw == w && nh == h {
            rgb.into_raw()
        } else {
            image::imageops::resize(&rgb, nw, nh, FilterType::Triangle).into_raw()
        };
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, quality).write_image(
            &pixels,
            nw,
            nh,
            ExtendedColorType::Rgb8,
        )?;
        out.push(Arc::new(jpeg));
    }
    Ok(out)
}

fn jpeg_dimensions(bytes: &Arc<Vec<u8>>) -> AppResult<(u32, u32)> {
    let reader = ImageReader::new(Cursor::new(bytes.as_slice())).with_guessed_format()?;
    Ok(reader.into_dimensions()?)
}
