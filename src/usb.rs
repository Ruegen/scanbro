use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info};

use crate::error::{AppError, AppResult};
use crate::state::{ScannerInfo, SharedState, Transport, TransportPref};

const IPP_USB_PORTS: &[u16] = &[60000, 60001, 60002, 60003, 631];
const DUPLEX_SOURCES: &[&str] = &[
    "Automatic Document Feeder(left aligned,Duplex)",
    "Automatic Document Feeder(Duplex)",
    "ADF Duplex",
    "Automatic Document Feeder(left aligned)",
    "Automatic Document Feeder",
    "ADF",
];
const SIMPLEX_SOURCES: &[&str] = &[
    "Automatic Document Feeder(left aligned)",
    "Automatic Document Feeder",
    "ADF",
    "Automatic Document Feeder(simplex)",
];

pub fn spawn(state: SharedState) {
    tokio::spawn(async move {
        let http = match reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(250))
            .timeout(Duration::from_millis(800))
            .pool_max_idle_per_host(0)
            .http1_only()
            .tcp_nodelay(true)
            .build()
        {
            Ok(http) => http,
            Err(_) => return,
        };
        loop {
            let pref = { state.lock().transport_pref };
            if pref.allows_usb() {
                let already_usb = {
                    state
                        .lock()
                        .scanner
                        .as_ref()
                        .is_some_and(|s| s.via == Transport::Usb)
                };
                match probe(&http, already_usb).await {
                    Some(scanner) => register(&state, scanner),
                    None => {
                        let plugged = brother_usb().is_some();
                        let mut guard = state.lock();
                        let holding_usb =
                            guard.scanner.as_ref().is_some_and(|s| s.via == Transport::Usb);
                        if !plugged && !holding_usb
                            && guard
                                .discovered
                                .iter()
                                .any(|s| s.via == Transport::Usb)
                        {
                            guard.forget_via(
                                Transport::Usb,
                                "USB scanner unplugged. Looking again…",
                            );
                        } else if holding_usb && !plugged {
                            guard.forget_via(
                                Transport::Usb,
                                "USB scanner unplugged. Looking again…",
                            );
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
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    });
}

async fn probe(http: &reqwest::Client, already_usb: bool) -> Option<ScannerInfo> {
    if let Some(scanner) = probe_ipp_usb(http).await {
        return Some(scanner);
    }
    if already_usb {
        return None;
    }
    list_brother_sane().await
}

async fn probe_ipp_usb(http: &reqwest::Client) -> Option<ScannerInfo> {
    let name = brother_usb().unwrap_or_else(|| "Brother".into());
    let mut futs = Vec::new();
    for port in IPP_USB_PORTS {
        let http = http.clone();
        let name = name.clone();
        futs.push(async move {
            let url = format!("http://127.0.0.1:{port}/eSCL/ScannerStatus");
            http.get(&url)
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .text()
                .await
                .ok()?;
            debug!(port, "USB network-style scanner responded");
            Some(ScannerInfo {
                name,
                hostname: "usb".into(),
                ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: *port,
                escl_root: "eSCL".into(),
                service_type: "usb".into(),
                via: Transport::Usb,
                sane_device: None,
            })
        });
    }
    let results = futures_util::future::join_all(futs).await;
    results.into_iter().flatten().next()
}

async fn list_brother_sane() -> Option<ScannerInfo> {
    if brother_usb().is_none() {
        return None;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("scanimage")
            .arg("-L")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
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
    info!(name = %scanner.name, "registered USB scanner");
    guard.offer_scanner(scanner);
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
        "scanbro-{}-{}",
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
            last_err = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
        if last_err.is_empty() {
            debug!(%source, status = ?output.status, "USB source rejected with no message");
            continue;
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
    } else if lower.contains("invalid argument") || lower.contains("unknown") {
        "USB scan settings weren't accepted. Try Scan again.".into()
    } else if raw.trim().is_empty() {
        "Couldn't scan over USB.".into()
    } else {
        tracing::error!(%raw, "USB scan failed");
        "Couldn't scan over USB.".into()
    }
}

pub fn brother_usb_present() -> bool {
    brother_usb().is_some()
}
