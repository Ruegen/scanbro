use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::widget::button::Status as BtnStatus;
use iced::widget::{
    button, checkbox, column, container, horizontal_space, image, pick_list, progress_bar, row,
    scrollable, stack, text,
};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Length, Shadow, Size, Subscription,
    Task, Theme, Vector,
};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::escl::EsclClient;
use crate::ocr::OcrEngine;
use crate::state::{AppState, ConnectionState, FeederKind, ScanPreset, SharedState, TransportPref};

const BUILD: &str = env!("SCANBUDDY_BUILD");

const BG: Color = Color::from_rgb(0.09, 0.10, 0.14);
const SURFACE: Color = Color::from_rgb(0.14, 0.16, 0.22);
const SURFACE_2: Color = Color::from_rgb(0.17, 0.19, 0.27);
const TEXT: Color = Color::from_rgb(0.78, 0.82, 0.96);
const MUTED: Color = Color::from_rgb(0.55, 0.60, 0.72);
const ACCENT: Color = Color::from_rgb(0.49, 0.81, 1.0);
const GREEN: Color = Color::from_rgb(0.18, 0.72, 0.38);
const GREEN_HOVER: Color = Color::from_rgb(0.32, 0.88, 0.50);
const GREEN_PRESS: Color = Color::from_rgb(0.12, 0.54, 0.28);
const AMBER: Color = Color::from_rgb(0.91, 0.72, 0.29);
const RED: Color = Color::from_rgb(0.91, 0.38, 0.38);
const INK: Color = Color::from_rgb(0.08, 0.09, 0.13);
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
}

pub fn run() -> iced::Result {
    iced::application(
        concat!("Scanbuddy · ", env!("SCANBUDDY_BUILD")),
        Dashboard::update,
        Dashboard::view,
    )
        .subscription(Dashboard::subscription)
        .theme(Dashboard::theme)
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
        iced::time::every(Duration::from_millis(160)).map(|_| Message::Tick)
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
                let mut guard = self.shared.lock();
                guard.transport_pref = pref;
                let mismatch = guard
                    .scanner
                    .as_ref()
                    .is_some_and(|s| !pref.matches(s.via));
                if mismatch {
                    guard.mark_disconnected(match pref {
                        TransportPref::Usb => "Looking for your scanner on USB…",
                        TransportPref::Wifi => "Looking for your scanner on Wi-Fi…",
                        TransportPref::Auto => "Looking for your scanner…",
                    });
                } else {
                    guard.status_line = match pref {
                        TransportPref::Usb => "Using USB.".into(),
                        TransportPref::Wifi => "Using Wi-Fi.".into(),
                        TransportPref::Auto => "Using Wi-Fi or USB.".into(),
                    };
                }
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
            column![header, row![controls, stage].spacing(8).height(Length::Fill)]
                .spacing(8)
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
        | ConnectionState::RunningOcr => (GREEN, "Connected"),
        ConnectionState::Degraded => (RED, "Degraded"),
        ConnectionState::AwaitingConnection => (AMBER, "Looking…"),
    };

    let name = snap
        .scanner
        .as_ref()
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "Brother DS-940DW".into());
    let via = snap
        .scanner
        .as_ref()
        .map(|s| s.via.label())
        .unwrap_or("—");
    let meta = snap
        .scanner
        .as_ref()
        .map(|s| match s.via {
            crate::state::Transport::Usb => "USB".into(),
            crate::state::Transport::Wifi => format!("Wi-Fi · {}", s.ip),
        })
        .unwrap_or_else(|| "Looking for your scanner…".into());

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

    let status_chip = chip(format!("{label} · {via}"), SURFACE_2, Some(dot));
    let feeder_chip = chip(feeder_kind.label().to_string(), SURFACE_2, Some(feeder_color));
    let recipe_chip = chip(
        {
            let dpi = snap.scan_preset.dpi();
            if snap.long_receipt {
                format!("Long receipt · one side · {dpi} dpi")
            } else {
                format!("Duplex · Color · {dpi} dpi")
            }
        },
        SURFACE_2,
        None,
    );

    let pending = if snap.long_receipt { 1 } else { 2 };
    let mut chips = row![status_chip, feeder_chip, recipe_chip].spacing(8);
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
                    format!("Sheet {sheets} · {pages} pages")
                } else {
                    format!("Sheet {sheets} · {pages} pages · {size}")
                },
                SURFACE_2,
                None,
            ));
        }
    }
    if capturing {
        if snap.long_receipt {
            chips = chips.push(chip("Receipt".into(), SURFACE_2, Some(AMBER)));
        } else {
            let front_color = if snap.progress.front_done { GREEN } else { AMBER };
            let back_color = if snap.progress.back_done {
                GREEN
            } else if snap.progress.front_done || snap.progress.current_side >= 2 {
                AMBER
            } else {
                MUTED
            };
            chips = chips.push(chip("Front".into(), SURFACE_2, Some(front_color)));
            chips = chips.push(chip("Back".into(), SURFACE_2, Some(back_color)));
        }
    }
    if let Some(percent) = battery {
        chips = chips.push(chip(format!("Battery {percent}%"), SURFACE_2, None));
    }

    let mut info = column![
        row![
            text(name).size(26).color(TEXT),
            horizontal_space(),
            text(BUILD).size(12).color(MUTED),
        ]
        .align_y(Alignment::Center),
        text(meta).size(14).color(MUTED),
        text(snap.status_line.clone()).size(13).color(MUTED),
    ]
    .spacing(4);
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

