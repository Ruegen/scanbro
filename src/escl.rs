use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_TYPE, LOCATION};
use reqwest::{Client, StatusCode};
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::state::{ScanProgress, ScannerInfo, ScannerStatus, SharedState};

const STATUS_PATH: &str = "/ScannerStatus";
const JOBS_PATH: &str = "/ScanJobs";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const SCAN_TIMEOUT: Duration = Duration::from_secs(180);

fn scan_settings(long_receipt: bool, dpi: u16) -> String {
    // Duplex hardware max is 14". Long receipt is one-sided, up to ~31.5" (9450 @ 300ths").
    let (height, duplex) = if long_receipt {
        (9450, "false")
    } else {
        (4200, "true")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScanSettings xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>2.0</pwg:Version>
  <scan:Intent>Document</scan:Intent>
  <pwg:ScanRegions>
    <pwg:ScanRegion>
      <pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits>
      <pwg:XOffset>0</pwg:XOffset>
      <pwg:YOffset>0</pwg:YOffset>
      <pwg:Width>2550</pwg:Width>
      <pwg:Height>{height}</pwg:Height>
    </pwg:ScanRegion>
  </pwg:ScanRegions>
  <pwg:InputSource>Feeder</pwg:InputSource>
  <scan:Duplex>{duplex}</scan:Duplex>
  <scan:AutoCrop>true</scan:AutoCrop>
  <pwg:ContentType>TextAndPhoto</pwg:ContentType>
  <scan:ColorMode>RGB24</scan:ColorMode>
  <scan:XResolution>{dpi}</scan:XResolution>
  <scan:YResolution>{dpi}</scan:YResolution>
  <pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>
</scan:ScanSettings>
"#
    )
}

#[derive(Clone)]
pub struct EsclClient {
    http: Client,
}

