use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::event::{self, Event, Status as EventStatus};
use iced::keyboard::{self, Key};
use iced::widget::button::Status as BtnStatus;
use iced::widget::{
    button, checkbox, column, container, horizontal_space, image, pick_list, progress_bar, row,
    scrollable, stack, text,
};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Font, Length, Shadow, Size,
    Subscription, Task, Theme,
};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::escl::EsclClient;
use crate::ocr::OcrEngine;
use crate::state::{AppState, ConnectionState, FeederKind, ScanPreset, SharedState, TransportPref};

const BUILD: &str = env!("SCANBRO_BUILD");
const FONT: Font = Font::with_name("CaskaydiaMono Nerd Font");

// Tokyo Night — matches the usual Omarchy riced look.
const BG: Color = Color::from_rgb(0.102, 0.106, 0.149); // #1a1b26
const SURFACE: Color = Color::from_rgb(0.141, 0.153, 0.227); // #24283b
const SURFACE_2: Color = Color::from_rgb(0.161, 0.173, 0.259); // #292e42
const LINE: Color = Color::from_rgb(0.227, 0.239, 0.322); // #3a3d52
const TEXT: Color = Color::from_rgb(0.753, 0.792, 0.961); // #c0caf5
const MUTED: Color = Color::from_rgb(0.337, 0.373, 0.537); // #565f89
const ACCENT: Color = Color::from_rgb(0.490, 0.812, 1.0); // #7dcfff
const GREEN: Color = Color::from_rgb(0.620, 0.808, 0.416); // #9ece6a
const GREEN_HOVER: Color = Color::from_rgb(0.720, 0.880, 0.520);
const GREEN_PRESS: Color = Color::from_rgb(0.420, 0.620, 0.280);
const AMBER: Color = Color::from_rgb(0.878, 0.686, 0.408); // #e0af68
const RED: Color = Color::from_rgb(0.969, 0.463, 0.557); // #f7768e
const INK: Color = Color::from_rgb(0.063, 0.067, 0.098); // #101019
const WHITE: Color = Color::from_rgb(0.98, 0.99, 1.0);

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    Scan,
    SavePdf,
    EmailPdf,
    SetPreset(ScanPreset),
    ToggleOcr(bool),
    ToggleContinuous(bool),
    ToggleLongReceipt(bool),
    ContinueFeed,
    CancelFeed,
    SetTransport(TransportPref),
    SelectScanner(String),
    FlipOcr,
    FlipContinuous,
    FlipLongReceipt,
    KeyDown(String),
    KeyUp(String),
    PdfDone(Result<PathBuf, String>),
    EmailDone(Result<(), String>),
}

enum BackendCmd {
    Scan,
}

pub struct Dashboard {
    shared: SharedState,
    cmd_tx: mpsc::UnboundedSender<BackendCmd>,
    page_handles: Vec<image::Handle>,
    page_lens: Vec<usize>,
    live_front: Option<image::Handle>,
    live_back: Option<image::Handle>,
    live_front_len: usize,
    live_back_len: usize,
    saving: bool,
    email_after: bool,
    pdf_dpi: u16,
    spin_phase: u8,
    pressed: HashSet<String>,
}

pub fn run() -> iced::Result {
    iced::application(
        concat!("Scanbro · ", env!("SCANBRO_BUILD")),
        Dashboard::update,
        Dashboard::view,
    )
        .subscription(Dashboard::subscription)
        .theme(Dashboard::theme)
        .default_font(FONT)
        .window_size(Size::new(1280.0, 820.0))
        .centered()
        .run_with(Dashboard::new)
}

