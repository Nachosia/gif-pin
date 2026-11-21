use std::ptr::null_mut;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromPoint, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, UpdateLayeredWindow, GWL_EXSTYLE,
    HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, ULW_ALPHA,
    WS_EX_TRANSPARENT,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A top-down 32bpp DIB + memory DC used as the backing store of a layered window.
/// Pixels are premultiplied BGRA, as required by UpdateLayeredWindow(ULW_ALPHA).
pub struct LayeredSurface {
    pub dc: HDC,
    bmp: HBITMAP,
    pub bits: *mut u8,
    pub w: i32,
    pub h: i32,
}

impl LayeredSurface {
    pub fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(screen);
            let _ = ReleaseDC(None, screen);
            if dc.is_invalid() {
                return None;
            }
            let mut s = Self {
                dc,
                bmp: HBITMAP::default(),
                bits: null_mut(),
                w: 0,
                h: 0,
            };
            s.alloc(w.max(1), h.max(1));
            if s.bits.is_null() {
                let _ = DeleteDC(dc);
                return None;
            }
            Some(s)
        }
    }

    fn alloc(&mut self, w: i32, h: i32) {
        unsafe {
            if !self.bmp.is_invalid() {
                let _ = DeleteObject(self.bmp);
                self.bmp = HBITMAP::default();
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..std::mem::zeroed()
            };
            let mut bits: *mut std::ffi::c_void = null_mut();
            let bmp = match CreateDIBSection(self.dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) => b,
                Err(_) => {
                    self.bits = null_mut();
                    return;
                }
            };
            self.bmp = bmp;
            self.bits = bits as *mut u8;
            self.w = w;
            self.h = h;
            SelectObject(self.dc, bmp);
        }
    }

    pub fn resize(&mut self, w: i32, h: i32) {
        let w = w.max(1);
        let h = h.max(1);
        if w != self.w || h != self.h {
            self.alloc(w, h);
        }
    }

    pub fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize) }
    }

    pub fn present(&self, hwnd: HWND, x: i32, y: i32, alpha: u8) {
        if self.bits.is_null() || hwnd.is_invalid() {
            return;
        }
        unsafe {
            let dst = POINT { x, y };
            let size = SIZE {
                cx: self.w,
                cy: self.h,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: 0, // AC_SRC_OVER
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: 1, // AC_SRC_ALPHA
            };
            let screen = GetDC(None);
            let _ = UpdateLayeredWindow(
                hwnd,
                screen,
                Some(&dst),
                Some(&size),
                self.dc,
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            let _ = ReleaseDC(None, screen);
        }
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.bmp.is_invalid() {
                let _ = DeleteObject(self.bmp);
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

pub fn work_area_at(x: i32, y: i32) -> RECT {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        }
    }
}

pub fn set_click_through(hwnd: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let bit = WS_EX_TRANSPARENT.0 as isize;
        let ex = if on { ex | bit } else { ex & !bit };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

pub fn dpi_scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

/// Straight-alpha RGBA -> premultiplied BGRA.
pub fn rgba_to_premul_bgra(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let a = s[3] as u32;
        d[0] = ((s[2] as u32 * a + 127) / 255) as u8;
        d[1] = ((s[1] as u32 * a + 127) / 255) as u8;
        d[2] = ((s[0] as u32 * a + 127) / 255) as u8;
        d[3] = a as u8;
    }
}

/// Scale a straight-alpha RGBA frame into a premultiplied BGRA surface.
pub fn blit_frame(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    if sw == dw && sh == dh {
        rgba_to_premul_bgra(src, dst);
        return;
    }
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let src_img = match fast_image_resize::images::Image::from_vec_u8(
        sw,
        sh,
        src.to_vec(),
        PixelType::U8x4,
    ) {
        Ok(i) => i,
        Err(_) => return,
    };
    let mut dst_img = fast_image_resize::images::Image::new(dw, dh, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    if Resizer::new().resize(&src_img, &mut dst_img, &opts).is_ok() {
        rgba_to_premul_bgra(dst_img.buffer(), dst);
    }
}

fn fill_rect(dst: &mut [u8], w: i32, h: i32, x: i32, y: i32, rw: i32, rh: i32, col: [u8; 4]) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + rw).min(w);
    let y1 = (y + rh).min(h);
    for yy in y0..y1 {
        for xx in x0..x1 {
            let o = ((yy * w + xx) * 4) as usize;
            dst[o..o + 4].copy_from_slice(&col);
        }
    }
}

