use std::ffi::c_void;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::slice;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, Frame, ImageDecoder};
use windows::core::{Interface, GUID, PCWSTR, PROPVARIANT};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Media::MediaFoundation::{
    MFCreateAttributes, MFCreateAudioRenderer, MFCreateMediaType, MFCreateSourceReaderFromURL,
    MFGetStrideForBitmapInfoHeader, MFMediaType_Video, MFVideoFormat_RGB32, MF_E_NOTACCEPTING,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM, IMF2DBuffer,
    IMFAttributes, IMFMediaBuffer, IMFMediaSink, IMFSample, IMFSourceReader, IMFStreamSink,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::overlay::WM_VIDEO_FRAME;

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const AUDIO_STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
const FLAG_EOS: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Gif,
    Image,
    Video,
}

pub fn kind_from_path(p: &Path) -> Option<Kind> {
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "gif" => Some(Kind::Gif),
        "png" | "jpg" | "jpeg" | "webp" => Some(Kind::Image),
        "mp4" => Some(Kind::Video),
        _ => None,
    }
}

fn mf_err(e: windows::core::Error) -> String {
    format!("Media Foundation error 0x{:08X}", e.code().0)
}

pub struct Media {
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    inner: Inner,
}

enum Inner {
    Gif(GifInner),
    Static(Vec<u8>),
    Video(VideoHandle),
}

struct GifInner {
    path: PathBuf,
    frames: image::Frames<'static>,
    frame: Vec<u8>,
    delay_ms: u32,
}

/// Latest-frame slot shared between the decode worker and the widget window.
struct VideoShared {
    state: Mutex<VideoState>,
    cv: Condvar,
    hwnd: AtomicIsize,
}

struct VideoState {
    frame: Arc<Vec<u8>>,
    seq: u64,
    paused: bool,
    muted: bool,
    stop: bool,
}

pub struct VideoHandle {
    shared: Arc<VideoShared>,
    worker: Option<std::thread::JoinHandle<()>>,
    interval_ms: u32,
    has_audio: bool,
}

impl Drop for VideoHandle {
    fn drop(&mut self) {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.stop = true;
        }
        self.shared.cv.notify_all();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

struct AudioOut {
    sink: IMFStreamSink,
    _parent: IMFMediaSink,
}

/// MF source readers are free-threaded (agile) COM objects, but windows 0.58
/// interfaces are !Send by default. Both creator and consumer threads are MTA,
/// so moving the reader into the decode worker is safe.
struct SendReader(IMFSourceReader);
unsafe impl Send for SendReader {}

impl Media {
    pub fn open(path: &Path) -> Result<Media, String> {
        let kind = kind_from_path(path).ok_or_else(|| "Unsupported format".to_string())?;
        match kind {
            Kind::Gif => {
                let (g, w, h) = open_gif(path)?;
                Ok(Media {
                    kind,
                    width: w,
                    height: h,
                    inner: Inner::Gif(g),
                })
            }
            Kind::Image => {
                let img = image::open(path).map_err(|e| format!("Image decode error: {e}"))?;
                let (w, h) = (img.width(), img.height());
                if w == 0 || h == 0 {
                    return Err("Zero-sized image".into());
                }
                let rgba = img.to_rgba8().into_raw();
                Ok(Media {
                    kind,
                    width: w,
                    height: h,
                    inner: Inner::Static(rgba),
                })
            }
            Kind::Video => {
                let (handle, w, h) = open_video(path)?;
                Ok(Media {
                    kind,
                    width: w,
                    height: h,
                    inner: Inner::Video(handle),
                })
            }
        }
    }

    /// Current frame for GIF / static image widgets. Video frames are delivered
    /// asynchronously via `video_frame_arc()` + WM_VIDEO_FRAME instead.
    pub fn frame(&self) -> &[u8] {
        match &self.inner {
            Inner::Gif(g) => &g.frame,
            Inner::Static(s) => s,
            Inner::Video(_) => &[],
        }
    }

    pub fn has_audio(&self) -> bool {
        match &self.inner {
            Inner::Video(v) => v.has_audio,
            _ => false,
        }
    }

    pub fn frame_interval_ms(&self) -> u32 {
        match &self.inner {
            Inner::Gif(g) => g.delay_ms,
            Inner::Video(v) => v.interval_ms,
            Inner::Static(_) => 100,
        }
    }