impl EsclClient {
    pub fn new() -> AppResult<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(4))
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .http1_only()
            .tcp_nodelay(true)
            .build()?;
        Ok(Self { http })
    }

    pub fn spawn_poller(&self, state: SharedState) {
        let client = self.clone();
        tokio::spawn(async move {
            let mut misses = 0u8;
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let scanner = { state.lock().scanner.clone() };
                let Some(scanner) = scanner else {
                    continue;
                };
                if scanner.sane_device.is_some() {
                    continue;
                }
                match client.fetch_status(&scanner).await {
                    Ok(status) => {
                        misses = 0;
                        let previous = state.lock().status.as_ref().and_then(|s| s.battery_percent);
                        let mut status = status;
                        if status.battery_percent.is_none() {
                            status.battery_percent =
                                client.fetch_battery(&scanner).await.or(previous);
                        }
                        let mut guard = state.lock();
                        guard.status = Some(status);
                        guard.last_seen = Some(std::time::Instant::now());
                        if guard.connection == crate::state::ConnectionState::AwaitingConnection
                            || guard.connection == crate::state::ConnectionState::Degraded
                        {
                            guard.connection = crate::state::ConnectionState::Connected;
                            guard.status_line = format!("Connected over {}.", scanner.via.label());
                        }
                    }
                    Err(err) => {
                        warn!("ScannerStatus poll failed: {err}");
                        if let Some(fixed) = client.try_alt_port(&scanner).await {
                            let mut guard = state.lock();
                            guard.status = Some(fixed.0);
                            guard.scanner = Some(fixed.1);
                            guard.last_seen = Some(std::time::Instant::now());
                            guard.connection = crate::state::ConnectionState::Connected;
                            guard.status_line = "Connected over Wi-Fi.".into();
                            misses = 0;
                            continue;
                        }
                        let mut guard = state.lock();
                        if matches!(
                            guard.connection,
                            crate::state::ConnectionState::Scanning
                                | crate::state::ConnectionState::RunningOcr
                        ) {
                            continue;
                        }
                        misses += 1;
                        if misses == 1 {
                            guard.connection = crate::state::ConnectionState::Degraded;
                            guard.status_line = "Checking the scanner again…".into();
                        } else if misses >= 4 {
                            guard.mark_disconnected("Scanner went offline. Looking again…");
                            misses = 0;
                        }
                    }
                }
            }
        });
    }

    pub async fn fetch_status(&self, scanner: &ScannerInfo) -> AppResult<ScannerStatus> {
        let url = format!("{}{STATUS_PATH}", scanner.base_url());
        let body = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(AppError::from)?
            .error_for_status()
            .map_err(|_| AppError::Escl("Couldn't read scanner status.".into()))?
            .text()
            .await
            .map_err(AppError::from)?;
        parse_scanner_status(&body)
    }

    async fn fetch_battery(&self, scanner: &ScannerInfo) -> Option<u8> {
        let host = match scanner.ip {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        };
        for port in [80_u16, 8080] {
            let url = format!("http://{host}:{port}/ft/gen_status");
            let Ok(resp) = self.http.get(&url).send().await else {
                continue;
            };
            let Ok(body) = resp.text().await else {
                continue;
            };
            if let Some(percent) = parse_battery_percent(&body) {
                return Some(percent);
            }
        }
        None
    }

    pub async fn try_alt_port(&self, scanner: &ScannerInfo) -> Option<(ScannerStatus, ScannerInfo)> {
        if scanner.sane_device.is_some() {
            return None;
        }
        for port in [8080_u16, 80] {
            if port == scanner.port {
                continue;
            }
            let mut alt = scanner.clone();
            alt.port = port;
            if let Ok(status) = self.fetch_status(&alt).await {
                info!(port, "reached scanner on alternate port");
                return Some((status, alt));
            }
        }
        None
    }

    pub async fn scan_duplex(
        &self,
        scanner: &ScannerInfo,
        state: &SharedState,
        long_receipt: bool,
        dpi: u16,
    ) -> AppResult<(Arc<Vec<u8>>, Arc<Vec<u8>>)> {
        let jobs_url = format!("{}{JOBS_PATH}", scanner.base_url());
        let sides = if long_receipt { 1 } else { 2 };
        info!(%jobs_url, long_receipt, dpi, "posting ScanJob");

        {
            let mut guard = state.lock();
            guard.progress = ScanProgress {
                job_posted: false,
                one_sided: long_receipt,
                back_done: long_receipt,
                ..ScanProgress::default()
            };
        }

        let response = self
            .http
            .post(&jobs_url)
            .timeout(SCAN_TIMEOUT)
            .header(CONTENT_TYPE, "text/xml")
            .header(http::header::ACCEPT, "*/*")
            .body(scan_settings(long_receipt, dpi))
            .send()
            .await
            .map_err(AppError::from)?;

        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            return Err(AppError::Escl(
                "Scanner is busy. Load a page and try Scan again.".into(),
            ));
        }

        if !response.status().is_success() && response.status() != StatusCode::CREATED {
            return Err(AppError::Escl(
                "Scanner couldn't start the scan. Load a page and try again.".into(),
            ));
        }

        let job_url = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
            .ok_or_else(|| AppError::Escl("Scanner didn't return a scan job.".into()))?;

        let job_url = job_url_on_scanner(scanner, &job_url);
        debug!(%job_url, "scan job created");
        state.lock().progress.job_posted = true;

        let mut pages: Vec<Arc<Vec<u8>>> = Vec::new();
        for index in 0..sides {
            {
                let mut guard = state.lock();
                guard.progress.current_side = (index + 1) as u8;
                guard.progress.current_bytes = 0;
                guard.progress.expected_bytes = 0;
                guard.status_line = if long_receipt {
                    "Scanning receipt…".into()
                } else if index == 0 {
                    "Scanning front…".into()
                } else {
                    "Scanning back…".into()
                };
            }
            let next = format!("{}/NextDocument", job_url.trim_end_matches('/'));
            let page_resp = fetch_next_document(&self.http, &next).await?;
            if page_resp.status() == StatusCode::NOT_FOUND {
                break;
            }
            if page_resp.status() == StatusCode::SERVICE_UNAVAILABLE {
                return Err(AppError::Escl("Load a page in the feeder, then scan.".into()));
            }
            let page_resp = page_resp
                .error_for_status()
                .map_err(|_| AppError::Escl("Couldn't download a scanned page.".into()))?;
            let extracted = collect_images(page_resp, state).await?;
            for image in extracted {
                {
                    let mut guard = state.lock();
                    if index == 0 && pages.is_empty() {
                        guard.progress.front_bytes = image.len();
                        guard.progress.front_done = true;
                        guard.front_image = Some(Arc::clone(&image));
                    } else {
                        guard.progress.back_bytes = image.len();
                        guard.progress.back_done = true;
                        guard.back_image = Some(Arc::clone(&image));
                    }
                    guard.progress.current_bytes = 0;
                    guard.progress.expected_bytes = 0;
                }
                pages.push(image);
            }
        }

        let _ = self.http.delete(&job_url).send().await;

        match pages.len() {
            0 => Err(AppError::Escl("scan job returned no pages".into())),
            1 => {
                warn!("duplex job yielded a single page; synthesizing empty back");
                Ok((pages.remove(0), Arc::new(Vec::new())))
            }
            _ => Ok((pages.remove(0), pages.remove(0))),
        }
    }
}

fn job_url_on_scanner(scanner: &ScannerInfo, location: &str) -> String {
    let origin = match scanner.ip {
        std::net::IpAddr::V4(ip) => format!("http://{ip}:{}", scanner.port),
        std::net::IpAddr::V6(ip) => format!("http://[{ip}]:{}", scanner.port),
    };
    let loc = location.trim();
    if let Some(idx) = loc.find("/eSCL") {
        return format!("{}{}", origin, &loc[idx..]);
    }
    if loc.starts_with('/') {
        return format!("{origin}{loc}");
    }
    format!("{origin}/eSCL/ScanJobs")
}

