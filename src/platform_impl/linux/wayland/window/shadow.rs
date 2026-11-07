// Modified by enzUI contributors. See FORK.md.
//! The drop shadow around a window whose application draws its own frame
//! (enzWinit).
//!
//! Four subsurfaces sit below the window, outside its window geometry: a strip
//! along the top and one along the bottom, each reaching inward by the corner
//! radius so the shadow fills the transparent corners, and one strip down each
//! side. They are painted in shared memory with the analytic shadow of a
//! blurred rounded rectangle, cut away where the window itself is, so nothing
//! shows through the window's own translucency. A thin band beside each edge
//! takes pointer input as a resize grip; the rest of the shadow lets it
//! through.

use sctk::compositor::{CompositorState, Region};
use sctk::reexports::client::protocol::wl_shm;
use sctk::reexports::client::protocol::wl_subsurface::WlSubsurface;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as XdgResizeEdge;
use sctk::shm::slot::SlotPool;
use sctk::subcompositor::SubcompositorState;

use crate::platform::wayland::{ShadowLayer, WindowShadow};
use crate::platform_impl::wayland::state::WinitState;

/// How close to a corner, along an edge, a press resizes both ways.
const CORNER_GRIP: f32 = 16.0;

/// The window edges that cast a shadow: none on an edge the window is tiled
/// against, and none at all while it is maximized or fullscreen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
}

impl Edges {
    #[cfg(test)]
    pub const ALL: Self = Self { left: true, top: true, right: true, bottom: true };
    pub const NONE: Self = Self { left: false, top: false, right: false, bottom: false };
}

/// Whole logical pixels of shadow past each window edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Margins {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The margins that hold every layer of both the active and the inactive
/// shadow, so the strips keep their size when the window's activation
/// changes.
pub fn margins(shadow: &WindowShadow, edges: Edges) -> Margins {
    let mut reach = [0.0f32; 4];
    for layer in shadow.active.iter().chain(&shadow.inactive) {
        if layer.color[3] <= 0.0 {
            continue;
        }
        let grow = (sane(layer.blur) + layer.spread).max(0.0);
        let (dx, dy) = (finite(layer.offset.0), finite(layer.offset.1));
        reach[0] = reach[0].max(grow - dx);
        reach[1] = reach[1].max(grow - dy);
        reach[2] = reach[2].max(grow + dx);
        reach[3] = reach[3].max(grow + dy);
    }
    let side = |on: bool, reach: f32| if on { reach.max(0.0).ceil() as i32 } else { 0 };
    Margins {
        left: side(edges.left, reach[0]),
        top: side(edges.top, reach[1]),
        right: side(edges.right, reach[2]),
        bottom: side(edges.bottom, reach[3]),
    }
}

/// A strip's place in logical window coordinates (the window's top-left
/// corner is the origin).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Strip {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The four strips around a `width` by `height` window: top, bottom, left,
/// right. A strip with no area is `None`.
pub fn strips(margins: Margins, width: i32, height: i32, radius: f32) -> [Option<Strip>; 4] {
    let corner = (sane(radius).min(width as f32 / 2.0).min(height as f32 / 2.0)).ceil() as i32;
    let any = margins != Margins::default();
    let full_width = width + margins.left + margins.right;
    let make = |x, y, w: i32, h: i32| {
        (any && w > 0 && h > 0).then_some(Strip { x, y, width: w, height: h })
    };
    [
        make(-margins.left, -margins.top, full_width, margins.top + corner),
        make(-margins.left, height - corner, full_width, margins.bottom + corner),
        make(-margins.left, corner, margins.left, height - 2 * corner),
        make(width, corner, margins.right, height - 2 * corner),
    ]
}