    /// Advance a GIF by one frame (loops forever). Returns the next delay in ms.
    pub fn advance_gif(&mut self) -> u32 {
        if let Inner::Gif(g) = &mut self.inner {
            match g.frames.next() {
                Some(Ok(f)) => {
                    g.delay_ms = gif_delay_ms(&f);
                    g.frame = f.into_buffer().into_raw();
                }
                _ => {
                    if let Ok((ng, _, _)) = open_gif(&g.path) {
                        *g = ng;
                    }
                }
            }
            g.delay_ms
        } else {
            100
        }
    }

    pub fn video_frame_arc(&self) -> Option<Arc<Vec<u8>>> {
        match &self.inner {
            Inner::Video(v) => Some(v.shared.state.lock().unwrap().frame.clone()),
            _ => None,
        }
    }

    pub fn set_video_hwnd(&self, hwnd: isize) {
        if let Inner::Video(v) = &self.inner {
            v.shared.hwnd.store(hwnd, Ordering::Relaxed);
        }
    }

    pub fn set_video_paused(&self, paused: bool) {
        if let Inner::Video(v) = &self.inner {
            v.shared.state.lock().unwrap().paused = paused;
            v.shared.cv.notify_all();
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        if let Inner::Video(v) = &self.inner {
            v.shared.state.lock().unwrap().muted = muted;
            v.shared.cv.notify_all();
        }
    }
}

fn gif_delay_ms(f: &Frame) -> u32 {
    let (n, d) = f.delay().numer_denom_ms();
    let ms = n as f64 / d.max(1) as f64;
    if ms <= 10.0 {
        100 // browser-style clamp for ultra-fast GIF frames
    } else {
        (ms as u32).max(20)
    }
}

fn open_gif(path: &Path) -> Result<(GifInner, u32, u32), String> {
    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let dec = GifDecoder::new(BufReader::new(file)).map_err(|e| format!("GIF error: {e}"))?;
    let (w, h) = dec.dimensions();
    let mut frames = dec.into_frames();
    let first = frames
        .next()
        .ok_or_else(|| "GIF has no frames".to_string())?
        .map_err(|e| format!("GIF frame error: {e}"))?;
    let delay_ms = gif_delay_ms(&first);
    Ok((
        GifInner {
            path: path.to_path_buf(),
            frames,
            frame: first.into_buffer().into_raw(),
            delay_ms,
        },
        w,
        h,
    ))
}

fn open_video(path: &Path) -> Result<(VideoHandle, u32, u32), String> {
    unsafe {
        let url = crate::win32_utils::wide(path.to_str().ok_or("Invalid path")?);
        let mut attrs_opt: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_opt, 2).map_err(mf_err)?;
        let attrs = attrs_opt.ok_or("MFCreateAttributes failed")?;
        // CPU-accessible RGB32 output via the MF video processing pipeline.
        // No DXGI device manager: GPU download stalls were freezing widgets on
        // some drivers; the software path is universally reliable.
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(mf_err)?;

        let reader = MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), &attrs).map_err(mf_err)?;

