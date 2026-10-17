use std::sync::Arc;

use eframe::egui::{pos2, vec2, Color32, Pos2, Rect, TextureHandle, Vec2};

/// Uncropped image, in normalized texture coordinates.
pub const FULL_CROP: Rect = Rect {
    min: Pos2 { x: 0.0, y: 0.0 },
    max: Pos2 { x: 1.0, y: 1.0 },
};

/// A decoded image living on the GPU, plus its original encoded bytes for saving / copying.
pub struct ImageAsset {
    pub bytes: Arc<Vec<u8>>,
    pub texture: TextureHandle,
    /// Original pixel size (the texture may be downscaled).
    pub size: Vec2,
}

#[derive(Clone)]
pub struct StrokeData {
    /// Points in local unscaled coordinates, relative to the item center.
    pub points: Arc<Vec<Pos2>>,
    pub color: Color32,
    pub width: f32,
}

#[derive(Clone)]
pub enum Content {
    Image(Arc<ImageAsset>),
    Stroke(StrokeData),
}

#[derive(Clone)]
pub struct Item {
    pub id: u64,
    pub content: Content,
    pub center: Pos2,
    /// Local, unscaled size.
    pub size: Vec2,
    pub scale: f32,
    pub rotation: f32,
    pub flip_x: bool,
    pub flip_y: bool,
    pub selected: bool,
    /// Visible part of an image, in normalized texture coordinates. Unused by other content.
    pub crop: Rect,
    /// Caption shown in a bar above the item; empty means no bar.
    pub title: String,
    /// Label colour of the title bar and card border (investigation-board style grouping).
    pub tag: Option<Color32>,
}

/// A connection between two items (by id), drawn as a line between their edges.
#[derive(Clone)]
pub struct Link {
    pub from: u64,
    pub to: u64,
    pub color: Color32,
    /// Draw an arrowhead at the `to` end.
    pub arrow: bool,
    pub selected: bool,
}

/// Title bar height relative to the card size (sqrt of its area), and its minimum in board pixels.
const TITLE_FRAC: f32 = 0.08;
const MIN_TITLE: f32 = 16.0;

pub fn rotate(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    vec2(v.x * c - v.y * s, v.x * s + v.y * c)
}

impl Item {
    pub fn new(id: u64, content: Content, center: Pos2, size: Vec2) -> Self {
        Self {
            id,
            content,
            center,
            size,
            scale: 1.0,
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            selected: false,
            crop: FULL_CROP,
            title: String::new(),
            tag: None,
        }
    }

    /// Height (world units) the title bar has, or would have once a title is set.
    pub fn title_bar_height(&self) -> f32 {
        let s = self.size * self.scale;
        ((s.x * s.y).abs().sqrt() * TITLE_FRAC).max(MIN_TITLE)
    }

    /// Height of the title bar above the item; 0 without a title.
    pub fn title_height(&self) -> f32 {
        if self.title.is_empty() {
            0.0
        } else {
            self.title_bar_height()
        }
    }

    /// `w` in the item's rotated (but unscaled, unflipped) frame, relative to the center.
    fn rotated_offset(&self, w: Pos2) -> Vec2 {
        rotate(w - self.center, -self.rotation)
    }

    /// Whether `w` lies on the title bar.
    pub fn hit_title(&self, w: Pos2) -> bool {
        let tb = self.title_height();
        let l = self.rotated_offset(w);
        let h = self.half();
        tb > 0.0 && l.x.abs() <= h.x && l.y < -h.y && l.y >= -h.y - tb
    }

    /// World-space corners of the item including its title bar (TL, TR, BR, BL).
    pub fn card_corners(&self) -> [Pos2; 4] {
        let h = self.half();
        let top = -h.y - self.title_height();
        [(-h.x, top), (h.x, top), (h.x, h.y), (-h.x, h.y)]
            .map(|(x, y)| self.center + rotate(vec2(x, y), self.rotation))
    }

    /// Where the line from the center towards `target` leaves the card (item plus title bar).
    /// `None` when `target` is inside the card.
    pub fn edge_toward(&self, target: Pos2) -> Option<Pos2> {
        let d = target - self.center;
        let l = rotate(d, -self.rotation);
        let h = self.half();
        let tx = if l.x != 0.0 { h.x / l.x.abs() } else { f32::INFINITY };
        let ty = if l.y < 0.0 {
            (h.y + self.title_height()) / -l.y
        } else if l.y > 0.0 {
            h.y / l.y
        } else {
            f32::INFINITY
        };
        let t = tx.min(ty);
        (t < 1.0).then(|| self.center + d * t)
    }

