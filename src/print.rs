use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use tracing::info;

use crate::error::{AppError, AppResult};

/// Build a PDF from the preview pages and send it to the default printer.
pub fn print_pages(pages: &[Arc<Vec<u8>>], dpi: u16, draft: bool) -> AppResult<String> {
    let images: Vec<&Arc<Vec<u8>>> = pages.iter().filter(|p| !p.is_empty()).collect();
    if images.is_empty() {
        return Err(AppError::Print("Scan a page before printing.".into()));
    }
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "scanbro-print-{}.pdf",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    crate::pdf::write_pdf(pages, &path, dpi)?;
    let duplex = images.len() >= 2;
    let result = send_to_printer(&path, draft, duplex);
    let _ = std::fs::remove_file(&path);
    result
}

fn send_to_printer(path: &Path, draft: bool, duplex: bool) -> AppResult<String> {
    let output = Command::new("lp")
        .args(lp_args(draft, duplex))
        .arg(path)
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                AppError::Print("Couldn't find a printer command (lp).".into())
            } else {
                AppError::Print(format!("Couldn't start the print job: {err}"))
            }
        })?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = print_accepted(&stdout, path);
        info!(%msg, draft, duplex, "sent scan to printer");
        return Ok(msg);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(AppError::Print(friendly_lp_error(&stderr)))
}

pub(crate) fn lp_args(draft: bool, duplex: bool) -> Vec<&'static str> {
    let mut args = vec![
        "-o",
        "media=A4",
        "-o",
        "fit-to-page",
        "-o",
        if draft {
            "print-quality=3"
        } else {
            "print-quality=4"
        },
        "-o",
        if duplex {
            "sides=two-sided-long-edge"
        } else {
            "sides=one-sided"
        },
    ];
    if draft {
        args.extend(["-o", "ColorModel=Gray"]);
    }
    args
}

fn print_accepted(stdout: &str, path: &Path) -> String {
    let trimmed = stdout.trim();
    if let Some(rest) = trimmed.strip_prefix("request id is ") {
        let id = rest.split_whitespace().next().unwrap_or(rest);
        format!("Printing {id}")
    } else if !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        format!("Printing {}", file_name(path))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("scan")
        .to_string()
}

pub(crate) fn friendly_lp_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("no default destination")
        || lower.contains("no destinations added")
        || lower.contains("unknown destination")
    {
        "No default printer. Set one in system settings, then try Print again.".into()
    } else if lower.contains("not accepting") || lower.contains("paused") {
        "The printer isn't accepting jobs right now.".into()
    } else if raw.trim().is_empty() {
        "Couldn't print.".into()
    } else {
        tracing::error!(%raw, "print failed");
        "Couldn't print.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_uses_low_quality_and_gray() {
        let args = lp_args(true, false);
        assert!(args.contains(&"media=A4"));
        assert!(args.contains(&"fit-to-page"));
        assert!(args.contains(&"print-quality=3"));
        assert!(args.contains(&"ColorModel=Gray"));
        assert!(args.contains(&"sides=one-sided"));
    }

    #[test]
    fn normal_keeps_color_and_duplex() {
        let args = lp_args(false, true);
        assert!(args.contains(&"print-quality=4"));
        assert!(args.contains(&"sides=two-sided-long-edge"));
        assert!(!args.contains(&"ColorModel=Gray"));
    }

    #[test]
    fn missing_printer_is_plain_language() {
        let msg = friendly_lp_error("lp: No default destination");
        assert!(msg.contains("default printer"));
        assert!(!msg.to_ascii_lowercase().contains("destination"));
    }
}