/// Paints `strip` of the shadow of a `size` window into `canvas`, ARGB8888
/// (premultiplied, little-endian) at integer `scale`.
#[allow(clippy::too_many_arguments)]
pub fn paint(
    canvas: &mut [u8],
    strip: Strip,
    index: u8,
    scale: u32,
    size: (f32, f32),
    radius: f32,
    layers: &[ShadowLayer],
    cache: &mut CornerCache,
) {
    cache.prepare(scale, layers, radius);
    let scale_f = scale as f32;
    let (bw, bh) = ((strip.width as u32 * scale) as usize, (strip.height as u32 * scale) as usize);
    let window = Shape::window(size, radius);
    let shapes: Vec<(Shape, &ShadowLayer)> = layers
        .iter()
        .filter(|layer| layer.color[3] > 0.0)
        .map(|layer| (Shape::layer(size, radius, layer), layer))
        .collect();
    // Along a straight stretch of every outline the shadow does not change in
    // that direction: a pixel there equals its neighbour, so each such run
    // repeats one computed value.
    let straight_x = shapes.iter().map(|(s, _)| s.straight_x()).fold(window.straight_x(), meet);
    let straight_y = shapes.iter().map(|(s, _)| s.straight_y()).fold(window.straight_y(), meet);
    let pixel = |lx: f32, ly: f32| -> [u8; 4] {
        let mut out = [0.0f32; 4];
        for (shape, layer) in &shapes {
            let alpha = (layer.color[3] * shape.coverage(lx, ly)).clamp(0.0, 1.0);
            let keep = 1.0 - alpha;
            for (channel, value) in out.iter_mut().take(3).enumerate() {
                *value = layer.color[channel] * alpha + *value * keep;
            }
            out[3] = alpha + out[3] * keep;
        }
        // Nothing under the window itself, which may be translucent.
        let outside = 1.0 - window.coverage(lx, ly);
        let byte = |v: f32| (v * outside * 255.0).round().clamp(0.0, 255.0) as u8;
        [byte(out[2]), byte(out[1]), byte(out[0]), byte(out[3])]
    };
    let logical = |b: usize, origin: i32| origin as f32 + (b as f32 + 0.5) / scale_f;
    let mut first_straight_row: Option<usize> = None;
    let horizontal = index < 2;
    // The buffer columns whose centres fall in the straight stretch.
    let column = |l: f32| ((l - strip.x as f32) * scale_f - 0.5).max(0.0);
    let first = (column(straight_x.0).ceil() as usize).min(bw);
    let last = ((column(straight_x.1).floor() as usize) + 1).min(bw).max(first);
    // The shadow near a corner, and along a straight stretch, keeps its look
    // relative to that corner whatever the window's size: those pixels come
    // from the cache once a straight run separates the two corners.
    let separated = if horizontal {
        first < last
    } else {
        (0..bh).any(|by| straight_y.contains(logical(by, strip.y)))
    };
    for by in 0..bh {
        let ly = logical(by, strip.y);
        let row = by * bw * 4;
        let straight_row = straight_y.contains(ly);
        if straight_row && !horizontal {
            if let Some(source) = first_straight_row {
                canvas.copy_within(source..source + bw * 4, row);
                continue;
            }
            first_straight_row = Some(row);
        }
        let row_bytes = &mut canvas[row..row + bw * 4];
        let mut at = |bx: usize, key: Option<(u8, u32, u32)>| -> [u8; 4] {
            match key.filter(|_| separated) {
                Some(key) => *cache
                    .pixels
                    .entry((index, key.0, key.1, key.2))
                    .or_insert_with(|| pixel(logical(bx, strip.x), ly)),
                None => pixel(logical(bx, strip.x), ly),
            }
        };
        if horizontal {
            for bx in (0..first).chain(last..bw) {
                let key = if bx < first {
                    (0, bx as u32, by as u32)
                } else {
                    (1, (bw - 1 - bx) as u32, by as u32)
                };
                let value = at(bx, Some(key));
                row_bytes[bx * 4..bx * 4 + 4].copy_from_slice(&value);
            }
            if first < last && straight_x.contains(logical(first, strip.x)) {
                let value = at(first, Some((2, 0, by as u32)));
                fill(&mut row_bytes[first * 4..last * 4], value);
            } else {
                for bx in first..last {
                    let value = at(bx, None);
                    row_bytes[bx * 4..bx * 4 + 4].copy_from_slice(&value);
                }
            }
        } else {
            for bx in 0..bw {
                let key = if straight_row {
                    (2, bx as u32, 0)
                } else if ly < straight_y.0 {
                    (0, bx as u32, by as u32)
                } else {
                    (1, bx as u32, (bh - 1 - by) as u32)
                };
                let value = at(bx, Some(key));
                row_bytes[bx * 4..bx * 4 + 4].copy_from_slice(&value);
            }
        }
    }
}