        let mt = MFCreateMediaType().map_err(mf_err)?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mf_err)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(mf_err)?;
        reader
            .SetCurrentMediaType(VIDEO_STREAM, None, &mt)
            .map_err(|_| "No decodable video stream (codec may be unsupported)".to_string())?;
        let out_type = reader.GetCurrentMediaType(VIDEO_STREAM).map_err(mf_err)?;

        // MF_MT_FRAME_SIZE / MF_MT_FRAME_RATE are packed UINT64 attributes.
        let fs = out_type.GetUINT64(&MF_MT_FRAME_SIZE).map_err(mf_err)?;
        let w = (fs >> 32) as u32;
        let h = (fs & 0xFFFF_FFFF) as u32;
        if w == 0 || h == 0 {
            return Err("Invalid video dimensions".into());
        }
        let (num, den) = match out_type.GetUINT64(&MF_MT_FRAME_RATE) {
            Ok(fr) => ((fr >> 32) as u32, (fr & 0xFFFF_FFFF) as u32),
            Err(_) => (30, 1), // default ~30 FPS when unknown
        };
        let interval_ms = if num > 0 {
            ((den as u64 * 1000) / num as u64).clamp(1, 1000) as u32
        } else {
            33
        };
        let stride = match out_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
            Ok(v) => v as i32,
            Err(_) => MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, w)
                .unwrap_or(w as i32 * 4),
        };

        let has_audio = reader.GetNativeMediaType(AUDIO_STREAM, 0).is_ok();
        if has_audio {
            let _ = reader.SetStreamSelection(AUDIO_STREAM, BOOL(0));
        }

        let shared = Arc::new(VideoShared {
            state: Mutex::new(VideoState {
                frame: Arc::new(vec![0u8; (w * h * 4) as usize]),
                seq: 0,
                paused: false,
                muted: true,
                stop: false,
            }),
            cv: Condvar::new(),
            hwnd: AtomicIsize::new(0),
        });

        let worker_shared = shared.clone();
        let send_reader = SendReader(reader);
        let worker = std::thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || {
                video_worker(worker_shared, send_reader, w, h, stride, interval_ms, has_audio)
            })
            .map_err(|e| format!("Cannot spawn decode thread: {e}"))?;

        // Wait (bounded) for the first decoded frame so the widget shows
        // content immediately instead of a black rectangle.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut st = shared.state.lock().unwrap();
        while st.seq == 0 && !st.stop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = shared.cv.wait_timeout(st, deadline - now).unwrap().0;
        }
        drop(st);

        Ok((
            VideoHandle {
                shared,
                worker: Some(worker),
                interval_ms,
                has_audio,
            },
            w,
            h,
        ))
    }
}