    pub fn is_stroke(&self) -> bool {
        matches!(self.content, Content::Stroke(_))
    }

    /// Visible part of an image of source size `full`, in source pixels.
    pub fn crop_px(&self, full: Vec2) -> Rect {
        Rect::from_min_max(
            pos2(self.crop.min.x * full.x, self.crop.min.y * full.y),
            pos2(self.crop.max.x * full.x, self.crop.max.y * full.y),
        )
    }

    /// Source pixel -> world position.
    pub fn px_to_world(&self, full: Vec2, px: Pos2) -> Pos2 {
        self.local_to_world(px - self.crop_px(full).center())
    }

    /// World position -> source pixel.
    pub fn world_to_px(&self, full: Vec2, w: Pos2) -> Pos2 {
        self.crop_px(full).center() + self.world_to_local(w)
    }

    /// Show region `px` (source pixels) of the image, starting from the state in `from`.
    /// With `pin_content` the picture stays put on the board and the frame changes (dragging crop
    /// edges); otherwise the frame stays put and the picture slides inside it.
    pub fn set_crop_px(&mut self, from: &Item, full: Vec2, px: Rect, pin_content: bool) {
        let old = from.crop_px(full);
        self.center = if pin_content {
            from.local_to_world(px.center() - old.center())
        } else {
            from.center
        };
        self.size = px.size();
        self.crop = Rect::from_min_max(
            pos2(px.min.x / full.x, px.min.y / full.y),
            pos2(px.max.x / full.x, px.max.y / full.y),
        );
    }

    /// Half extents in world units.
    pub fn half(&self) -> Vec2 {
        self.size * self.scale * 0.5
    }

    /// Local unscaled offset (relative to center) -> world position. Applies flip, scale, rotation.
    pub fn local_to_world(&self, p: Vec2) -> Pos2 {
        let p = vec2(
            if self.flip_x { -p.x } else { p.x },
            if self.flip_y { -p.y } else { p.y },
        ) * self.scale;
        self.center + rotate(p, self.rotation)
    }

    /// World position -> local unscaled offset relative to center.
    pub fn world_to_local(&self, w: Pos2) -> Vec2 {
        let p = rotate(w - self.center, -self.rotation) / self.scale;
        vec2(
            if self.flip_x { -p.x } else { p.x },
            if self.flip_y { -p.y } else { p.y },
        )
    }

    /// World-space corners of the oriented bounding box (TL, TR, BR, BL), ignoring flips.
    pub fn corners(&self) -> [Pos2; 4] {
        let h = self.half();
        [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .map(|(sx, sy)| self.center + rotate(vec2(sx * h.x, sy * h.y), self.rotation))
    }

    /// Bounds including the title bar.
    pub fn world_aabb(&self) -> Rect {
        Rect::from_points(&self.card_corners())
    }

    /// Hit test in world space. `tol` is a world-space tolerance used for thin strokes.
    pub fn hit(&self, w: Pos2, tol: f32) -> bool {
        if self.hit_title(w) {
            return true;
        }
        let l = self.world_to_local(w);
        let h = self.size * 0.5;
        match &self.content {
            Content::Stroke(s) => {
                let tol_local = tol / self.scale;
                if l.x.abs() > h.x + tol_local || l.y.abs() > h.y + tol_local {
                    return false;
                }
                let r = s.width * 0.5 + tol_local;
                polyline_distance(&s.points, l.to_pos2()) <= r
            }
            _ => l.x.abs() <= h.x && l.y.abs() <= h.y,
        }
    }
}

pub fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return p.distance(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.distance(a + ab * t)
}

pub fn polyline_distance(points: &[Pos2], p: Pos2) -> f32 {
    match points {
        [] => f32::INFINITY,
        [a] => p.distance(*a),
        _ => points
            .windows(2)
            .map(|w| segment_distance(p, w[0], w[1]))
            .fold(f32::INFINITY, f32::min),
    }
}

/// Endpoints of a link line from `a` to `b`, clipped to both cards. `None` when they overlap.
pub fn link_ends(a: &Item, b: &Item) -> Option<(Pos2, Pos2)> {
    let pa = a.edge_toward(b.center)?;
    let pb = b.edge_toward(a.center)?;
    ((pb - pa).dot(b.center - a.center) > 0.0).then_some((pa, pb))
}

/// Build a stroke item from world-space points. Returns `(center, size, local_points)`.
pub fn stroke_geometry(world_points: &[Pos2], width: f32) -> (Pos2, Vec2, Vec<Pos2>) {
    let bbox = Rect::from_points(world_points).expand(width * 0.5);
    let center = bbox.center();
    let local = world_points
        .iter()
        .map(|p| pos2(p.x - center.x, p.y - center.y))
        .collect();
    (center, bbox.size(), local)
}

/// An oriented rectangle used for the selection frame.
#[derive(Clone, Copy)]
pub struct Frame {
    pub center: Pos2,
    pub half: Vec2,
    pub rotation: f32,
}

impl Frame {
    pub fn corners(&self) -> [Pos2; 4] {
        [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)].map(|(sx, sy)| {
            self.center + rotate(vec2(sx * self.half.x, sy * self.half.y), self.rotation)
        })
    }

