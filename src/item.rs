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
    /// Which edge of the item the title bar is attached to, in local coordinates (see
    /// `EDGE_NORMALS`). Chosen when the title is first set: the edge that was on top then.
    pub title_edge: u8,
}

/// Outward normals of the local edges: top, right, bottom, left.
const EDGE_NORMALS: [Vec2; 4] = [vec2(0.0, -1.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(-1.0, 0.0)];

/// A title bar in world space. `origin` is the outer corner where the text starts; the text runs
/// along `along` and its "up" is `up`, which points away from the item.
#[derive(Clone, Copy, Debug)]
pub struct TitleBar {
    pub origin: Pos2,
    pub along: Vec2,
    pub up: Vec2,
    pub length: f32,
    pub height: f32,
}

impl TitleBar {
    /// Outer edge first (where the text starts, where it ends), then the inner edge.
    pub fn corners(&self) -> [Pos2; 4] {
        let end = self.along * self.length;
        let inner = -self.up * self.height;
        [self.origin, self.origin + end, self.origin + end + inner, self.origin + inner]
    }

    /// The colour square at the end of the bar: center and half side. `None` when the bar is
    /// too short to fit it next to some text.
    pub fn swatch(&self) -> Option<(Pos2, f32)> {
        (self.length >= self.height * 3.0).then(|| {
            let c = self.origin + self.along * (self.length - self.height * 0.5)
                - self.up * (self.height * 0.5);
            (c, self.height * 0.3)
        })
    }

    pub fn hit_swatch(&self, w: Pos2) -> bool {
        self.swatch().is_some_and(|(c, q)| {
            let r = w - c;
            // A little extra margin: the square is small.
            let q = q * 1.4;
            r.dot(self.along).abs() <= q && r.dot(self.up).abs() <= q
        })
    }

    /// Room the text may use, leaving space for the colour square.
    pub fn text_length(&self, pad: f32) -> f32 {
        let swatch = if self.swatch().is_some() { self.height } else { 0.0 };
        self.length - pad * 2.0 - swatch
    }

    pub fn contains(&self, w: Pos2) -> bool {
        let r = w - self.origin;
        let (a, u) = (r.dot(self.along), r.dot(self.up));
        (0.0..=self.length).contains(&a) && (-self.height..=0.0).contains(&u)
    }
}

/// A connection between two items (by id): a line between two pins stuck to the items.
#[derive(Clone)]
pub struct Link {
    pub from: u64,
    pub to: u64,
    /// Pin positions in each item's content coordinates (see `Item::world_to_content`).
    pub from_at: Pos2,
    pub to_at: Pos2,
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
            title_edge: 0,
        }
    }

    /// World direction of a local direction (applies flips and rotation, not scale).
    fn local_dir_to_world(&self, d: Vec2) -> Vec2 {
        let d = vec2(
            if self.flip_x { -d.x } else { d.x },
            if self.flip_y { -d.y } else { d.y },
        );
        rotate(d, self.rotation)
    }

    /// The local edge that currently faces up on the board.
    pub fn upward_edge(&self) -> u8 {
        (0..4u8)
            .max_by(|&a, &b| {
                let up = |e: u8| -self.local_dir_to_world(EDGE_NORMALS[e as usize]).y;
                up(a).total_cmp(&up(b))
            })
            .unwrap()
    }

    /// The title bar on local edge `edge`, whether or not the item has a title.
    pub fn title_bar_on(&self, edge: u8) -> TitleBar {
        let n_local = EDGE_NORMALS[edge as usize % 4];
        let h = self.half();
        let (depth, length) = if n_local.x == 0.0 { (h.y, h.x * 2.0) } else { (h.x, h.y * 2.0) };
        let height = self.title_bar_height();
        let up = self.local_dir_to_world(n_local);
        // Text reads left to right when `up` points up the screen, and is never mirrored.
        let along = vec2(-up.y, up.x);
        TitleBar {
            origin: self.center + up * (depth + height) - along * (length * 0.5),
            along,
            up,
            length,
            height,
        }
    }

    /// The title bar, if the item has a title.
    pub fn title_bar(&self) -> Option<TitleBar> {
        (!self.title.is_empty()).then(|| self.title_bar_on(self.title_edge))
    }

    /// Height (world units) the title bar has, or would have once a title is set.
    pub fn title_bar_height(&self) -> f32 {
        let s = self.size * self.scale;
        ((s.x * s.y).abs().sqrt() * TITLE_FRAC).max(MIN_TITLE)
    }

    /// Whether `w` lies on the title bar.
    pub fn hit_title(&self, w: Pos2) -> bool {
        self.title_bar().is_some_and(|b| b.contains(w))
    }

    /// A world point in content coordinates: source pixels for images (so it stays on the same
    /// spot of the picture through cropping), local unscaled coordinates for strokes.
    pub fn world_to_content(&self, w: Pos2) -> Pos2 {
        match &self.content {
            Content::Image(a) => self.world_to_px(a.size, w),
            Content::Stroke(_) => self.world_to_local(w).to_pos2(),
        }
    }

    pub fn content_to_world(&self, p: Pos2) -> Pos2 {
        match &self.content {
            Content::Image(a) => self.px_to_world(a.size, p),
            Content::Stroke(_) => self.local_to_world(p.to_vec2()),
        }
    }

    /// The nearest content point that is visible (inside the crop / the stroke's box).
    pub fn clamp_content(&self, p: Pos2) -> Pos2 {
        match &self.content {
            Content::Image(a) => self.crop_px(a.size).clamp(p),
            Content::Stroke(_) => Rect::from_center_size(Pos2::ZERO, self.size).clamp(p),
        }
    }

    /// Where a pin at content point `p` sits on the board; kept on the visible part of the item.
    pub fn pin_world(&self, p: Pos2) -> Pos2 {
        self.content_to_world(self.clamp_content(p))
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
        let r = Rect::from_points(&self.corners());
        match self.title_bar() {
            Some(b) => r.union(Rect::from_points(&b.corners())),
            None => r,
        }
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
        let tb = it.title_bar_height();
        assert!(tb >= MIN_TITLE);
        assert!(it.hit_title(above) && it.hit(above, 0.0));
        assert!(!it.hit_title(pos2(0.0, 0.0)));
        assert!((it.world_aabb().min.y - (plain.min.y - tb)).abs() < 1e-3);
        assert!(it.world_aabb().max.distance(plain.max) < 1e-3);
    }

    #[test]
    fn title_goes_on_the_edge_that_was_on_top_and_stays_there() {
        let mut it = Item::new(1, image_item().content, pos2(0.0, 0.0), vec2(200.0, 100.0));
        let quarter = std::f32::consts::FRAC_PI_2;
        // Rotated a quarter turn clockwise, the local left edge faces up.
        it.rotation = quarter;
        it.title_edge = it.upward_edge();
        assert_eq!(it.title_edge, 3);
        it.title = "x".into();
        let bar = it.title_bar().unwrap();
        // Upright, readable text along the top of the rotated image (now 100 wide, 200 tall).
        assert!((bar.up - vec2(0.0, -1.0)).length() < 1e-5);
        assert!((bar.along - vec2(1.0, 0.0)).length() < 1e-5);
        assert!(bar.origin.distance(pos2(-50.0, -100.0 - bar.height)) < 1e-3);
        assert!((bar.length - 100.0).abs() < 1e-3);
        // Rotating back carries the bar to the image's left side, reading bottom to top.
        it.rotation = 0.0;
        let bar = it.title_bar().unwrap();
        assert!((bar.up - vec2(-1.0, 0.0)).length() < 1e-5);
        assert!((bar.along - vec2(0.0, -1.0)).length() < 1e-5);
        // A horizontal flip moves it to the right side, with the text still not mirrored.
        it.flip_x = true;
        let bar = it.title_bar().unwrap();
        assert!((bar.up - vec2(1.0, 0.0)).length() < 1e-5);
        assert!((bar.along - vec2(0.0, 1.0)).length() < 1e-5);
        assert!(it.hit_title(pos2(100.0 + bar.height * 0.5, 0.0)));
        // The colour square sits at the end of the bar, here its bottom.
        let (c, _) = bar.swatch().unwrap();
        assert!(c.distance(pos2(100.0 + bar.height * 0.5, 50.0 - bar.height * 0.5)) < 1e-3);
        assert!(bar.hit_swatch(c) && !bar.hit_swatch(pos2(100.0 + bar.height * 0.5, 0.0)));
    }

    #[test]
    fn pins_stay_on_the_same_spot_of_the_picture() {
        let ctx = eframe::egui::Context::default();
        let texture = ctx.load_texture(
            "t",
            eframe::egui::ColorImage::filled([4, 2], Color32::WHITE),
            Default::default(),
        );
        let full = vec2(400.0, 200.0);
        let asset = Arc::new(ImageAsset { bytes: Arc::new(vec![]), texture, size: full });
        let mut it = Item::new(1, Content::Image(asset), pos2(100.0, 50.0), full);
        it.scale = 0.5;
        it.rotation = 0.4;
        it.flip_y = true;
        let spot = pos2(300.0, 40.0); // source pixel
        let w = it.content_to_world(spot);
        assert!(it.world_to_content(w).distance(spot) < 1e-3);
        // Cropping (edges pinned to the board) leaves the pin on the same world spot.
        let orig = it.clone();
        it.set_crop_px(&orig, full, Rect::from_min_max(pos2(200.0, 0.0), pos2(400.0, 100.0)), true);
        assert!(it.pin_world(spot).distance(w) < 1e-3);
        // A pin on a cropped-away part is shown on the nearest visible edge.
        assert!((it.clamp_content(pos2(50.0, 40.0)) - pos2(200.0, 40.0)).length() < 1e-3);
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
