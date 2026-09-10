use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use futures_util::StreamExt;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info};

use crate::error::AppResult;
use crate::state::{ConnectionState, ScannerInfo, SharedState, Transport};

const USCAN: &str = "_uscan._tcp.local.";
const TARGET_MARKERS: &[&str] = &["DS-940DW", "DS940DW", "DS-940"];

pub fn spawn(state: SharedState) -> AppResult<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;
    let uscan = mdns.browse(USCAN)?;
    info!("mDNS browse started for {USCAN}");

    let state_http = SharedState::clone(&state);
    tokio::spawn(async move {
        while let Ok(event) = uscan.recv_async().await {
            handle_event(&state_http, event);
        }
    });

    Ok(mdns)
}

fn handle_event(state: &SharedState, event: ServiceEvent) {
    match event {
        ServiceEvent::ServiceResolved(info) => {
            debug!(
                fullname = info.get_fullname(),
                host = info.get_hostname(),
                port = info.get_port(),
                "mDNS resolved"
            );
            let Some(scanner) = scanner_from_info(&info) else {
                return;
            };
            let mut guard = state.lock();
            if !guard.transport_pref.allows_wifi() {
                return;
            }
            if guard
                .scanner
                .as_ref()
                .is_some_and(|s| s.via == Transport::Usb)
            {
                return;
            }
            let take = match &guard.scanner {
                None => true,
                Some(existing) if existing.via == Transport::Wifi => {
                    scanner.wifi_quality() > existing.wifi_quality()
                        || (is_target(&scanner.name) && !is_target(&existing.name))
                }
                Some(_) => false,
            };
            if take {
                info!(
                    name = %scanner.name,
                    ip = %scanner.ip,
                    port = scanner.port,
                    root = %scanner.escl_root,
                    "registered scanner"
                );
                guard.status_line = format!("Found {} on Wi-Fi.", scanner.name);
                if matches!(
                    guard.connection,
                    ConnectionState::AwaitingConnection | ConnectionState::Degraded
                ) {
                    guard.connection = ConnectionState::Connected;
                }
                guard.scanner = Some(scanner);
                guard.last_seen = Some(Instant::now());
            }
        }
        ServiceEvent::ServiceRemoved(_, fullname) => {
            // Dual-stack mDNS often removes IPv6 or sibling records while HTTP still works.
            debug!(%fullname, "mDNS removal ignored; status poll owns Wi-Fi liveness");
        }
        ServiceEvent::SearchStarted(ty) => debug!(%ty, "mDNS search started"),
        ServiceEvent::SearchStopped(ty) => debug!(%ty, "mDNS search stopped"),
        _ => {}
    }
}

pub fn scanner_from_info(info: &ServiceInfo) -> Option<ScannerInfo> {
    let ip = pick_ipv4(info.get_addresses())?;
    let name = txt(info, "ty")
        .map(friendly_device_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| friendly_device_name(display_name(info.get_fullname())));
    let root = txt(info, "rs")
        .map(|s| s.trim_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "eSCL".into());

    Some(ScannerInfo {
        name,
        hostname: info.get_hostname().trim_end_matches('.').to_string(),
        ip,
        port: info.get_port(),
        escl_root: root,
        service_type: info.get_type().to_string(),
        via: Transport::Wifi,
        sane_device: None,
    })
}

