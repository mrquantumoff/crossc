//! The notification-area icon, drawn in code: three ascending bars, a
//! quality meter. How many bars are lit says the mode (SSC one, HiFi two,
//! UHQ three); lit in green: audio is streaming, lit in the taskbar's text
//! colour: the stream is open and idle; all dim: waiting; dim with a red
//! cross: no service; dim with an amber dot: a problem.
//!
//! [`render`] is pure (pixels out), so the drawing is tested; the HICON is
//! made from those pixels at whatever size the taskbar's DPI asks for.

use crate::view::Glyph;

/// One pixel, straight (not premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

/// A square picture, rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub size: usize,
    pub pixels: Vec<Rgba>,
}

impl Bitmap {
    fn new(size: usize) -> Bitmap {
        Bitmap {
            size,
            pixels: vec![Rgba(0, 0, 0, 0); size * size],
        }
    }

    pub fn at(&self, x: usize, y: usize) -> Rgba {
        self.pixels[y * self.size + x]
    }

    fn fill(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, c: Rgba) {
        for y in y0..y1.min(self.size) {
            for x in x0..x1.min(self.size) {
                self.pixels[y * self.size + x] = c;
            }
        }
    }

    /// BGRA with premultiplied alpha, top-down: what a 32-bit DIB wants.
    pub fn to_bgra_premultiplied(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for &Rgba(r, g, b, a) in &self.pixels {
            let pre = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
            out.extend_from_slice(&[pre(b), pre(g), pre(r), a]);
        }
        out
    }
}

const GREEN_ON_DARK: Rgba = Rgba(0x4c, 0xc3, 0x8a, 255);
const GREEN_ON_LIGHT: Rgba = Rgba(0x1a, 0x8f, 0x5a, 255);
const RED: Rgba = Rgba(0xe5, 0x53, 0x4b, 255);
const AMBER_ON_DARK: Rgba = Rgba(0xe5, 0xb0, 0x4a, 255);
const AMBER_ON_LIGHT: Rgba = Rgba(0xb8, 0x7a, 0x00, 255);

/// Draw `glyph` at `size` x `size` pixels for a dark or light taskbar.
pub fn render(glyph: Glyph, size: usize, dark_taskbar: bool) -> Bitmap {
    let size = size.clamp(8, 256);
    let mut bmp = Bitmap::new(size);
    let fg = if dark_taskbar {
        Rgba(255, 255, 255, 255)
    } else {
        Rgba(0x1f, 0x1f, 0x1f, 255)
    };
    let dim = Rgba(fg.0, fg.1, fg.2, if dark_taskbar { 96 } else { 80 });
    let green = if dark_taskbar {
        GREEN_ON_DARK
    } else {
        GREEN_ON_LIGHT
    };
    let (lit, colour) = match glyph {
        Glyph::Streaming(q) => (q.bars(), green),
        Glyph::Ready(q) => (q.bars(), fg),
        Glyph::Waiting | Glyph::Off | Glyph::Problem => (0, dim),
    };

    // Three bars with two gaps between a margin on each side.
    let margin = (size / 16).max(1);
    let gap = (size / 8).max(1);
    let inner = size - 2 * margin;
    let bar = (inner - 2 * gap) / 3;
    let left = margin + (inner - 3 * bar - 2 * gap) / 2;
    let bottom = size - margin;
    for (i, percent) in [40, 70, 100].into_iter().enumerate() {
        let height = (inner * percent).div_ceil(100);
        let x0 = left + i * (bar + gap);
        bmp.fill(
            x0,
            bottom - height,
            x0 + bar,
            bottom,
            if i < lit { colour } else { dim },
        );
    }

    // The mark in the top left corner, where the bars are low.
    let mark = (size * 3 / 8).max(3);
    match glyph {
        Glyph::Off => {
            // A cross; thicker on larger icons.
            let thick = (size / 12).max(1);
            for i in 0..mark {
                let j = mark - 1 - i;
                let (y0, y1) = (margin + i, margin + i + thick);
                bmp.fill(margin + i, y0, margin + i + 1, y1, RED);
                bmp.fill(margin + j, y0, margin + j + 1, y1, RED);
            }
        }
        Glyph::Problem => {
            // A dot.
            let amber = if dark_taskbar {
                AMBER_ON_DARK
            } else {
                AMBER_ON_LIGHT
            };
            let d = mark - 1;
            let r2 = (d * d) as i64;
            for y in 0..d {
                for x in 0..d {
                    // The pixel centre's distance from the dot's, doubled.
                    let dx = 2 * x as i64 + 1 - d as i64;
                    let dy = 2 * y as i64 + 1 - d as i64;
                    if dx * dx + dy * dy <= r2 {
                        bmp.fill(
                            margin + x,
                            margin + y,
                            margin + x + 1,
                            margin + y + 1,
                            amber,
                        );
                    }
                }
            }
        }
        _ => {}
    }
    bmp
}