impl Dashboard {
    fn new() -> (Self, Task<Message>) {
        let shared: SharedState = Arc::new(parking_lot::Mutex::new(AppState::default()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        spawn_backend(Arc::clone(&shared), cmd_rx);

        (
            Self {
                shared,
                cmd_tx,
                page_handles: Vec::new(),
                page_lens: Vec::new(),
                live_front: None,
                live_back: None,
                live_front_len: 0,
                live_back_len: 0,
                saving: false,
                email_after: false,
                pdf_dpi: 300,
                spin_phase: 0,
                pressed: HashSet::new(),
            },
            Task::none(),
        )
    }

    fn theme(&self) -> Theme {
        Theme::custom(
            "omarchy".into(),
            iced::theme::Palette {
                background: BG,
                text: TEXT,
                primary: GREEN,
                success: GREEN,
                danger: RED,
            },
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(160)).map(|_| Message::Tick),
            event::listen_with(|event, status, _| {
                if status == EventStatus::Captured {
                    return None;
                }
                match event {
                    Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                        if modifiers.control() || modifiers.alt() || modifiers.logo() {
                            return None;
                        }
                        Some(Message::KeyDown(key_id(&key)))
                    }
                    Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) => {
                        Some(Message::KeyUp(key_id(&key)))
                    }
                    _ => None,
                }
            }),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                self.spin_phase = self.spin_phase.wrapping_add(1);
                self.sync_previews();
                let kick = {
                    let mut guard = self.shared.lock();
                    let scanning = matches!(
                        guard.connection,
                        ConnectionState::Scanning | ConnectionState::RunningOcr
                    );
                    let ready = guard.status.as_ref().is_some_and(|s| s.paper_ready());
                    if guard.waiting_for_paper
                        && !scanning
                        && ready
                        && matches!(guard.connection, ConnectionState::Connected)
                    {
                        guard.waiting_for_paper = false;
                        guard.scan_append = true;
                        guard.status_line = "Paper detected. Scanning…".into();
                        true
                    } else {
                        false
                    }
                };
                if kick {
                    let _ = self.cmd_tx.send(BackendCmd::Scan);
                }
                Task::none()
            }
            Message::Scan => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().scan_append = false;
                let _ = self.cmd_tx.send(BackendCmd::Scan);
                Task::none()
            }
            Message::SavePdf => {
                self.email_after = false;
                self.save_pdf(true)
            }
            Message::EmailPdf => self.email_pdf(),
            Message::SetPreset(preset) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().scan_preset = preset;
                Task::none()
            }
            Message::ToggleOcr(enabled) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().ocr_enabled = enabled;
                Task::none()
            }
            Message::ToggleContinuous(enabled) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().continuous = enabled;
                Task::none()
            }
            Message::ToggleLongReceipt(enabled) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().long_receipt = enabled;
                Task::none()
            }
            Message::FlipOcr => {
                if self.is_busy() {
                    return Task::none();
                }
                let mut guard = self.shared.lock();
                guard.ocr_enabled = !guard.ocr_enabled;
                Task::none()
            }
            Message::FlipContinuous => {
                if self.is_busy() {
                    return Task::none();
                }
                let mut guard = self.shared.lock();
                guard.continuous = !guard.continuous;
                Task::none()
            }
            Message::FlipLongReceipt => {
                if self.is_busy() {
                    return Task::none();
                }
                let mut guard = self.shared.lock();
                guard.long_receipt = !guard.long_receipt;
                Task::none()
            }
            Message::SelectScanner(id) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().select_scanner(&id);
                Task::none()
            }
            Message::KeyDown(key) => {
                if !self.pressed.insert(key.clone()) {
                    return Task::none();
                }
                self.on_key(&key)
            }
            Message::KeyUp(key) => {
                self.pressed.remove(&key);
                Task::none()
            }
            Message::ContinueFeed => {
                let scan_now = {
                    let mut guard = self.shared.lock();
                    if !guard.prompt_continue {
                        return Task::none();
                    }
                    guard.prompt_continue = false;
                    guard.scan_append = true;
                    guard.waiting_for_paper = true;
                    guard.status_line = "Load the next page…".into();
                    guard.status.as_ref().is_some_and(|s| s.paper_ready())
                };
                if scan_now {
                    let mut guard = self.shared.lock();
                    guard.waiting_for_paper = false;
                    drop(guard);
                    let _ = self.cmd_tx.send(BackendCmd::Scan);
                }
                Task::none()
            }
            Message::CancelFeed => {
                let mut guard = self.shared.lock();
                guard.prompt_continue = false;
                guard.waiting_for_paper = false;
                guard.scan_append = false;
                let n = guard.pages.len();
                guard.status_line = if n == 0 {
                    "Scan cancelled.".into()
                } else {
                    format!("Document ready ({n} pages).")
                };
                Task::none()
            }
            Message::SetTransport(pref) => {
                if self.is_busy() {
                    return Task::none();
                }
                self.shared.lock().apply_transport(pref);
                Task::none()
            }
            Message::PdfDone(result) => {
                self.saving = false;
                let email_after = self.email_after;
                self.email_after = false;
                match result {
                    Ok(path) => {
                        {
                            let mut guard = self.shared.lock();
                            let shown = path.display().to_string();
                            if !email_after {
                                guard.last_pdf = Some(shown.clone());
                                guard.last_pdf_dpi = Some(self.pdf_dpi);
                            }
                            guard.status_line = if email_after {
                                format!(
                                    "Opening mail with a {:.1} MB PDF…",
                                    std::fs::metadata(&path)
                                        .map(|m| m.len() as f64 / 1_048_576.0)
                                        .unwrap_or(0.0)
                                )
                            } else {
                                let size = std::fs::metadata(&path)
                                    .map(|m| format!(" · {}", format_bytes(m.len())))
                                    .unwrap_or_default();
                                format!("saved {shown}{size}")
                            };
                            guard.session.last_error = None;
                        }
                        if email_after {
                            return Task::perform(attach_email(path), Message::EmailDone);
                        }
                    }
                    Err(err) => {
                        self.shared.lock().session.last_error = Some(err);
                    }
                }
                Task::none()
            }
            Message::EmailDone(result) => {
                let mut guard = self.shared.lock();
                match result {
                    Ok(()) => {
                        guard.status_line = "Mail composer opened.".into();
                        guard.session.last_error = None;
                    }
                    Err(err) => {
                        guard.session.last_error = Some(err);
                    }
                }
                Task::none()
            }
        }
    }

    fn on_key(&mut self, key: &str) -> Task<Message> {
        let prompt = self.shared.lock().prompt_continue;
        let waiting = self.shared.lock().waiting_for_paper;
        match key {
            "s" => self.update(Message::Scan),
            "p" => self.update(Message::SavePdf),
            "e" => self.update(Message::EmailPdf),
            "o" => self.update(Message::FlipOcr),
            "c" => self.update(Message::FlipContinuous),
            "l" => self.update(Message::FlipLongReceipt),
            "a" => self.update(Message::SetTransport(TransportPref::Auto)),
            "w" => self.update(Message::SetTransport(TransportPref::Wifi)),
            "u" => self.update(Message::SetTransport(TransportPref::Usb)),
            "1" => self.update(Message::SetPreset(ScanPreset::Web)),
            "2" => self.update(Message::SetPreset(ScanPreset::Email)),
            "3" => self.update(Message::SetPreset(ScanPreset::Document)),
            "4" => self.update(Message::SetPreset(ScanPreset::Photo)),
            "5" => self.update(Message::SetPreset(ScanPreset::Fine)),
            "[" => {
                if !self.is_busy() {
                    self.shared.lock().cycle_scanner(-1);
                }
                Task::none()
            }
            "]" => {
                if !self.is_busy() {
                    self.shared.lock().cycle_scanner(1);
                }
                Task::none()
            }
            "enter" if prompt => self.update(Message::ContinueFeed),
            "enter" if !waiting => self.update(Message::Scan),
            "escape" => self.update(Message::CancelFeed),
            _ => Task::none(),
        }
    }

    fn is_busy(&self) -> bool {
        self.saving || {
            let guard = self.shared.lock();
            matches!(
                guard.connection,
                ConnectionState::Scanning | ConnectionState::RunningOcr
            ) || guard.prompt_continue
                || guard.waiting_for_paper
        }
    }

    fn collect_pages(&self) -> (Vec<Arc<Vec<u8>>>, u16) {
        let guard = self.shared.lock();
        let pages = if !guard.pages.is_empty() {
            guard.pages.clone()
        } else {
            let mut pages = Vec::new();
            if let Some(img) = guard.front_image.clone().filter(|b| !b.is_empty()) {
                pages.push(img);
            }
            if let Some(img) = guard.back_image.clone().filter(|b| !b.is_empty()) {
                pages.push(img);
            }
            pages
        };
        (pages, if guard.pages_dpi > 0 {
            guard.pages_dpi
        } else {
            guard.scan_preset.dpi()
        })
    }

    fn save_pdf(&mut self, pick_path: bool) -> Task<Message> {
        if self.saving {
            return Task::none();
        }
        let (pages, dpi) = self.collect_pages();
        if pages.is_empty() {
            self.shared.lock().session.last_error = Some("Scan a page before saving a PDF.".into());
            return Task::none();
        }
        self.saving = true;
        self.pdf_dpi = dpi;
        self.shared.lock().status_line = "Saving PDF…".into();
        if pick_path {
            Task::perform(write_pdf_dialog(pages, dpi), Message::PdfDone)
        } else {
            Task::perform(write_pdf_default(pages, dpi), Message::PdfDone)
        }
    }

    fn email_pdf(&mut self) -> Task<Message> {
        if self.saving {
            return Task::none();
        }
        let (pages, dpi) = self.collect_pages();
        if pages.is_empty() {
            self.shared.lock().session.last_error =
                Some("Scan a page before emailing a PDF.".into());
            return Task::none();
        }
        self.saving = true;
        self.email_after = true;
        self.pdf_dpi = dpi;
        self.shared.lock().status_line = "Preparing a PDF under 5 MB…".into();
        Task::perform(write_email_pdf_default(pages, dpi), Message::PdfDone)
    }

    fn sync_previews(&mut self) {
        let guard = self.shared.lock();
        let lens: Vec<usize> = guard.pages.iter().map(|p| p.len()).collect();
        if lens != self.page_lens {
            let same_prefix = self.page_lens.len() <= lens.len()
                && self
                    .page_lens
                    .iter()
                    .zip(lens.iter())
                    .all(|(a, b)| a == b);
            if !same_prefix {
                self.page_handles.clear();
            }
            for bytes in guard.pages.iter().skip(self.page_handles.len()) {
                if !bytes.is_empty() {
                    self.page_handles.push(jpeg_handle(bytes));
                }
            }
            self.page_lens = lens;
        }
        if let Some(bytes) = &guard.front_image {
            if !bytes.is_empty() && bytes.len() != self.live_front_len {
                self.live_front_len = bytes.len();
                self.live_front = Some(jpeg_handle(bytes));
            }
        } else {
            self.live_front = None;
            self.live_front_len = 0;
        }
        if let Some(bytes) = &guard.back_image {
            if !bytes.is_empty() && bytes.len() != self.live_back_len {
                self.live_back_len = bytes.len();
                self.live_back = Some(jpeg_handle(bytes));
            }
        } else {
            self.live_back = None;
            self.live_back_len = 0;
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let snap = self.shared.lock().clone();
        let capturing = snap.connection == ConnectionState::Scanning;
        let scanning = capturing
            || matches!(snap.connection, ConnectionState::RunningOcr);
        let busy = scanning || self.saving || snap.prompt_continue || snap.waiting_for_paper;
        let can_scan = snap.connection == ConnectionState::Connected && !busy;
        let can_save = !snap.pages.is_empty() && !self.saving && !scanning;

        let header = header_bar(&snap);
        let controls = control_column(&snap, can_scan, can_save, busy);
        let stage = scan_stage(
            &self.page_handles,
            self.live_front.clone(),
            self.live_back.clone(),
            capturing,
            snap.pages.len(),
            snap.sheets,
            snap.progress.fraction(),
            self.spin_phase,
            snap.long_receipt,
        );

        let main = container(
            column![header, row![controls, stage].spacing(6).height(Length::Fill)]
                .spacing(6)
                .height(Length::Fill),
        )
        .padding(8)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(BG)),
            text_color: Some(TEXT),
            border: Border::default(),
            shadow: Shadow::default(),
        });

        if snap.prompt_continue {
            stack![main, continue_overlay(snap.sheets, snap.pages.len())].into()
        } else if snap.waiting_for_paper {
            stack![main, waiting_overlay(snap.sheets, snap.pages.len())].into()
        } else {
            main.into()
        }
    }
}

