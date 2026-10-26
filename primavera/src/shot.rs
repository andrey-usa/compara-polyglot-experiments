//! `expect screenshot`: a PNG compared with its baseline pixel by pixel.
//! Two pixels differ when their perceived colour distance (YIQ, as
//! pixelmatch measures it) is over the threshold; the diff image paints
//! those red over a faded baseline.

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA, 8 bits a channel, row by row.
    pub rgba: Vec<u8>,
}

pub fn decode(bytes: &[u8]) -> Result<Image, String> {
    let mut d = png::Decoder::new(bytes);
    d.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = d.read_info().map_err(|e| format!("not a PNG: {e}"))?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).map_err(|e| format!("not a PNG: {e}"))?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("an indexed PNG wasn't expanded".into()),
    };
    Ok(Image { width: info.width, height: info.height, rgba })
}

pub fn encode(img: &Image) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, img.width, img.height);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        if let Ok(mut w) = e.write_header() {
            let _ = w.write_image_data(&img.rgba);
        }
    }
    out
}

/// The part at `x, y` (clipped to the image).
pub fn crop(img: &Image, x: u32, y: u32, width: u32, height: u32) -> Image {
    let x = x.min(img.width);
    let y = y.min(img.height);
    let w = width.min(img.width - x);
    let h = height.min(img.height - y);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let start = ((row * img.width + x) * 4) as usize;
        rgba.extend_from_slice(&img.rgba[start..start + (w * 4) as usize]);
    }
    Image { width: w, height: h, rgba }
}

/// A pixel over white, as RGB.
fn blend(p: &[u8]) -> (f64, f64, f64) {
    let a = p[3] as f64 / 255.0;
    let over = |c: u8| 255.0 + (c as f64 - 255.0) * a;
    (over(p[0]), over(p[1]), over(p[2]))
}

fn yiq(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    (
        r * 0.298_895_31 + g * 0.586_622_47 + b * 0.114_482_23,
        r * 0.595_977_99 - g * 0.274_176_10 - b * 0.321_801_89,
        r * 0.211_470_17 - g * 0.522_617_11 + b * 0.311_146_94,
    )
}

#[derive(Debug)]
pub struct Diff {
    pub pixels: u64,
    pub total: u64,
    pub image: Image,
}

/// How many pixels of `actual` differ from `baseline` over `threshold`
/// (0 = exact, 1 = anything goes; 0.2 by default), or why they can't be
/// compared.
pub fn compare(baseline: &Image, actual: &Image, threshold: f64) -> Result<Diff, String> {
    if (baseline.width, baseline.height) != (actual.width, actual.height) {
        return Err(format!(
            "it is {}x{}, the baseline {}x{}",
            actual.width, actual.height, baseline.width, baseline.height
        ));
    }
    // The largest YIQ distance, 35215, scaled by the threshold squared.
    let max = 35_215.0 * threshold * threshold;
    let mut pixels = 0;
    let mut out = Vec::with_capacity(baseline.rgba.len());
    for (a, b) in baseline.rgba.chunks(4).zip(actual.rgba.chunks(4)) {
        let (ya, ia, qa) = {
            let (r, g, b) = blend(a);
            yiq(r, g, b)
        };
        let (yb, ib, qb) = {
            let (r, g, b) = blend(b);
            yiq(r, g, b)
        };
        let delta = 0.5053 * (ya - yb).powi(2) + 0.299 * (ia - ib).powi(2) + 0.1957 * (qa - qb).powi(2);
        if delta > max {
            pixels += 1;
            out.extend_from_slice(&[255, 0, 0, 255]);
        } else {
            let faded = (255.0 + (ya - 255.0) * 0.1) as u8;
            out.extend_from_slice(&[faded, faded, faded, 255]);
        }
    }
    Ok(Diff {
        pixels,
        total: baseline.width as u64 * baseline.height as u64,
        image: Image { width: baseline.width, height: baseline.height, rgba: out },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, p: [u8; 4]) -> Image {
        Image { width: w, height: h, rgba: p.repeat((w * h) as usize) }
    }

    #[test]
    fn round_trips_and_crops() {
        let mut img = solid(4, 3, [10, 20, 30, 255]);
        img.rgba[(4 + 2) * 4] = 200;
        let back = decode(&encode(&img)).unwrap();
        assert_eq!(back, img);
        let part = crop(&img, 2, 1, 5, 5);
        assert_eq!((part.width, part.height), (2, 2));
        assert_eq!(part.rgba[0], 200);
    }

    #[test]
    fn differences_over_the_threshold_count() {
        let a = solid(10, 10, [255, 255, 255, 255]);
        let mut b = a.clone();
        // One pixel black, one a hair off white.
        b.rgba[0..4].copy_from_slice(&[0, 0, 0, 255]);
        b.rgba[4..8].copy_from_slice(&[250, 250, 250, 255]);
        let d = compare(&a, &b, 0.2).unwrap();
        assert_eq!((d.pixels, d.total), (1, 100));
        assert_eq!(&d.image.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(compare(&a, &b, 0.0).unwrap().pixels, 2);
        assert!(compare(&a, &solid(9, 10, [0; 4]), 0.2).unwrap_err().contains("9x10, the baseline 10x10"));
    }
}