/// Dedicated decode thread: never blocks the widget's Win32 message loop.
/// Paces frames against the presentation clock, pushes the latest RGBA frame
/// into the shared slot and notifies the window with WM_VIDEO_FRAME.
fn video_worker(
    shared: Arc<VideoShared>,
    reader: SendReader,
    w: u32,
    h: u32,
    stride: i32,
    interval_ms: u32,
    has_audio: bool,
) {
    let reader = reader.0;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut freq);
        let freq = freq.max(1);
        let mut start = 0i64;
        let _ = QueryPerformanceCounter(&mut start);
        let mut frame_time: i64 = 0;
        let interval_100ns = interval_ms as i64 * 10_000;

        let mut audio: Option<AudioOut> = None;
        let mut audio_pending: Option<IMFSample> = None;
        let mut audio_enabled = false;
        let mut was_paused = false;

        let mut st = shared.state.lock().unwrap();
        loop {
            if st.stop {
                break;
            }
            if st.paused {
                was_paused = true;
                st = shared
                    .cv
                    .wait_timeout(st, Duration::from_millis(100))
                    .unwrap()
                    .0;
                continue;
            }
            if was_paused {
                was_paused = false;
                let mut n = 0i64;
                let _ = QueryPerformanceCounter(&mut n);
                start = n - frame_time * freq / 10_000_000;
            }

            let muted = st.muted;
            if has_audio && muted != audio_enabled {
                if muted {
                    let _ = reader.SetStreamSelection(AUDIO_STREAM, BOOL(0));
                    if let Some(a) = &audio {
                        let _ = a.sink.Flush();
                    }
                    audio_pending = None;
                    audio_enabled = false;
                } else {
                    let _ = reader.SetStreamSelection(AUDIO_STREAM, BOOL(1));
                    if audio.is_none() {
                        if let Ok(parent) = MFCreateAudioRenderer(None) {
                            if let Ok(sink) = parent.GetStreamSinkByIndex(0) {
                                if let Ok(mt) = reader.GetCurrentMediaType(AUDIO_STREAM) {
                                    if let Ok(handler) = sink.GetMediaTypeHandler() {
                                        if handler.SetCurrentMediaType(&mt).is_ok() {
                                            audio = Some(AudioOut {
                                                sink,
                                                _parent: parent,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    audio_enabled = audio.is_some();
                    if !audio_enabled {
                        let _ = reader.SetStreamSelection(AUDIO_STREAM, BOOL(0));
                    }
                }
            }
            // Never hold the state lock across blocking MF reads.
            drop(st);

            let mut n = 0i64;
            let _ = QueryPerformanceCounter(&mut n);
            let elapsed = (n - start) * 10_000_000 / freq;

            let mut new_frame: Option<Arc<Vec<u8>>> = None;
            for _ in 0..8 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample: Option<IMFSample> = None;
                if reader
                    .ReadSample(
                        VIDEO_STREAM,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut ts),
                        Some(&mut sample),
                    )
                    .is_err()
                {
                    break;
                }
                if flags & FLAG_EOS != 0 {
                    // Seamless infinite loop: seek to 0 and immediately pull
                    // the first frame of the new pass — no freeze on the last
                    // frame, no waiting for the next tick.
                    let pv = PROPVARIANT::from(0i64);
                    let _ = reader.SetCurrentPosition(&GUID::zeroed(), &pv);
                    if let Some(a) = &audio {
                        let _ = a.sink.Flush();
                    }
                    audio_pending = None;
                    frame_time = 0;
                    let _ = QueryPerformanceCounter(&mut start);

                    let mut f2 = 0u32;
                    let mut t2 = 0i64;
                    let mut s2: Option<IMFSample> = None;
                    if reader
                        .ReadSample(
                            VIDEO_STREAM,
                            0,
                            None,
                            Some(&mut f2),
                            Some(&mut t2),
                            Some(&mut s2),
                        )
                        .is_ok()
                    {
                        if let Some(s) = s2 {
                            let mut buf = vec![0u8; (w * h * 4) as usize];
                            if copy_sample(&s, &mut buf, w, h, stride).is_ok() {
                                frame_time = t2;
                                new_frame = Some(Arc::new(buf));
                            }
                        }
                    }
                    break;
                }
                let Some(s) = sample else { break };
                let mut buf = vec![0u8; (w * h * 4) as usize];
                if copy_sample(&s, &mut buf, w, h, stride).is_ok() {
                    frame_time = ts;
                    new_frame = Some(Arc::new(buf));
                }
                if ts > elapsed {
                    break;
                }
            }

            if audio_enabled {
                if let Some(a) = &audio {
                    for _ in 0..16 {
                        let s = match audio_pending.take() {
                            Some(s) => s,
                            None => {
                                let mut flags = 0u32;
                                let mut ts = 0i64;
                                let mut sample: Option<IMFSample> = None;
                                if reader
                                    .ReadSample(
                                        AUDIO_STREAM,
                                        0,
                                        None,
                                        Some(&mut flags),
                                        Some(&mut ts),
                                        Some(&mut sample),
                                    )
                                    .is_err()
                                {
                                    break;
                                }
                                if flags & FLAG_EOS != 0 {
                                    break;
                                }
                                match sample {
                                    Some(s) => s,
                                    None => break,
                                }
                            }
                        };
                        let t = s.GetSampleTime().unwrap_or(0);
                        if t > elapsed + 1_000_000 {
                            audio_pending = Some(s);
                            break;
                        }
                        match a.sink.ProcessSample(&s) {
                            Ok(_) => {}
                            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                                audio_pending = Some(s);
                                break;
                            }
                            Err(_) => {
                                audio_enabled = false;
                                break;
                            }
                        }
                    }
                }
            }

            let mut st2 = shared.state.lock().unwrap();
            if let Some(f) = new_frame {
                st2.frame = f;
                st2.seq += 1;
                let hwnd = shared.hwnd.load(Ordering::Relaxed);
                if hwnd != 0 {
                    let _ = PostMessageW(
                        HWND(hwnd as *mut c_void),
                        WM_VIDEO_FRAME,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            if st2.stop {
                break;
            }
            // Sleep until the next frame's presentation deadline; the condvar
            // wakes immediately on pause/mute/stop requests.
            let mut nn = 0i64;
            let _ = QueryPerformanceCounter(&mut nn);
            let deadline = start + (frame_time + interval_100ns) * freq / 10_000_000;
            let sleep_ms = ((deadline - nn) * 1000 / freq).clamp(1, 100) as u64;
            st = shared
                .cv
                .wait_timeout(st2, Duration::from_millis(sleep_ms))
                .unwrap()
                .0;
        }

        drop(audio);
        drop(reader);
        CoUninitialize();
    }
}

/// Row offset for image row `y` given the buffer's real stride sign.
/// Negative stride = bottom-up buffer (first row in memory is the image's
/// bottom row), so rows must be flipped to match our top-down RGBA target.
fn row_offset(y: u32, h: u32, stride: i32) -> usize {
    let step = stride.unsigned_abs() as usize;
    if stride >= 0 {
        y as usize * step
    } else {
        (h as usize - 1 - y as usize) * step
    }
}

/// Copy BGRA rows into top-down RGBA, forcing alpha to 255 (MF RGB32 leaves
/// the 4th byte 0x00, which would be invisible under ULW_ALPHA).
unsafe fn copy_rows(base: *mut u8, stride: i32, dst: &mut [u8], w: u32, h: u32) {
    let row_bytes = (w * 4) as usize;
    for y in 0..h {
        let row = slice::from_raw_parts(base.add(row_offset(y, h, stride)), row_bytes);
        let d = &mut dst[y as usize * row_bytes..(y as usize + 1) * row_bytes];
        for x in 0..w as usize {
            d[x * 4] = row[x * 4 + 2];
            d[x * 4 + 1] = row[x * 4 + 1];
            d[x * 4 + 2] = row[x * 4];
            d[x * 4 + 3] = 255;
        }
    }
}

/// Copy a decoded RGB32 sample into a top-down RGBA buffer. Prefers
/// IMF2DBuffer::Lock2D (the buffer's REAL pitch/orientation, which can differ
/// from the media type's MF_MT_DEFAULT_STRIDE); falls back to Lock + stride.
unsafe fn copy_sample(
    sample: &IMFSample,
    dst: &mut [u8],
    w: u32,
    h: u32,
    fallback_stride: i32,
) -> Result<(), String> {
    let buf: IMFMediaBuffer = sample.ConvertToContiguousBuffer().map_err(mf_err)?;
    if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
        let mut scanline0: *mut u8 = null_mut();
        let mut pitch = 0i32;
        if buf2d.Lock2D(&mut scanline0, &mut pitch).is_ok() && !scanline0.is_null() {
            copy_rows(scanline0, pitch, dst, w, h);
            let _ = buf2d.Unlock2D();
            return Ok(());
        }
    }
    let mut ptr: *mut u8 = null_mut();
    let mut len = 0u32;
    buf.Lock(&mut ptr, None, Some(&mut len)).map_err(mf_err)?;
    let need = fallback_stride.unsigned_abs() as usize * h as usize;
    if ptr.is_null() || (len as usize) < need {
        let _ = buf.Unlock();
        return Err("Short sample buffer".into());
    }
    copy_rows(ptr, fallback_stride, dst, w, h);
    buf.Unlock().map_err(mf_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_offset_orientation() {
        // top-down: rows in natural order
        assert_eq!(row_offset(0, 4, 16), 0);
        assert_eq!(row_offset(3, 4, 16), 48);
        // bottom-up (negative stride): image top row lives at the buffer end
        assert_eq!(row_offset(0, 4, -16), 48);
        assert_eq!(row_offset(3, 4, -16), 0);
    }

    #[test]
    fn bottom_up_video_rows_are_flipped_and_alpha_forced() {
        // 2x2 BGRA, bottom-up: memory = [bottom row][top row], alpha bytes 0.
        let mem: Vec<u8> = vec![
            1, 1, 1, 0, 2, 2, 2, 0, // bottom row (image y=1)
            3, 3, 3, 0, 4, 4, 4, 0, // top row (image y=0)
        ];
        let mut dst = vec![0u8; 16];
        unsafe {
            copy_rows(mem.as_ptr() as *mut u8, -8, &mut dst, 2, 2);
        }
        assert_eq!(
            &dst[0..8],
            &[3, 3, 3, 255, 4, 4, 4, 255],
            "top row first, alpha 255"
        );
        assert_eq!(&dst[8..16], &[1, 1, 1, 255, 2, 2, 2, 255]);
    }

    #[test]
    fn top_down_video_rows_kept_and_alpha_forced() {
        let mem: Vec<u8> = vec![
            3, 3, 3, 0, 4, 4, 4, 0, // top row (image y=0)
            1, 1, 1, 0, 2, 2, 2, 0, // bottom row (image y=1)
        ];
        let mut dst = vec![0u8; 16];
        unsafe {
            copy_rows(mem.as_ptr() as *mut u8, 8, &mut dst, 2, 2);
        }
        assert_eq!(&dst[0..8], &[3, 3, 3, 255, 4, 4, 4, 255]);
        assert_eq!(&dst[8..16], &[1, 1, 1, 255, 2, 2, 2, 255]);
    }
}