fn header_bar(snap: &AppState) -> Element<'static, Message> {
    let (dot, label) = match snap.connection {
        ConnectionState::Connected
        | ConnectionState::Scanning
        | ConnectionState::RunningOcr => (GREEN, "CONNECTED"),
        ConnectionState::Degraded => (RED, "DEGRADED"),
        ConnectionState::AwaitingConnection => (AMBER, "LOOKING"),
    };

    let via = snap.scanner.as_ref().map(|s| s.via.label()).unwrap_or("—");
    let prompt = snap
        .scanner
        .as_ref()
        .map(|s| match s.via {
            crate::state::Transport::Usb => format!("▸  {}  ·  USB", s.name),
            crate::state::Transport::Wifi => format!("▸  {}  ·  Wi-Fi  {}", s.name, s.ip),
        })
        .unwrap_or_else(|| "▸  no scanner".into());

    let capturing = snap.connection == ConnectionState::Scanning;
    let feeder_kind = if capturing {
        FeederKind::Pulling
    } else {
        snap.status
            .as_ref()
            .map(crate::state::ScannerStatus::feeder_kind)
            .unwrap_or(FeederKind::Empty)
    };
    let feeder_color = match feeder_kind {
        FeederKind::Loaded => GREEN,
        FeederKind::Pulling => AMBER,
        FeederKind::Jam | FeederKind::Mispick => RED,
        FeederKind::Empty | FeederKind::Unknown => MUTED,
    };
    let fault = snap.status.as_ref().and_then(|s| s.fault_message());
    let battery = snap.status.as_ref().and_then(|s| s.battery_percent);

    let recipe = {
        let dpi = snap.scan_preset.dpi();
        if snap.long_receipt {
            format!("RECEIPT  1-SIDE  {dpi}DPI")
        } else {
            format!("DUPLEX  COLOR  {dpi}DPI")
        }
    };

    let mut chips = row![
        chip(format!("{label}  {via}"), Some(dot)),
        chip(feeder_kind.label().to_ascii_uppercase(), Some(feeder_color)),
        chip(recipe, None),
    ]
    .spacing(6);
    let pending = if snap.long_receipt { 1 } else { 2 };
    if snap.pages.len() + usize::from(capturing) * pending > 0 {
        let sheets = if capturing { snap.sheets + 1 } else { snap.sheets };
        let pages = if capturing {
            snap.pages.len() + pending
        } else {
            snap.pages.len()
        };
        if sheets > 0 || pages > 0 {
            let size = format_bytes(scan_jpeg_bytes(snap) as u64);
            chips = chips.push(chip(
                if size == "—" {
                    format!("SHEET {sheets}  {pages}P")
                } else {
                    format!("SHEET {sheets}  {pages}P  {size}")
                },
                None,
            ));
        }
    }
    if capturing {
        if snap.long_receipt {
            chips = chips.push(chip("RECEIPT", Some(AMBER)));
        } else {
            let front_color = if snap.progress.front_done { GREEN } else { AMBER };
            let back_color = if snap.progress.back_done {
                GREEN
            } else if snap.progress.front_done || snap.progress.current_side >= 2 {
                AMBER
            } else {
                MUTED
            };
            chips = chips.push(chip("FRONT", Some(front_color)));
            chips = chips.push(chip("BACK", Some(back_color)));
        }
    }
    if let Some(percent) = battery {
        chips = chips.push(chip(format!("BAT {percent}%"), None));
    }

    let mut info = column![
        row![
            text("SCANBRO").size(14).color(ACCENT),
            horizontal_space(),
            text("[S] scan  [P] pdf  [E] email  [1-5] quality  [ ] scanner").size(12).color(MUTED),
        ]
        .align_y(Alignment::Center),
        text(prompt).size(20).color(TEXT),
        text(snap.status_line.clone()).size(13).color(MUTED),
    ]
    .spacing(6);
    if let Some(fault) = fault {
        info = info.push(text(fault).size(14).color(RED));
    }
    info = info.push(chips);

    container(info)
        .padding(12)
        .width(Length::Fill)
        .style(|_| card())
        .into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannerChoice {
    key: String,
    line: String,
}

