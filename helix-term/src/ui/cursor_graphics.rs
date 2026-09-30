//! Independently animated cursor corners rendered as a pixel image.
//!
//! The terminal image is a separate layer: no text cells are painted or replaced.
//! Measured cell pixels determine geometry, including vertical movement distance.

use std::time::{Duration, Instant};

use helix_core::Position;
use helix_view::{
    editor::CursorSmearConfig,
    graphics::{Color, CursorKind, Rect},
};
use tui::backend::{CellSize, CursorImage};

const MAX_FRAME_PIXELS: usize = 256 * 1024;
const MAX_CELL_PIXELS: u16 = 512;
const FRONT_DURATION: f64 = 0.25;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn lerp(self, other: Self, fraction: f64) -> Self {
        Self {
            x: self.x + (other.x - self.x) * fraction,
            y: self.y + (other.y - self.y) * fraction,
        }
    }

    fn distance(self, other: Self) -> f64 {
        (other.x - self.x).hypot(other.y - self.y)
    }
}

type Quad = [Point; 4];

fn center(quad: Quad) -> Point {
    Point {
        x: quad.iter().map(|point| point.x).sum::<f64>() / 4.0,
        y: quad.iter().map(|point| point.y).sum::<f64>() / 4.0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settings {
    duration: Duration,
    max_distance: u16,
}

impl From<&CursorSmearConfig> for Settings {
    fn from(config: &CursorSmearConfig) -> Self {
        Self {
            duration: Duration::from_millis(config.duration.clamp(16, 1000)),
            max_distance: config.max_distance.clamp(1, 256),
        }
    }
}

#[derive(Debug)]
struct Observation<I> {
    bounds: Rect,
    identity: I,
    settings: Settings,
    cell_size: CellSize,
    kind: CursorKind,
    target: Quad,
}

#[derive(Debug, Clone, Copy)]
struct Animation {
    from: Quad,
    to: Quad,
    started: Instant,
    durations: [Duration; 4],
}

impl Animation {
    fn new(from: Quad, to: Quad, duration: Duration, started: Instant) -> Option<Self> {
        let old_center = center(from);
        let new_center = center(to);
        let dx = new_center.x - old_center.x;
        let dy = new_center.y - old_center.y;
        let projections =
            to.map(|point| (point.x - new_center.x) * dx + (point.y - new_center.y) * dy);
        let extreme = projections
            .iter()
            .map(|value| value.abs())
            .fold(0.0, f64::max);
        let durations = std::array::from_fn(|index| {
            if from[index].distance(to[index]) < 0.01 {
                return Duration::ZERO;
            }
            let alignment = if extreme > f64::EPSILON {
                (projections[index] / extreme + 1.0) * 0.5
            } else {
                0.0
            };
            // Direction decides the leading corners. Corners settle exactly,
            // rather than maintaining an indefinitely decaying spring tail.
            duration.mul_f64(1.0 - (1.0 - FRONT_DURATION) * alignment)
        });
        durations
            .iter()
            .any(|duration| !duration.is_zero())
            .then_some(Self {
                from,
                to,
                started,
                durations,
            })
    }

    fn corners(self, now: Instant) -> Quad {
        let elapsed = now.saturating_duration_since(self.started);
        std::array::from_fn(|index| {
            let duration = self.durations[index];
            let fraction = if duration.is_zero() {
                1.0
            } else {
                (elapsed.as_secs_f64() / duration.as_secs_f64()).min(1.0)
            };
            let remaining = 1.0 - fraction;
            self.from[index].lerp(self.to[index], 1.0 - remaining * remaining * remaining)
        })
    }

    fn is_active(self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        self.durations.iter().any(|duration| elapsed < *duration)
    }
}

/// The identity should include view, document and editor mode. The caller
/// handles viewport scrolling separately from cursor movement.
/// State is four corners plus one reusable, bounded RGBA allocation.
#[derive(Debug)]
pub(crate) struct CursorGraphics<I> {
    last: Option<Observation<I>>,
    animation: Option<Animation>,
    rgba: Vec<u8>,
}

impl<I> Default for CursorGraphics<I> {
    fn default() -> Self {
        Self {
            last: None,
            animation: None,
            rgba: Vec::new(),
        }
    }
}

impl<I: Eq> CursorGraphics<I> {
    pub(crate) fn clear(&mut self) {
        self.last = None;
        self.pause();
    }

    /// Stop painting while retaining the observed cursor as the next origin.
    pub(crate) fn pause(&mut self) {
        self.animation = None;
        self.rgba.clear();
    }

    pub(crate) fn is_active(&self, now: Instant) -> bool {
        self.animation
            .is_some_and(|animation| animation.is_active(now))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        cursor: Option<Position>,
        bounds: Rect,
        identity: I,
        config: &CursorSmearConfig,
        cell_size: CellSize,
        kind: CursorKind,
        width_cells: u16,
        now: Instant,
    ) {
        let Some(cursor) = cursor.filter(|&position| {
            config.enabled
                && kind != CursorKind::Hidden
                && valid_metrics(cell_size)
                && contains(bounds, position)
        }) else {
            self.clear();
            return;
        };
        let settings = Settings::from(config);
        let target = target_quad(cursor, bounds, cell_size, kind, width_cells);
        if let Some(previous) = &self.last {
            if previous.identity != identity
                || previous.bounds != bounds
                || previous.settings != settings
                || previous.cell_size != cell_size
                || previous.kind != kind
            {
                self.animation = None;
            } else if previous.target != target {
                let from = self
                    .animation
                    .map_or(previous.target, |animation| animation.corners(now));
                let maximum = settings.max_distance as f64 * cell_size.width as f64;
                let from = bounded_source(from, target, maximum, bounds, cell_size);
                self.animation = Animation::new(from, target, settings.duration, now);
            } else if !self.is_active(now) {
                self.animation = None;
            }
        }
        self.last = Some(Observation {
            bounds,
            identity,
            settings,
            cell_size,
            kind,
            target,
        });
    }

    pub(crate) fn frame(&mut self, color: Color, now: Instant) -> Option<CursorImage<'_>> {
        let color = color_rgb(color)?;
        let last = self.last.as_ref()?;
        let active = self.is_active(now);
        let mut corners = self
            .animation
            .map_or(last.target, |animation| animation.corners(now));
        if !active {
            self.animation = None;
        }
        let mut polygon = Polygon::from(corners);
        let initial_bounds = polygon.raster_bounds(last.bounds, last.cell_size);
        let raster = if initial_bounds.is_none_or(|bounds| bounds.pixels() > MAX_FRAME_PIXELS) {
            // Extreme settings or large cells must not create megabytes of
            // transient diagonal transparency. A static destination is safe.
            self.animation = None;
            corners = last.target;
            polygon = Polygon::from(corners);
            let raster = polygon.raster_bounds(last.bounds, last.cell_size)?;
            if raster.pixels() > MAX_FRAME_PIXELS {
                return None;
            }
            raster
        } else {
            initial_bounds.unwrap()
        };
        let bytes = raster.pixels() * 4;
        if bytes > self.rgba.capacity() {
            self.rgba.reserve_exact(bytes - self.rgba.len());
        }
        self.rgba.resize(bytes, 0);
        self.rgba.fill(0);
        let opacity = if self
            .animation
            .is_some_and(|animation| animation.is_active(now))
        {
            230u16
        } else {
            255
        };
        let mut painted = false;
        for y in 0..raster.height {
            let spans = [
                polygon.horizontal_span(raster.y as f64 + y as f64 + 0.25),
                polygon.horizontal_span(raster.y as f64 + y as f64 + 0.75),
            ];
            let start = spans
                .iter()
                .flatten()
                .map(|span| span.0)
                .fold(f64::INFINITY, f64::min);
            let end = spans
                .iter()
                .flatten()
                .map(|span| span.1)
                .fold(f64::NEG_INFINITY, f64::max);
            if !start.is_finite() || !end.is_finite() {
                continue;
            }
            let first_x = (start.floor().max(raster.x as f64) as u32) - raster.x;
            let last_x = (end.ceil().min((raster.x + raster.width) as f64) as u32) - raster.x;
            for x in first_x..last_x {
                let mut coverage = 0u16;
                for span in spans.into_iter().flatten() {
                    for offset in [0.25, 0.75] {
                        let pixel_x = raster.x as f64 + x as f64 + offset;
                        coverage += u16::from(pixel_x >= span.0 && pixel_x < span.1);
                    }
                }
                if coverage == 0 {
                    continue;
                }
                painted = true;
                let index = ((y as usize * raster.width as usize) + x as usize) * 4;
                self.rgba[index..index + 4].copy_from_slice(&[
                    color.0,
                    color.1,
                    color.2,
                    ((opacity * coverage + 2) / 4) as u8,
                ]);
            }
        }
        if !painted && self.animation.is_some() {
            // A very thin deformed polygon can fit between sample points.
            // Snap once to the nonempty destination instead of hiding it.
            self.animation = None;
            return self.frame(Color::Rgb(color.0, color.1, color.2), now);
        }
        Some(CursorImage {
            position: Position::new(
                (raster.y / last.cell_size.height as u32) as usize,
                (raster.x / last.cell_size.width as u32) as usize,
            ),
            offset_x: (raster.x % last.cell_size.width as u32) as u16,
            offset_y: (raster.y % last.cell_size.height as u32) as u16,
            width: raster.width,
            height: raster.height,
            rgba: &self.rgba,
        })
    }
}

fn bounded_source(from: Quad, target: Quad, maximum: f64, bounds: Rect, size: CellSize) -> Quad {
    let displacement = from
        .iter()
        .zip(target)
        .map(|(&from, target)| from.distance(target))
        .fold(0.0, f64::max);
    let scale = if displacement > maximum {
        maximum / displacement
    } else {
        1.0
    };
    let source = |scale| std::array::from_fn(|index| target[index].lerp(from[index], scale));
    let fits = |from: Quad| {
        raster_bounds(from.into_iter().chain(target), bounds, size)
            .is_some_and(|raster| raster.pixels() <= MAX_FRAME_PIXELS)
    };
    let limited = if scale == 1.0 { from } else { source(scale) };
    if fits(limited) {
        return limited;
    }
    if !fits(target) {
        return target;
    }
    // Every corner stays inside the source+target bounding box throughout its
    // interpolation. Shortening this box once bounds every subsequent frame,
    // including diagonal motion whose transparent pixels count toward storage.
    let mut low = 0.0;
    let mut high = scale;
    for _ in 0..24 {
        let middle = (low + high) * 0.5;
        if fits(source(middle)) {
            low = middle;
        } else {
            high = middle;
        }
    }
    source(low)
}

fn contains(bounds: Rect, position: Position) -> bool {
    position.col >= bounds.left() as usize
        && position.col < bounds.right() as usize
        && position.row >= bounds.top() as usize
        && position.row < bounds.bottom() as usize
}

fn valid_metrics(size: CellSize) -> bool {
    size.width > 0
        && size.height > 0
        && size.width <= MAX_CELL_PIXELS
        && size.height <= MAX_CELL_PIXELS
}

fn target_quad(
    cursor: Position,
    bounds: Rect,
    size: CellSize,
    kind: CursorKind,
    width_cells: u16,
) -> Quad {
    let x = cursor.col as f64 * size.width as f64;
    let y = cursor.row as f64 * size.height as f64;
    let width_cells = (width_cells.clamp(1, 16) as usize).min(bounds.right() as usize - cursor.col);
    let width = width_cells as f64 * size.width as f64;
    let height = size.height as f64;
    let (x, y, width, height) = match kind {
        CursorKind::Bar => (x, y, (size.width as f64 * 0.15).round().max(1.0), height),
        CursorKind::Underline => {
            let thickness = (height * 0.1).round().max(1.0);
            (x, y + height - thickness, width, thickness)
        }
        CursorKind::Block | CursorKind::Hidden => (x, y, width, height),
    };
    [
        Point { x, y },
        Point { x: x + width, y },
        Point {
            x: x + width,
            y: y + height,
        },
        Point { x, y: y + height },
    ]
}

#[derive(Debug)]
struct Polygon {
    points: [Point; 4],
    len: usize,
}

impl From<Quad> for Polygon {
    fn from(mut points: Quad) -> Self {
        // Convex hull keeps sharp turns/reversal from forming a crossed polygon.
        points.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
        let mut hull = [Point::default(); 8];
        let mut len = 0;
        let cross =
            |a: Point, b: Point, c: Point| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
        for point in points {
            while len >= 2 && cross(hull[len - 2], hull[len - 1], point) <= 0.0 {
                len -= 1;
            }
            hull[len] = point;
            len += 1;
        }
        let lower = len;
        for point in points.into_iter().rev().skip(1) {
            while len > lower && cross(hull[len - 2], hull[len - 1], point) <= 0.0 {
                len -= 1;
            }
            hull[len] = point;
            len += 1;
        }
        len = len.saturating_sub(1).min(4);
        points[..len].copy_from_slice(&hull[..len]);
        Self { points, len }
    }
}

impl Polygon {
    fn raster_bounds(&self, bounds: Rect, size: CellSize) -> Option<RasterBounds> {
        if self.len < 3 {
            return None;
        }
        raster_bounds(self.points[..self.len].iter().copied(), bounds, size)
    }

    fn horizontal_span(&self, y: f64) -> Option<(f64, f64)> {
        let mut first = f64::INFINITY;
        let mut last = f64::NEG_INFINITY;
        for index in 0..self.len {
            let a = self.points[index];
            let b = self.points[(index + 1) % self.len];
            if y >= a.y.min(b.y) && y < a.y.max(b.y) {
                let x = a.x + (b.x - a.x) * (y - a.y) / (b.y - a.y);
                first = first.min(x);
                last = last.max(x);
            }
        }
        first.is_finite().then_some((first, last))
    }
}

fn raster_bounds(
    points: impl IntoIterator<Item = Point>,
    bounds: Rect,
    size: CellSize,
) -> Option<RasterBounds> {
    let mut left = f64::INFINITY;
    let mut top = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for point in points {
        left = left.min(point.x);
        top = top.min(point.y);
        right = right.max(point.x);
        bottom = bottom.max(point.y);
    }
    let x = left.floor().max(bounds.left() as f64 * size.width as f64) as u32;
    let y = top.floor().max(bounds.top() as f64 * size.height as f64) as u32;
    let right = right.ceil().min(bounds.right() as f64 * size.width as f64) as u32;
    let bottom = bottom
        .ceil()
        .min(bounds.bottom() as f64 * size.height as f64) as u32;
    (right > x && bottom > y).then_some(RasterBounds {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    })
}

#[derive(Clone, Copy)]
struct RasterBounds {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl RasterBounds {
    fn pixels(self) -> usize {
        (self.width as usize).saturating_mul(self.height as usize)
    }
}

pub(crate) fn color_rgb(color: Color) -> Option<(u8, u8, u8)> {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let index = match color {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => return Some((r, g, b)),
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::LightGray => 7,
        Color::Gray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(index) => index,
    };
    Some(match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let index = index - 16;
            let level = |value| if value == 0 { 0 } else { 55 + 40 * value };
            (level(index / 36), level(index / 6 % 6), level(index % 6))
        }
        _ => {
            let gray = 8 + 10 * (index - 232);
            (gray, gray, gray)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellSize = CellSize {
        width: 10,
        height: 20,
    };

    fn config() -> CursorSmearConfig {
        CursorSmearConfig {
            enabled: true,
            ..CursorSmearConfig::default()
        }
    }

    fn update(graphics: &mut CursorGraphics<u8>, position: Position, now: Instant) {
        graphics.update(
            Some(position),
            Rect::new(2, 1, 60, 30),
            0,
            &config(),
            CELL,
            CursorKind::Block,
            1,
            now,
        );
    }

    #[test]
    fn static_cursor_shapes_use_exact_cell_pixels_and_remain_visible_at_rest() {
        let now = Instant::now();
        for (kind, width, height, offset_y) in [
            (CursorKind::Block, 20, 20, 0),
            (CursorKind::Bar, 2, 20, 0),
            (CursorKind::Underline, 20, 2, 18),
        ] {
            let mut graphics = CursorGraphics::default();
            graphics.update(
                Some(Position::new(4, 8)),
                Rect::new(2, 1, 60, 30),
                0u8,
                &config(),
                CELL,
                kind,
                2,
                now,
            );
            assert!(!graphics.is_active(now));
            let image = graphics
                .frame(Color::Rgb(12, 34, 56), now + Duration::from_secs(1))
                .unwrap();
            assert_eq!(image.position, Position::new(4, 8));
            assert_eq!(
                (image.width, image.height, image.offset_x, image.offset_y),
                (width, height, 0, offset_y)
            );
            assert_eq!(image.rgba.len(), width as usize * height as usize * 4);
            assert!(image
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [12, 34, 56, 255]));
        }
    }

    #[test]
    fn front_corners_arrive_first_and_rasterization_keeps_subcell_motion_and_transparency() {
        let now = Instant::now();
        let mut graphics = CursorGraphics::default();
        update(&mut graphics, Position::new(3, 4), now);
        update(&mut graphics, Position::new(3, 14), now);
        let sampled = now + Duration::from_millis(30);
        let corners = graphics.animation.unwrap().corners(sampled);
        assert_eq!(corners[1].x, 150.0);
        assert_eq!(corners[2].x, 150.0);
        assert!(corners[0].x < 140.0);
        assert!(corners[3].x < 140.0);
        let image = graphics.frame(Color::White, sampled).unwrap();
        assert!(image.width > CELL.width as u32);
        assert_ne!(image.offset_x, 0);
        assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
        assert!(graphics.is_active(sampled));
        let settled = now + Duration::from_millis(120);
        assert!(!graphics.is_active(settled));
        let image = graphics.frame(Color::White, settled).unwrap();
        assert_eq!(image.position, Position::new(3, 14));
        assert_eq!((image.width, image.height), (10, 20));
        assert!(image.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn far_jumps_animate_with_capped_trails_and_settle_at_the_destination() {
        let now = Instant::now();
        for destination in [
            Position::new(2, 60),
            Position::new(30, 3),
            Position::new(30, 60),
        ] {
            let mut graphics = CursorGraphics::default();
            update(&mut graphics, Position::new(2, 3), now);
            let origin = graphics.last.as_ref().unwrap().target;
            update(&mut graphics, destination, now);
            let animation = graphics.animation.unwrap();
            let maximum = config().max_distance as f64 * CELL.width as f64;
            assert!(origin[0].distance(animation.to[0]) > maximum);
            for (from, to) in animation.from.into_iter().zip(animation.to) {
                assert!((from.distance(to) - maximum).abs() < 0.00001);
            }
            assert!(graphics.is_active(now));
            let during = now + Duration::from_millis(30);
            let image = graphics.frame(Color::White, during).unwrap();
            assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert!(image.rgba.len() <= MAX_FRAME_PIXELS * 4);
            assert!(graphics.is_active(during));
            let settled = now + Duration::from_millis(120);
            let image = graphics.frame(Color::White, settled).unwrap();
            assert_eq!(image.position, destination);
            assert_eq!((image.width, image.height), (10, 20));
            assert!(image.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
            assert!(!graphics.is_active(settled));
        }
    }

    #[test]
    fn high_dpi_diagonal_trails_shorten_to_fit_every_animation_frame() {
        let now = Instant::now();
        let bounds = Rect::new(0, 0, 80, 40);
        let size = CellSize {
            width: 48,
            height: 96,
        };
        let config = CursorSmearConfig {
            max_distance: 256,
            ..config()
        };
        let mut graphics = CursorGraphics::default();
        for cursor in [Position::new(1, 1), Position::new(38, 78)] {
            graphics.update(
                Some(cursor),
                bounds,
                0u8,
                &config,
                size,
                CursorKind::Block,
                1,
                now,
            );
        }
        let animation = graphics.animation.unwrap();
        assert!(animation.from[0].distance(animation.to[0]) > size.width as f64);
        let complete_bounds =
            raster_bounds(animation.from.into_iter().chain(animation.to), bounds, size).unwrap();
        assert!(complete_bounds.pixels() <= MAX_FRAME_PIXELS);
        for millis in (0..120).step_by(10) {
            let sampled = now + Duration::from_millis(millis);
            let image = graphics.frame(Color::White, sampled).unwrap();
            assert!(image.rgba.len() <= MAX_FRAME_PIXELS * 4);
            assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert!(graphics.is_active(sampled));
        }
        assert!(graphics.rgba.capacity() <= MAX_FRAME_PIXELS * 4);
        let image = graphics
            .frame(Color::White, now + Duration::from_millis(120))
            .unwrap();
        assert_eq!(image.position, Position::new(38, 78));
    }

    #[test]
    fn repeated_far_retargets_keep_the_trail_and_storage_bounded() {
        let now = Instant::now();
        let mut graphics = CursorGraphics::default();
        update(&mut graphics, Position::new(2, 3), now);
        for turn in 0..12 {
            let sampled = now + Duration::from_millis(turn * 10);
            let destination = if turn % 2 == 0 {
                Position::new(30, 60)
            } else {
                Position::new(2, 3)
            };
            update(&mut graphics, destination, sampled);
            let animation = graphics.animation.unwrap();
            let maximum = config().max_distance as f64 * CELL.width as f64;
            assert!(animation
                .from
                .into_iter()
                .zip(animation.to)
                .all(|(from, to)| from.distance(to) <= maximum + 0.00001));
            let image = graphics.frame(Color::White, sampled).unwrap();
            assert!(image.rgba.len() <= MAX_FRAME_PIXELS * 4);
            assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert!(graphics.is_active(sampled));
        }
        assert!(graphics.rgba.capacity() <= MAX_FRAME_PIXELS * 4);
    }

    #[test]
    fn pausing_retains_the_origin_for_the_next_move_but_context_changes_reset_it() {
        let now = Instant::now();
        let mut graphics = CursorGraphics::default();
        update(&mut graphics, Position::new(3, 4), now);
        update(&mut graphics, Position::new(3, 14), now);
        graphics
            .frame(Color::White, now + Duration::from_millis(20))
            .unwrap();
        let previous_target = graphics.last.as_ref().unwrap().target;
        graphics.pause();
        assert!(!graphics.is_active(now));
        assert!(graphics.rgba.is_empty());
        update(
            &mut graphics,
            Position::new(3, 24),
            now + Duration::from_millis(30),
        );
        assert_eq!(graphics.animation.unwrap().from, previous_target);
        graphics.pause();
        graphics.update(
            Some(Position::new(3, 34)),
            Rect::new(2, 1, 60, 30),
            1u8,
            &config(),
            CELL,
            CursorKind::Block,
            1,
            now + Duration::from_millis(40),
        );
        assert!(!graphics.is_active(now + Duration::from_millis(40)));
        assert!(graphics.last.is_some());
        graphics.clear();
        assert!(graphics.last.is_none());
    }

    #[test]
    fn retargeting_preserves_current_corners_and_reversal_frames_stay_visible() {
        let now = Instant::now();
        let mut graphics = CursorGraphics::default();
        update(&mut graphics, Position::new(3, 4), now);
        update(&mut graphics, Position::new(6, 18), now);
        let later = now + Duration::from_millis(20);
        let corners = graphics.animation.unwrap().corners(later);
        update(&mut graphics, Position::new(2, 3), later);
        assert_eq!(graphics.animation.unwrap().from, corners);
        for millis in 0..=120 {
            let image = graphics
                .frame(Color::White, later + Duration::from_millis(millis))
                .unwrap();
            assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
        }
        update(
            &mut graphics,
            Position::new(4, 14),
            later + Duration::from_millis(150),
        );
        let target = graphics.last.as_ref().unwrap().target;
        graphics.animation = Some(Animation {
            from: [Point { x: 30.0, y: 40.0 }; 4],
            to: target,
            started: later,
            durations: [Duration::from_millis(120); 4],
        });
        let image = graphics.frame(Color::White, later).unwrap();
        assert!(image.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
        assert!(!graphics.is_active(later));
    }

    #[test]
    fn subpixel_needle_without_sample_coverage_snaps_to_opaque_destination() {
        let now = Instant::now();
        let mut graphics = CursorGraphics::default();
        update(&mut graphics, Position::new(4, 14), now);
        let needle = [
            Point { x: 30.0, y: 40.05 },
            Point { x: 80.0, y: 40.05 },
            Point { x: 80.0, y: 40.15 },
            Point { x: 30.0, y: 40.15 },
        ];
        let observation = graphics.last.as_ref().unwrap();
        assert!(Polygon::from(needle)
            .raster_bounds(observation.bounds, observation.cell_size)
            .is_some());
        graphics.animation = Some(Animation {
            from: needle,
            to: observation.target,
            started: now,
            durations: [Duration::from_millis(120); 4],
        });
        let image = graphics.frame(Color::Rgb(20, 40, 60), now).unwrap();
        assert_eq!(image.position, Position::new(4, 14));
        assert_eq!((image.width, image.height), (10, 20));
        assert!(image
            .rgba
            .chunks_exact(4)
            .all(|pixel| pixel == [20, 40, 60, 255]));
        assert!(!graphics.is_active(now));
    }

    #[test]
    fn trail_distance_uses_measured_aspect_and_context_changes_snap_without_animation() {
        let now = Instant::now();
        let bounds = Rect::new(0, 0, 80, 30);
        let config = CursorSmearConfig {
            max_distance: 3,
            ..config()
        };
        let mut graphics = CursorGraphics::default();
        graphics.update(
            Some(Position::new(2, 2)),
            bounds,
            0u8,
            &config,
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        graphics.update(
            Some(Position::new(3, 2)),
            bounds,
            0,
            &config,
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        assert!(graphics.is_active(now));
        graphics.update(
            Some(Position::new(6, 2)),
            bounds,
            0,
            &config,
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        assert!(graphics.is_active(now));
        let animation = graphics.animation.unwrap();
        assert!((animation.from[0].distance(animation.to[0]) - 30.0).abs() < 0.00001);
        graphics.update(
            Some(Position::new(6, 3)),
            bounds,
            1,
            &config,
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        assert!(!graphics.is_active(now));
        graphics.update(
            Some(Position::new(6, 4)),
            Rect::new(0, 0, 79, 30),
            1,
            &config,
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        assert!(!graphics.is_active(now));
        let other = CellSize {
            width: 12,
            height: 24,
        };
        graphics.update(
            Some(Position::new(6, 5)),
            bounds,
            1,
            &config,
            other,
            CursorKind::Bar,
            1,
            now,
        );
        assert!(!graphics.is_active(now));
        graphics.update(None, bounds, 1, &config, other, CursorKind::Bar, 1, now);
        assert!(graphics.frame(Color::White, now).is_none());
        graphics.update(
            Some(Position::new(6, 5)),
            bounds,
            1,
            &CursorSmearConfig::default(),
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        assert!(graphics.frame(Color::White, now).is_none());
        graphics.update(
            Some(Position::new(6, 5)),
            bounds,
            1,
            &config,
            CellSize {
                width: 0,
                height: 20,
            },
            CursorKind::Block,
            1,
            now,
        );
        assert!(graphics.frame(Color::White, now).is_none());
    }

    #[test]
    fn raster_is_clipped_and_large_deformations_have_bounded_reusable_storage() {
        let now = Instant::now();
        let bounds = Rect::new(2, 1, 30, 20);
        let mut graphics = CursorGraphics::default();
        graphics.update(
            Some(Position::new(1, 2)),
            bounds,
            0u8,
            &config(),
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        graphics.update(
            Some(Position::new(10, 24)),
            bounds,
            0,
            &config(),
            CELL,
            CursorKind::Block,
            1,
            now,
        );
        let image = graphics
            .frame(Color::White, now + Duration::from_millis(25))
            .unwrap();
        let x = image.position.col as u32 * CELL.width as u32 + image.offset_x as u32;
        let y = image.position.row as u32 * CELL.height as u32 + image.offset_y as u32;
        assert!(x >= bounds.left() as u32 * CELL.width as u32);
        assert!(y >= bounds.top() as u32 * CELL.height as u32);
        assert!(x + image.width <= bounds.right() as u32 * CELL.width as u32);
        assert!(y + image.height <= bounds.bottom() as u32 * CELL.height as u32);
        let huge = CellSize {
            width: 512,
            height: 512,
        };
        let config = CursorSmearConfig {
            max_distance: 256,
            ..config()
        };
        graphics.update(
            Some(Position::new(1, 2)),
            bounds,
            0,
            &config,
            huge,
            CursorKind::Block,
            1,
            now,
        );
        graphics.update(
            Some(Position::new(10, 24)),
            bounds,
            0,
            &config,
            huge,
            CursorKind::Block,
            1,
            now,
        );
        let image = graphics
            .frame(Color::White, now + Duration::from_millis(30))
            .unwrap();
        assert_eq!(image.position, Position::new(10, 24));
        assert_eq!(image.rgba.len(), MAX_FRAME_PIXELS * 4);
        assert!(!graphics.is_active(now + Duration::from_millis(30)));
        assert!(graphics.rgba.capacity() <= MAX_FRAME_PIXELS * 4);
        let allocation = graphics.rgba.as_ptr();
        graphics
            .frame(Color::Red, now + Duration::from_millis(40))
            .unwrap();
        assert_eq!(graphics.rgba.as_ptr(), allocation);
        graphics.clear();
        assert!(graphics.frame(Color::White, now).is_none());
    }

    #[test]
    fn standard_palette_and_runtime_clamps_match_existing_configuration() {
        assert_eq!(color_rgb(Color::Gray), color_rgb(Color::Indexed(8)));
        assert_eq!(color_rgb(Color::LightGray), color_rgb(Color::Indexed(7)));
        assert_eq!(color_rgb(Color::Indexed(196)), Some((255, 0, 0)));
        assert_eq!(color_rgb(Color::Reset), None);
        let low = Settings::from(&CursorSmearConfig {
            duration: 0,
            max_distance: 0,
            ..config()
        });
        let high = Settings::from(&CursorSmearConfig {
            duration: u64::MAX,
            max_distance: u16::MAX,
            ..config()
        });
        assert_eq!(low.duration, Duration::from_millis(16));
        assert_eq!(high.duration, Duration::from_millis(1000));
        assert_eq!((low.max_distance, high.max_distance), (1, 256));
    }
}
