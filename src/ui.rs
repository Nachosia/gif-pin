use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClassNameW, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WindowFromPoint, HHOOK, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_MOUSEWHEEL,
};

use crate::media::{kind_from_path, Kind};
use crate::overlay::{
    self, HubEvent, WidgetInfo, CMD_CLOSE, CMD_HIDE, CMD_SET_ASPECT_LOCK, CMD_SET_LOCKED,
    CMD_SET_MUTED, CMD_SET_OPACITY, CMD_SET_PAUSED, CMD_SET_SHOW_BOUNDS, CMD_SHOW, WIDGET_CLASS,
};

const BG: egui::Color32 = egui::Color32::from_rgb(0x15, 0x15, 0x1c);
const CARD: egui::Color32 = egui::Color32::from_rgb(0x1e, 0x1e, 0x28);
const BORDER: egui::Color32 = egui::Color32::from_rgb(0x2e, 0x2e, 0x3c);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x60, 0xcd, 0xff);
const DIM: egui::Color32 = egui::Color32::from_rgb(0x9a, 0x9a, 0xa8);
const TEXT: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xe8, 0xf0);
const RED: egui::Color32 = egui::Color32::from_rgb(0xff, 0x7b, 0x72);

struct Entry {
    id: u64,
    name: String,
    kind: Kind,
    info: Option<WidgetInfo>,
    failed: Option<String>,
    visible: bool,
    muted: bool,
    locked: bool,
    paused: bool,
    opacity: u8,
    lock_aspect: bool,
    show_bounds: bool,
}

enum Pending {
    Close(u64),
    Visible(u64, bool),
    Mute(u64, bool),
    Pause(u64, bool),
    Lock(u64, bool),
    Opacity(u64, u8),
    Aspect(u64, bool),
    Bounds(u64, bool),
}

pub struct HubApp {
    entries: Vec<Entry>,
    next_id: u64,
    cascade: usize,
    hub_rx: Receiver<HubEvent>,
    hub_tx: Sender<HubEvent>,
    dlg_rx: Receiver<Vec<PathBuf>>,
    dlg_tx: Sender<Vec<PathBuf>>,
    dialog_open: bool,
    _tray: Option<TrayIcon>,
    hook: Option<HHOOK>,
    status: Option<(String, Instant)>,
    title_font: egui::FontId,
}