/// Shadow pixels kept between paints of the same shadow, by strip, the
/// corner they are measured from and their place from it. Only the straight
/// runs between corners change when the window resizes.
#[derive(Default)]
pub struct CornerCache {
    key: Option<(u32, Vec<ShadowLayer>, u32)>,
    pixels: std::collections::HashMap<
        (u8, u8, u32, u32),
        [u8; 4],
        std::hash::BuildHasherDefault<SmallKeyHasher>,
    >,
}

/// A multiply-and-rotate hasher for the cache's small integer keys; the
/// default hasher's flood resistance is not needed for pixel positions.
#[derive(Default)]
pub struct SmallKeyHasher(u64);

impl std::hash::Hasher for SmallKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl CornerCache {
    fn prepare(&mut self, scale: u32, layers: &[ShadowLayer], radius: f32) {
        let fresh = self.key.as_ref().map_or(true, |(s, l, r)| {
            *s != scale || l.as_slice() != layers || *r != radius.to_bits()
        });
        if fresh {
            self.pixels.clear();
            self.key = Some((scale, layers.to_vec(), radius.to_bits()));
        }
    }
}

/// Repeats one pixel across `run`, doubling the filled prefix each step so
/// the work is a few block copies rather than one per pixel.
fn fill(run: &mut [u8], value: [u8; 4]) {
    if run.len() < 4 {
        return;
    }
    run[..4].copy_from_slice(&value);
    let mut filled = 4;
    while filled < run.len() {
        let count = filled.min(run.len() - filled);
        run.copy_within(..count, filled);
        filled += count;
    }
}

/// The surfaces that carry a window's shadow.
pub struct ShadowSurfaces {
    parts: [Option<Part>; 4],
    /// One cache for the active look and one for the inactive.
    caches: [CornerCache; 2],
    pool: SlotPool,
    window_size: (i32, i32),
}

struct Part {
    surface: WlSurface,
    subsurface: WlSubsurface,
    strip: Strip,
}

impl Part {
    fn unmap(&self, sync: bool) {
        if sync {
            self.subsurface.set_sync();
        } else {
            self.subsurface.set_desync();
        }
        self.surface.attach(None, 0, 0);
        self.surface.commit();
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        self.subsurface.destroy();
        self.surface.destroy();
    }
}

impl ShadowSurfaces {
    pub fn new(pool: SlotPool) -> Self {
        Self {
            parts: [None, None, None, None],
            caches: [CornerCache::default(), CornerCache::default()],
            pool,
            window_size: (0, 0),
        }
    }

    pub(super) fn hide(&self, sync: bool) {
        for part in self.parts.iter().flatten() {
            part.unmap(sync);
        }
    }

