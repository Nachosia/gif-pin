use gif_pin::overlay::compute_resize;

/// The opposite (anchor) corner must stay 100% stationary for every corner drag.
#[test]
fn anchor_corner_never_moves() {
    let (orig_w, orig_h) = (200u32, 100u32);
    let aspect = orig_w as f32 / orig_h as f32;
    // (corner, anchor, expected fixed-corner expression)
    let cases = [
        (0u8, 500i32, 400i32), // dragging TL, anchor = bottom-right
        (1, 100, 400),         // dragging TR, anchor = bottom-left
        (2, 500, 100),         // dragging BL, anchor = top-right
        (3, 100, 100),         // dragging BR, anchor = top-left
    ];
    for (corner, ax, ay) in cases {
        for (cx, cy) in [(300, 250), (700, 600), (150, 120), (ax + 1, ay + 1)] {
            let (x, y, w, h) =
                compute_resize(corner, ax, ay, cx, cy, orig_w, orig_h, aspect, false);
            let fixed_x = if corner == 0 || corner == 2 { x + w } else { x };
            let fixed_y = if corner == 0 || corner == 1 { y + h } else { y };
            assert_eq!((fixed_x, fixed_y), (ax, ay), "corner {corner} cursor ({cx},{cy})");
            assert!(w >= 32 && h >= 32, "minimum size enforced");
        }
    }
}

#[test]
fn aspect_ratio_locked_by_default() {
    let (orig_w, orig_h) = (300u32, 150u32);
    let aspect = 2.0f32;
    for (cx, cy) in [(900, 200), (200, 900), (600, 600), (301, 151)] {
        let (_, _, w, h) = compute_resize(3, 0, 0, cx, cy, orig_w, orig_h, aspect, false);
        let ratio = w as f32 / h as f32;
        assert!(
            (ratio - aspect).abs() < 0.05,
            "cursor ({cx},{cy}) gave {w}x{h} (ratio {ratio})"
        );
    }
}

#[test]
fn shift_allows_free_stretch() {
    let (_, _, w, h) = compute_resize(3, 0, 0, 500, 137, 300, 150, 2.0, true);
    assert_eq!((w, h), (500, 137));
}

#[test]
fn collapse_clamps_to_minimum() {
    // Cursor right on the anchor: must not invert or collapse to zero.
    let (x, y, w, h) = compute_resize(0, 500, 400, 500, 400, 200, 100, 2.0, false);
    assert_eq!((w, h), (32, 32));
    assert_eq!((x + w, y + h), (500, 400));

    let (x, y, w, h) = compute_resize(3, 100, 100, 100, 100, 200, 100, 2.0, true);
    assert_eq!((w, h), (32, 32));
    assert_eq!((x, y), (100, 100));
}

#[test]
fn larger_axis_drives_proportional_resize() {
    // Wide drag on a 2:1 media: width dominates, height follows the ratio.
    let (_, _, w, h) = compute_resize(3, 0, 0, 800, 100, 200, 100, 2.0, false);
    assert_eq!((w, h), (800, 400));
    // Tall drag: height dominates.
    let (_, _, w, h) = compute_resize(3, 0, 0, 100, 800, 200, 100, 2.0, false);
    assert_eq!((w, h), (1600, 800));
}