    pub fn contains(&self, w: Pos2) -> bool {
        let l = rotate(w - self.center, -self.rotation);
        l.x.abs() <= self.half.x && l.y.abs() <= self.half.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_item() -> Item {
        let mut it = Item::new(1, Content::Stroke(StrokeData {
            points: Arc::new(vec![]),
            color: Color32::WHITE,
            width: 1.0,
        }), pos2(100.0, 50.0), vec2(400.0, 200.0));
        it.scale = 2.0;
        it.rotation = 0.7;
        it.flip_x = true;
        it
    }

    #[test]
    fn cropping_keeps_content_in_place() {
        let full = vec2(400.0, 200.0);
        let orig = image_item();
        // A source pixel's world position must not change when the crop edges move.
        let probe = pos2(250.0, 120.0);
        let before = orig.px_to_world(full, probe);
        let mut it = orig.clone();
        it.set_crop_px(&orig, full, Rect::from_min_max(pos2(200.0, 100.0), pos2(380.0, 190.0)), true);
        assert_eq!(it.size, vec2(180.0, 90.0));
        assert!(it.px_to_world(full, probe).distance(before) < 1e-3);
        assert!(it.world_to_px(full, before).distance(probe) < 1e-3);
        // Uncropping again restores the original geometry.
        let cropped = it.clone();
        it.set_crop_px(&cropped, full, Rect::from_min_size(Pos2::ZERO, full), true);
        assert!(it.center.distance(orig.center) < 1e-3);
        assert_eq!(it.crop, FULL_CROP);
    }

    #[test]
    fn title_bar_extends_hit_area_and_bounds() {
        let mut it = Item::new(1, image_item().content, pos2(0.0, 0.0), vec2(200.0, 100.0));
        let above = pos2(0.0, -55.0);
        assert!(!it.hit(above, 0.0));
        let plain = it.world_aabb();
        it.title = "Suspect".into();
        let tb = it.title_height();
        assert!(tb >= MIN_TITLE);
        assert!(it.hit_title(above) && it.hit(above, 0.0));
        assert!(!it.hit_title(pos2(0.0, 0.0)));
        assert_eq!(it.world_aabb().min.y, plain.min.y - tb);
        assert_eq!(it.world_aabb().max, plain.max);
    }

    #[test]
    fn links_end_at_card_edges() {
        let content = image_item().content;
        let a = Item::new(1, content.clone(), pos2(0.0, 0.0), vec2(100.0, 100.0));
        let mut b = Item::new(2, content, pos2(300.0, 0.0), vec2(100.0, 100.0));
        let (pa, pb) = link_ends(&a, &b).unwrap();
        assert!(pa.distance(pos2(50.0, 0.0)) < 1e-3);
        assert!(pb.distance(pos2(250.0, 0.0)) < 1e-3);
        // Vertically, a title bar on the lower card moves the end up to the top of the bar.
        b.center = pos2(0.0, 300.0);
        b.title = "x".into();
        let (_, pb) = link_ends(&a, &b).unwrap();
        assert!((pb.y - (250.0 - b.title_height())).abs() < 1e-3);
        // Overlapping cards have no line.
        b.center = pos2(20.0, 20.0);
        assert!(link_ends(&a, &b).is_none());
    }

    #[test]
    fn sliding_keeps_frame_in_place() {
        let full = vec2(400.0, 200.0);
        let mut orig = image_item();
        orig.set_crop_px(&orig.clone(), full, Rect::from_min_max(pos2(0.0, 0.0), pos2(200.0, 100.0)), true);
        let mut it = orig.clone();
        it.set_crop_px(&orig, full, Rect::from_min_max(pos2(100.0, 50.0), pos2(300.0, 150.0)), false);
        assert_eq!(it.center, orig.center);
        assert_eq!(it.size, orig.size);
    }
}