    /// Paints the shadow of a `size` window and commits the strips. With
    /// `sync` the strips appear with the window's next commit (a resize);
    /// otherwise at once (an activation change).
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        parent: &WlSurface,
        subcompositor: &SubcompositorState,
        compositor: &CompositorState,
        queue_handle: &QueueHandle<WinitState>,
        shadow: &WindowShadow,
        active: bool,
        edges: Edges,
        size: (u32, u32),
        scale: u32,
        sync: bool,
    ) {
        let (width, height) = (size.0 as i32, size.1 as i32);
        let margins = margins(shadow, edges);
        self.window_size = (width, height);
        let layers = if active { &shadow.active } else { &shadow.inactive };
        let scale = scale.max(1);
        let cache = &mut self.caches[usize::from(active)];
        for (index, (slot, strip)) in self
            .parts
            .iter_mut()
            .zip(strips(margins, width, height, shadow.corner_radius))
            .enumerate()
        {
            let Some(strip) = strip else {
                // Parent transitions can still animate these surfaces after they stop casting a shadow.
                if let Some(part) = slot.as_ref() {
                    part.unmap(sync);
                }
                continue;
            };
            let part = slot.get_or_insert_with(|| {
                let (subsurface, surface) =
                    subcompositor.create_subsurface(parent.clone(), queue_handle);
                subsurface.place_below(parent);
                Part { surface, subsurface, strip }
            });
            part.strip = strip;
            let (bw, bh) = (strip.width * scale as i32, strip.height * scale as i32);
            let Ok((buffer, canvas)) =
                self.pool.create_buffer(bw, bh, bw * 4, wl_shm::Format::Argb8888)
            else {
                continue;
            };
            paint(
                canvas,
                strip,
                index as u8,
                scale,
                (width as f32, height as f32),
                shadow.corner_radius,
                layers,
                cache,
            );
            if sync {
                part.subsurface.set_sync();
            } else {
                part.subsurface.set_desync();
            }
            part.surface.set_buffer_scale(scale as i32);
            part.subsurface.set_position(strip.x, strip.y);
            if buffer.attach_to(&part.surface).is_err() {
                continue;
            }
            if part.surface.version() >= 4 {
                part.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            } else {
                part.surface.damage(0, 0, i32::MAX, i32::MAX);
            }
            if let Ok(region) = Region::new(compositor) {
                let (x, y, w, h) =
                    grip(margins, sane(shadow.resize_border), (width, height), strip);
                region.add(x, y, w, h);
                // The window's own rectangle, where a corner strip reaches
                // inward, belongs to the window.
                region.subtract(-strip.x, -strip.y, width, height);
                part.surface.set_input_region(Some(region.wl_region()));
            }
            part.surface.commit();
        }
    }

    /// Whether `surface` is one of the strips.
    pub fn owns(&self, surface: &WlSurface) -> bool {
        self.parts.iter().flatten().any(|part| part.surface.id() == surface.id())
    }

    /// The edge a press at `(x, y)` on `surface` resizes.
    pub fn resize_edge(&self, surface: &WlSurface, x: f64, y: f64) -> Option<XdgResizeEdge> {
        let part = self.parts.iter().flatten().find(|part| part.surface.id() == surface.id())?;
        let (width, height) = (self.window_size.0 as f32, self.window_size.1 as f32);
        Some(edge_at(part.strip.x as f32 + x as f32, part.strip.y as f32 + y as f32, width, height))
    }
}

/// The window grown by the resize band on each side, in `strip`'s own
/// coordinates: the input region of the strips before the window itself is
/// taken away.
fn grip(margins: Margins, border: f32, size: (i32, i32), strip: Strip) -> (i32, i32, i32, i32) {
    let band = |margin: i32| (margin as f32).min(border).round() as i32;
    let (left, top) = (band(margins.left), band(margins.top));
    let (right, bottom) = (band(margins.right), band(margins.bottom));
    (-left - strip.x, -top - strip.y, size.0 + left + right, size.1 + top + bottom)
}

/// The edge a press at `(x, y)` outside a `width` by `height` window resizes;
/// within [`CORNER_GRIP`] of a corner it resizes both ways.
pub fn edge_at(x: f32, y: f32, width: f32, height: f32) -> XdgResizeEdge {
    let beside_x = x < 0.0 || x > width;
    let beside_y = y < 0.0 || y > height;
    let left = x < 0.0 || (beside_y && x < CORNER_GRIP);
    let right = x > width || (beside_y && x > width - CORNER_GRIP);
    let top = y < 0.0 || (beside_x && y < CORNER_GRIP);
    let bottom = y > height || (beside_x && y > height - CORNER_GRIP);
    match (left, right, top, bottom) {
        (true, _, true, _) => XdgResizeEdge::TopLeft,
        (true, _, _, true) => XdgResizeEdge::BottomLeft,
        (_, true, true, _) => XdgResizeEdge::TopRight,
        (_, true, _, true) => XdgResizeEdge::BottomRight,
        (true, ..) => XdgResizeEdge::Left,
        (_, true, ..) => XdgResizeEdge::Right,
        (_, _, true, _) => XdgResizeEdge::Top,
        (_, _, _, true) => XdgResizeEdge::Bottom,
        _ => XdgResizeEdge::None,
    }
}

/// A rounded rectangle and how soft its edge is.
struct Shape {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
    sigma: f32,
}