impl HubApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = CARD;
        visuals.extreme_bg_color = egui::Color32::from_rgb(0x10, 0x10, 0x16);
        visuals.faint_bg_color = egui::Color32::from_rgb(0x1a, 0x1a, 0x22);
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
        visuals.hyperlink_color = ACCENT;
        visuals.window_corner_radius = 12.0.into();
        visuals.menu_corner_radius = 10.0.into();
        visuals.widgets.noninteractive.bg_fill = CARD;
        visuals.widgets.noninteractive.corner_radius = 8.0.into();
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x26, 0x26, 0x32);
        visuals.widgets.inactive.corner_radius = 8.0.into();
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x30, 0x30, 0x40);
        visuals.widgets.hovered.corner_radius = 8.0.into();
        visuals.widgets.hovered.bg_stroke =
            egui::Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.6));
        visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.8);
        visuals.widgets.active.corner_radius = 8.0.into();
        ctx.set_visuals(visuals);
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
        });

        // Windows 11 system font, with graceful fallback to egui's bundled fonts.
        let mut fonts = egui::FontDefinitions::default();
        let mut title_font = egui::FontId::new(24.0, egui::FontFamily::Proportional);
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
            fonts
                .font_data
                .insert("segoeui".to_owned(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "segoeui".to_owned());
        }
        if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeuib.ttf") {
            fonts
                .font_data
                .insert("segoeuib".to_owned(), egui::FontData::from_owned(bytes).into());
            fonts.families.insert(
                egui::FontFamily::Name("bold".into()),
                vec!["segoeuib".to_owned(), "segoeui".to_owned()],
            );
            title_font = egui::FontId::new(24.0, egui::FontFamily::Name("bold".into()));
        }
        ctx.set_fonts(fonts);

        let (hub_tx, hub_rx) = channel();
        let (dlg_tx, dlg_rx) = channel();

        // Route mouse-wheel events over widgets to the widget under the cursor
        // (WM_MOUSEWHEEL normally goes to the focused window, not the hovered one).
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) }.ok();

        let tray = build_tray();

        Self {
            entries: Vec::new(),
            next_id: 1,
            cascade: 0,
            hub_rx,
            hub_tx,
            dlg_rx,
            dlg_tx,
            dialog_open: false,
            _tray: tray,
            hook,
            status: Some(("Ready. Add media to spawn floating overlays.".into(), Instant::now())),
            title_font,
        }
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    fn show_hub(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn open_dialog(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let tx = self.dlg_tx.clone();
        std::thread::spawn(move || {
            let files = rfd::FileDialog::new()
                .add_filter("Media", &["gif", "mp4", "png", "jpg", "jpeg", "webp"])
                .pick_files();
            // Always send, even on cancel, so the dialog-open guard resets.
            let _ = tx.send(files.unwrap_or_default());
        });
    }

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        let mut spawned = 0;
        for path in paths {
            let Some(kind) = kind_from_path(&path) else {
                self.set_status(format!(
                    "Unsupported format: {}",
                    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                ));
                continue;
            };
            let id = self.next_id;
            self.next_id += 1;
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "media".into());
            self.entries.push(Entry {
                id,
                name,
                kind,
                info: None,
                failed: None,
                visible: true,
                muted: true,
                locked: false,
                paused: false,
                opacity: 100,
                lock_aspect: true,
                show_bounds: false,
            });
            overlay::spawn_widget(id, path, self.hub_tx.clone(), self.cascade);
            self.cascade = self.cascade.wrapping_add(1);
            spawned += 1;
        }
        if spawned > 0 {
            self.set_status(format!("Spawned {spawned} widget(s)."));
        }
    }

    fn poll_events(&mut self) {
        while let Ok(ev) = self.hub_rx.try_recv() {
            match ev {
                HubEvent::Created(info) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == info.id) {
                        e.info = Some(info);
                    }
                }
                HubEvent::Failed { id, error, .. } => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.failed = Some(error);
                    }
                }
                HubEvent::Closed { id } => {
                    self.entries.retain(|e| e.id != id || e.failed.is_some());
                }
                HubEvent::Opacity { id, value } => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.opacity = value;
                    }
                }
                HubEvent::AspectLock { id, value } => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.lock_aspect = value;
                    }
                }
                HubEvent::Bounds { id, value } => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.show_bounds = value;
                    }
                }
            }
        }
        if let Ok(files) = self.dlg_rx.try_recv() {
            self.dialog_open = false;
            self.add_files(files);
        }
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.0.as_str() {
                "show" => self.show_hub(ctx),
                "show_all" => self.set_all_visible(true),
                "hide_all" => self.set_all_visible(false),
                "exit" => {
                    self.close_all();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = ev
            {
                self.show_hub(ctx);
            }
        }
    }

    fn set_all_visible(&mut self, visible: bool) {
        for e in &mut self.entries {
            if let Some(info) = &e.info {
                e.visible = visible;
                overlay::post_cmd(
                    info.hwnd,
                    if visible { CMD_SHOW } else { CMD_HIDE },
                    0,
                );
            }
        }
    }

    fn close_all(&mut self) {
        for e in &self.entries {
            if let Some(info) = &e.info {
                overlay::post_cmd(info.hwnd, CMD_CLOSE, 0);
            }
        }
        self.entries.clear();
        self.set_status("All widgets closed.");
    }

    fn apply_pending(&mut self, pending: Vec<Pending>) {
        for p in pending {
            match p {
                Pending::Close(id) => {
                    if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
                        let e = self.entries.remove(pos);
                        if let Some(info) = e.info {
                            overlay::post_cmd(info.hwnd, CMD_CLOSE, 0);
                        }
                    }
                }
                Pending::Visible(id, v) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.visible = v;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, if v { CMD_SHOW } else { CMD_HIDE }, 0);
                        }
                    }
                }
                Pending::Mute(id, m) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.muted = m;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, CMD_SET_MUTED, m as isize);
                        }
                    }
                }
                Pending::Pause(id, p) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.paused = p;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, CMD_SET_PAUSED, p as isize);
                        }
                    }
                }
                Pending::Lock(id, l) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.locked = l;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, CMD_SET_LOCKED, l as isize);
                        }
                    }
                }
                Pending::Opacity(id, v) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.opacity = v;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, CMD_SET_OPACITY, v as isize);
                        }
                    }
                }
                Pending::Aspect(id, l) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.lock_aspect = l;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, CMD_SET_ASPECT_LOCK, l as isize);
                        }
                    }
                }
                Pending::Bounds(id, b) => {
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.show_bounds = b;
                        if let Some(info) = &e.info {
                            overlay::post_cmd(info.hwnd, CMD_SET_SHOW_BOUNDS, b as isize);
                        }
                    }
                }
            }
        }
    }
}

