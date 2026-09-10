use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    #[default]
    Wifi,
    Usb,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wifi => "Wi-Fi",
            Self::Usb => "USB",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportPref {
    #[default]
    Auto,
    Wifi,
    Usb,
}

impl TransportPref {
    pub fn allows_wifi(self) -> bool {
        matches!(self, Self::Auto | Self::Wifi)
    }

    pub fn allows_usb(self) -> bool {
        matches!(self, Self::Auto | Self::Usb)
    }

    pub fn matches(self, via: Transport) -> bool {
        match self {
            Self::Auto => true,
            Self::Wifi => via == Transport::Wifi,
            Self::Usb => via == Transport::Usb,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    AwaitingConnection,
    Connected,
    Scanning,
    RunningOcr,
    Degraded,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::AwaitingConnection => "Awaiting Connection",
            Self::Connected => "Connected",
            Self::Scanning => "Scanning",
            Self::RunningOcr => "OCR",
            Self::Degraded => "Degraded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScannerInfo {
    pub name: String,
    pub hostname: String,
    pub ip: IpAddr,
    pub port: u16,
    pub escl_root: String,
    pub service_type: String,
    pub via: Transport,
    pub sane_device: Option<String>,
}

impl ScannerInfo {
    pub fn base_url(&self) -> String {
        let root = self.escl_root.trim_matches('/');
        match self.ip {
            IpAddr::V4(ip) => format!("http://{ip}:{}/{root}", self.port),
            IpAddr::V6(ip) => format!("http://[{ip}]:{}/{root}", self.port),
        }
    }

    pub fn wifi_quality(&self) -> i32 {
        let mut score = 0;
        if self.ip.is_ipv4() {
            score += 20;
        }
        if self.port != 443 {
            score += 30;
        }
        if self.port == 8080 {
            score += 10;
        }
        if !self.service_type.contains("uscans") {
            score += 10;
        }
        score
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScannerStatus {
    pub state: String,
    pub adf_state: String,
    pub battery_percent: Option<u8>,
    pub reasons: Vec<String>,
}

impl ScannerStatus {
    pub fn paper_ready(&self) -> bool {
        let adf = self.adf_state.to_ascii_lowercase();
        if adf.contains("jam") || adf.contains("process") {
            return false;
        }
        adf.contains("loaded") || adf.contains("ready")
    }

    pub fn feeder_kind(&self) -> FeederKind {
        let adf = self.adf_state.to_ascii_lowercase();
        if adf.contains("jam") {
            FeederKind::Jam
        } else if adf.contains("mispick") || adf.contains("double") || adf.contains("multi") {
            FeederKind::Mispick
        } else if adf.contains("process") {
            FeederKind::Pulling
        } else if adf.contains("loaded") || adf.contains("ready") {
            FeederKind::Loaded
        } else if adf.contains("empty") || adf.is_empty() {
            FeederKind::Empty
        } else {
            FeederKind::Unknown
        }
    }

    pub fn fault_message(&self) -> Option<String> {
        match self.feeder_kind() {
            FeederKind::Jam => return Some("Paper jam. Clear the feeder.".into()),
            FeederKind::Mispick => return Some("Feeder mispick. Reload the sheet.".into()),
            _ => {}
        }
        for reason in &self.reasons {
            let lower = reason.to_ascii_lowercase();
            if lower.contains("success")
                || lower.contains("none")
                || lower.contains("jobcompleted")
                || lower.contains("attentionrequired")
            {
                continue;
            }
            if lower.contains("jam") {
                return Some("Paper jam. Clear the feeder.".into());
            }
            if lower.contains("cover") || lower.contains("door") {
                return Some("Close the scanner cover.".into());
            }
            if lower.contains("error") || lower.contains("fail") || lower.contains("stopped") {
                return Some(reason.clone());
            }
        }
        let state = self.state.to_ascii_lowercase();
        if state.contains("stopped") {
            return Some("Scanner stopped.".into());
        }
        if state.contains("down") {
            return Some("Scanner reported an error.".into());
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeederKind {
    Empty,
    Loaded,
    Pulling,
    Jam,
    Mispick,
    Unknown,
}

impl FeederKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "Empty",
            Self::Loaded => "Sheet loaded",
            Self::Pulling => "Pulling through",
            Self::Jam => "Jam",
            Self::Mispick => "Mispick",
            Self::Unknown => "Feeder",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanProgress {
    pub job_posted: bool,
    pub current_side: u8,
    pub current_bytes: usize,
    pub expected_bytes: usize,
    pub front_bytes: usize,
    pub back_bytes: usize,
    pub front_done: bool,
    pub back_done: bool,
    pub one_sided: bool,
}

impl ScanProgress {
    pub fn fraction(self) -> f32 {
        if self.one_sided {
            if self.front_done {
                return 1.0;
            }
            let side = download_frac(self.current_bytes, self.expected_bytes);
            return if !self.job_posted {
                0.05
            } else {
                0.08 + 0.88 * side
            };
        }
        if self.front_done && self.back_done {
            return 1.0;
        }
        let side = download_frac(self.current_bytes, self.expected_bytes);
        if !self.job_posted {
            0.04
        } else if !self.front_done {
            0.08 + 0.40 * side
        } else {
            0.52 + 0.42 * side
        }
    }
}

fn download_frac(got: usize, expected: usize) -> f32 {
    if expected > 0 {
        (got as f32 / expected as f32).clamp(0.0, 0.96)
    } else {
        (got as f32 / 1_400_000.0).clamp(0.0, 0.92)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub jobs_ok: u32,
    pub jobs_failed: u32,
    pub last_front_bytes: usize,
    pub last_back_bytes: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ScanPreset {
    Web,
    Email,
    #[default]
    Document,
    Photo,
    Fine,
}

impl ScanPreset {
    pub const ALL: &'static [Self] = &[
        Self::Web,
        Self::Email,
        Self::Document,
        Self::Photo,
        Self::Fine,
    ];

    pub fn dpi(self) -> u16 {
        match self {
            Self::Web => 150,
            Self::Email => 200,
            Self::Document => 300,
            Self::Photo => 400,
            Self::Fine => 600,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Web => "Web (150 dpi)",
            Self::Email => "Small (200 dpi)",
            Self::Document => "Document (300 dpi)",
            Self::Photo => "Photo (400 dpi)",
            Self::Fine => "Fine (600 dpi)",
        }
    }
}

impl std::fmt::Display for ScanPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub connection: ConnectionState,
    pub scanner: Option<ScannerInfo>,
    pub status: Option<ScannerStatus>,
    pub session: SessionStats,
    pub progress: ScanProgress,
    pub front_image: Option<Arc<Vec<u8>>>,
    pub back_image: Option<Arc<Vec<u8>>>,
    pub front_text: String,
    pub back_text: String,
    pub last_seen: Option<Instant>,
    pub status_line: String,
    pub last_pdf: Option<String>,
    pub last_pdf_dpi: Option<u16>,
    pub pages_dpi: u16,
    pub scan_preset: ScanPreset,
    pub ocr_enabled: bool,
    pub continuous: bool,
    pub long_receipt: bool,
    pub prompt_continue: bool,
    pub waiting_for_paper: bool,
    pub scan_append: bool,
    pub pages: Vec<Arc<Vec<u8>>>,
    pub page_texts: Vec<String>,
    pub sheets: u32,
    pub transport_pref: TransportPref,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            connection: ConnectionState::AwaitingConnection,
            scanner: None,
            status: None,
            session: SessionStats::default(),
            progress: ScanProgress::default(),
            front_image: None,
            back_image: None,
            front_text: String::new(),
            back_text: String::new(),
            last_seen: None,
            status_line: "Looking for your scanner…".into(),
            last_pdf: None,
            last_pdf_dpi: None,
            pages_dpi: 0,
            scan_preset: ScanPreset::Document,
            ocr_enabled: true,
            continuous: false,
            long_receipt: false,
            prompt_continue: false,
            waiting_for_paper: false,
            scan_append: false,
            pages: Vec::new(),
            page_texts: Vec::new(),
            sheets: 0,
            transport_pref: TransportPref::Auto,
        }
    }
}

impl AppState {
    pub fn mark_disconnected(&mut self, reason: impl Into<String>) {
        self.connection = ConnectionState::AwaitingConnection;
        self.scanner = None;
        self.status = None;
        self.progress = ScanProgress::default();
        self.status_line = reason.into();
    }
}

pub type SharedState = Arc<parking_lot::Mutex<AppState>>;
