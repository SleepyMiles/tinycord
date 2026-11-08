//! Screen pixels (BGRA) -> what the H.264 encoder takes (I420), scaled to the
//! chosen resolution, with the mouse pointer drawn in.

/// Output size for a `sw`x`sh` screen limited to `max_h` lines (0 = keep the
/// screen's size). Keeps the aspect ratio; both sides even, as I420 needs.
pub fn target_size(sw: usize, sh: usize, max_h: usize) -> (usize, usize) {
    let (w, h) = if max_h == 0 || sh <= max_h {
        (sw, sh)
    } else {
        ((sw * max_h + sh / 2) / sh, max_h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// Average 2x2 blocks of a BGRA image (for shrinking by 2x or more).
fn halve(src: &[u8], w: usize, h: usize, stride: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        let r0 = &src[(2 * y).min(h - 1) * stride..];
        let r1 = &src[(2 * y + 1).min(h - 1) * stride..];
        let o = &mut out[y * nw * 4..(y + 1) * nw * 4];
        for x in 0..nw {
            let (a, b) = (8 * x, (8 * x + 4).min((w - 1) * 4));
            for c in 0..4 {
                o[x * 4 + c] =
                    ((r0[a + c] as u32 + r0[b + c] as u32 + r1[a + c] as u32 + r1[b + c] as u32 + 2) >> 2) as u8;
            }
        }
    }
    (out, nw, nh)
}

/// Bilinear resize of BGRA to exactly `dw`x`dh` (tightly packed output).
pub fn scale_bgra(src: &[u8], sw: usize, sh: usize, stride: usize, dw: usize, dh: usize, out: &mut Vec<u8>) {
    out.resize(dw * dh * 4, 0);
    let mut mip: Option<(Vec<u8>, usize, usize)> = None;
    loop {
        let next = {
            let (s, w, h, st) = match &mip {
                Some((b, w, h)) => (&b[..], *w, *h, *w * 4),
                None => (src, sw, sh, stride),
            };
            (w >= dw * 2 && h >= dh * 2).then(|| halve(s, w, h, st))
        };
        match next {
            Some(n) => mip = Some(n),
            None => break,
        }
    }
    let (s, w, h, st) = match &mip {
        Some((b, w, h)) => (&b[..], *w, *h, *w * 4),
        None => (src, sw, sh, stride),
    };
    let pos = |i: usize, sn: usize, dn: usize| -> (usize, usize, u32) {
        let p = ((((i as i64 * 2 + 1) * sn as i64 * 256) / (dn as i64 * 2)) - 128).clamp(0, ((sn - 1) as i64) << 8);
        let a = (p >> 8) as usize;
        (a, (a + 1).min(sn - 1), (p & 0xff) as u32)
    };
    let cols: Vec<(usize, usize, u32)> = (0..dw).map(|x| pos(x, w, dw)).collect();
    let px = |row: &[u8], i: usize| u32::from_le_bytes([row[i * 4], row[i * 4 + 1], row[i * 4 + 2], row[i * 4 + 3]]);
    for y in 0..dh {
        let (y0, y1, fy) = pos(y, h, dh);
        let r0 = &s[y0 * st..];
        let r1 = &s[y1 * st..];
        let o = &mut out[y * dw * 4..(y + 1) * dw * 4];
        for (x, &(x0, x1, fx)) in cols.iter().enumerate() {
            let top = lerp(px(r0, x0), px(r0, x1), fx);
            let bot = lerp(px(r1, x0), px(r1, x1), fx);
            o[x * 4..x * 4 + 4].copy_from_slice(&lerp(top, bot, fy).to_le_bytes());
        }
    }
}

/// Blend two BGRA pixels, all four channels at once: `f` = 0..256 of `b`.
#[inline(always)]
fn lerp(a: u32, b: u32, f: u32) -> u32 {
    let g = 256 - f;
    let rb = (((a & 0x00FF_00FF) * g + (b & 0x00FF_00FF) * f) >> 8) & 0x00FF_00FF;
    let ag = (((a >> 8) & 0x00FF_00FF) * g + ((b >> 8) & 0x00FF_00FF) * f) & 0xFF00_FF00;
    rb | ag
}

/// An I420 picture for the encoder: Y plane, then U and V at half size.
pub struct I420 {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

/// BGRA -> I420 using BT.709 limited range (what the SPS will say).
pub fn bgra_to_i420(src: &[u8], w: usize, h: usize, stride: usize, out: &mut I420) {
    out.w = w;
    out.h = h;
    let (cw, ch) = (w / 2, h / 2);
    out.data.resize(w * h + 2 * cw * ch, 0);
    let (yp, uv) = out.data.split_at_mut(w * h);
    let (up, vp) = uv.split_at_mut(cw * ch);
    for y in 0..h {
        let row = &src[y * stride..y * stride + w * 4];
        let yr = &mut yp[y * w..(y + 1) * w];
        for x in 0..w {
            let (b, g, r) = (row[x * 4] as i32, row[x * 4 + 1] as i32, row[x * 4 + 2] as i32);
            yr[x] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
        }
    }
    for cy in 0..ch {
        let r0 = &src[(2 * cy) * stride..];
        let r1 = &src[(2 * cy + 1) * stride..];
        for cx in 0..cw {
            let i = cx * 8;
            let sum = |c: usize| r0[i + c] as i32 + r0[i + 4 + c] as i32 + r1[i + c] as i32 + r1[i + 4 + c] as i32;
            let (b, g, r) = ((sum(0) + 2) >> 2, (sum(1) + 2) >> 2, (sum(2) + 2) >> 2);
            up[cy * cw + cx] = (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            vp[cy * cw + cx] = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

/// Mouse pointer image, as Windows' screen duplication reports it.
#[derive(Clone, Default)]
pub struct Pointer {
    /// 1 = monochrome (AND+XOR masks), 2 = colour (BGRA, alpha),
    /// 4 = masked colour.
    pub kind: u32,
    pub w: usize,
    /// For monochrome this is twice the visible height (two masks).
    pub h: usize,
    pub pitch: usize,
    pub data: Vec<u8>,
}

/// Draw `p` onto a BGRA image (`w`x`h`, packed) with its top-left at
/// screen position (`px`,`py`), where the image is the screen scaled by
/// `sx`,`sy` (image size / screen size).
pub fn draw_pointer(img: &mut [u8], w: usize, h: usize, p: &Pointer, px: i32, py: i32, sx: f32, sy: f32) {
    draw_pointer_at(img, w, h, 0, 0, p, px, py, sx, sy);
}

/// Where the pointer lands on the scaled picture: (x, y, width, height).
fn pointer_rect(p: &Pointer, px: i32, py: i32, sx: f32, sy: f32) -> (i32, i32, usize, usize) {
    let vis_h = if p.kind == 1 { p.h / 2 } else { p.h };
    // Draw the pointer at its scaled size, but never smaller than half size.
    let dw = ((p.w as f32 * sx).round() as usize).max(p.w / 2);
    let dh = ((vis_h as f32 * sy).round() as usize).max(vis_h / 2);
    ((px as f32 * sx).round() as i32, (py as f32 * sy).round() as i32, dw, dh)
}

/// `draw_pointer` onto a `w`x`h` patch whose top-left is (`x0`,`y0`) on the
/// scaled picture.
#[allow(clippy::too_many_arguments)]
fn draw_pointer_at(img: &mut [u8], w: usize, h: usize, x0: i32, y0: i32, p: &Pointer, px: i32, py: i32, sx: f32, sy: f32) {
    let vis_h = if p.kind == 1 { p.h / 2 } else { p.h };
    if p.w == 0 || vis_h == 0 || p.pitch == 0 {
        return;
    }
    let (ox, oy, dw, dh) = pointer_rect(p, px, py, sx, sy);
    let (ox, oy) = (ox - x0, oy - y0);
    let need = |row: usize, bytes: usize| row * p.pitch + bytes <= p.data.len();
    for dy in 0..dh as i32 {
        let ty = oy + dy;
        if ty < 0 || ty >= h as i32 {
            continue;
        }
        let syi = (dy as usize * vis_h) / dh;
        for dx in 0..dw as i32 {
            let tx = ox + dx;
            if tx < 0 || tx >= w as i32 {
                continue;
            }
            let sxi = (dx as usize * p.w) / dw;
            let ok = match p.kind {
                1 => need(syi + vis_h, sxi / 8 + 1),
                _ => need(syi, sxi * 4 + 4),
            };
            if !ok {
                continue;
            }
            let o = (ty as usize * w + tx as usize) * 4;
            match p.kind {
                1 => {
                    let bit = 0x80 >> (sxi % 8);
                    let and = p.data[syi * p.pitch + sxi / 8] & bit != 0;
                    let xor = p.data[(syi + vis_h) * p.pitch + sxi / 8] & bit != 0;
                    match (and, xor) {
                        (false, false) => img[o..o + 3].copy_from_slice(&[0, 0, 0]),
                        (false, true) => img[o..o + 3].copy_from_slice(&[255, 255, 255]),
                        (true, true) => {
                            for c in 0..3 {
                                img[o + c] = 255 - img[o + c];
                            }
                        }
                        (true, false) => {}
                    }
                }
                2 => {
                    let s = &p.data[syi * p.pitch + sxi * 4..syi * p.pitch + sxi * 4 + 4];
                    let a = s[3] as u32;
                    for c in 0..3 {
                        img[o + c] = ((s[c] as u32 * a + img[o + c] as u32 * (255 - a) + 127) / 255) as u8;
                    }
                }
                4 => {
                    let s = &p.data[syi * p.pitch + sxi * 4..syi * p.pitch + sxi * 4 + 4];
                    if s[3] == 0 {
                        img[o..o + 3].copy_from_slice(&s[..3]);
                    } else {
                        for c in 0..3 {
                            img[o + c] ^= s[c];
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Draw the pointer into an I420 frame. `src` is the same picture as BGRA
/// (`stride` bytes per row): the pixels under the pointer are taken from it,
/// the pointer is drawn on top, and only that small patch is converted.
#[allow(clippy::too_many_arguments)]
pub fn pointer_into_i420(src: &[u8], stride: usize, frame: &mut I420, p: &Pointer, px: i32, py: i32, sx: f32, sy: f32) {
    let (w, h) = (frame.w as i32, frame.h as i32);
    let (ox, oy, dw, dh) = pointer_rect(p, px, py, sx, sy);
    // Clip, then widen to even coordinates (chroma is per 2x2 block).
    let x0 = (ox.max(0) & !1).min(w);
    let y0 = (oy.max(0) & !1).min(h);
    let x1 = ((ox + dw as i32).min(w) + 1) & !1;
    let y1 = ((oy + dh as i32).min(h) + 1) & !1;
    let (x1, y1) = (x1.min(w), y1.min(h));
    if x1 <= x0 || y1 <= y0 || frame.data.len() < frame.w * frame.h * 3 / 2 {
        return;
    }
    let (pw, ph) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut patch = vec![0u8; pw * ph * 4];
    for r in 0..ph {
        let s = (y0 as usize + r) * stride + x0 as usize * 4;
        patch[r * pw * 4..(r + 1) * pw * 4].copy_from_slice(&src[s..s + pw * 4]);
    }
    draw_pointer_at(&mut patch, pw, ph, x0, y0, p, px, py, sx, sy);
    let mut small = I420 { w: 0, h: 0, data: Vec::new() };
    bgra_to_i420(&patch, pw, ph, pw * 4, &mut small);
    let (fw, fh) = (frame.w, frame.h);
    let (yp, uv) = frame.data.split_at_mut(fw * fh);
    let (up, vp) = uv.split_at_mut((fw / 2) * (fh / 2));
    let (sy_, suv) = small.data.split_at(pw * ph);
    let (su, sv) = suv.split_at((pw / 2) * (ph / 2));
    for r in 0..ph {
        let d = (y0 as usize + r) * fw + x0 as usize;
        yp[d..d + pw].copy_from_slice(&sy_[r * pw..(r + 1) * pw]);
    }
    let (cx, cy, cw) = (x0 as usize / 2, y0 as usize / 2, pw / 2);
    for r in 0..ph / 2 {
        let d = (cy + r) * (fw / 2) + cx;
        up[d..d + cw].copy_from_slice(&su[r * cw..(r + 1) * cw]);
        vp[d..d + cw].copy_from_slice(&sv[r * cw..(r + 1) * cw]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn screen(w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                // Left half red, right half blue; a white square in the middle.
                let white = x > w * 2 / 5 && x < w * 3 / 5 && y > h * 2 / 5 && y < h * 3 / 5;
                let (b, g, r) = if white { (255, 255, 255) } else if x < w / 2 { (0, 0, 255) } else { (255, 0, 0) };
                v[o..o + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        v
    }

    #[test]
    fn sizes() {
        assert_eq!(target_size(1920, 1080, 720), (1280, 720));
        assert_eq!(target_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(target_size(2560, 1440, 1080), (1920, 1080));
        assert_eq!(target_size(1366, 768, 480), (854, 480));
        assert_eq!(target_size(1280, 720, 1080), (1280, 720));
    }

    /// 1080p screen -> 720p I420 -> decode with OpenH264 round trip colour
    /// check through the same encoder settings the stream uses.
    #[test]
    fn convert_and_colours() {
        let (sw, sh) = (1920, 1080);
        let src = screen(sw, sh);
        let (dw, dh) = target_size(sw, sh, 720);
        let mut scaled = Vec::new();
        let mut pic = I420 { w: 0, h: 0, data: Vec::new() };
        let t = Instant::now();
        for _ in 0..5 {
            scale_bgra(&src, sw, sh, sw * 4, dw, dh, &mut scaled);
            bgra_to_i420(&scaled, dw, dh, dw * 4, &mut pic);
        }
        let per = t.elapsed() / 5;
        let y_at = |x: usize, y: usize| pic.data[y * dw + x];
        let u_at = |x: usize, y: usize| pic.data[dw * dh + (y / 2) * (dw / 2) + x / 2];
        let v_at = |x: usize, y: usize| pic.data[dw * dh + (dw / 2) * (dh / 2) + (y / 2) * (dw / 2) + x / 2];
        println!(
            "1080p -> {dw}x{dh}: {per:?} per frame; white Y {} red Y/U/V {}/{}/{} blue Y/U/V {}/{}/{}",
            y_at(dw / 2, dh / 2), y_at(10, 10), u_at(10, 10), v_at(10, 10), y_at(dw - 10, 10), u_at(dw - 10, 10), v_at(dw - 10, 10)
        );
        assert_eq!(y_at(dw / 2, dh / 2), 235); // white
        // BT.709 limited: red Y~63 V~240, blue Y~32 U~240.
        assert!((y_at(10, 10) as i32 - 63).abs() <= 2 && v_at(10, 10) > 235);
        assert!((y_at(dw - 10, 10) as i32 - 32).abs() <= 2 && u_at(dw - 10, 10) > 235);

        // Same frame at source size (no scaling).
        let t = Instant::now();
        bgra_to_i420(&src, sw, sh, sw * 4, &mut pic);
        println!("1080p convert only: {:?}", t.elapsed());
    }

    #[test]
    fn pointer_drawing() {
        let (w, h) = (100, 100);
        let mut img = vec![128u8; w * h * 4];
        // 8x8 colour pointer: opaque green.
        let p = Pointer { kind: 2, w: 8, h: 8, pitch: 32, data: [0u8, 255, 0, 255].repeat(64) };
        draw_pointer(&mut img, w, h, &p, 20, 30, 1.0, 1.0);
        let at = |img: &[u8], x: usize, y: usize| img[(y * w + x) * 4..(y * w + x) * 4 + 3].to_vec();
        assert_eq!(at(&img, 22, 32), vec![0, 255, 0]);
        assert_eq!(at(&img, 10, 10), vec![128, 128, 128]);
        // Monochrome: AND=0, XOR=1 -> white.
        let mut mono = vec![0u8; 8 * 2];
        for r in 8..16 {
            mono[r] = 0xFF;
        }
        let p = Pointer { kind: 1, w: 8, h: 16, pitch: 1, data: mono };
        draw_pointer(&mut img, w, h, &p, 60, 60, 0.5, 0.5);
        assert_eq!(at(&img, 31, 31), vec![255, 255, 255]);
    }

    /// Drawing into I420 gives the same result as drawing on the whole
    /// BGRA picture and converting it all, including at the picture's edge.
    #[test]
    fn pointer_into_i420_matches_full_draw() {
        let (w, h) = (64, 48);
        let base = screen(w, h);
        let p = Pointer { kind: 2, w: 9, h: 7, pitch: 36, data: [0u8, 200, 50, 255].repeat(63) };
        for &(px, py) in &[(11, 13), (60, 44), (-3, -2)] {
            let mut full = base.clone();
            draw_pointer(&mut full, w, h, &p, px, py, 1.0, 1.0);
            let mut expect = I420 { w: 0, h: 0, data: Vec::new() };
            bgra_to_i420(&full, w, h, w * 4, &mut expect);
            let mut got = I420 { w: 0, h: 0, data: Vec::new() };
            bgra_to_i420(&base, w, h, w * 4, &mut got);
            pointer_into_i420(&base, w * 4, &mut got, &p, px, py, 1.0, 1.0);
            assert!(got.data == expect.data, "pointer at {px},{py}");
        }
    }
}