impl std::fmt::Display for ScannerChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.line)
    }
}

fn scanner_pick_list(snap: &AppState) -> Element<'static, Message> {
    let options: Vec<ScannerChoice> = snap
        .visible_scanners()
        .into_iter()
        .map(|s| ScannerChoice {
            key: s.device_key(),
            line: s.picker_line(),
        })
        .collect();
    let selected = snap.scanner.as_ref().and_then(|cur| {
        options
            .iter()
            .find(|choice| choice.key == cur.device_key())
            .cloned()
    });
    pick_list(options, selected, |choice| Message::SelectScanner(choice.key))
        .placeholder("Looking for a scanner…")
        .text_size(14)
        .padding([8, 10])
        .width(Length::Fill)
        .style(pick_style)
        .into()
}

fn pick_style(_theme: &Theme, status: iced::widget::pick_list::Status) -> iced::widget::pick_list::Style {
    use iced::widget::pick_list::Status;
    let border_c = match status {
        Status::Hovered | Status::Opened => ACCENT,
        Status::Active => LINE,
    };
    iced::widget::pick_list::Style {
        text_color: TEXT,
        placeholder_color: MUTED,
        handle_color: MUTED,
        background: Background::Color(SURFACE_2),
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: border_c,
        },
    }
}

fn chip(label: impl Into<String>, dot: Option<Color>) -> Element<'static, Message> {
    let mut inner = row![].spacing(8).align_y(Alignment::Center);
    if let Some(color) = dot {
        inner = inner.push(
            container(horizontal_space())
                .width(7)
                .height(7)
                .style(move |_| container::Style {
                    background: Some(Background::Color(color)),
                    border: Border {
                        radius: 0.0.into(),
                        ..Border::default()
                    },
                    text_color: None,
                    shadow: Shadow::default(),
                }),
        );
    }
    inner = inner.push(text(format!("[ {} ]", label.into())).size(12));
    container(inner)
        .padding([4, 8])
        .style(|_| module())
        .into()
}

