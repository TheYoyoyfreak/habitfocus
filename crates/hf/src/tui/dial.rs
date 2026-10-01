//! The sundial: 24 hours of screen time as a ring of wedges, drawn as a
//! picture for terminals that show images (and as halfblocks elsewhere).
//!
//! Midnight is at the top and the day runs clockwise. A wedge's area grows
//! with the time used in its hour, split by color from the inside out.

use std::f32::consts::{PI, TAU};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub type Rgb = [u8; 3];

/// One hour of the dial: colored parts, inside first.
pub type Hour = Vec<(Rgb, u64)>;

/// Hollow middle, as a share of the outer radius.
const INNER: f32 = 0.24;
/// Gap between neighbouring wedges, in radians.
const GAP: f32 = 0.025;
const RING: Rgb = [128, 128, 128];
const HAND: Rgb = [230, 230, 230];

/// Draws `hours` (24 entries) into a `width`×`height` picture with a
/// transparent background. `now` is the time of day in hours (e.g. 14.5) for
/// the hand.
pub fn render(hours: &[Hour], now: Option<f32>, width: u32, height: u32) -> image::RgbaImage {
    let Some(mut pixmap) = Pixmap::new(width.max(1), height.max(1)) else {
        return image::RgbaImage::new(width, height);
    };
    let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
    let outer = (width.min(height) as f32 / 2.0) * 0.86;
    let inner = outer * INNER;
    let line = (outer / 60.0).max(1.0);

    // The face: the outer edge, the hollow middle and a tick per hour.
    stroke_circle(&mut pixmap, cx, cy, outer, RING, 70, line);
    stroke_circle(&mut pixmap, cx, cy, inner, RING, 50, line);
    for hour in 0..24 {
        let major = hour % 6 == 0;
        let (from, to) = (outer + line * 2.0, outer + line * if major { 7.0 } else { 4.0 });
        let a = angle(hour as f32);
        let mut path = PathBuilder::new();
        path.move_to(cx + from * a.cos(), cy + from * a.sin());
        path.line_to(cx + to * a.cos(), cy + to * a.sin());
        if let Some(path) = path.finish() {
            let alpha = if major { 200 } else { 90 };
            stroke(&mut pixmap, &path, RING, alpha, if major { line * 1.5 } else { line });
        }
    }

    // Equal area for equal time: the radius grows with the square root.
    let max = hours.iter().map(|h| h.iter().map(|(_, ms)| ms).sum::<u64>()).max().unwrap_or(0).max(1);
    let radius = |ms: u64| (inner * inner + (outer * outer - inner * inner) * ms as f32 / max as f32).sqrt();
    for (hour, parts) in hours.iter().enumerate().take(24) {
        let (start, end) = (angle(hour as f32) + GAP, angle(hour as f32 + 1.0) - GAP);
        let mut done = 0;
        for &(color, ms) in parts.iter().filter(|(_, ms)| *ms > 0) {
            let (r0, r1) = (radius(done), radius(done + ms));
            done += ms;
            if let Some(path) = sector(cx, cy, r0, r1, start, end) {
                let mut paint = Paint::default();
                paint.set_color_rgba8(color[0], color[1], color[2], 255);
                paint.anti_alias = true;
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
    }

    if let Some(now) = now {
        let a = angle(now);
        let mut path = PathBuilder::new();
        path.move_to(cx + inner * 0.4 * a.cos(), cy + inner * 0.4 * a.sin());
        path.line_to(cx + (outer + line * 7.0) * a.cos(), cy + (outer + line * 7.0) * a.sin());
        if let Some(path) = path.finish() {
            stroke(&mut pixmap, &path, HAND, 230, line * 1.5);
        }
        if let Some(dot) = PathBuilder::from_circle(cx, cy, inner * 0.4) {
            let mut paint = Paint::default();
            paint.set_color_rgba8(HAND[0], HAND[1], HAND[2], 230);
            paint.anti_alias = true;
            pixmap.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }

    image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixmap.take_demultiplied())
        .unwrap_or_else(|| image::RgbaImage::new(width, height))
}

/// Screen angle of a time of day in hours: midnight at the top, clockwise.
fn angle(hour: f32) -> f32 {
    hour / 24.0 * TAU - PI / 2.0
}

/// A ring segment between two radii and two angles.
fn sector(cx: f32, cy: f32, r0: f32, r1: f32, start: f32, end: f32) -> Option<tiny_skia::Path> {
    const STEPS: usize = 8;
    let mut path = PathBuilder::new();
    for i in 0..=STEPS {
        let a = start + (end - start) * i as f32 / STEPS as f32;
        let (x, y) = (cx + r1 * a.cos(), cy + r1 * a.sin());
        if i == 0 { path.move_to(x, y) } else { path.line_to(x, y) }
    }
    for i in (0..=STEPS).rev() {
        let a = start + (end - start) * i as f32 / STEPS as f32;
        path.line_to(cx + r0 * a.cos(), cy + r0 * a.sin());
    }
    path.close();
    path.finish()
}

fn stroke_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Rgb, alpha: u8, width: f32) {
    if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
        stroke(pixmap, &path, color, alpha, width);
    }
}

fn stroke(pixmap: &mut Pixmap, path: &tiny_skia::Path, color: Rgb, alpha: u8, width: f32) {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], alpha);
    paint.anti_alias = true;
    let stroke = Stroke { width, ..Stroke::default() };
    pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = [255, 0, 0];
    const BLUE: Rgb = [0, 0, 255];

    /// The pixel at `hour` (its middle) and `share` of the outer radius.
    fn pixel(img: &image::RgbaImage, hour: f32, share: f32) -> [u8; 4] {
        let (cx, cy) = (img.width() as f32 / 2.0, img.height() as f32 / 2.0);
        let r = img.width().min(img.height()) as f32 / 2.0 * 0.86 * share;
        let a = angle(hour + 0.5);
        img.get_pixel((cx + r * a.cos()) as u32, (cy + r * a.sin()) as u32).0
    }

    #[test]
    fn wedges_grow_with_time_and_stack_inside_out() {
        let mut hours = vec![Vec::new(); 24];
        hours[0] = vec![(RED, 30), (BLUE, 30)];
        hours[6] = vec![(BLUE, 15)];
        let img = render(&hours, None, 200, 200);
        // Midnight's wedge reaches the edge: red inside, blue outside.
        assert_eq!(pixel(&img, 0.0, 0.35), [255, 0, 0, 255]);
        assert_eq!(pixel(&img, 0.0, 0.9), [0, 0, 255, 255]);
        // A quarter of the time covers a quarter of the area: short of the edge.
        assert_eq!(pixel(&img, 6.0, 0.4), [0, 0, 255, 255]);
        assert_eq!(pixel(&img, 6.0, 0.8)[3], 0);
        // Unused hours stay transparent.
        assert_eq!(pixel(&img, 12.0, 0.6)[3], 0);
    }

    #[test]
    fn empty_and_tiny_dials_render() {
        let img = render(&[], Some(13.25), 1, 1);
        assert_eq!(img.dimensions(), (1, 1));
        let img = render(&vec![Vec::new(); 24], Some(0.0), 80, 60);
        assert_eq!(img.dimensions(), (80, 60));
    }
}
