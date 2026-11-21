use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::slice;

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, Frame, ImageDecoder};
use windows::core::{GUID, PCWSTR, PROPVARIANT};
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Media::MediaFoundation::{
    MFCreateAttributes, MFCreateAudioRenderer, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFGetStrideForBitmapInfoHeader, MFMediaType_Video,
    MFVideoFormat_RGB32, MF_E_NOTACCEPTING, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM, IMFAttributes,
    IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaSink, IMFSample, IMFSourceReader, IMFStreamSink,
};

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
    Video(VideoInner),
}

struct GifInner {
    path: PathBuf,
    frames: image::Frames<'static>,
    frame: Vec<u8>,
    delay_ms: u32,
}

struct VideoInner {
    reader: IMFSourceReader,
    // Keep the D3D device + DXGI manager alive: the source reader depends on them.
    _d3d_device: Option<ID3D11Device>,
    _dxgi_manager: Option<IMFDXGIDeviceManager>,
    frame: Vec<u8>,
    stride: i32,
    interval_ms: u32,
    frame_time: i64,
    has_audio: bool,
    audio: Option<AudioOut>,
    muted: bool,
}

struct AudioOut {
    sink: IMFStreamSink,
    _parent: IMFMediaSink,
    pending: Option<IMFSample>,
    active: bool,
}

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
                let (v, w, h) = open_video(path)?;
                let mut m = Media {
                    kind,
                    width: w,
                    height: h,
                    inner: Inner::Video(v),
                };
                m.video_tick(-1); // decode the first frame
                Ok(m)
            }
        }
    }

    pub fn frame(&self) -> &[u8] {
        match &self.inner {
            Inner::Gif(g) => &g.frame,
            Inner::Static(s) => s,
            Inner::Video(v) => &v.frame,
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

    pub fn video_time(&self) -> i64 {
        match &self.inner {
            Inner::Video(v) => v.frame_time,
            _ => 0,
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

    /// Read video (and audio) samples due at `elapsed` (100ns units since play start).
    /// Returns (new_video_frame, looped).
    pub fn video_tick(&mut self, elapsed: i64) -> (bool, bool) {
        let Inner::Video(v) = &mut self.inner else {
            return (false, false);
        };
        let mut changed = false;
        let mut looped = false;
        unsafe {
            for _ in 0..8 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample: Option<IMFSample> = None;
                if v
                    .reader
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
                    let pv = PROPVARIANT::from(0i64);
                    let _ = v.reader.SetCurrentPosition(&GUID::zeroed(), &pv);
                    if let Some(a) = &v.audio {
                        let _ = a.sink.Flush();
                    }
                    v.frame_time = 0;
                    looped = true;
                    break;
                }
                let Some(s) = sample else { break };
                if copy_sample(&s, &mut v.frame, self.width, self.height, v.stride).is_ok() {
                    v.frame_time = ts;
                    changed = true;
                }
                if ts > elapsed {
                    break;
                }
            }

            if !v.muted {
                if let Some(a) = &mut v.audio {
                    if a.active {
                        for _ in 0..16 {
                            let s = match a.pending.take() {
                                Some(s) => s,
                                None => {
                                    let mut flags = 0u32;
                                    let mut ts = 0i64;
                                    let mut sample: Option<IMFSample> = None;
                                    if v
                                        .reader
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
                                // keep a small lookahead buffer, feed the rest later
                                a.pending = Some(s);
                                break;
                            }
                            match a.sink.ProcessSample(&s) {
                                Ok(_) => {}
                                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                                    a.pending = Some(s);
                                    break;
                                }
                                Err(_) => {
                                    a.active = false;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        (changed, looped)
    }

    pub fn set_muted(&mut self, muted: bool) {
        let Inner::Video(v) = &mut self.inner else {
            return;
        };
        v.muted = muted;
        if !v.has_audio {
            return;
        }
        unsafe {
            if muted {
                let _ = v.reader.SetStreamSelection(AUDIO_STREAM, BOOL(0));
                if let Some(a) = &mut v.audio {
                    a.active = false;
                    a.pending = None;
                    let _ = a.sink.Flush();
                }
            } else {
                let _ = v.reader.SetStreamSelection(AUDIO_STREAM, BOOL(1));
                if v.audio.is_none() {
                    if let Ok(parent) = MFCreateAudioRenderer(None) {
                        if let Ok(sink) = parent.GetStreamSinkByIndex(0) {
                            if let Ok(mt) = v.reader.GetCurrentMediaType(AUDIO_STREAM) {
                                if let Ok(handler) = sink.GetMediaTypeHandler() {
                                    if handler.SetCurrentMediaType(&mt).is_ok() {
                                        v.audio = Some(AudioOut {
                                            sink,
                                            _parent: parent,
                                            pending: None,
                                            active: true,
                                        });
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(a) = &mut v.audio {
                    a.active = true;
                }
            }
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

fn open_video(path: &Path) -> Result<(VideoInner, u32, u32), String> {
    unsafe {
        let url = crate::win32_utils::wide(path.to_str().ok_or("Invalid path")?);
        let mut attrs_opt: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_opt, 4).map_err(mf_err)?;
        let attrs = attrs_opt.ok_or("MFCreateAttributes failed")?;

        // Hardware-accelerated decoding via a D3D11 DXGI device manager.
        let mut device: Option<ID3D11Device> = None;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        let levels = [D3D_FEATURE_LEVEL_11_0];
        let hr = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        );
        if hr.is_ok() {
            if let Some(dev) = &device {
                let mut token = 0u32;
                if MFCreateDXGIDeviceManager(&mut token, &mut manager).is_ok() {
                    if let Some(mgr) = &manager {
                        let _ = mgr.ResetDevice(dev, token);
                        let _ = attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, mgr);
                        let _ = attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1);
                    }
                }
            }
        }
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
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
            Err(_) => (30, 1),
        };
        let interval_ms = if num > 0 {
            ((den as u64 * 1000) / num as u64).max(1) as u32
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

        Ok((
            VideoInner {
                reader,
                _d3d_device: device,
                _dxgi_manager: manager,
                frame: vec![0u8; (w * h * 4) as usize],
                stride,
                interval_ms,
                frame_time: 0,
                has_audio,
                audio: None,
                muted: true,
            },
            w,
            h,
        ))
    }
}

/// Copy an RGB32 sample into a top-down RGBA buffer (handles bottom-up strides).
unsafe fn copy_sample(
    sample: &IMFSample,
    dst: &mut [u8],
    w: u32,
    h: u32,
    stride: i32,
) -> Result<(), String> {
    let buf: IMFMediaBuffer = sample.ConvertToContiguousBuffer().map_err(mf_err)?;
    let mut ptr: *mut u8 = null_mut();
    let mut len = 0u32;
    buf.Lock(&mut ptr, None, Some(&mut len)).map_err(mf_err)?;
    let row_bytes = (w * 4) as usize;
    let astride = stride.unsigned_abs() as usize;
    if ptr.is_null() || (len as usize) < astride * h as usize {
        let _ = buf.Unlock();
        return Err("Short sample buffer".into());
    }
    for y in 0..h as usize {
        let off = if stride >= 0 {
            y * astride
        } else {
            (h as usize - 1 - y) * astride
        };
        let row = slice::from_raw_parts(ptr.add(off), row_bytes);
        let d = &mut dst[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..w as usize {
            d[x * 4] = row[x * 4 + 2];
            d[x * 4 + 1] = row[x * 4 + 1];
            d[x * 4 + 2] = row[x * 4];
            d[x * 4 + 3] = 255;
        }
    }
    buf.Unlock().map_err(mf_err)?;
    Ok(())
}