fn chip(label: String, fill: Color, dot: Option<Color>) -> Element<'static, Message> {
    let mut inner = row![].spacing(8).align_y(Alignment::Center);
    if let Some(color) = dot {
        inner = inner.push(
            container(horizontal_space())
                .width(8)
                .height(8)
                .style(move |_| container::Style {
                    background: Some(Background::Color(color)),
                    border: Border {
                        radius: 8.0.into(),
                        ..Border::default()
                    },
                    text_color: None,
                    shadow: Shadow::default(),
                }),
        );
    }
    inner = inner.push(text(label).size(13));
    container(inner)
        .padding([6, 12])
        .style(move |_| pill(fill))
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

    let scan_btn = action_button(scan_label, 28.0, can_scan.then_some(Message::Scan), true);
    let save_label = if busy && snap.connection != ConnectionState::Scanning {
        "Working…".into()
    } else if can_save {
        format!("Save PDF · {size_label}")
    } else {
        "Save PDF".into()
    };
    let save_btn = action_button(
        save_label,
        20.0,
        can_save.then_some(Message::SavePdf),
        false,
    );
    let email_label = if !can_save {
        "Email PDF".into()
    } else if scan_bytes <= crate::pdf::EMAIL_MAX_BYTES {
        format!("Email PDF · {size_label}")
    } else {
        "Email PDF · under 5 MB".into()
    };
    let email_btn = action_button(
        email_label,
        18.0,
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
        .text_size(15)
        .padding([8, 12])
        .width(Length::Fill)
        .style(|_, status| {
            use iced::widget::pick_list::Status;
            let border_c = match status {
                Status::Hovered | Status::Opened => ACCENT,
                Status::Active => Color::from_rgb(0.22, 0.25, 0.34),
            };
            iced::widget::pick_list::Style {
                text_color: TEXT,
                placeholder_color: MUTED,
                handle_color: MUTED,
                background: Background::Color(SURFACE_2),
                border: Border {
                    radius: 8.0.into(),
                    width: 1.0,
                    color: border_c,
                },
            }
        });

    let transport = row![
        seg_button("Auto", snap.transport_pref == TransportPref::Auto, Message::SetTransport(TransportPref::Auto), !busy),
        seg_button("Wi-Fi", snap.transport_pref == TransportPref::Wifi, Message::SetTransport(TransportPref::Wifi), !busy),
        seg_button("USB", snap.transport_pref == TransportPref::Usb, Message::SetTransport(TransportPref::Usb), !busy),
    ]
    .spacing(8);

    let mut ocr = checkbox("Extract text (OCR)", snap.ocr_enabled)
        .text_size(16)
        .style(|_, status| checkbox_style(status));
    if !busy {
        ocr = ocr.on_toggle(Message::ToggleOcr);
    }

    let mut continuous = checkbox("Continuous feed", snap.continuous)
        .text_size(16)
        .style(|_, status| checkbox_style(status));
    if !busy {
        continuous = continuous.on_toggle(Message::ToggleContinuous);
    }

    let mut long_receipt = checkbox("Long receipt", snap.long_receipt)
        .text_size(16)
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

    let mut controls = column![scan_btn, save_btn, email_btn].spacing(12);
    if let Some(hint) = email_hint {
        controls = controls.push(text(hint).size(12).color(MUTED));
    }
    controls = controls.push(text("Quality").size(11).color(MUTED));
    controls = controls.push(quality);
    controls = controls.push(text("Connection").size(11).color(MUTED));
    controls = controls.push(transport);
    controls = controls.push(ocr);
    controls = controls.push(continuous);
    controls = controls.push(long_receipt);
    if can_save {
        controls = controls.push(kv("Scan size", size_label));
    }
    controls = controls.push(kv("Last PDF", last_pdf));
    controls = controls.push(scrollable(text(err).size(13).color(RED)).height(Length::Fill));

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
        container(text("No scan yet").size(18).color(MUTED))
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
        row![text("Scan").size(18).color(ACCENT), horizontal_space(), counter]
            .align_y(Alignment::Center)
    ]
    .spacing(8)
    .height(Length::Fill);
    if capturing {
        stage = stage.push(
            progress_bar(0.0..=1.0, progress)
                .height(8.0)
                .style(|_theme| progress_bar::Style {
                    background: SURFACE_2.into(),
                    bar: ACCENT.into(),
                    border: Border {
                        radius: 4.0.into(),
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
            radius: 12.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.22, 0.25, 0.34),
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
            20.0,
            Some(Message::ContinueFeed),
            true,
        ));
    }
    actions = actions.push(action_button(
        "Cancel",
        20.0,
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
            radius: 12.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.22, 0.25, 0.34),
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
    size: f32,
    on_press: Option<Message>,
    scan: bool,
) -> Element<'static, Message> {
    button(text(label.into()).size(size))
        .on_press_maybe(on_press)
        .width(Length::Fill)
        .padding([22, 18])
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
    let (bg, border_w, border_c, shadow) = match status {
        BtnStatus::Hovered => (
            GREEN_HOVER,
            2.0,
            WHITE,
            Shadow {
                color: Color::from_rgba(0.18, 0.72, 0.38, 0.55),
                offset: Vector::new(0.0, 6.0),
                blur_radius: 22.0,
            },
        ),
        BtnStatus::Pressed => (
            GREEN_PRESS,
            2.0,
            WHITE,
            Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.2),
                offset: Vector::new(0.0, 2.0),
                blur_radius: 8.0,
            },
        ),
        BtnStatus::Disabled => (
            Color::from_rgb(0.22, 0.28, 0.24),
            1.0,
            Color::from_rgb(0.30, 0.36, 0.32),
            Shadow::default(),
        ),
        BtnStatus::Active => (
            GREEN,
            1.0,
            Color::from_rgb(0.45, 0.95, 0.62),
            Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.28),
                offset: Vector::new(0.0, 4.0),
                blur_radius: 18.0,
            },
        ),
    };
    let text_color = match status {
        BtnStatus::Disabled => MUTED,
        _ => WHITE,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color,
        border: Border {
            radius: 16.0.into(),
            width: border_w,
            color: border_c,
        },
        shadow,
    }
}