/// A 32-bit uncompressed BMP file of `bitmaps` side by side on
/// `background`, for looking at the icons (`--dump-icons`).
pub fn sheet(bitmaps: &[Bitmap], background: Rgba, pad: usize) -> Vec<u8> {
    let height = bitmaps.iter().map(|b| b.size).max().unwrap_or(0) + 2 * pad;
    let width: usize = bitmaps.iter().map(|b| b.size + pad).sum::<usize>() + pad;
    let mut px = vec![background; width * height];
    let mut x0 = pad;
    for b in bitmaps {
        let y0 = (height - b.size) / 2;
        for y in 0..b.size {
            for x in 0..b.size {
                let Rgba(r, g, bl, a) = b.at(x, y);
                let a = u16::from(a);
                let over = |top: u8, under: u8| {
                    ((u16::from(top) * a + u16::from(under) * (255 - a) + 127) / 255) as u8
                };
                px[(y0 + y) * width + x0 + x] = Rgba(
                    over(r, background.0),
                    over(g, background.1),
                    over(bl, background.2),
                    255,
                );
            }
        }
        x0 += b.size + pad;
    }
    let data = width * height * 4;
    let mut out = Vec::with_capacity(54 + data);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + data) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    // Negative height: rows top to bottom.
    out.extend_from_slice(&(-(height as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for Rgba(r, g, b, a) in px {
        out.extend_from_slice(&[b, g, r, a]);
    }
    out
}

/// An icon handle for `bitmap`. The caller destroys it (DestroyIcon).
#[cfg(windows)]
pub fn to_hicon(bitmap: &Bitmap) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};

    let size = bitmap.size as i32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `bits` receives the section's memory, `size * size * 4`
    // bytes, valid until the bitmap is deleted; exactly that many bytes are
    // written, and the GDI objects are deleted before returning.
    unsafe {
        let colour = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(colour.into());
            return None;
        }
        let data = bitmap.to_bgra_premultiplied();
        std::ptr::copy_nonoverlapping(data.as_ptr(), bits.cast::<u8>(), data.len());
        // With a 32-bit colour bitmap the mask is ignored but must exist.
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: colour,
        });
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(colour.into());
        icon.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Quality;

    const ALL: [Glyph; 9] = [
        Glyph::Off,
        Glyph::Problem,
        Glyph::Waiting,
        Glyph::Ready(Quality::Ssc),
        Glyph::Ready(Quality::Hifi),
        Glyph::Ready(Quality::Uhq),
        Glyph::Streaming(Quality::Ssc),
        Glyph::Streaming(Quality::Hifi),
        Glyph::Streaming(Quality::Uhq),
    ];

    #[test]
    fn every_state_has_its_own_picture_at_every_size_and_theme() {
        for size in [16, 20, 24, 32, 40, 48, 64] {
            for dark in [true, false] {
                let pics: Vec<Bitmap> = ALL.iter().map(|g| render(*g, size, dark)).collect();
                for (i, a) in pics.iter().enumerate() {
                    assert_eq!(a.pixels.len(), size * size);
                    assert!(
                        a.pixels.iter().any(|p| p.3 != 0),
                        "{:?} at {size} is empty",
                        ALL[i]
                    );
                    for (j, b) in pics.iter().enumerate().skip(i + 1) {
                        assert_ne!(
                            a, b,
                            "{:?} and {:?} look the same at {size}",
                            ALL[i], ALL[j]
                        );
                    }
                }
            }
            for g in ALL {
                assert_ne!(render(g, size, true), render(g, size, false), "{g:?}");
            }
        }
    }

    /// Runs of columns that hold a pixel of exactly `colour`: the bars.
    fn bars_of(b: &Bitmap, colour: Rgba) -> usize {
        let has = |x: usize| (0..b.size).any(|y| b.at(x, y) == colour);
        (0..b.size)
            .filter(|&x| has(x) && (x == 0 || !has(x - 1)))
            .count()
    }

    #[test]
    fn lit_bars_count_the_quality() {
        for size in [16, 24, 32] {
            for (q, n) in [(Quality::Ssc, 1), (Quality::Hifi, 2), (Quality::Uhq, 3)] {
                let s = render(Glyph::Streaming(q), size, true);
                assert_eq!(bars_of(&s, GREEN_ON_DARK), n, "{q:?} at {size}");
                let r = render(Glyph::Ready(q), size, true);
                assert_eq!(bars_of(&r, Rgba(255, 255, 255, 255)), n);
                assert_eq!(bars_of(&r, GREEN_ON_DARK), 0);
            }
            let w = render(Glyph::Waiting, size, true);
            assert_eq!(bars_of(&w, Rgba(255, 255, 255, 96)), 3);
            assert!(render(Glyph::Off, size, true).pixels.contains(&RED));
            assert!(
                render(Glyph::Problem, size, true)
                    .pixels
                    .contains(&AMBER_ON_DARK)
            );
        }
    }

    #[test]
    fn bars_ascend_and_stay_inside_the_margin() {
        for size in [16, 20, 24, 32, 48] {
            let b = render(Glyph::Streaming(Quality::Uhq), size, true);
            let margin = (size / 16).max(1);
            let height = |x: usize| (0..size).filter(|&y| b.at(x, y).3 != 0).count();
            let columns: Vec<usize> = (0..size).map(height).collect();
            let filled: Vec<usize> = columns.iter().copied().filter(|h| *h > 0).collect();
            let heights: Vec<usize> = filled.chunk_by(|a, b| a == b).map(|c| c[0]).collect();
            assert_eq!(heights.len(), 3, "{size}: {columns:?}");
            assert!(heights[0] < heights[1] && heights[1] < heights[2]);
            assert_eq!(heights[2], size - 2 * margin);
            assert!(columns[..margin].iter().all(|h| *h == 0));
            assert!(columns[size - margin..].iter().all(|h| *h == 0));
            assert!((0..size).all(|x| b.at(x, size - 1).3 == 0), "bottom margin");
        }
    }

    #[test]
    fn odd_sizes_do_not_panic() {
        for size in 0..80 {
            for g in ALL {
                let b = render(g, size, true);
                assert_eq!(b.size, size.clamp(8, 256));
            }
        }
    }

    #[test]
    fn pixels_for_the_dib_are_premultiplied_bgra() {
        let b = Bitmap {
            size: 1,
            pixels: vec![Rgba(255, 128, 0, 128)],
        };
        assert_eq!(b.to_bgra_premultiplied(), [0, 64, 128, 128]);
        let s = sheet(
            &[render(Glyph::Waiting, 16, true)],
            Rgba(32, 32, 32, 255),
            4,
        );
        assert_eq!(&s[..2], b"BM");
        assert_eq!(s.len(), 54 + (16 + 8) * (16 + 8) * 4);
    }
}
