use std::sync::Arc;

use eframe::egui::{pos2, vec2, Color32, Pos2, Rect, TextureHandle, Vec2};

/// Inner padding of a note, in local (unscaled) units.
pub const NOTE_PAD: f32 = 10.0;
pub const NOTE_DEFAULT_WIDTH: f32 = 260.0;
pub const NOTE_DEFAULT_FONT: f32 = 18.0;
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
pub struct Note {
    pub text: String,
    pub bg: Color32,
    pub fg: Color32,
    pub font_size: f32,
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
    Note(Note),
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
}

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
        }
    }

    pub fn is_note(&self) -> bool {
        matches!(self.content, Content::Note(_))
    }

    pub fn is_stroke(&self) -> bool {
        matches!(self.content, Content::Stroke(_))
    }

    pub fn note_mut(&mut self) -> Option<&mut Note> {
        match &mut self.content {
            Content::Note(n) => Some(n),
            _ => None,
        }
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

    pub fn world_aabb(&self) -> Rect {
        Rect::from_points(&self.corners())
    }

    /// Hit test in world space. `tol` is a world-space tolerance used for thin strokes.
    pub fn hit(&self, w: Pos2, tol: f32) -> bool {
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
