use std::path::Path;

use gif_pin::media::{kind_from_path, Kind, Media};

fn test_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("gif_pin_tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn kind_detection() {
    assert_eq!(kind_from_path(Path::new("a.gif")), Some(Kind::Gif));
    assert_eq!(kind_from_path(Path::new("a.GIF")), Some(Kind::Gif));
    assert_eq!(kind_from_path(Path::new("a.mp4")), Some(Kind::Video));
    assert_eq!(kind_from_path(Path::new("a.png")), Some(Kind::Image));
    assert_eq!(kind_from_path(Path::new("a.jpg")), Some(Kind::Image));
    assert_eq!(kind_from_path(Path::new("a.jpeg")), Some(Kind::Image));
    assert_eq!(kind_from_path(Path::new("a.webp")), Some(Kind::Image));
    assert_eq!(kind_from_path(Path::new("a.txt")), None);
    assert_eq!(kind_from_path(Path::new("noext")), None);
}

#[test]
fn unsupported_format_errors() {
    let path = test_dir().join("bad.txt");
    std::fs::write(&path, b"not media").unwrap();
    assert!(Media::open(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn gif_decodes_and_loops() {
    let path = test_dir().join("anim.gif");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = image::codecs::gif::GifEncoder::new(file);
        let mut a = image::RgbaImage::new(8, 8);
        for p in a.pixels_mut() {
            *p = image::Rgba([255, 0, 0, 255]);
        }
        let mut b = image::RgbaImage::new(8, 8);
        for p in b.pixels_mut() {
            *p = image::Rgba([0, 0, 255, 255]);
        }
        let delay = image::Delay::from_numer_denom_ms(40, 1);
        enc.encode_frame(image::Frame::from_parts(a, 0, 0, delay))
            .unwrap();
        enc.encode_frame(image::Frame::from_parts(b, 0, 0, delay))
            .unwrap();
    }

    let mut m = Media::open(&path).unwrap();
    assert_eq!(m.kind, Kind::Gif);
    assert_eq!((m.width, m.height), (8, 8));
    assert_eq!(m.frame().len(), 8 * 8 * 4);
    assert!(!m.has_audio());

    let first = m.frame().to_vec();
    let delay = m.advance_gif();
    assert!(delay >= 20, "delay clamped to a sane minimum");
    let second = m.frame().to_vec();
    assert_ne!(first, second, "frames must differ");

    // Advance well past the end: the decoder must loop, not fail.
    for _ in 0..9 {
        m.advance_gif();
    }
    assert_eq!(m.frame().len(), 8 * 8 * 4);

    std::fs::remove_file(&path).ok();
}

#[test]
fn png_alpha_preserved() {
    let path = test_dir().join("alpha.png");
    let mut img = image::RgbaImage::new(4, 4);
    img.put_pixel(0, 0, image::Rgba([10, 20, 30, 0])); // fully transparent
    img.put_pixel(1, 0, image::Rgba([200, 100, 50, 128])); // semi-transparent
    img.put_pixel(2, 0, image::Rgba([1, 2, 3, 255])); // opaque
    image::save_buffer(&path, img.as_raw(), 4, 4, image::ExtendedColorType::Rgba8).unwrap();

    let m = Media::open(&path).unwrap();
    assert_eq!(m.kind, Kind::Image);
    assert_eq!((m.width, m.height), (4, 4));
    assert_eq!(&m.frame()[0..4], &[10, 20, 30, 0]);
    assert_eq!(&m.frame()[4..8], &[200, 100, 50, 128]);
    assert_eq!(&m.frame()[8..12], &[1, 2, 3, 255]);

    std::fs::remove_file(&path).ok();
}

#[test]
fn premultiply_and_scaling() {
    // straight RGBA -> premultiplied BGRA
    let src = [200u8, 100, 50, 128, 10, 20, 30, 0, 255, 255, 255, 255];
    let mut dst = [0u8; 12];
    gif_pin::win32_utils::rgba_to_premul_bgra(&src, &mut dst);
    assert_eq!(&dst[0..4], &[25, 50, 100, 128]); // (c*128+127)/255, BGR order
    assert_eq!(&dst[4..8], &[0, 0, 0, 0]);
    assert_eq!(&dst[8..12], &[255, 255, 255, 255]);

    // 2x2 -> 4x4 upscale keeps alpha semantics
    let src2 = vec![255u8, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 128, 255, 255, 255, 255];
    let mut dst2 = vec![0u8; 4 * 4 * 4];
    gif_pin::win32_utils::blit_frame(&src2, 2, 2, &mut dst2, 4, 4);
    assert!(dst2.chunks_exact(4).any(|p| p[3] == 0), "transparent area survives");
    assert!(dst2.chunks_exact(4).any(|p| p[3] == 255), "opaque area survives");
}

#[test]
fn mp4_missing_file_errors() {
    unsafe {
        windows::Win32::Media::MediaFoundation::MFStartup(
            windows::Win32::Media::MediaFoundation::MF_VERSION,
            0,
        )
        .unwrap();
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let r = Media::open(Path::new("Z:\\gif_pin_definitely_missing_12345.mp4"));
    assert!(r.is_err());
}