fn control_column(
    snap: &AppState,
    can_scan: bool,
    can_save: bool,
    busy: bool,
) -> Element<'static, Message> {
    let scan_label = if snap.connection == ConnectionState::Scanning {
        "Scanning…"
    } else {
        "Scan"
    };

    let scan_bytes = scan_jpeg_bytes(snap);
    let size_label = format_bytes(scan_bytes as u64);

    let scan_btn = action_button(scan_label, "S", 22.0, can_scan.then_some(Message::Scan), true);
    let save_label = if busy && snap.connection != ConnectionState::Scanning {
        "Working…".into()
    } else if can_save {
        format!("Save PDF  {size_label}")
    } else {
        "Save PDF".into()
    };
    let save_btn = action_button(
        save_label,
        "P",
        16.0,
        can_save.then_some(Message::SavePdf),
        false,
    );
    let email_label = if !can_save {
        "Email PDF".into()
    } else if scan_bytes <= crate::pdf::EMAIL_MAX_BYTES {
        format!("Email PDF  {size_label}")
    } else {
        "Email PDF  under 5 MB".into()
    };
    let email_btn = action_button(
        email_label,
        "E",
        16.0,
        can_save.then_some(Message::EmailPdf),
        false,
    );
    let email_hint = if !can_save {
        None
    } else if scan_bytes <= crate::pdf::EMAIL_MAX_BYTES {
        Some(format!("{size_label} — fits in email as-is."))
    } else {
        Some(format!(
            "{size_label} full scan. Email copy will be under 5 MB."
        ))
    };

    let quality = pick_list(ScanPreset::ALL, Some(snap.scan_preset), Message::SetPreset)
        .text_size(14)
        .padding([8, 10])
        .width(Length::Fill)
        .style(pick_style);
    let scanner = scanner_pick_list(snap);

    let transport = row![
        seg_button("AUTO  [A]", snap.transport_pref == TransportPref::Auto, Message::SetTransport(TransportPref::Auto), !busy),
        seg_button("WI-FI  [W]", snap.transport_pref == TransportPref::Wifi, Message::SetTransport(TransportPref::Wifi), !busy),
        seg_button("USB  [U]", snap.transport_pref == TransportPref::Usb, Message::SetTransport(TransportPref::Usb), !busy),
    ]
    .spacing(0);

    let mut ocr = checkbox("Extract text (OCR)  [O]", snap.ocr_enabled)
        .text_size(14)
        .style(|_, status| checkbox_style(status));
    if !busy {
        ocr = ocr.on_toggle(Message::ToggleOcr);
    }

    let mut continuous = checkbox("Continuous feed  [C]", snap.continuous)
        .text_size(14)
        .style(|_, status| checkbox_style(status));
    if !busy {
        continuous = continuous.on_toggle(Message::ToggleContinuous);
    }

    let mut long_receipt = checkbox("Long receipt  [L]", snap.long_receipt)
        .text_size(14)
        .style(|_, status| checkbox_style(status));
    if !busy {
        long_receipt = long_receipt.on_toggle(Message::ToggleLongReceipt);
    }

    let err = snap.session.last_error.clone().unwrap_or_default();
    let last_pdf = match snap.last_pdf.as_deref() {
        Some(path) => {
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path);
            match std::fs::metadata(path) {
                Ok(meta) => format!("{} · {}", name, format_bytes(meta.len())),
                Err(_) => name.to_string(),
            }
        }
        None => "—".into(),
    };

    let mut controls = column![scan_btn, save_btn, email_btn].spacing(8);
    if let Some(hint) = email_hint {
        controls = controls.push(text(hint).size(12).color(MUTED));
    }
    controls = controls.push(label("SCANNER  [ ]"));
    controls = controls.push(scanner);
    controls = controls.push(label("QUALITY  [1-5]"));
    controls = controls.push(quality);
    controls = controls.push(label("CONNECTION"));
    controls = controls.push(transport);
    controls = controls.push(ocr);
    controls = controls.push(continuous);
    controls = controls.push(long_receipt);
    if can_save {
        controls = controls.push(kv("SCAN SIZE", size_label));
    }
    controls = controls.push(kv("LAST PDF", last_pdf));
    controls = controls.push(scrollable(text(err).size(13).color(RED)).height(Length::Fill));
    controls = controls.push(text(format!("build: {BUILD}")).size(10).color(MUTED));

    container(controls)
    .padding(12)
    .width(300)
    .height(Length::Fill)
    .style(|_| card())
    .into()
}

fn scan_stage(
    pages: &[image::Handle],
    live_front: Option<image::Handle>,
    live_back: Option<image::Handle>,
    capturing: bool,
    page_count: usize,
    sheets: u32,
    progress: f32,
    spin: u8,
    long_receipt: bool,
) -> Element<'static, Message> {
    let mut column_pages = column![].spacing(12);
    let mut page_no = 0usize;

    for handle in pages {
        page_no += 1;
        column_pages = column_pages.push(page_caption(page_no));
        column_pages = column_pages.push(page_image(handle.clone()));
    }

    if capturing {
        if let Some(handle) = live_front {
            page_no += 1;
            column_pages = column_pages.push(page_caption(page_no));
            column_pages = column_pages.push(page_image(handle));
        } else {
            page_no += 1;
            column_pages = column_pages.push(page_caption(page_no));
            column_pages = column_pages.push(page_spinner(spin));
        }
        if !long_receipt {
            if let Some(handle) = live_back {
                page_no += 1;
                column_pages = column_pages.push(page_caption(page_no));
                column_pages = column_pages.push(page_image(handle));
            } else {
                page_no += 1;
                column_pages = column_pages.push(page_caption(page_no));
                column_pages = column_pages.push(page_spinner(spin.wrapping_add(1)));
            }
        }
    }

    let any = page_no > 0;
    let body: Element<'static, Message> = if any {
        scrollable(column_pages)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        container(text("// no scan yet").size(16).color(MUTED))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    };

    let shown_pages = page_no.max(page_count);
    let shown_sheets = if capturing && sheets == 0 {
        1
    } else if capturing {
        sheets + 1
    } else {
        sheets
    };
    let counter = if shown_pages == 0 {
        text(String::new()).size(14).color(MUTED)
    } else {
        text(format!(
            "Page 1 of {shown_pages}  ·  Sheet {shown_sheets} · {shown_pages} pages"
        ))
        .size(14)
        .color(MUTED)
    };

    let mut stage = column![
        row![text("PREVIEW").size(14).color(ACCENT), horizontal_space(), counter]
            .align_y(Alignment::Center)
    ]
    .spacing(8)
    .height(Length::Fill);
    if capturing {
        stage = stage.push(
            progress_bar(0.0..=1.0, progress)
                .height(4.0)
                .style(|_theme| progress_bar::Style {
                    background: SURFACE_2.into(),
                    bar: ACCENT.into(),
                    border: Border {
                        radius: 0.0.into(),
                        ..Border::default()
                    },
                }),
        );
    }
    stage = stage.push(body);

    container(stage)
        .padding(10)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| card())
        .into()
}