/// "Show Bounds" edit frame: a translucent RGBA(20,20,25,35) backdrop composited
/// over the whole widget plus a 1-2px #60CDFF perimeter border. The backdrop
/// guarantees alpha > 0 on every pixel, so Windows hit-tests the full bounding
/// box (including fully transparent media corners) and they can be grabbed.
pub fn draw_bounds_frame(dst: &mut [u8], w: i32, h: i32, scale: f32) {
    let a = 35u32;
    let inv = 255 - a;
    let sr = (20 * a + 127) / 255;
    let sg = (20 * a + 127) / 255;
    let sb = (25 * a + 127) / 255;
    for px in dst.chunks_exact_mut(4) {
        // premultiplied "src over dst"
        px[0] = (sb + px[0] as u32 * inv / 255).min(255) as u8;
        px[1] = (sg + px[1] as u32 * inv / 255).min(255) as u8;
        px[2] = (sr + px[2] as u32 * inv / 255).min(255) as u8;
        px[3] = (a + px[3] as u32 * inv / 255).min(255) as u8;
    }
    let th = if scale > 1.5 { 2 } else { 1 };
    let accent = [255, 205, 96, 255]; // #60CDFF, premultiplied BGRA (opaque)
    fill_rect(dst, w, h, 0, 0, w, th, accent);
    fill_rect(dst, w, h, 0, h - th, w, th, accent);
    fill_rect(dst, w, h, 0, 0, th, h, accent);
    fill_rect(dst, w, h, w - th, 0, th, h, accent);
}

/// Subtle L-shaped corner resize handles (premultiplied near-white).
pub fn draw_corner_handles(dst: &mut [u8], w: i32, h: i32, scale: f32) {
    let len = ((14.0 * scale) as i32).max(6);
    let th = ((3.0 * scale) as i32).max(2);
    let off = ((4.0 * scale) as i32).max(2);
    let c = [225, 225, 225, 235];
    let corners = [
        (off, off, 1i32, 1i32),
        (w - off, off, -1, 1),
        (off, h - off, 1, -1),
        (w - off, h - off, -1, -1),
    ];
    for (cx, cy, sx, sy) in corners {
        let hx = if sx > 0 { cx } else { cx - len };
        let hy = if sy > 0 { cy } else { cy - th };
        fill_rect(dst, w, h, hx, hy, len, th, c);
        let vx = if sx > 0 { cx } else { cx - th };
        let vy = if sy > 0 { cy } else { cy - len };
        fill_rect(dst, w, h, vx, vy, th, len, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_frame_makes_transparent_pixels_hit_testable() {
        let mut buf = vec![0u8; 4 * 4 * 4]; // fully transparent media
        draw_bounds_frame(&mut buf, 4, 4, 1.0);
        assert!(
            buf.chunks_exact(4).all(|p| p[3] >= 35),
            "every pixel must have alpha > 0 for Windows hit-testing"
        );
        assert_eq!(&buf[0..4], &[255, 205, 96, 255], "accent border corner");
        let interior = (4 + 1) * 4;
        assert_eq!(buf[interior + 3], 35, "interior keeps the subtle backdrop alpha");
    }

    #[test]
    fn bounds_frame_preserves_opaque_media() {
        let mut buf = vec![0u8; 2 * 2 * 4];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&[10, 20, 30, 255]);
        }
        draw_bounds_frame(&mut buf, 2, 2, 1.0);
        // 2x2 with 1px border: every pixel is border -> opaque accent
        assert!(buf.chunks_exact(4).all(|p| p[3] == 255));
    }
}
