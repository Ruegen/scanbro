use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info};

use crate::error::{AppError, AppResult};
use crate::escl::EsclClient;
use crate::state::{ConnectionState, ScannerInfo, SharedState, Transport, TransportPref};

const IPP_USB_PORTS: &[u16] = &[60000, 60001, 60002, 60003, 631];
const DUPLEX_SOURCES: &[&str] = &[
    "Automatic Document Feeder(Duplex)",
    "ADF Duplex",
    "Automatic Document Feeder",
    "ADF",
];
const SIMPLEX_SOURCES: &[&str] = &[
    "Automatic Document Feeder",
    "ADF",
    "Automatic Document Feeder(simplex)",
];

pub fn spawn(state: SharedState, escl: EsclClient) {
    tokio::spawn(async move {
        loop {
            let pref = { state.lock().transport_pref };
            if pref.allows_usb() {
                match probe(&escl).await {
                    Some(scanner) => register(&state, scanner),
                    None => {
                        let plugged = brother_usb().is_some();
                        let mut guard = state.lock();
                        let holding_usb = guard.scanner.as_ref().is_some_and(|s| s.via == Transport::Usb);
                        if holding_usb && !plugged {
                            guard.mark_disconnected("USB scanner unplugged. Looking again…");
                        } else if pref == TransportPref::Usb && !holding_usb {
                            guard.status_line = if plugged {
                                "USB scanner plugged in. Getting it ready…".into()
                            } else {
                                "No USB scanner plugged in. Power it on and use a data cable.".into()
                            };
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

async fn probe(escl: &EsclClient) -> Option<ScannerInfo> {
    if let Some(scanner) = list_brother_sane().await {
        return Some(scanner);
    }
    if brother_usb().is_none() {
        return probe_ipp_usb(escl).await;
    }
    probe_ipp_usb(escl).await
}

async fn probe_ipp_usb(escl: &EsclClient) -> Option<ScannerInfo> {
    for port in IPP_USB_PORTS {
        let candidate = ScannerInfo {
            name: "Brother DS-940DW".into(),
            hostname: "usb".into(),
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: *port,
            escl_root: "eSCL".into(),
            service_type: "usb".into(),
            via: Transport::Usb,
            sane_device: None,
        };
        match escl.fetch_status(&candidate).await {
            Ok(_) => {
                debug!(port, "USB network-style scanner responded");
                return Some(candidate);
            }
            Err(err) => debug!(port, %err, "USB network-style miss"),
        }
    }
    None
}

async fn list_brother_sane() -> Option<ScannerInfo> {
    if brother_usb().is_none() {
        return None;
    }
    let output = tokio::process::Command::new("scanimage")
        .arg("-L")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("device `") else {
            continue;
        };
        let Some((device, desc)) = rest.split_once("' is a ") else {
            continue;
        };
        let device = device.trim();
        if !is_usb_sane_device(device) {
            continue;
        }
        if !(device.to_ascii_lowercase().contains("brother")
            || desc.to_ascii_lowercase().contains("brother"))
        {
            continue;
        }
        let name = {
            let trimmed = desc.trim();
            if trimmed.is_empty() || trimmed.to_ascii_lowercase().contains("uscan") {
                brother_usb()
                    .and_then(|n| if n.is_empty() { None } else { Some(n) })
                    .unwrap_or_else(|| "Brother DS-940DW".into())
            } else {
                trimmed.to_string()
            }
        };
        info!(%device, %name, "found USB scanner");
        return Some(ScannerInfo {
            name,
            hostname: "usb".into(),
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            escl_root: String::new(),
            service_type: "sane".into(),
            via: Transport::Usb,
            sane_device: Some(device.to_string()),
        });
    }
    None
}

fn is_usb_sane_device(device: &str) -> bool {
    let d = device.trim().to_ascii_lowercase();
    if d.starts_with("escl:")
        || d.starts_with("airscan:")
        || d.starts_with("v4l:")
        || d.contains("://")
        || d.contains("https:")
        || d.contains("http:")
    {
        return false;
    }
    d.starts_with("brother")
}

fn register(state: &SharedState, scanner: ScannerInfo) {
    let mut guard = state.lock();
    if !guard.transport_pref.allows_usb() {
        return;
    }
    if scanner.sane_device.is_some() && brother_usb().is_none() {
        return;
    }
    let replace = match &guard.scanner {
        None => true,
        Some(existing) if existing.via == Transport::Wifi && brother_usb().is_none() => false,
        Some(existing) => {
            existing.via != Transport::Usb
                || existing.sane_device != scanner.sane_device
                || existing.port != scanner.port
        }
    };
    if !replace {
        return;
    }
    info!(name = %scanner.name, "registered USB scanner");
    guard.status_line = format!("Found {} on USB.", scanner.name);
    guard.status = Some(crate::state::ScannerStatus {
        state: "Idle".into(),
        adf_state: "Ready".into(),
        battery_percent: None,
        reasons: Vec::new(),
    });
    if matches!(
        guard.connection,
        ConnectionState::AwaitingConnection | ConnectionState::Degraded
    ) {
        guard.connection = ConnectionState::Connected;
    }
    guard.scanner = Some(scanner);
    guard.last_seen = Some(std::time::Instant::now());
}

pub fn brother_usb() -> Option<String> {
    let entries = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let vendor = std::fs::read_to_string(path.join("idVendor")).unwrap_or_default();
        if vendor.trim() != "04f9" {
            continue;
        }
        let product = std::fs::read_to_string(path.join("product")).unwrap_or_default();
        let name = product.trim();
        return Some(if name.is_empty() {
            "Brother DS-940DW".into()
        } else {
            name.to_string()
        });
    }
    None
}

pub async fn scan_duplex(
    device: &str,
    state: &SharedState,
    long_receipt: bool,
    dpi: u16,
) -> AppResult<(Arc<Vec<u8>>, Arc<Vec<u8>>)> {
    let dir = PathBuf::from("/dev/shm").join(format!(
        "ds940dw-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    let prefix = dir.join("page");
    let pattern = format!("{}%d.jpg", prefix.display());
    let sources = if long_receipt {
        SIMPLEX_SOURCES
    } else {
        DUPLEX_SOURCES
    };
    let batch_count = if long_receipt { "1" } else { "2" };

    let mut last_err = String::from("Couldn't scan over USB.");
    let mut scanned = false;
    for source in sources {
        {
            let mut guard = state.lock();
            guard.status_line = if long_receipt {
                "Starting USB receipt scan…".into()
            } else {
                "Starting USB scan…".into()
            };
            guard.progress.job_posted = true;
            guard.progress.one_sided = long_receipt;
            if long_receipt {
                guard.progress.back_done = true;
            }
        }
        let mut cmd = tokio::process::Command::new("scanimage");
        cmd.arg("-d")
            .arg(device)
            .arg("--format=jpeg")
            .arg(format!("--resolution={dpi}"))
            .arg(format!("--source={source}"))
            .arg(format!("--batch={pattern}"))
            .arg(format!("--batch-count={batch_count}"));
        if long_receipt {
            cmd.arg("-y").arg("800");
        }
        let output = cmd
            .output()
            .await
            .map_err(|err| AppError::Escl(format!("Couldn't start USB scan: {err}")))?;

        let got_page = dir.join("page1.jpg").exists() || dir.join("page1.pnm").exists();
        if output.status.success() || got_page {
            scanned = true;
            break;
        }

        if output.status.signal().is_some() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(AppError::Escl(
                "USB scan crashed. Use Wi-Fi Scan instead if the scanner is on the network.".into(),
            ));
        }

        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if last_err.is_empty() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(AppError::Escl("Couldn't scan over USB.".into()));
        }
        debug!(%source, %last_err, "USB source rejected");
    }

    let result = if scanned {
        load_pages(&dir, state)
    } else {
        Err(AppError::Escl(friendly_sane_error(&last_err)))
    };
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn load_pages(dir: &std::path::Path, state: &SharedState) -> AppResult<(Arc<Vec<u8>>, Arc<Vec<u8>>)> {
    let front = read_page(dir, 1)?;
    let back = match read_page(dir, 2) {
        Ok(bytes) => bytes,
        Err(_) => Arc::new(Vec::new()),
    };
    {
        let mut guard = state.lock();
        guard.progress.front_done = true;
        guard.progress.front_bytes = front.len();
        if !back.is_empty() {
            guard.progress.back_done = true;
            guard.progress.back_bytes = back.len();
        } else {
            guard.progress.back_done = true;
        }
        guard.progress.current_bytes = 0;
    }
    if front.is_empty() {
        return Err(AppError::Escl("USB scan produced no pages.".into()));
    }
    Ok((front, back))
}

fn read_page(dir: &std::path::Path, n: u32) -> AppResult<Arc<Vec<u8>>> {
    for ext in ["jpg", "jpeg", "pnm", "png", "tiff", "tif"] {
        let path = dir.join(format!("page{n}.{ext}"));
        if path.exists() {
            return Ok(Arc::new(std::fs::read(path)?));
        }
    }
    Err(AppError::Escl(format!("missing USB page {n}")))
}

fn friendly_sane_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("document feeder out of documents") || lower.contains("no documents") {
        "Load a page into the feeder, then scan.".into()
    } else if lower.contains("busy") {
        "Scanner is busy. Try again in a moment.".into()
    } else if lower.contains("permission") || lower.contains("access") {
        "No permission to use the USB scanner.".into()
    } else if raw.trim().is_empty() {
        "Couldn't scan over USB.".into()
    } else {
        "Couldn't scan over USB.".into()
    }
}

pub fn brother_usb_present() -> bool {
    brother_usb().is_some()
}
