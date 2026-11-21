# GIF Pin (`gif-pin`)

A lightweight, native **Windows 11** desktop overlay utility written in **Rust**.
Pin independent, floating, always-on-top media widgets — GIFs, images, and MP4 videos — anywhere on your screen.

## Key Features

- **True Per-Pixel Transparency** — transparent `.gif` and `.png` files float cleanly over games, browsers, and desktop windows with zero background boxes or window borders (layered windows with `UpdateLayeredWindow` / `ULW_ALPHA`).
- **Multi-Format Support** — smooth looping `.gif`, hardware-accelerated `.mp4` (via native Windows Media Foundation), `.png`, `.jpg`, `.jpeg`, and `.webp`.
- **Effortless Controls** — drag anywhere to move (multi-monitor & DPI-aware), proportional corner resizing with a per-widget **Aspect Ratio Lock** toggle (hold `Shift` for a temporary free stretch), mouse-wheel scaling, per-widget opacity slider (10%–100%), click-through lock mode, a **Show Bounds** edit frame for grabbing fully transparent images, and System Tray integration.
- **Zero Dependencies** — compiles into a single standalone `gif-pin.exe` (~5 MB). No FFmpeg, no OpenCV, no Python, no runtime installs.

## Quick Start

1. Download `gif-pin.exe` from the [**Releases**](https://github.com/Nachosia/gif-pin/releases) page (or build from source below).
2. Run it — the **Overlay Hub** opens.
3. Click **＋ Add Media** (or drag & drop files onto the hub) and pick `.gif`, `.mp4`, `.png`, `.jpg`, `.jpeg`, or `.webp` files.
4. Each file spawns its own floating, always-on-top widget. Done.

Closing the hub window (**X**) minimizes it to the **system tray** — your overlays keep running.
Left-click the tray icon (or **Show Hub** in its menu) to reopen the hub; the tray menu also offers
Show/Hide All and **Exit**, which closes every overlay and quits the app completely. The hub footer
has matching **Hide to Tray** and **Exit App** buttons.

### Widget controls

| Action | How |
|---|---|
| Move | Left-drag anywhere on the widget |
| Scale | Mouse wheel over the widget (aspect preserved) |
| Resize | Drag a corner handle (appears on hover); proportional by default |
| Aspect ratio | Hub **Ratio: On/Off** button or right-click **Lock Aspect Ratio** check item; hold `Shift` while dragging for a temporary free stretch |
| Show Bounds | Hub **Bounds: On/Off** button or right-click **Show Bounds** check item — draws a subtle backdrop + accent border so fully transparent GIFs/PNGs can be grabbed by the corners |
| Opacity | Slider in the hub, or right-click menu on the widget (10–100%) |
| Pause / Resume | Hub button or right-click menu (GIF / MP4) |
| Mute / Unmute | Hub button (MP4; widgets start muted) |
| Click-through lock | Hub **Lock** button or right-click menu — unlock from the hub |
| Reset size / Close | Right-click menu, or the hub's widget list |

## Build from Source

Requirements:

- Windows 11 (x86_64)
- [Rust](https://rustup.rs/) stable, `x86_64-pc-windows-msvc` toolchain (includes the MSVC linker via Visual Studio Build Tools)

```bat
git clone https://github.com/Nachosia/gif-pin.git
cd gif-pin
cargo build --release
target\release\gif-pin.exe
```

Run the test suite (decoders, alpha math, anchor-resize geometry):

```bat
cargo test
```

No external C libraries are needed — MP4 decoding uses the Windows Media Foundation
pipeline built into the OS, with a D3D11 DXGI device manager for hardware acceleration.

## Architecture

| File | Responsibility |
|---|---|
| `src/main.rs` | Entry point, Media Foundation startup/shutdown, hub bootstrap |
| `src/ui.rs` | egui dark-mode control hub, tray icon, native file dialogs, drag & drop, mouse-wheel routing hook |
| `src/overlay.rs` | Widget windows (one thread + message loop each), anchor-based drag/resize, layered right-click menu with opacity slider |
| `src/media.rs` | GIF/PNG/JPG/WEBP decoding (`image`), MP4 via `IMFSourceReader` + D3D11, audio via `MFCreateAudioRenderer` |
| `src/win32_utils.rs` | Layered DIB surfaces, premultiplied-alpha presentation, SIMD scaling (`fast_image_resize`), DPI/monitor helpers |

Each widget is a frameless `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW` window rendered
with `UpdateLayeredWindow` — the Windows mechanism that guarantees true per-pixel alpha.
Geometry changes (resize, wheel scaling, DPI moves) are applied atomically in a single
`UpdateLayeredWindow` call so widgets never jump. Every widget owns its decoder, GDI
surface, and COM references; all of them are released when the widget closes.

## Notes

- **Resizing transparent images:** Windows passes mouse clicks through pixels with `alpha == 0`, so the empty corners of a transparent GIF/PNG can't be grabbed directly. Enable **Show Bounds** (hub button or right-click menu) to composite a faint backdrop with `alpha > 0` across the whole bounding box plus an accent border and always-visible corner handles; toggle it off to float cleanly again.
- MP4 audio plays through the Windows audio renderer sink; widgets start **muted** — unmute from the hub.
- Codecs without a Media Foundation decoder (e.g. HEVC without the Windows extension) show an error in the hub instead of failing silently.
- Animated `.webp` is displayed as its first frame.

## License

[MIT](LICENSE)