impl Shape {
    fn window(size: (f32, f32), radius: f32) -> Self {
        Self::rounded(0.0, 0.0, size.0, size.1, radius, 0.0)
    }

    fn layer(size: (f32, f32), radius: f32, layer: &ShadowLayer) -> Self {
        let spread = finite(layer.spread);
        Self::rounded(
            finite(layer.offset.0) - spread,
            finite(layer.offset.1) - spread,
            size.0 + 2.0 * spread,
            size.1 + 2.0 * spread,
            sane(radius) + spread,
            // A CSS blur radius fades over about twice the Gaussian deviation.
            sane(layer.blur) / 2.0,
        )
    }

    fn rounded(x: f32, y: f32, width: f32, height: f32, radius: f32, sigma: f32) -> Self {
        let (width, height) = (width.max(0.0), height.max(0.0));
        let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
        Self { x, y, width, height, radius, sigma }
    }

    /// Signed distance from `(px, py)` to the outline, negative inside.
    fn distance(&self, px: f32, py: f32) -> f32 {
        let (hw, hh) = (self.width / 2.0, self.height / 2.0);
        let qx = (px - (self.x + hw)).abs() - (hw - self.radius);
        let qy = (py - (self.y + hh)).abs() - (hh - self.radius);
        let outside = qx.max(0.0).hypot(qy.max(0.0));
        outside + qx.max(qy).min(0.0) - self.radius
    }

    /// How much of the blurred shape covers `(px, py)`, 0 to 1.
    fn coverage(&self, px: f32, py: f32) -> f32 {
        let distance = self.distance(px, py);
        if self.sigma < 0.25 {
            // A hard edge, antialiased over one pixel.
            (0.5 - distance).clamp(0.0, 1.0)
        } else {
            0.5 * erfc(distance / (self.sigma * std::f32::consts::SQRT_2))
        }
    }

    /// The horizontal range in which the outline is straight on both its top
    /// and bottom.
    fn straight_x(&self) -> Span {
        Span(self.x + self.radius, self.x + self.width - self.radius)
    }

    fn straight_y(&self) -> Span {
        Span(self.y + self.radius, self.y + self.height - self.radius)
    }
}

#[derive(Clone, Copy)]
struct Span(f32, f32);

impl Span {
    fn contains(self, value: f32) -> bool {
        value >= self.0 && value <= self.1
    }
}

fn meet(a: Span, b: Span) -> Span {
    Span(a.0.max(b.0), a.1.min(b.1))
}

/// The complementary error function, to within 1.5e-7 (Abramowitz and
/// Stegun 7.1.26).
fn erfc(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let poly = t
        * (0.254_829_6
            + t * (-0.284_496_74 + t * (1.421_413_7 + t * (-1.453_152_1 + t * 1.061_405_4))));
    let tail = poly * (-x * x).exp();
    if x >= 0.0 {
        tail
    } else {
        2.0 - tail
    }
}