async fn fetch_next_document(http: &Client, url: &str) -> AppResult<reqwest::Response> {
    let mut last = None;
    for attempt in 0..6 {
        match http
            .get(url)
            .timeout(SCAN_TIMEOUT)
            .header(http::header::ACCEPT, "image/jpeg, */*")
            .send()
            .await
        {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(Duration::from_millis(400 + attempt * 250)).await;
            }
        }
    }
    Err(last.map(AppError::from).unwrap_or_else(|| {
        AppError::Escl("Couldn't download the scan.".into())
    }))
}

async fn collect_images(
    response: reqwest::Response,
    state: &SharedState,
) -> AppResult<Vec<Arc<Vec<u8>>>> {
    use futures_util::StreamExt;

    let expected = response.content_length().unwrap_or(0) as usize;
    state.lock().progress.expected_bytes = expected;

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if let Some(boundary) = multipart_boundary(&content_type) {
        let stream = response.bytes_stream();
        let mut multipart = multer::Multipart::new(stream, boundary);
        let mut pages = Vec::new();
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|err| AppError::Multipart(err.to_string()))?
        {
            let bytes = field
                .bytes()
                .await
                .map_err(|err| AppError::Multipart(err.to_string()))?;
            state.lock().progress.current_bytes = bytes.len();
            if looks_like_image(&bytes) {
                pages.push(Arc::new(bytes.to_vec()));
            }
        }
        return Ok(pages);
    }

    let mut buf = Vec::with_capacity(expected.max(64 * 1024));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(AppError::from)?;
        buf.extend_from_slice(&chunk);
        state.lock().progress.current_bytes = buf.len();
    }
    if buf.is_empty() {
        return Err(AppError::Escl("empty NextDocument body".into()));
    }
    Ok(vec![Arc::new(buf)])
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    if !content_type.to_ascii_lowercase().starts_with("multipart/") {
        return None;
    }
    content_type.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("boundary=")
            .or_else(|| part.strip_prefix("boundary=\"").and_then(|s| s.strip_suffix('"')))
            .map(|b| b.trim_matches('"').to_string())
    })
}

fn looks_like_image(bytes: &Bytes) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF]) || bytes.starts_with(b"\x89PNG") || bytes.len() > 128
}

pub fn parse_scanner_status(xml: &str) -> AppResult<ScannerStatus> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut status = ScannerStatus::default();
    let mut current = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                current = local_name(e.name().as_ref());
                if status.battery_percent.is_none() {
                    for attr in e.attributes().flatten() {
                        let key = local_name(attr.key.as_ref());
                        let battery_attr = key.to_ascii_lowercase().contains("battery")
                            || current.to_ascii_lowercase().contains("battery");
                        if battery_attr {
                            if let Ok(val) = attr.unescape_value() {
                                status.battery_percent = parse_percent(&val);
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let text = t
                    .unescape()
                    .map_err(|err| AppError::Xml(err.to_string()))?
                    .into_owned();
                match current.as_str() {
                    "State" => status.state = text,
                    "AdfState" => status.adf_state = text,
                    "StateReason" => status.reasons.push(text),
                    "Battery" | "BatteryLevel" | "BatteryPercent" | "Charge" => {
                        status.battery_percent = parse_percent(&text);
                    }
                    _ => {
                        if status.battery_percent.is_none()
                            && current.to_ascii_lowercase().contains("battery")
                        {
                            status.battery_percent = parse_percent(&text);
                        }
                    }
                }
            }
            Ok(Event::End(_)) => current.clear(),
            Ok(Event::Eof) => break,
            Err(err) => return Err(AppError::Xml(err.to_string())),
            _ => {}
        }
        buf.clear();
    }

    if status.state.is_empty() && status.adf_state.is_empty() && status.battery_percent.is_none()
    {
        return Err(AppError::Xml("ScannerStatus missing expected elements".into()));
    }
    Ok(status)
}

fn local_name(raw: &[u8]) -> String {
    let full = String::from_utf8_lossy(raw);
    full.rsplit_once('}')
        .map(|(_, n)| n.to_string())
        .or_else(|| full.rsplit_once(':').map(|(_, n)| n.to_string()))
        .unwrap_or_else(|| full.into_owned())
}

fn parse_percent(text: &str) -> Option<u8> {
    let trimmed = text.trim().trim_end_matches('%');
    trimmed.parse::<f32>().ok().map(|v| v.clamp(0.0, 100.0) as u8)
}

fn parse_battery_percent(xml: &str) -> Option<u8> {
    parse_scanner_status(xml).ok().and_then(|s| s.battery_percent)
}
