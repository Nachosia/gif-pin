#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use windows::core::{w, HSTRING};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MF_VERSION};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

fn main() -> eframe::Result<()> {
    unsafe {
        if let Err(e) = MFStartup(MF_VERSION, 0) {
            MessageBoxW(
                None,
                &HSTRING::from(format!(
                    "Media Foundation initialization failed (0x{:08X}).\nMP4 playback will not work.",
                    e.code().0
                )),
                w!("GIF Pin"),
                MB_OK | MB_ICONERROR,
            );
            return Ok(());
        }
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([440.0, 680.0])
            .with_min_inner_size([400.0, 520.0])
            .with_title(gif_pin::ui::HUB_TITLE),
        ..Default::default()
    };

    let result = eframe::run_native(
        "GIF Pin",
        options,
        Box::new(|cc| Ok(Box::new(gif_pin::ui::HubApp::new(cc)))),
    );

    unsafe {
        let _ = MFShutdown();
    }
    result
}