fn jpeg_handle(bytes: &[u8]) -> image::Handle {
    match decode_preview(bytes) {
        Some((width, height, pixels)) => image::Handle::from_rgba(width, height, pixels),
        None => image::Handle::from_bytes(bytes.to_vec()),
    }
}

fn decode_preview(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let src = if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        bytes
    } else {
        bytes
            .windows(2)
            .position(|w| w == [0xFF, 0xD8])
            .map(|i| &bytes[i..])
            .unwrap_or(bytes)
    };
    let img = ::image::load_from_memory(src).ok()?;
    // Wells are 560px; 2× is enough to look sharp. 1800px RGBA is a long CPU hitch.
    let img = img.thumbnail(960, 960);
    let rgba = img.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    if width < 8 || height < 8 {
        return None;
    }
    Some((width, height, rgba.into_raw()))
}

fn page_caption(page_no: usize) -> Element<'static, Message> {
    text(format!("Page {page_no}")).size(13).color(MUTED).into()
}

fn page_spinner(spin: u8) -> Element<'static, Message> {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let frame = FRAMES[(spin as usize) % FRAMES.len()];
    container(
        column![
            text(frame).size(42).color(ACCENT),
            text("Scanning…").size(14).color(MUTED),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(560.0)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .padding(8)
    .style(|_| container::Style {
        background: Some(Background::Color(INK)),
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: LINE,
        },
        text_color: Some(TEXT),
        shadow: Shadow::default(),
    })
    .into()
}

fn continue_overlay(sheets: u32, page_count: usize) -> Element<'static, Message> {
    feed_overlay(
        "Next sheet?",
        format!("Sheet {sheets} · {page_count} pages in this document. Load the next sheet, then Continue. Cancel finishes the document."),
        true,
    )
}

fn waiting_overlay(sheets: u32, page_count: usize) -> Element<'static, Message> {
    feed_overlay(
        "Waiting for paper…",
        format!("Sheet {sheets} · {page_count} pages so far. Load the next sheet — scanning starts automatically. Cancel finishes the document."),
        false,
    )
}

fn feed_overlay(
    title: &'static str,
    body: String,
    show_continue: bool,
) -> Element<'static, Message> {
    let mut actions = row![].spacing(12);
    if show_continue {
        actions = actions.push(action_button(
            "Continue",
            "↵",
            16.0,
            Some(Message::ContinueFeed),
            true,
        ));
    }
    actions = actions.push(action_button(
        "Cancel",
        "ESC",
        16.0,
        Some(Message::CancelFeed),
        false,
    ));

    let card_body = container(
        column![
            text(title).size(22).color(TEXT),
            text(body).size(15).color(MUTED),
            actions,
        ]
        .spacing(16),
    )
    .padding(28)
    .width(460)
    .style(|_| card());

    container(card_body)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.55))),
            text_color: Some(TEXT),
            border: Border::default(),
            shadow: Shadow::default(),
        })
        .into()
}

fn page_image(handle: image::Handle) -> Element<'static, Message> {
    container(
        image(handle)
            .width(Length::Fill)
            .height(Length::Fill)
            .content_fit(ContentFit::Contain),
    )
    .width(Length::Fill)
    .height(560.0)
    .padding(8)
    .style(|_| container::Style {
        background: Some(Background::Color(INK)),
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: LINE,
        },
        text_color: None,
        shadow: Shadow::default(),
    })
    .into()
}

fn scan_jpeg_bytes(snap: &AppState) -> usize {
    if !snap.pages.is_empty() {
        return snap.pages.iter().map(|p| p.len()).sum();
    }
    snap.front_image.as_ref().map(|p| p.len()).unwrap_or(0)
        + snap.back_image.as_ref().map(|p| p.len()).unwrap_or(0)
}

fn format_bytes(n: u64) -> String {
    const MB: f64 = 1_048_576.0;
    const KB: f64 = 1024.0;
    if n >= 1_048_576 {
        format!("{:.1} MB", n as f64 / MB)
    } else if n >= 1024 {
        format!("{:.0} KB", n as f64 / KB)
    } else if n == 0 {
        "—".into()
    } else {
        format!("{n} B")
    }
}

fn action_button(
    label: impl Into<String>,
    key: &'static str,
    size: f32,
    on_press: Option<Message>,
    scan: bool,
) -> Element<'static, Message> {
    button(
        row![
            text(label.into()).size(size),
            horizontal_space(),
            text(format!("[{key}]")).size(12).color(MUTED),
        ]
        .align_y(Alignment::Center),
    )
    .on_press_maybe(on_press)
    .width(Length::Fill)
    .padding([14, 12])
    .style(move |_, status| {
        if scan {
            scan_style(status)
        } else {
            secondary_style(status)
        }
    })
    .into()
}

fn scan_style(status: BtnStatus) -> button::Style {
    let bg = match status {
        BtnStatus::Hovered => GREEN_HOVER,
        BtnStatus::Pressed => GREEN_PRESS,
        BtnStatus::Disabled => Color::from_rgb(0.18, 0.22, 0.20),
        BtnStatus::Active => GREEN,
    };
    let text_color = match status {
        BtnStatus::Disabled => MUTED,
        _ => INK,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color,
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: match status {
                BtnStatus::Disabled => LINE,
                BtnStatus::Hovered => WHITE,
                _ => GREEN_HOVER,
            },
        },
        shadow: Shadow::default(),
    }
}

fn secondary_style(status: BtnStatus) -> button::Style {
    let (bg, border_c) = match status {
        BtnStatus::Hovered => (SURFACE_2, ACCENT),
        BtnStatus::Pressed => (BG, ACCENT),
        BtnStatus::Disabled => (BG, LINE),
        BtnStatus::Active => (SURFACE_2, LINE),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: if matches!(status, BtnStatus::Disabled) {
            MUTED
        } else {
            TEXT
        },
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: border_c,
        },
        shadow: Shadow::default(),
    }
}

