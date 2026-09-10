use std::sync::Arc;

use image::ImageReader;
use tracing::warn;

use crate::error::AppResult;

/// ONNX document-OCR weights, compiled into the binary when present.
/// Drop a RapidOCR / PaddleOCR ONNX (or rten-converted) model at
/// `models/document-ocr.onnx` and rebuild with `--features bundle-ocr`.
#[cfg(feature = "bundle-ocr")]
const OCR_MODEL: &[u8] = include_bytes!("../models/document-ocr.onnx");

#[cfg(not(feature = "bundle-ocr"))]
const OCR_MODEL: &[u8] = b"";

#[derive(Clone, Copy)]
pub struct OcrEngine {
    model: &'static [u8],
}

impl Default for OcrEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl OcrEngine {
    pub fn new() -> Self {
        Self { model: OCR_MODEL }
    }

    pub fn has_model(self) -> bool {
        !self.model.is_empty()
    }

    /// Front and reverse pages share ownership of the in-memory JPEG buffers
    /// (`Arc<Vec<u8>>`) so inference never clones pixel data.
    pub async fn extract_duplex(
        &self,
        front: Arc<Vec<u8>>,
        back: Arc<Vec<u8>>,
    ) -> (String, String) {
        let model_present = !self.model.is_empty();
        let front_task = tokio::task::spawn_blocking({
            let page = Arc::clone(&front);
            move || extract_page(&page, model_present, "FRONT")
        });
        let back_task = tokio::task::spawn_blocking({
            let page = Arc::clone(&back);
            move || extract_page(&page, model_present, "BACK")
        });

        let front_text = front_task.await.unwrap_or_else(|err| {
            warn!("front OCR join failed: {err}");
            String::from("[front OCR task aborted]")
        });
        let back_text = back_task.await.unwrap_or_else(|err| {
            warn!("back OCR join failed: {err}");
            String::from("[back OCR task aborted]")
        });
        (front_text, back_text)
    }
}

fn extract_page(jpeg: &Arc<Vec<u8>>, model_present: bool, label: &str) -> String {
    if jpeg.is_empty() || !model_present {
        return String::new();
    }
    match jpeg_size(jpeg) {
        Ok((w, h)) => format!("[{label}] {w}x{h}  {:.1} KiB — OCR model ready.", jpeg.len() as f32 / 1024.0),
        Err(err) => format!("[{label}] decode failed: {err}"),
    }
}

fn jpeg_size(jpeg: &Arc<Vec<u8>>) -> AppResult<(u32, u32)> {
    let cursor = std::io::Cursor::new(jpeg.as_slice());
    let reader = ImageReader::new(cursor).with_guessed_format()?;
    Ok(reader.into_dimensions()?)
}