fn secondary_style(status: BtnStatus) -> button::Style {
    let (bg, border_c, width) = match status {
        BtnStatus::Hovered => (Color::from_rgb(0.26, 0.32, 0.44), ACCENT, 2.0),
        BtnStatus::Pressed => (Color::from_rgb(0.14, 0.16, 0.22), ACCENT, 2.0),
        BtnStatus::Disabled => (Color::from_rgb(0.16, 0.18, 0.22), Color::from_rgb(0.28, 0.30, 0.36), 1.0),
        BtnStatus::Active => (SURFACE_2, Color::from_rgb(0.32, 0.38, 0.50), 1.0),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: if matches!(status, BtnStatus::Disabled) { MUTED } else { WHITE },
        border: Border {
            radius: 16.0.into(),
            width,
            color: border_c,
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.22),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 14.0,
        },
    }
}

fn seg_button(
    label: &'static str,
    selected: bool,
    msg: Message,
    enabled: bool,
) -> Element<'static, Message> {
    button(text(label).size(14))
        .on_press_maybe(enabled.then_some(msg))
        .padding([10, 12])
        .width(Length::Fill)
        .style(move |_, status| {
            let hovered = enabled && matches!(status, BtnStatus::Hovered | BtnStatus::Pressed);
            let bg = if selected {
                if hovered { GREEN_HOVER } else { GREEN }
            } else if hovered {
                Color::from_rgb(0.26, 0.32, 0.44)
            } else {
                SURFACE_2
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: if enabled { WHITE } else { MUTED },
                border: Border {
                    radius: 10.0.into(),
                    width: if (hovered || selected) && enabled { 2.0 } else { 1.0 },
                    color: if (selected || hovered) && enabled {
                        WHITE
                    } else {
                        Color::from_rgb(0.32, 0.38, 0.50)
                    },
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
        background: Background::Color(if checked { GREEN } else if hovered { Color::from_rgb(0.26, 0.32, 0.44) } else { SURFACE_2 }),
        icon_color: WHITE,
        border: Border {
            radius: 6.0.into(),
            width: if hovered { 2.0 } else { 1.5 },
            color: if checked || hovered { GREEN_HOVER } else { Color::from_rgb(0.40, 0.46, 0.58) },
        },
        text_color: Some(TEXT),
    }
}

fn kv(key: &'static str, value: impl Into<String>) -> Element<'static, Message> {
    column![
        text(key).size(11).color(MUTED),
        text(value.into()).size(15).color(TEXT),
    ]
    .spacing(2)
    .into()
}

fn card() -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        text_color: Some(TEXT),
        border: Border {
            radius: 18.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.22, 0.25, 0.34),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.25),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
    }
}

fn pill(fill: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(fill)),
        border: Border {
            radius: 999.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        text_color: Some(TEXT),
        shadow: Shadow::default(),
    }
}

fn spawn_backend(state: SharedState, mut cmd_rx: mpsc::UnboundedReceiver<BackendCmd>) {
    std::thread::Builder::new()
        .name("scanbuddy-backend".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("scanbuddy-worker")
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
                        crate::usb::spawn(Arc::clone(&state), escl.clone());
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