fn seg_button(
    label: &'static str,
    selected: bool,
    msg: Message,
    enabled: bool,
) -> Element<'static, Message> {
    button(text(label).size(12))
        .on_press_maybe(enabled.then_some(msg))
        .padding([10, 8])
        .width(Length::Fill)
        .style(move |_, status| {
            let hovered = enabled && matches!(status, BtnStatus::Hovered | BtnStatus::Pressed);
            let bg = if selected {
                if hovered {
                    GREEN_HOVER
                } else {
                    GREEN
                }
            } else if hovered {
                SURFACE_2
            } else {
                SURFACE
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: if !enabled {
                    MUTED
                } else if selected {
                    INK
                } else {
                    TEXT
                },
                border: Border {
                    radius: 0.0.into(),
                    width: 1.0,
                    color: if selected && enabled { GREEN } else { LINE },
                },
                shadow: Shadow::default(),
            }
        })
        .into()
}

fn checkbox_style(status: iced::widget::checkbox::Status) -> checkbox::Style {
    let (checked, hovered) = match status {
        iced::widget::checkbox::Status::Active { is_checked } => (is_checked, false),
        iced::widget::checkbox::Status::Hovered { is_checked } => (is_checked, true),
        iced::widget::checkbox::Status::Disabled { is_checked } => (is_checked, false),
    };
    checkbox::Style {
        background: Background::Color(if checked {
            GREEN
        } else if hovered {
            SURFACE_2
        } else {
            SURFACE
        }),
        icon_color: INK,
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: if checked || hovered { GREEN } else { LINE },
        },
        text_color: Some(TEXT),
    }
}

fn kv(key: &'static str, value: impl Into<String>) -> Element<'static, Message> {
    column![
        text(key).size(11).color(MUTED),
        text(value.into()).size(14).color(TEXT),
    ]
    .spacing(2)
    .into()
}

fn label(s: &'static str) -> Element<'static, Message> {
    text(s).size(11).color(MUTED).into()
}

fn card() -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        text_color: Some(TEXT),
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: LINE,
        },
        shadow: Shadow::default(),
    }
}

fn module() -> container::Style {
    container::Style {
        background: Some(Background::Color(BG)),
        border: Border {
            radius: 0.0.into(),
            width: 1.0,
            color: LINE,
        },
        text_color: Some(TEXT),
        shadow: Shadow::default(),
    }
}

fn key_id(key: &Key) -> String {
    match key {
        Key::Named(keyboard::key::Named::Enter) => "enter".into(),
        Key::Named(keyboard::key::Named::Escape) => "escape".into(),
        Key::Character(c) => c.to_lowercase(),
        _ => String::new(),
    }
}

fn spawn_backend(state: SharedState, mut cmd_rx: mpsc::UnboundedReceiver<BackendCmd>) {
    std::thread::Builder::new()
        .name("scanbro-backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("scanbro-worker")
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let _mdns = match crate::discovery::spawn(Arc::clone(&state)) {
                    Ok(daemon) => Some(daemon),
                    Err(err) => {
                        error!("network search failed: {err}");
                        state.lock().status_line = "Couldn't search the network for a scanner.".into();
                        None
                    }
                };
                match EsclClient::new() {
                    Ok(escl) => {
                        escl.spawn_poller(Arc::clone(&state));
                        crate::discovery::spawn_lan_probe(Arc::clone(&state));
                        crate::usb::spawn(Arc::clone(&state));
                        let ocr = OcrEngine::new();
                        while let Some(cmd) = cmd_rx.recv().await {
                            match cmd {
                                BackendCmd::Scan => start_scan(Arc::clone(&state), escl.clone(), ocr),
                            }
                        }
                    }
                    Err(err) => {
                        error!("HTTP client failed: {err}");
                        state.lock().status_line = "Couldn't start a connection to the scanner.".into();
                    }
                }
            });
        })
        .expect("backend thread");
}