impl eframe::App for HubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        self.poll_tray(ctx);

        if ctx.input(|i| i.viewport().close_requested()) {
            // Close button minimizes to tray; overlays keep running.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.set_status("Hub hidden to tray — overlays keep running.");
        }

        // Lightweight polling of widget/tray/dialog events while idle.
        ctx.request_repaint_after(Duration::from_millis(150));

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.add_files(dropped);
        }

        let mut pending: Vec<Pending> = Vec::new();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("GIF Pin")
                        .font(self.title_font.clone())
                        .color(ACCENT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("desktop media overlays")
                            .size(12.0)
                            .color(DIM),
                    );
                });
            });
            ui.add_space(10.0);

            let add = ui.add_sized(
                [ui.available_width(), 44.0],
                egui::Button::new(
                    egui::RichText::new("＋  Add Media")
                        .size(16.0)
                        .strong()
                        .color(egui::Color32::from_rgb(0x0a, 0x14, 0x20)),
                )
                .fill(ACCENT)
                .corner_radius(10.0),
            );
            if add.clicked() {
                self.open_dialog();
            }
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("or drop files here — GIF · MP4 · PNG · JPG · WEBP")
                    .size(11.5)
                    .color(DIM),
            );
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            let active = self.entries.iter().filter(|e| e.info.is_some()).count();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("Active widgets ({active})"))
                        .size(14.0)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(active > 0, egui::Button::new("Close All").small())
                        .clicked()
                    {
                        self.close_all();
                    }
                    if ui
                        .add_enabled(active > 0, egui::Button::new("Hide All").small())
                        .clicked()
                    {
                        self.set_all_visible(false);
                    }
                    if ui
                        .add_enabled(active > 0, egui::Button::new("Show All").small())
                        .clicked()
                    {
                        self.set_all_visible(true);
                    }
                });
            });
            ui.add_space(6.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.entries.is_empty() {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new("No widgets yet").size(15.0).color(DIM));
                            ui.label(
                                egui::RichText::new("Add media to spawn floating overlays.")
                                    .size(12.0)
                                    .color(DIM),
                            );
                        });
                    }
                    for entry in &mut self.entries {
                        egui::Frame::new()
                            .fill(CARD)
                            .corner_radius(10.0)
                            .stroke(egui::Stroke::new(1.0_f32, BORDER))
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (badge, color) = match entry.kind {
                                        Kind::Gif => ("GIF", egui::Color32::from_rgb(0xc6, 0x78, 0xdd)),
                                        Kind::Video => ("MP4", ACCENT),
                                        Kind::Image => ("IMG", egui::Color32::from_rgb(0x98, 0xc3, 0x79)),
                                    };
                                    ui.label(
                                        egui::RichText::new(badge).size(10.0).strong().color(color),
                                    );
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(&entry.name).size(13.0))
                                            .truncate(),
                                    );
                                    if let Some(info) = &entry.info {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{}×{}",
                                                info.width, info.height
                                            ))
                                            .size(10.5)
                                            .color(DIM),
                                        );
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button("×")
                                                .on_hover_text("Close widget")
                                                .clicked()
                                            {
                                                pending.push(Pending::Close(entry.id));
                                            }
                                        },
                                    );
                                });
                                if let Some(err) = &entry.failed {
                                    ui.label(egui::RichText::new(err).size(11.0).color(RED));
                                } else if entry.info.is_none() {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label(egui::RichText::new("Loading…").color(DIM));
                                    });
                                } else {
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        let label = if entry.visible { "Hide" } else { "Show" };
                                        if ui
                                            .add_sized([52.0, 24.0], egui::Button::new(label).small())
                                            .clicked()
                                        {
                                            pending.push(Pending::Visible(entry.id, !entry.visible));
                                        }
                                        if entry.kind == Kind::Video {
                                            let has_audio = entry
                                                .info
                                                .as_ref()
                                                .map(|i| i.has_audio)
                                                .unwrap_or(false);
                                            let label = if entry.muted { "Unmute" } else { "Mute" };
                                            let resp = ui.add_enabled(
                                                has_audio,
                                                egui::Button::new(label)
                                                    .small()
                                                    .min_size([58.0, 24.0].into()),
                                            );
                                            if resp.clicked() {
                                                pending.push(Pending::Mute(entry.id, !entry.muted));
                                            }
                                            if !has_audio {
                                                resp.on_hover_text("No audio track");
                                            }
                                        }
                                        if entry.kind != Kind::Image {
                                            let label = if entry.paused { "Resume" } else { "Pause" };
                                            if ui
                                                .add_sized(
                                                    [58.0, 24.0],
                                                    egui::Button::new(label).small(),
                                                )
                                                .clicked()
                                            {
                                                pending.push(Pending::Pause(entry.id, !entry.paused));
                                            }
                                        }
                                        let label = if entry.locked { "Unlock" } else { "Lock" };
                                        if ui
                                            .add_sized([56.0, 24.0], egui::Button::new(label).small())
                                            .on_hover_text("Click-through mode (WS_EX_TRANSPARENT)")
                                            .clicked()
                                        {
                                            pending.push(Pending::Lock(entry.id, !entry.locked));
                                        }
                                        let label = if entry.lock_aspect {
                                            "Ratio: On"
                                        } else {
                                            "Ratio: Off"
                                        };
                                        if ui
                                            .add_sized([72.0, 24.0], egui::Button::new(label).small())
                                            .on_hover_text("Lock / Unlock Aspect Ratio")
                                            .clicked()
                                        {
                                            pending.push(Pending::Aspect(
                                                entry.id,
                                                !entry.lock_aspect,
                                            ));
                                        }
                                    });
                                    ui.add_space(2.0);
                                    ui.horizontal(|ui| {
                                        let label = if entry.show_bounds {
                                            "Bounds: On"
                                        } else {
                                            "Bounds: Off"
                                        };
                                        if ui
                                            .add_sized([78.0, 24.0], egui::Button::new(label).small())
                                            .on_hover_text("Show/Hide bounding frame so transparent images can be easily resized by corners")
                                            .clicked()
                                        {
                                            pending.push(Pending::Bounds(
                                                entry.id,
                                                !entry.show_bounds,
                                            ));
                                        }
                                        ui.label(
                                            egui::RichText::new("Opacity").size(11.0).color(DIM),
                                        );
                                        let mut op = entry.opacity;
                                        let width = ui.available_width();
                                        let resp = ui.add_sized(
                                            [width, 20.0],
                                            egui::Slider::new(&mut op, 10..=100).suffix("%"),
                                        );
                                        if resp.changed() {
                                            entry.opacity = op;
                                            pending.push(Pending::Opacity(entry.id, op));
                                        }
                                    });
                                }
                            });
                        ui.add_space(6.0);
                    }
                });

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                if let Some((msg, t)) = &self.status {
                    if t.elapsed() < Duration::from_secs(6) {
                        ui.label(egui::RichText::new(msg).size(11.5).color(DIM));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Hide to Tray").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                });
            });
        });

        self.apply_pending(pending);
    }
}