fn pick_ipv4(addrs: &HashSet<IpAddr>) -> Option<IpAddr> {
    addrs
        .iter()
        .copied()
        .find(IpAddr::is_ipv4)
        .filter(|ip| *ip != IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

fn txt(info: &ServiceInfo, key: &str) -> Option<String> {
    info.get_property_val_str(key)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn display_name(fullname: &str) -> String {
    fullname
        .replace("\\032", " ")
        .replace('\\', "")
        .split("._")
        .next()
        .unwrap_or(fullname)
        .trim()
        .to_string()
}

fn friendly_device_name(raw: impl AsRef<str>) -> String {
    let name = raw
        .as_ref()
        .replace("\\032", " ")
        .replace('\\', "");
    let lower = name.to_ascii_lowercase();
    if name.is_empty() || lower.contains("uscan") || lower.starts_with('_') {
        "Brother DS-940DW".into()
    } else {
        name.trim().to_string()
    }
}

fn is_target(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    TARGET_MARKERS.iter().any(|m| upper.contains(m))
}

pub async fn discover_once(timeout: std::time::Duration) -> AppResult<Vec<ScannerInfo>> {
    let mdns = ServiceDaemon::new()?;
    let receiver = mdns.browse(USCAN)?;
    let mut found = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            break;
        }
        match tokio::time::timeout(remain, receiver.recv_async()).await {
            Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                if let Some(scanner) = scanner_from_info(&info) {
                    println!(
                        "resolved  {}  {}:{}/{}  ({})",
                        scanner.name,
                        scanner.ip,
                        scanner.port,
                        scanner.escl_root,
                        scanner.hostname
                    );
                    found.push(scanner);
                }
            }
            Ok(Ok(ServiceEvent::SearchStarted(_))) => {}
            Ok(Ok(other)) => println!("event     {other:?}"),
            Ok(Err(err)) => {
                mdns.shutdown().ok();
                return Err(crate::error::AppError::Mdns(err.to_string()));
            }
            Err(_) => break,
        }
    }

    mdns.shutdown().ok();
    Ok(found)
}

pub fn spawn_lan_probe(state: SharedState) {
    tokio::spawn(async move {
        let http = match reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(250))
            .timeout(std::time::Duration::from_millis(700))
            .build()
        {
            Ok(http) => http,
            Err(_) => return,
        };
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            {
                let guard = state.lock();
                if !guard.transport_pref.allows_wifi() {
                    continue;
                }
                if guard.scanner.as_ref().is_some_and(|s| s.via == Transport::Wifi) {
                    continue;
                }
            }
            if let Some(scanner) = probe_lan_escl(&http).await {
                let mut guard = state.lock();
                if !guard.transport_pref.allows_wifi() {
                    continue;
                }
                if guard.scanner.as_ref().is_some_and(|s| s.via == Transport::Usb) {
                    continue;
                }
                if guard.scanner.is_none()
                    || guard
                        .scanner
                        .as_ref()
                        .is_some_and(|s| scanner.wifi_quality() > s.wifi_quality())
                {
                    info!(ip = %scanner.ip, port = scanner.port, "found scanner on the LAN");
                    guard.status_line = format!("Found {} on Wi-Fi.", scanner.name);
                    if matches!(
                        guard.connection,
                        ConnectionState::AwaitingConnection | ConnectionState::Degraded
                    ) {
                        guard.connection = ConnectionState::Connected;
                    }
                    guard.scanner = Some(scanner);
                    guard.last_seen = Some(Instant::now());
                }
            }
        }
    });
}

async fn probe_lan_escl(http: &reqwest::Client) -> Option<ScannerInfo> {
    let addrs = if_addrs::get_if_addrs().ok()?;
    let mut hosts = Vec::new();
    for iface in addrs {
        if iface.is_loopback() {
            continue;
        }
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        let ip = v4.ip;
        let prefix = u32::from(v4.netmask).count_ones();
        if !(24..=30).contains(&prefix) {
            continue;
        }
        let bits = u32::from(ip);
        let mask = !0u32 << (32 - prefix);
        let base = bits & mask;
        let max = (1u32 << (32 - prefix)).saturating_sub(2);
        for i in 1..=max {
            let host = Ipv4Addr::from(base + i);
            if host != ip {
                hosts.push(host);
            }
        }
    }
    if hosts.is_empty() {
        return None;
    }
    let ports = [8080u16, 80];
    let mut tasks = Vec::new();
    for host in hosts {
        for port in ports {
            let http = http.clone();
            tasks.push(async move {
                let url = format!("http://{host}:{port}/eSCL/ScannerStatus");
                let body = http.get(&url).send().await.ok()?.error_for_status().ok()?.text().await.ok()?;
                crate::escl::parse_scanner_status(&body).ok()?;
                Some(ScannerInfo {
                    name: "Brother DS-940DW".into(),
                    hostname: host.to_string(),
                    ip: IpAddr::V4(host),
                    port,
                    escl_root: "eSCL".into(),
                    service_type: "_uscan._tcp.local.".into(),
                    via: Transport::Wifi,
                    sane_device: None,
                })
            });
        }
    }
    let mut stream = futures_util::stream::iter(tasks).buffer_unordered(48);
    while let Some(item) = stream.next().await {
        if item.is_some() {
            return item;
        }
    }
    None
}