fn start_scan(state: SharedState, escl: EsclClient, ocr: OcrEngine) {
    let (scanner, ocr_on, long_receipt, dpi) = {
        let mut guard = state.lock();
        if !matches!(guard.connection, ConnectionState::Connected) {
            guard.session.last_error = Some("Connect the scanner first.".into());
            return;
        }
        if guard
            .status
            .as_ref()
            .is_some_and(|s| s.feeder_kind() == crate::state::FeederKind::Empty)
        {
            guard.session.last_error = Some("Load a page into the feeder, then scan.".into());
            return;
        }
        let append = guard.scan_append;
        let long_receipt = guard.long_receipt;
        guard.scan_append = false;
        guard.connection = ConnectionState::Scanning;
        guard.status_line = match (long_receipt, append) {
            (true, true) => "Scanning next receipt…".into(),
            (true, false) => "Starting receipt scan…".into(),
            (false, true) => "Scanning next page…".into(),
            (false, false) => "Starting scan…".into(),
        };
        guard.session.last_error = None;
        guard.progress = crate::state::ScanProgress::default();
        guard.front_image = None;
        guard.back_image = None;
        guard.front_text.clear();
        guard.back_text.clear();
        if !append {
            guard.pages.clear();
            guard.page_texts.clear();
            guard.sheets = 0;
            guard.pages_dpi = 0;
        }
        (guard.scanner.clone(), guard.ocr_enabled, guard.long_receipt, guard.scan_preset.dpi())
    };
    let Some(scanner) = scanner else {
        state.lock().mark_disconnected("Scan cancelled. No scanner connected.");
        return;
    };

    tokio::spawn(async move {
        let result = async {
            let (front, back) = if let Some(device) = scanner.sane_device.as_deref().filter(|d| {
                let d = d.to_ascii_lowercase();
                d.starts_with("brother") && !d.starts_with("escl:") && !d.contains("://")
            }) {
                crate::usb::scan_duplex(device, &state, long_receipt, dpi).await?
            } else {
                escl.scan_duplex(&scanner, &state, long_receipt, dpi).await?
            };
            {
                let mut guard = state.lock();
                guard.session.last_front_bytes = front.len();
                guard.session.last_back_bytes = back.len();
                if !front.is_empty() {
                    guard.pages.push(Arc::clone(&front));
                    guard.page_texts.push(String::new());
                    guard.front_image = Some(Arc::clone(&front));
                }
                if !back.is_empty() {
                    guard.pages.push(Arc::clone(&back));
                    guard.page_texts.push(String::new());
                    guard.back_image = Some(Arc::clone(&back));
                }
                guard.sheets += 1;
                guard.pages_dpi = if guard.pages_dpi == 0 {
                    dpi
                } else {
                    guard.pages_dpi.max(dpi)
                };
                guard.progress.front_done = true;
                guard.progress.back_done = true;
                let run_ocr = ocr_on && ocr.has_model();
                guard.connection = if run_ocr {
                    ConnectionState::RunningOcr
                } else {
                    ConnectionState::Connected
                };
                let n = guard.pages.len();
                let sheets = guard.sheets;
                if run_ocr {
                    guard.status_line = format!("Sheet {sheets} in preview. Reading text…");
                } else if guard.continuous {
                    guard.prompt_continue = true;
                    guard.status_line =
                        format!("Sheet {sheets} added ({n} pages). Continue or cancel.");
                } else {
                    guard.status_line = format!("Scan complete. Sheet {sheets} · {n} pages.");
                }
            }
            if ocr_on && ocr.has_model() {
                let (front_text, back_text) =
                    ocr.extract_duplex(Arc::clone(&front), Arc::clone(&back)).await;
                let mut guard = state.lock();
                guard.front_text = front_text.clone();
                guard.back_text = back_text.clone();
                let start = guard.pages.len().saturating_sub(
                    usize::from(!front.is_empty()) + usize::from(!back.is_empty()),
                );
                if !front.is_empty() {
                    if let Some(slot) = guard.page_texts.get_mut(start) {
                        *slot = front_text;
                    }
                }
                if !back.is_empty() {
                    let idx = start + usize::from(!front.is_empty());
                    if let Some(slot) = guard.page_texts.get_mut(idx) {
                        *slot = back_text;
                    }
                }
                let n = guard.pages.len();
                let sheets = guard.sheets;
                guard.connection = ConnectionState::Connected;
                guard.session.jobs_ok += 1;
                if guard.continuous {
                    guard.prompt_continue = true;
                    guard.status_line =
                        format!("Sheet {sheets} added ({n} pages). Continue or cancel.");
                } else {
                    guard.status_line = format!("Scan complete. Sheet {sheets} · {n} pages.");
                }
            } else {
                state.lock().session.jobs_ok += 1;
            }
            Ok::<_, crate::error::AppError>(())
        }
        .await;

        if let Err(err) = result {
            error!("duplex capture failed: {err}");
            let mut guard = state.lock();
            guard.session.jobs_failed += 1;
            guard.session.last_error = Some(err.to_string());
            match err {
                crate::error::AppError::Network(_) | crate::error::AppError::Disconnected => {
                    guard.mark_disconnected("Lost the scanner during the scan. Looking again…");
                }
                _ => {
                    guard.connection = ConnectionState::Connected;
                    guard.status_line = "Scan failed. The scanner is still connected.".into();
                }
            }
        }
    });
}

fn scan_pdf_name() -> String {
    format!(
        "scan_{}.pdf",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    )
}

async fn write_pdf_dialog(pages: Vec<Arc<Vec<u8>>>, dpi: u16) -> Result<PathBuf, String> {
    let default_name = scan_pdf_name();
    let suggested = dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Scans");
    let _ = std::fs::create_dir_all(&suggested);

    let picked = rfd::AsyncFileDialog::new()
        .add_filter("PDF", &["pdf"])
        .set_file_name(&default_name)
        .set_directory(&suggested)
        .save_file()
        .await;

    let path = match picked {
        Some(handle) => handle.path().to_path_buf(),
        None => suggested.join(default_name),
    };

    write_pdf_to(pages, path, dpi).await
}

async fn write_pdf_default(pages: Vec<Arc<Vec<u8>>>, dpi: u16) -> Result<PathBuf, String> {
    let name = scan_pdf_name();
    let dir = dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Scans");
    let _ = std::fs::create_dir_all(&dir);
    write_pdf_to(pages, dir.join(name), dpi).await
}

async fn write_email_pdf_default(pages: Vec<Arc<Vec<u8>>>, dpi: u16) -> Result<PathBuf, String> {
    let name = format!(
        "scan_{}-email.pdf",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    let dir = dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Scans");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    let pages_clone = pages.clone();
    let path_clone = path.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        crate::pdf::write_email_pdf(&pages_clone, &path_clone, dpi)
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(|err| err.to_string())?;
    info!(path = %path.display(), dpi, bytes, "wrote email PDF");
    Ok(path)
}

async fn write_pdf_to(
    pages: Vec<Arc<Vec<u8>>>,
    path: PathBuf,
    dpi: u16,
) -> Result<PathBuf, String> {
    let pages_clone = pages.clone();
    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || crate::pdf::write_pdf(&pages_clone, &path_clone, dpi))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())?;
    info!(path = %path.display(), dpi, "wrote PDF");
    Ok(path)
}

async fn attach_email(path: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !path.is_file() {
            return Err("The PDF is gone, so nothing could be attached.".into());
        }
        let path_json = json_string(path.to_string_lossy().as_ref());
        let payload = format!(
            r#"{{"compose":true,"attachments":[{path_json}],"mailto":"mailto:?subject=Scan"}}"#
        );
        let summoned = std::process::Command::new("omarchy-shell")
            .args(["shell", "summon", "omamail", &payload])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if summoned {
            return Ok(());
        }
        std::process::Command::new("xdg-email")
            .arg("--attach")
            .arg(&path)
            .spawn()
            .map(|_| ())
            .map_err(|err| format!("Couldn't open mail with the PDF attached: {err}"))
    })
    .await
    .map_err(|err| err.to_string())?
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