fn sane(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn finite(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soft() -> WindowShadow {
        let layer = ShadowLayer {
            color: [0.0, 0.0, 0.0, 0.3],
            offset: (0.0, 2.0),
            blur: 16.0,
            spread: 0.0,
        };
        WindowShadow {
            corner_radius: 12.0,
            active: vec![layer],
            inactive: vec![layer],
            resize_border: 8.0,
        }
    }

    fn alpha_at(canvas: &[u8], width: usize, x: usize, y: usize) -> u8 {
        canvas[(y * width + x) * 4 + 3]
    }

    #[test]
    fn the_shadow_fades_away_from_the_window_and_leaves_it_clear() {
        let shadow = soft();
        let margins = margins(&shadow, Edges::ALL);
        assert_eq!(margins, Margins { left: 16, top: 14, right: 16, bottom: 18 });
        let [top, ..] = strips(margins, 200, 100, 12.0);
        let top = top.expect("a top strip");
        let mut canvas = vec![0u8; (top.width * top.height * 4) as usize];
        paint(
            &mut canvas,
            top,
            0,
            1,
            (200.0, 100.0),
            12.0,
            &shadow.active,
            &mut CornerCache::default(),
        );
        let width = top.width as usize;
        // Down the middle of the top edge, from the strip's outer edge to the window.
        let column = width / 2;
        let near = alpha_at(&canvas, width, column, (top.height - 13) as usize);
        let far = alpha_at(&canvas, width, column, 0);
        assert!(near > far, "darker near the window: {near} vs {far}");
        assert!(far < 8, "nearly clear at the strip's edge: {far}");
        // Inside the window, below its top edge, nothing.
        assert_eq!(alpha_at(&canvas, width, column, (top.height - 2) as usize), 0);
        // In the transparent corner, outside the rounded window, some shadow.
        let corner =
            alpha_at(&canvas, width, (margins.left + 1) as usize, (top.height - 11) as usize);
        assert!(corner > 0, "the corner outside the rounding is shaded");
    }

    #[test]
    fn a_maximized_or_tiled_edge_casts_no_shadow() {
        let shadow = soft();
        assert_eq!(margins(&shadow, Edges::NONE), Margins::default());
        assert_eq!(strips(Margins::default(), 200, 100, 12.0), [None, None, None, None]);
        let tiled_left = margins(&shadow, Edges { left: false, ..Edges::ALL });
        assert_eq!(tiled_left.left, 0);
        let [_, _, left, right] = strips(tiled_left, 200, 100, 12.0);
        assert!(left.is_none() && right.is_some());
    }

    #[test]
    fn presses_beside_an_edge_or_a_corner_resize_that_way() {
        assert_eq!(edge_at(-3.0, 50.0, 200.0, 100.0), XdgResizeEdge::Left);
        assert_eq!(edge_at(100.0, 103.0, 200.0, 100.0), XdgResizeEdge::Bottom);
        assert_eq!(edge_at(-3.0, -3.0, 200.0, 100.0), XdgResizeEdge::TopLeft);
        // Along the top, near the right corner.
        assert_eq!(edge_at(195.0, -2.0, 200.0, 100.0), XdgResizeEdge::TopRight);
    }

    #[test]
    fn the_straight_runs_repeat_what_a_full_paint_computes() {
        let shadow = soft();
        let margins = margins(&shadow, Edges::ALL);
        // A cache warmed a few pixels larger first, as on a resize, so cached
        // corner pixels overlap the ones this size needs.
        let mut cache = CornerCache::default();
        for (index, strip) in strips(margins, 304, 164, 12.0).into_iter().enumerate() {
            if let Some(strip) = strip {
                let mut warm = vec![0u8; (strip.width * strip.height * 16) as usize];
                paint(
                    &mut warm,
                    strip,
                    index as u8,
                    2,
                    (304.0, 164.0),
                    12.0,
                    &shadow.active,
                    &mut cache,
                );
            }
        }
        for (index, strip) in strips(margins, 300, 160, 12.0).into_iter().enumerate() {
            let Some(strip) = strip else { continue };
            let mut fast = vec![0u8; (strip.width * strip.height * 16) as usize];
            paint(
                &mut fast,
                strip,
                index as u8,
                2,
                (300.0, 160.0),
                12.0,
                &shadow.active,
                &mut cache,
            );
            // Every pixel computed independently.
            let mut slow = vec![0u8; fast.len()];
            let (bw, bh) = ((strip.width * 2) as usize, (strip.height * 2) as usize);
            for by in 0..bh {
                for bx in 0..bw {
                    let lx = strip.x as f32 + (bx as f32 + 0.5) / 2.0;
                    let ly = strip.y as f32 + (by as f32 + 0.5) / 2.0;
                    let window = Shape::window((300.0, 160.0), 12.0);
                    let layer = Shape::layer((300.0, 160.0), 12.0, &shadow.active[0]);
                    let alpha = shadow.active[0].color[3] * layer.coverage(lx, ly);
                    let a = (alpha * (1.0 - window.coverage(lx, ly)) * 255.0).round() as u8;
                    slow[(by * bw + bx) * 4 + 3] = a;
                }
            }
            for i in (3..fast.len()).step_by(4) {
                assert!(
                    fast[i].abs_diff(slow[i]) <= 1,
                    "pixel {} differs: {} vs {}",
                    i / 4,
                    fast[i],
                    slow[i]
                );
            }
        }
    }
}