impl Drop for HubApp {
    fn drop(&mut self) {
        unsafe {
            if let Some(h) = self.hook {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_MOUSEWHEEL {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let hwnd = WindowFromPoint(info.pt);
        if !hwnd.is_invalid() {
            let mut buf = [0u16; 64];
            let n = GetClassNameW(hwnd, &mut buf);
            if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == WIDGET_CLASS {
                let _ = PostMessageW(hwnd, WM_MOUSEWHEEL, wparam, LPARAM(0));
                return LRESULT(1); // swallow: the widget handles scaling itself
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn build_tray() -> Option<TrayIcon> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "Show Hub", true, None);
    let show_all = MenuItem::with_id("show_all", "Show All Widgets", true, None);
    let hide_all = MenuItem::with_id("hide_all", "Hide All Widgets", true, None);
    let exit = MenuItem::with_id("exit", "Exit GIF Pin", true, None);
    let _ = menu.append(&show);
    let _ = menu.append(&show_all);
    let _ = menu.append(&hide_all);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit);

    let icon = Icon::from_rgba(make_icon_rgba(), 32, 32).ok()?;
    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("GIF Pin — media overlays")
        .with_menu(Box::new(menu))
        .build()
        .ok()
}

fn make_icon_rgba() -> Vec<u8> {
    let n = 32usize;
    let mut v = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            // rounded-square SDF
            let qx = (fx - 16.0).abs() - (14.0 - 7.0);
            let qy = (fy - 16.0).abs() - (14.0 - 7.0);
            let d = (qx.max(0.0) * qx.max(0.0) + qy.max(0.0) * qy.max(0.0)).sqrt()
                + qx.max(qy).min(0.0)
                - 7.0;
            if d <= 0.0 {
                let t = (fx + fy) / 64.0;
                let mut c = [
                    (96.0 + 26.0 * t) as u8,
                    (205.0 - 65.0 * t) as u8,
                    255,
                    255,
                ];
                // play triangle
                if fx >= 12.0 && fx <= 23.0 && (fy - 16.0).abs() <= (fx - 12.0) * (6.0 / 11.0) {
                    c = [255, 255, 255, 255];
                }
                let o = (y * n + x) * 4;
                v[o..o + 4].copy_from_slice(&c);
            }
        }
    }
    v
}
