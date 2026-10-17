use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use eframe::egui::epaint::TextShape;
use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, pos2, vec2, Align2, Color32, CursorIcon, Event, FontId, Id, Key, Mesh, Modifiers, Order,
    PointerButton, Pos2, Rect, Sense, Shape, Stroke, StrokeKind,
    TextureFilter, TextureOptions, Vec2, ViewportCommand,
};

use crate::item::{
    link_ends, polyline_distance, rotate, segment_distance, stroke_geometry, Content,
    Frame as SelFrame, ImageAsset, Item, Link, StrokeData, FULL_CROP,
};
use crate::loader::{self, Decoded, Layout, LoadMsg};
use crate::pdf;
use crate::scene::{self, SavedItem, SavedKind, SavedLink, SceneFile};
use crate::smooth;

const HANDLE_PX: f32 = 9.0;
const ROTATE_PX: f32 = 28.0;
const MIN_ZOOM: f32 = 0.005;
const MAX_ZOOM: f32 = 100.0;
const UNDO_LIMIT: usize = 200;
/// Grid sizes (board pixels) that N / Shift+N step through.
const GRID_PRESETS: [f32; 16] = [
    2.0, 4.0, 5.0, 8.0, 10.0, 16.0, 20.0, 25.0, 32.0, 50.0, 64.0, 100.0, 128.0, 200.0, 256.0, 500.0,
];
const DEFAULT_GRID: f32 = 32.0;
/// Dots closer than this on screen are thinned out (snapping still uses the real spacing).
const MIN_DOT_SPACING_PX: f32 = 14.0;
/// Smallest crop, in source pixels.
const MIN_CROP_PX: f32 = 4.0;
const ACCENT: Color32 = Color32::from_rgb(80, 160, 255);
/// eframe storage key holding the path of the board that was open on exit.
const LAST_BOARD_KEY: &str = "last_board";
const ROTATE_SNAP: f32 = std::f32::consts::PI / 12.0; // 15°

const INK_COLORS: [Color32; 9] = [
    Color32::from_rgb(245, 245, 245),
    Color32::from_rgb(20, 20, 20),
    Color32::from_rgb(140, 140, 140),
    Color32::from_rgb(235, 64, 52),
    Color32::from_rgb(255, 150, 30),
    Color32::from_rgb(250, 215, 50),
    Color32::from_rgb(70, 200, 90),
    Color32::from_rgb(60, 140, 255),
    Color32::from_rgb(170, 90, 230),
];

/// Title bar / label colours, muted so white text stays readable.
const TAG_COLORS: [Color32; 8] = [
    Color32::from_rgb(72, 72, 76),
    Color32::from_rgb(196, 64, 64),
    Color32::from_rgb(222, 132, 52),
    Color32::from_rgb(196, 168, 48),
    Color32::from_rgb(56, 156, 96),
    Color32::from_rgb(48, 150, 160),
    Color32::from_rgb(64, 110, 200),
    Color32::from_rgb(120, 86, 196),
];
/// Link line width and arrowhead length, in screen pixels.
const LINK_PX: f32 = 2.0;
const ARROW_PX: f32 = 11.0;

const BACKGROUNDS: [(&str, Color32); 4] = [
    ("Dark", Color32::from_rgb(34, 34, 36)),
    ("Grey", Color32::from_rgb(70, 70, 74)),
    ("Light", Color32::from_rgb(228, 228, 222)),
    ("White", Color32::WHITE),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Pen,
    Eraser,
    Link,
}

#[derive(Clone, Copy)]
enum Handle {
    Scale(usize),
    Rotate,
}

/// A crop handle: which image edges it moves, or `Slide` to move the picture inside the frame.
#[derive(Clone, Copy)]
enum CropGrip {
    Edges {
        left: bool,
        top: bool,
        right: bool,
        bottom: bool,
    },
    Slide,
}

/// An image being cropped, with its state before cropping started (for cancel).
struct CropState {
    id: u64,
    orig: Item,
}

enum Drag {
    None,
    Crop {
        grip: CropGrip,
        orig: Item,
        start_px: Pos2,
    },
    Pan {
        last: Pos2,
    },
    /// `orig`: (index, center) of each moved item; `anchor` is the point that lands on the grid
    /// when snapping (see `snap_anchor`).
    Move {
        start: Pos2,
        anchor: Pos2,
        orig: Vec<(usize, Pos2)>,
    },
    /// `orig`: (index, center, scale)
    Scale {
        anchor: Pos2,
        start: Pos2,
        orig: Vec<(usize, Pos2, f32)>,
    },
    /// `orig`: (index, center, rotation)
    Rotate {
        pivot: Pos2,
        a0: f32,
        base_rot: f32,
        orig: Vec<(usize, Pos2, f32)>,
    },
    Rubber {
        start: Pos2,
        base: Vec<u64>,
    },
    Draw {
        /// Pointer samples (world).
        points: Vec<Pos2>,
        /// Per sample: snapped to the grid (Shift held) rather than freehand.
        snapped: Vec<bool>,
        grid_pen: smooth::GridPen,
    },
    Erase {
        changed: bool,
    },
    /// Dragging a new link out of item `from`; `at` is the pointer (world).
    Link {
        from: u64,
        at: Pos2,
    },
}

/// Undo state: everything the user edits on the board.
struct Snapshot {
    items: Vec<Item>,
    links: Vec<Link>,
}

/// An item title being typed in the inline editor.
struct TitleEdit {
    id: u64,
    text: String,
    /// Focus the text field on the next frame (just opened).
    focus: bool,
}

enum Pending {
    Close,
    New,
    Open(Option<PathBuf>),
}

pub struct App {
    items: Vec<Item>,
    links: Vec<Link>,
    next_id: u64,

    pan: Vec2,
    zoom: f32,
    view_init: bool,
    canvas_rect: Rect,

    tool: Tool,
    drag: Drag,

    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,

    path: Option<PathBuf>,
    dirty: bool,
    pending: Option<Pending>,
    allow_close: bool,
    last_title: String,

    tx: Sender<LoadMsg>,
    rx: Receiver<LoadMsg>,
    loading: usize,

    clipboard: Option<arboard::Clipboard>,
    paste_via_event: bool,

    pen_color: Color32,
    /// Pen width in board pixels: strokes keep their size relative to the board at any zoom.
    pen_width: f32,
    /// Stroke smoothing strength, 0..=1.
    smoothing: f32,
    eraser_radius: f32,


    cropping: Option<CropState>,
    title_edit: Option<TitleEdit>,

    background: Color32,
    grid: bool,
    /// Grid spacing in board pixels.
    grid_size: f32,
    always_on_top: bool,
    show_toolbar: bool,
    show_help: bool,
    status: Option<(String, f64)>,

    pdf_dialogs: VecDeque<pdf::ImportDialog>,
    pdf_dpi: u32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        // Ctrl +/-/0 should drive the canvas, not the UI scale.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);

        let (tx, rx) = channel();
        let mut app = Self {
            items: Vec::new(),
            links: Vec::new(),
            next_id: 1,
            pan: Vec2::ZERO,
            zoom: 1.0,
            view_init: false,
            canvas_rect: Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0)),
            tool: Tool::Select,
            drag: Drag::None,
            undo: Vec::new(),
            redo: Vec::new(),
            path: None,
            dirty: false,
            pending: None,
            allow_close: false,
            last_title: String::new(),
            tx,
            rx,
            loading: 0,
            clipboard: arboard::Clipboard::new().ok(),
            paste_via_event: false,
            pen_color: INK_COLORS[0],
            pen_width: 4.0,
            smoothing: 0.5,
            eraser_radius: 16.0,
            cropping: None,
            title_edit: None,
            background: BACKGROUNDS[0].1,
            grid: false,
            grid_size: DEFAULT_GRID,
            always_on_top: false,
            show_toolbar: true,
            show_help: false,
            status: None,
            pdf_dialogs: VecDeque::new(),
            pdf_dpi: 150,
        };
        // Without a file argument, reopen whatever board was open last time.
        let startup = startup.or_else(|| {
            cc.storage
                .and_then(|s| s.get_string(LAST_BOARD_KEY))
                .map(PathBuf::from)
                .filter(|p| p.is_file())
        });
        if let Some(path) = startup {
            if is_scene_path(&path) {
                app.start_scene_load(&cc.egui_ctx, path);
            } else {
                app.start_file_load(&cc.egui_ctx, vec![path], Pos2::ZERO);
            }
        }
        app
    }

    // ---------------------------------------------------------------- camera

    fn w2s(&self, p: Pos2) -> Pos2 {
        pos2(p.x * self.zoom + self.pan.x, p.y * self.zoom + self.pan.y)
    }

    fn s2w(&self, s: Pos2) -> Pos2 {
        ((s.to_vec2() - self.pan) / self.zoom).to_pos2()
    }

    fn zoom_at(&mut self, screen: Pos2, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let f = new_zoom / self.zoom;
        self.pan = screen.to_vec2() - (screen.to_vec2() - self.pan) * f;
        self.zoom = new_zoom;
    }

    fn fit_rect(&mut self, r: Rect) {
        let view = self.canvas_rect.shrink(40.0);
        if !r.is_positive() || !view.is_positive() {
            return;
        }
        self.zoom = (view.width() / r.width())
            .min(view.height() / r.height())
            .clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = view.center().to_vec2() - r.center().to_vec2() * self.zoom;
    }

    fn fit(&mut self) {
        let sel = self.union_aabb(true);
        if let Some(r) = sel.or_else(|| self.union_aabb(false)) {
            self.fit_rect(r);
        } else {
            self.zoom = 1.0;
            self.pan = self.canvas_rect.center().to_vec2();
        }
    }

    fn cursor_world(&self, ctx: &egui::Context) -> Pos2 {
        let p = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| self.canvas_rect.contains(*p))
            .unwrap_or(self.canvas_rect.center());
        self.s2w(p)
    }

    fn max_side(ctx: &egui::Context) -> usize {
        ctx.input(|i| i.max_texture_side).min(8192)
    }

    // ------------------------------------------------------------- selection

    fn selected(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.selected)
    }

    fn any_selected(&self) -> bool {
        self.items.iter().any(|i| i.selected)
    }

    fn deselect_all(&mut self) {
        for it in &mut self.items {
            it.selected = false;
        }
        for l in &mut self.links {
            l.selected = false;
        }
    }

    fn any_link_selected(&self) -> bool {
        self.links.iter().any(|l| l.selected)
    }

    fn item_by_id(&self, id: u64) -> Option<&Item> {
        self.items.iter().find(|it| it.id == id)
    }

    /// Endpoints of each drawable link (both items present and apart), by link index.
    fn link_lines(&self) -> Vec<(usize, Pos2, Pos2)> {
        let by_id: HashMap<u64, &Item> = self.items.iter().map(|it| (it.id, it)).collect();
        self.links
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                let (a, b) = (by_id.get(&l.from)?, by_id.get(&l.to)?);
                link_ends(a, b).map(|(pa, pb)| (i, pa, pb))
            })
            .collect()
    }

    /// Topmost link passing near `w`.
    fn hit_link(&self, w: Pos2) -> Option<usize> {
        let tol = 6.0 / self.zoom;
        self.link_lines()
            .into_iter()
            .rev()
            .find(|&(_, a, b)| segment_distance(w, a, b) <= tol)
            .map(|(i, ..)| i)
    }

    /// Link under `w` unless a stroke (drawn above links) is in the way.
    fn hit_link_on_top(&self, w: Pos2) -> Option<usize> {
        match self.hit_item(w) {
            Some(i) if self.items[i].is_stroke() => None,
            _ => self.hit_link(w),
        }
    }

    /// Drop links whose items no longer exist.
    fn prune_links(&mut self) {
        let ids: HashSet<u64> = self.items.iter().map(|it| it.id).collect();
        self.links.retain(|l| ids.contains(&l.from) && ids.contains(&l.to));
    }

    fn union_aabb(&self, selected_only: bool) -> Option<Rect> {
        self.items
            .iter()
            .filter(|i| !selected_only || i.selected)
            .map(Item::world_aabb)
            .reduce(|a, b| a.union(b))
    }

    fn selection_frame(&self) -> Option<SelFrame> {
        let mut sel = self.selected();
        let first = sel.next()?;
        if sel.next().is_none() {
            return Some(SelFrame {
                center: first.center,
                half: first.half(),
                rotation: first.rotation,
            });
        }
        let r = self.union_aabb(true)?;
        Some(SelFrame {
            center: r.center(),
            half: r.size() * 0.5,
            rotation: 0.0,
        })
    }

    /// Topmost item under `w`. Strokes form the top layer, so they are tested first.
    /// The point of the selection that Shift-moving puts on the grid: the first point of a selected
    /// stroke (so strokes drawn on the grid stay on it), otherwise an image's top-left corner.
    fn snap_anchor(&self) -> Option<Pos2> {
        let stroke_point = self.selected().find_map(|it| match &it.content {
            Content::Stroke(s) => s.points.first().map(|p| it.local_to_world(p.to_vec2())),
            _ => None,
        });
        stroke_point.or_else(|| {
            self.selected().next().map(|it| {
                let h = it.size * 0.5;
                it.local_to_world(vec2(-h.x, -h.y))
            })
        })
    }

    fn hit_item(&self, w: Pos2) -> Option<usize> {
        let tol = 5.0 / self.zoom;
        let hit = |strokes: bool| {
            self.items
                .iter()
                .rposition(|it| it.is_stroke() == strokes && it.hit(w, tol))
        };
        hit(true).or_else(|| hit(false))
    }

    fn hit_handle(&self, screen: Pos2, mods: Modifiers) -> Option<Handle> {
        if self.cropping.is_some() {
            return None;
        }
        let f = self.selection_frame()?;
        let corners = f.corners().map(|c| self.w2s(c));
        for (i, c) in corners.iter().enumerate() {
            if c.distance(screen) <= HANDLE_PX {
                return Some(if mods.command {
                    Handle::Rotate
                } else {
                    Handle::Scale(i)
                });
            }
        }
        if !f.contains(self.s2w(screen)) {
            for c in corners.iter() {
                if c.distance(screen) <= ROTATE_PX {
                    return Some(Handle::Rotate);
                }
            }
        }
        None
    }

    // ------------------------------------------------------------------ undo

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            items: self.items.clone(),
            links: self.links.clone(),
        }
    }

    fn restore(&mut self, s: Snapshot) -> Snapshot {
        Snapshot {
            items: std::mem::replace(&mut self.items, s.items),
            links: std::mem::replace(&mut self.links, s.links),
        }
    }

    fn push_undo(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.dirty = true;
    }

    fn undo(&mut self) {
        self.finish_crop();
        if let Some(prev) = self.undo.pop() {
            let cur = self.restore(prev);
            self.redo.push(cur);
            self.dirty = true;
        }
    }

    fn redo(&mut self) {
        self.finish_crop();
        if let Some(next) = self.redo.pop() {
            let cur = self.restore(next);
            self.undo.push(cur);
            self.dirty = true;
        }
    }

    fn set_status(&mut self, ctx: &egui::Context, msg: impl Into<String>) {
        let t = ctx.input(|i| i.time);
        self.status = Some((msg.into(), t + 4.0));
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    // ---------------------------------------------------------------- loading

    fn start_file_load(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>, at: Pos2) {
        let (pdfs, images): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| pdf::is_pdf_path(p));
        for p in pdfs {
            self.loading += 1;
            pdf::open(p, at, self.tx.clone(), ctx.clone());
        }
        if !images.is_empty() {
            self.loading += 1;
            loader::load_files(images, at, Self::max_side(ctx), self.tx.clone(), ctx.clone());
        }
    }

    fn start_pdf_import(&mut self, ctx: &egui::Context, d: &pdf::ImportDialog, pages: Vec<usize>, dpi: u32) {
        self.loading += 1;
        pdf::import(
            d.name.clone(),
            d.data.clone(),
            pages,
            dpi,
            d.at,
            Self::max_side(ctx),
            self.tx.clone(),
            ctx.clone(),
        );
    }

    fn pdf_dialog(&mut self, ctx: &egui::Context) {
        let mut dpi = self.pdf_dpi;
        let Some(result) = self.pdf_dialogs.front_mut().and_then(|d| d.show(ctx, &mut dpi)) else {
            self.pdf_dpi = dpi;
            return;
        };
        self.pdf_dpi = dpi;
        let d = self.pdf_dialogs.pop_front().expect("dialog was shown");
        if let pdf::DialogResult::Import { pages, dpi } = result {
            self.start_pdf_import(ctx, &d, pages, dpi);
        }
    }

    fn start_scene_load(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.loading += 1;
        loader::load_scene(path, Self::max_side(ctx), self.tx.clone(), ctx.clone());
    }

    fn make_asset(&mut self, ctx: &egui::Context, d: Decoded) -> Arc<ImageAsset> {
        let opts = TextureOptions {
            mipmap_mode: Some(TextureFilter::Linear),
            ..TextureOptions::LINEAR
        };
        let id = self.alloc_id();
        let texture = ctx.load_texture(format!("img-{id}"), d.image, opts);
        Arc::new(ImageAsset {
            bytes: d.bytes,
            texture,
            size: vec2(d.orig_size[0] as f32, d.orig_size[1] as f32),
        })
    }

    fn handle_msg(&mut self, ctx: &egui::Context, msg: LoadMsg) {
        if !matches!(msg, LoadMsg::PdfThumb { .. }) {
            self.loading = self.loading.saturating_sub(1);
        }
        match msg {
            LoadMsg::PdfOpened {
                path,
                data,
                pages,
                at,
            } => {
                let doc = self.alloc_id();
                let count = pages.len();
                let (dialog, cancel) = pdf::ImportDialog::new(doc, &path, data.clone(), pages, at);
                if count == 1 {
                    // Nothing to choose: import the only page straight away.
                    self.start_pdf_import(ctx, &dialog, vec![0], self.pdf_dpi);
                } else {
                    pdf::thumbnails(doc, data, count, cancel, self.tx.clone(), ctx.clone());
                    self.pdf_dialogs.push_back(dialog);
                }
            }
            LoadMsg::PdfThumb { doc, page, image } => {
                if let Some(d) = self.pdf_dialogs.iter_mut().find(|d| d.doc == doc) {
                    d.set_thumb(ctx, page, image);
                }
            }
            LoadMsg::Images {
                images,
                at,
                errors,
                layout,
            } => {
                if !errors.is_empty() {
                    self.set_status(ctx, format!("Failed to load: {}", errors.join("; ")));
                }
                if images.is_empty() {
                    return;
                }
                self.push_undo();
                self.deselect_all();
                let assets: Vec<_> = images.into_iter().map(|d| self.make_asset(ctx, d)).collect();
                self.place_batch(assets, at, layout);
            }
            LoadMsg::Scene {
                path,
                scene,
                images,
            } => self.apply_scene(ctx, path, scene, images),
            LoadMsg::Error(e) => self.set_status(ctx, e),
        }
    }

    /// Add new images as rows (one row, or a roughly square grid) centered on `at`.
    fn place_batch(&mut self, assets: Vec<Arc<ImageAsset>>, at: Pos2, layout: Layout) {
        let gap = 10.0;
        let cols = match layout {
            Layout::Row => assets.len(),
            Layout::Grid => (assets.len() as f32).sqrt().ceil() as usize,
        }
        .max(1);
        let rows: Vec<&[Arc<ImageAsset>]> = assets.chunks(cols).collect();
        let row_w = |r: &[Arc<ImageAsset>]| {
            r.iter().map(|a| a.size.x).sum::<f32>() + gap * (r.len() - 1) as f32
        };
        let row_h = |r: &[Arc<ImageAsset>]| r.iter().map(|a| a.size.y).fold(0.0, f32::max);
        let total_w = rows.iter().map(|r| row_w(r)).fold(0.0, f32::max);
        let total_h = rows.iter().map(|r| row_h(r)).sum::<f32>() + gap * (rows.len() - 1) as f32;
        let mut y = at.y - total_h * 0.5;
        let mut new_items = Vec::new();
        for r in &rows {
            let h = row_h(r);
            let mut x = at.x - total_w * 0.5;
            for asset in r.iter() {
                let size = asset.size;
                let center = pos2(x + size.x * 0.5, y + h * 0.5);
                new_items.push((asset.clone(), center, size));
                x += size.x + gap;
            }
            y += h + gap;
        }
        for (asset, center, size) in new_items {
            let id = self.alloc_id();
            let mut it = Item::new(id, Content::Image(asset), center, size);
            it.selected = true;
            self.items.push(it);
        }
    }

    // ----------------------------------------------------------------- scenes

    fn clear_scene(&mut self) {
        self.cropping = None;
        self.title_edit = None;
        self.items.clear();
        self.links.clear();
        self.undo.clear();
        self.redo.clear();
        self.path = None;
        self.dirty = false;
        self.zoom = 1.0;
        self.pan = self.canvas_rect.center().to_vec2();
    }

    fn build_scene(&self) -> SceneFile {
        let offset = self.pan - self.canvas_rect.center().to_vec2();
        let mut scene = SceneFile::new(
            self.background.to_array(),
            self.grid,
            self.grid_size,
            ([offset.x, offset.y], self.zoom),
        );
        let mut image_index: HashMap<*const ImageAsset, usize> = HashMap::new();
        let mut item_index: HashMap<u64, usize> = HashMap::new();
        for it in &self.items {
            let kind = match &it.content {
                Content::Image(a) => {
                    let idx = *image_index
                        .entry(Arc::as_ptr(a))
                        .or_insert_with(|| scene.add_image(&a.bytes));
                    let c = it.crop;
                    SavedKind::Image {
                        image: idx,
                        crop: (c != FULL_CROP).then_some([c.min.x, c.min.y, c.max.x, c.max.y]),
                    }
                }
                Content::Stroke(s) => SavedKind::Stroke {
                    points: s.points.iter().map(|p| [p.x, p.y]).collect(),
                    color: s.color.to_array(),
                    width: s.width,
                    size: [it.size.x, it.size.y],
                },
            };
            item_index.insert(it.id, scene.items.len());
            scene.items.push(SavedItem {
                center: [it.center.x, it.center.y],
                scale: it.scale,
                rotation: it.rotation,
                flip_x: it.flip_x,
                flip_y: it.flip_y,
                title: it.title.clone(),
                tag: it.tag.map(|c| c.to_array()),
                kind,
            });
        }
        scene.links = self
            .links
            .iter()
            .filter_map(|l| {
                Some(SavedLink {
                    from: *item_index.get(&l.from)?,
                    to: *item_index.get(&l.to)?,
                    color: l.color.to_array(),
                    arrow: l.arrow,
                })
            })
            .collect();
        scene
    }

    fn apply_scene(
        &mut self,
        ctx: &egui::Context,
        path: PathBuf,
        scene: SceneFile,
        images: Vec<Option<Decoded>>,
    ) {
        self.clear_scene();
        let assets: Vec<Option<Arc<ImageAsset>>> = images
            .into_iter()
            .map(|d| d.map(|d| self.make_asset(ctx, d)))
            .collect();
        let mut missing = 0;
        // Items this version no longer supports (e.g. text notes from older boards).
        let mut skipped = 0;
        // New item id for each saved item index, for reconnecting links.
        let mut ids: Vec<Option<u64>> = Vec::with_capacity(scene.items.len());
        for s in scene.items {
            ids.push(None);
            let center = pos2(s.center[0], s.center[1]);
            let mut crop_rect = FULL_CROP;
            let (content, size) = match s.kind {
                SavedKind::Image { image, crop } => match assets.get(image).cloned().flatten() {
                    Some(a) => {
                        if let Some([x0, y0, x1, y1]) = crop {
                            crop_rect = Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
                                .intersect(FULL_CROP);
                        }
                        let size = a.size * crop_rect.size();
                        (Content::Image(a), size)
                    }
                    None => {
                        missing += 1;
                        continue;
                    }
                },
                SavedKind::Unsupported => {
                    skipped += 1;
                    continue;
                }
                SavedKind::Stroke {
                    points,
                    color: c,
                    width,
                    size,
                } => (
                    Content::Stroke(StrokeData {
                        points: Arc::new(points.into_iter().map(|p| pos2(p[0], p[1])).collect()),
                        color: color(c),
                        width,
                    }),
                    vec2(size[0], size[1]),
                ),
            };
            let id = self.alloc_id();
            let mut it = Item::new(id, content, center, size);
            it.scale = s.scale;
            it.rotation = s.rotation;
            it.flip_x = s.flip_x;
            it.flip_y = s.flip_y;
            it.crop = crop_rect;
            it.title = s.title;
            it.tag = s.tag.map(color);
            *ids.last_mut().unwrap() = Some(id);
            self.items.push(it);
        }
        let id_at = |i: usize| ids.get(i).copied().flatten();
        self.links = scene
            .links
            .iter()
            .filter_map(|l| {
                Some(Link {
                    from: id_at(l.from)?,
                    to: id_at(l.to)?,
                    color: color(l.color),
                    arrow: l.arrow,
                    selected: false,
                })
            })
            .collect();
        self.background = color(scene.background);
        self.grid = scene.grid;
        self.grid_size = scene.grid_size.unwrap_or(DEFAULT_GRID).clamp(1.0, 2000.0);
        match scene.view {
            Some((offset, zoom)) => {
                self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                self.pan = self.canvas_rect.center().to_vec2() + vec2(offset[0], offset[1]);
            }
            None => self.fit(),
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut msg = format!("Opened {name}");
        if missing > 0 {
            msg += &format!(" · {missing} image(s) could not be decoded");
        }
        if skipped > 0 {
            msg += &format!(" · {skipped} unsupported item(s) skipped (e.g. text notes)");
        }
        self.set_status(ctx, msg);
        self.path = Some(path);
        self.dirty = false;
    }

    /// Returns true if the board was saved.
    fn save_board(&mut self, ctx: &egui::Context, save_as: bool) -> bool {
        self.finish_crop();
        let path = if save_as || self.path.is_none() {
            rfd::FileDialog::new()
                .add_filter("RifRef board", &[scene::EXTENSION])
                .set_file_name(format!("board.{}", scene::EXTENSION))
                .save_file()
        } else {
            self.path.clone()
        };
        let Some(mut path) = path else {
            return false;
        };
        if path.extension().is_none() {
            path.set_extension(scene::EXTENSION);
        }
        match scene::write(&path, &self.build_scene()) {
            Ok(()) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.set_status(ctx, format!("Saved {name}"));
                self.path = Some(path);
                self.dirty = false;
                true
            }
            Err(e) => {
                self.set_status(ctx, format!("Save failed: {e}"));
                false
            }
        }
    }

    fn import_dialog(&mut self, ctx: &egui::Context) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                "Images & PDFs",
                &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "tga", "pdf"],
            )
            .pick_files()
        {
            let at = self.s2w(self.canvas_rect.center());
            self.start_file_load(ctx, paths, at);
        }
    }

    /// Run `p` now, or after asking about unsaved changes.
    fn request(&mut self, ctx: &egui::Context, p: Pending) {
        self.pending = Some(p);
        if !self.dirty {
            self.proceed(ctx);
        }
    }

    fn proceed(&mut self, ctx: &egui::Context) {
        match self.pending.take() {
            Some(Pending::Close) => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Some(Pending::New) => self.clear_scene(),
            Some(Pending::Open(path)) => {
                let path = path.or_else(|| {
                    rfd::FileDialog::new()
                        .add_filter("RifRef board", &scene::OPEN_EXTENSIONS)
                        .pick_file()
                });
                if let Some(p) = path {
                    self.start_scene_load(ctx, p);
                }
            }
            None => {}
        }
    }

    // --------------------------------------------------------------- commands

    fn delete_selected(&mut self) {
        if self.any_selected() || self.any_link_selected() {
            self.push_undo();
            self.items.retain(|i| !i.selected);
            self.links.retain(|l| !l.selected);
            self.prune_links();
        }
    }

    fn select_all(&mut self) {
        for it in &mut self.items {
            it.selected = true;
        }
    }

    fn duplicate(&mut self) {
        if !self.any_selected() {
            return;
        }
        self.push_undo();
        let offset = Vec2::splat(24.0 / self.zoom);
        let mut copies = Vec::new();
        for it in &mut self.items {
            if it.selected {
                it.selected = false;
                let mut c = it.clone();
                c.center += offset;
                c.selected = true;
                copies.push(c);
            }
        }
        let mut new_ids = HashMap::new();
        for mut c in copies {
            let id = self.alloc_id();
            new_ids.insert(c.id, id);
            c.id = id;
            self.items.push(c);
        }
        // Links inside the duplicated group come along.
        let copied: Vec<Link> = self
            .links
            .iter()
            .filter_map(|l| {
                Some(Link {
                    from: *new_ids.get(&l.from)?,
                    to: *new_ids.get(&l.to)?,
                    selected: false,
                    ..l.clone()
                })
            })
            .collect();
        self.links.extend(copied);
    }

    fn flip(&mut self, horizontal: bool) {
        if !self.any_selected() {
            return;
        }
        self.push_undo();
        for it in self.items.iter_mut().filter(|i| i.selected) {
            if horizontal {
                it.flip_x = !it.flip_x;
            } else {
                it.flip_y = !it.flip_y;
            }
        }
    }

    fn reset_transform(&mut self) {
        if !self.any_selected() {
            return;
        }
        self.push_undo();
        for it in self.items.iter_mut().filter(|i| i.selected) {
            it.scale = 1.0;
            it.rotation = 0.0;
            it.flip_x = false;
            it.flip_y = false;
        }
    }

    fn reorder(&mut self, to_front: bool) {
        if !self.any_selected() {
            return;
        }
        self.push_undo();
        let (sel, rest): (Vec<Item>, Vec<Item>) =
            std::mem::take(&mut self.items).into_iter().partition(|i| i.selected);
        self.items = if to_front {
            rest.into_iter().chain(sel).collect()
        } else {
            sel.into_iter().chain(rest).collect()
        };
    }

    fn target_indices(&self) -> Vec<usize> {
        let sel: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].selected).collect();
        if sel.is_empty() {
            (0..self.items.len()).collect()
        } else {
            sel
        }
    }

    /// Shelf-pack the selection (or everything) into a compact block.
    fn arrange(&mut self) {
        let mut idxs = self.target_indices();
        if idxs.len() < 2 {
            return;
        }
        self.push_undo();
        let rects: HashMap<usize, Rect> =
            idxs.iter().map(|&i| (i, self.items[i].world_aabb())).collect();
        let origin = rects.values().copied().reduce(|a, b| a.union(b)).unwrap().min;
        let gap = 8.0;
        let area: f32 = rects
            .values()
            .map(|r| (r.width() + gap) * (r.height() + gap))
            .sum();
        let max_w = rects.values().map(|r| r.width()).fold(0.0, f32::max);
        let row_w = (area.sqrt() * 1.25).max(max_w);
        idxs.sort_by(|a, b| rects[b].height().total_cmp(&rects[a].height()));
        let (mut x, mut y, mut row_h) = (0.0f32, 0.0f32, 0.0f32);
        for i in idxs {
            let r = rects[&i];
            if x > 0.0 && x + r.width() > row_w {
                x = 0.0;
                y += row_h + gap;
                row_h = 0.0;
            }
            self.items[i].center += (origin + vec2(x, y)) - r.min;
            x += r.width() + gap;
            row_h = row_h.max(r.height());
        }
    }

    /// Give every selected item the same height.
    fn normalize(&mut self) {
        let idxs: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].selected).collect();
        if idxs.len() < 2 {
            return;
        }
        self.push_undo();
        let avg = idxs
            .iter()
            .map(|&i| self.items[i].size.y * self.items[i].scale)
            .sum::<f32>()
            / idxs.len() as f32;
        for i in idxs {
            let it = &mut self.items[i];
            it.scale = avg / it.size.y.max(1.0);
        }
    }

    fn recolor_ink(&mut self, c: Color32) {
        if !self.selected().any(Item::is_stroke) && !self.any_link_selected() {
            return;
        }
        self.push_undo();
        for l in self.links.iter_mut().filter(|l| l.selected) {
            l.color = c;
        }
        for it in self.items.iter_mut().filter(|i| i.selected) {
            if let Content::Stroke(s) = &mut it.content {
                s.color = c;
            }
        }
    }

    // ----------------------------------------------------------- titles, links

    /// Open the inline title editor on image `idx`.
    fn start_title_edit(&mut self, idx: usize) {
        self.finish_crop();
        let it = &self.items[idx];
        self.title_edit = Some(TitleEdit {
            id: it.id,
            text: it.title.clone(),
            focus: true,
        });
    }

    /// Title the single selected image (T / F2 / menu).
    fn title_selected(&mut self, ctx: &egui::Context) {
        match self.single_selected_image() {
            Some(i) => self.start_title_edit(i),
            None => self.set_status(ctx, "Select a single image to give it a title"),
        }
    }

    fn commit_title(&mut self) {
        let Some(edit) = self.title_edit.take() else {
            return;
        };
        let text = edit.text.trim().to_string();
        let Some(i) = self.items.iter().position(|it| it.id == edit.id) else {
            return;
        };
        if self.items[i].title != text {
            self.push_undo();
            self.items[i].title = text;
        }
    }

    fn has_selected_image(&self) -> bool {
        self.selected().any(|it| !it.is_stroke())
    }

    /// Set the label colour of the selected images (`None`: default).
    fn set_tag(&mut self, tag: Option<Color32>) {
        if !self.selected().any(|it| !it.is_stroke() && it.tag != tag) {
            return;
        }
        self.push_undo();
        for it in self.items.iter_mut().filter(|i| i.selected && !i.is_stroke()) {
            it.tag = tag;
        }
    }

    /// Connect two items; returns false if they already are (either direction).
    fn add_link(&mut self, from: u64, to: u64) -> bool {
        if from == to
            || self
                .links
                .iter()
                .any(|l| (l.from, l.to) == (from, to) || (l.from, l.to) == (to, from))
        {
            return false;
        }
        self.push_undo();
        self.deselect_all();
        self.links.push(Link {
            from,
            to,
            color: self.pen_color,
            arrow: true,
            selected: true,
        });
        true
    }

    fn reverse_links(&mut self) {
        if !self.any_link_selected() {
            return;
        }
        self.push_undo();
        for l in self.links.iter_mut().filter(|l| l.selected) {
            std::mem::swap(&mut l.from, &mut l.to);
        }
    }

    fn set_link_arrows(&mut self, arrow: bool) {
        if !self.links.iter().any(|l| l.selected && l.arrow != arrow) {
            return;
        }
        self.push_undo();
        for l in self.links.iter_mut().filter(|l| l.selected) {
            l.arrow = arrow;
        }
    }

    /// Move to the next larger (`dir` > 0) or smaller preset grid size, KiCad style.
    fn step_grid(&mut self, dir: i32) {
        let next = if dir > 0 {
            GRID_PRESETS.iter().find(|&&g| g > self.grid_size + 1e-3)
        } else {
            GRID_PRESETS.iter().rev().find(|&&g| g < self.grid_size - 1e-3)
        };
        if let Some(&g) = next {
            self.set_grid_size(g);
        }
        self.grid = true;
    }

    fn set_grid_size(&mut self, g: f32) {
        let g = g.clamp(1.0, 2000.0);
        if g != self.grid_size {
            self.grid_size = g;
            self.dirty = true;
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        self.finish_crop();
        self.tool = tool;
    }

    // ------------------------------------------------------------------- crop

    fn single_selected_image(&self) -> Option<usize> {
        let mut sel = (0..self.items.len()).filter(|&i| self.items[i].selected);
        let i = sel.next()?;
        (sel.next().is_none() && matches!(self.items[i].content, Content::Image(_))).then_some(i)
    }

    /// Index and source size of the image being cropped.
    fn crop_target(&self) -> Option<(usize, Vec2)> {
        let id = self.cropping.as_ref()?.id;
        let idx = self.items.iter().position(|i| i.id == id)?;
        match &self.items[idx].content {
            Content::Image(a) => Some((idx, a.size)),
            _ => None,
        }
    }

    fn start_crop(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.single_selected_image() else {
            self.set_status(ctx, "Select a single image to crop");
            return;
        };
        self.tool = Tool::Select;
        self.push_undo();
        self.cropping = Some(CropState {
            id: self.items[idx].id,
            orig: self.items[idx].clone(),
        });
    }

    fn finish_crop(&mut self) {
        let Some(state) = self.cropping.take() else {
            return;
        };
        let unchanged = self
            .items
            .iter()
            .find(|i| i.id == state.id)
            .is_none_or(|i| i.crop == state.orig.crop && i.center == state.orig.center);
        if unchanged {
            self.undo.pop();
        }
    }

    fn cancel_crop(&mut self) {
        let Some(state) = self.cropping.take() else {
            return;
        };
        if let Some(it) = self.items.iter_mut().find(|i| i.id == state.id) {
            it.center = state.orig.center;
            it.size = state.orig.size;
            it.crop = state.orig.crop;
        }
        self.undo.pop();
    }

    fn toggle_crop(&mut self, ctx: &egui::Context) {
        if self.cropping.is_some() {
            self.finish_crop();
        } else {
            self.start_crop(ctx);
        }
    }

    fn reset_crop(&mut self) {
        self.finish_crop();
        let targets: Vec<usize> = (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                it.selected && it.crop != FULL_CROP && matches!(it.content, Content::Image(_))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        self.push_undo();
        for i in targets {
            if let Content::Image(a) = &self.items[i].content {
                let full = a.size;
                let orig = self.items[i].clone();
                self.items[i].set_crop_px(&orig, full, Rect::from_min_size(Pos2::ZERO, full), true);
            }
        }
    }

    /// Screen positions of the eight crop handles.
    fn crop_grips(&self) -> Vec<(Pos2, CropGrip)> {
        let Some((idx, full)) = self.crop_target() else {
            return Vec::new();
        };
        let it = &self.items[idx];
        let r = it.crop_px(full);
        [
            (0.0, 0.0),
            (0.5, 0.0),
            (1.0, 0.0),
            (1.0, 0.5),
            (1.0, 1.0),
            (0.5, 1.0),
            (0.0, 1.0),
            (0.0, 0.5),
        ]
        .into_iter()
        .map(|(fx, fy): (f32, f32)| {
            let px = pos2(
                r.min.x + r.width() * fx,
                r.min.y + r.height() * fy,
            );
            let grip = CropGrip::Edges {
                left: fx == 0.0,
                top: fy == 0.0,
                right: fx == 1.0,
                bottom: fy == 1.0,
            };
            (self.w2s(it.px_to_world(full, px)), grip)
        })
        .collect()
    }

    fn hit_crop(&self, screen: Pos2) -> Option<CropGrip> {
        if let Some((_, grip)) = self
            .crop_grips()
            .into_iter()
            .find(|(p, _)| p.distance(screen) <= HANDLE_PX + 2.0)
        {
            return Some(grip);
        }
        let (idx, full) = self.crop_target()?;
        let px = self.items[idx].world_to_px(full, self.s2w(screen));
        Rect::from_min_size(Pos2::ZERO, full)
            .contains(px)
            .then_some(CropGrip::Slide)
    }

    fn drag_crop(&mut self, grip: CropGrip, orig: &Item, start_px: Pos2, w: Pos2) {
        let Some((idx, full)) = self.crop_target() else {
            return;
        };
        let px = orig.world_to_px(full, w);
        let r0 = orig.crop_px(full);
        let min = MIN_CROP_PX.min(full.x).min(full.y);
        match grip {
            CropGrip::Edges {
                left,
                top,
                right,
                bottom,
            } => {
                let mut r = r0;
                if left {
                    r.min.x = px.x.clamp(0.0, r.max.x - min);
                }
                if right {
                    r.max.x = px.x.clamp(r.min.x + min, full.x);
                }
                if top {
                    r.min.y = px.y.clamp(0.0, r.max.y - min);
                }
                if bottom {
                    r.max.y = px.y.clamp(r.min.y + min, full.y);
                }
                self.items[idx].set_crop_px(orig, full, r, true);
            }
            CropGrip::Slide => {
                // Move the picture with the pointer: the visible window moves the other way.
                let r = r0.translate(start_px - px);
                let shift = |lo: f32, hi: f32, max: f32| {
                    if lo < 0.0 {
                        -lo
                    } else if hi > max {
                        max - hi
                    } else {
                        0.0
                    }
                };
                let r = r.translate(vec2(
                    shift(r.min.x, r.max.x, full.x),
                    shift(r.min.y, r.max.y, full.y),
                ));
                self.items[idx].set_crop_px(orig, full, r, false);
            }
        }
    }

    // ---------------------------------------------------------------- sketching

    fn commit_stroke(&mut self, points: Vec<Pos2>) {
        if points.is_empty() {
            return;
        }
        let width = self.pen_width;
        let (center, size, local) = stroke_geometry(&points, width);
        self.push_undo();
        let id = self.alloc_id();
        self.items.push(Item::new(
            id,
            Content::Stroke(StrokeData {
                points: Arc::new(local),
                color: self.pen_color,
                width,
            }),
            center,
            size,
        ));
    }

    /// Erase stroke parts within `radius` (world units) of `w`, splitting strokes as needed.
    fn erase_at(&mut self, w: Pos2, radius: f32) -> bool {
        // Links the eraser touches go away whole.
        let doomed: HashSet<usize> = self
            .link_lines()
            .into_iter()
            .filter(|&(_, a, b)| segment_distance(w, a, b) <= radius)
            .map(|(i, ..)| i)
            .collect();
        let mut changed = !doomed.is_empty();
        if changed {
            let mut i = 0;
            self.links.retain(|_| {
                i += 1;
                !doomed.contains(&(i - 1))
            });
        }
        let mut i = 0;
        while i < self.items.len() {
            let it = &self.items[i];
            let Content::Stroke(s) = &it.content else {
                i += 1;
                continue;
            };
            if !it.world_aabb().expand(radius).contains(w) {
                i += 1;
                continue;
            }
            let local = it.world_to_local(w).to_pos2();
            let r = radius / it.scale + s.width * 0.5;
            if polyline_distance(&s.points, local) > r {
                i += 1;
                continue;
            }
            let mut runs: Vec<Vec<Pos2>> = Vec::new();
            let mut cur: Vec<Pos2> = Vec::new();
            for p in s.points.iter() {
                if p.distance(local) <= r {
                    if !cur.is_empty() {
                        runs.push(std::mem::take(&mut cur));
                    }
                    continue;
                }
                if let Some(prev) = cur.last() {
                    if segment_distance(local, *prev, *p) <= r {
                        runs.push(std::mem::take(&mut cur));
                    }
                }
                cur.push(*p);
            }
            if !cur.is_empty() {
                runs.push(cur);
            }
            let width = s.width * it.scale;
            let color = s.color;
            let pieces: Vec<Item> = runs
                .into_iter()
                .map(|run| {
                    let world: Vec<Pos2> =
                        run.iter().map(|p| it.local_to_world(p.to_vec2())).collect();
                    let (center, size, local) = stroke_geometry(&world, width);
                    Item::new(
                        0,
                        Content::Stroke(StrokeData {
                            points: Arc::new(local),
                            color,
                            width,
                        }),
                        center,
                        size,
                    )
                })
                .collect();
            let n = pieces.len();
            self.items.splice(i..i + 1, pieces);
            for k in i..i + n {
                self.items[k].id = self.alloc_id();
            }
            i += n;
            changed = true;
        }
        // Erasing a linked stroke replaces it with new pieces.
        self.prune_links();
        changed
    }

    // -------------------------------------------------------------- clipboard

    fn paste(&mut self, ctx: &egui::Context, text: Option<String>) {
        let at = self.cursor_world(ctx);
        if let Some(cb) = self.clipboard.as_mut() {
            if let Ok(img) = cb.get_image() {
                self.loading += 1;
                loader::load_rgba(
                    img.width,
                    img.height,
                    img.bytes.into_owned(),
                    at,
                    Self::max_side(ctx),
                    self.tx.clone(),
                    ctx.clone(),
                );
                return;
            }
        }
        let text = text.or_else(|| self.clipboard.as_mut().and_then(|cb| cb.get_text().ok()));
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let paths: Vec<PathBuf> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| PathBuf::from(l.strip_prefix("file://").map(percent_decode).unwrap_or(l.into())))
            .collect();
        if paths.iter().all(|p| p.is_file()) {
            let (scenes, images): (Vec<_>, Vec<_>) =
                paths.into_iter().partition(|p| is_scene_path(p));
            if !images.is_empty() {
                self.start_file_load(ctx, images, at);
            }
            if let Some(s) = scenes.into_iter().next() {
                self.request(ctx, Pending::Open(Some(s)));
            }
        } else {
            self.set_status(ctx, "Nothing to paste: the clipboard has no image or image file");
        }
    }

    /// Copies the first selected image (cropped) to the system clipboard.
    fn copy(&mut self, ctx: &egui::Context) -> bool {
        let Some(it) = self
            .selected()
            .find(|i| matches!(i.content, Content::Image(_)))
        else {
            return false;
        };
        let crop = it.crop;
        let result = match &it.content {
            Content::Image(a) => loader::decode_dynamic(&a.bytes)
                .and_then(|img| {
                    let (w, h) = (img.width() as f32, img.height() as f32);
                    let px = Rect::from_min_max(
                        pos2(crop.min.x * w, crop.min.y * h),
                        pos2(crop.max.x * w, crop.max.y * h),
                    );
                    let rgba = img
                        .crop_imm(
                            px.min.x.round() as u32,
                            px.min.y.round() as u32,
                            (px.width().round() as u32).max(1),
                            (px.height().round() as u32).max(1),
                        )
                        .to_rgba8();
                    let data = arboard::ImageData {
                        width: rgba.width() as usize,
                        height: rgba.height() as usize,
                        bytes: rgba.into_raw().into(),
                    };
                    self.clipboard
                        .as_mut()
                        .ok_or("no clipboard".to_string())?
                        .set_image(data)
                        .map_err(|e| e.to_string())
                }),
            Content::Stroke(_) => return false,
        };
        match result {
            Ok(()) => {
                self.set_status(ctx, "Copied to clipboard");
                true
            }
            Err(e) => {
                self.set_status(ctx, format!("Copy failed: {e}"));
                false
            }
        }
    }

    // ------------------------------------------------------------------ input

    fn handle_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = dropped.into_iter().filter_map(|f| f.path).collect();
        let (scenes, images): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| is_scene_path(p));
        if !images.is_empty() {
            let at = self.cursor_world(ctx);
            self.start_file_load(ctx, images, at);
        }
        if let Some(s) = scenes.into_iter().next() {
            self.request(ctx, Pending::Open(Some(s)));
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() || self.pending.is_some() || !self.pdf_dialogs.is_empty() {
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            match ev {
                Event::Paste(text) => {
                    self.paste_via_event = true;
                    self.paste(ctx, Some(text));
                }
                Event::Copy => {
                    self.copy(ctx);
                }
                Event::Cut => {
                    if self.copy(ctx) {
                        self.delete_selected();
                    }
                }
                // egui-winit swallows the Ctrl+V press when the clipboard holds no text
                // (e.g. an image), but still reports the release.
                Event::Key {
                    key: Key::V,
                    pressed: false,
                    modifiers,
                    ..
                } if modifiers.command => {
                    if !std::mem::take(&mut self.paste_via_event) {
                        self.paste(ctx, None);
                    }
                }
                _ => {}
            }
        }

        let k = |m: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(m, key));
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let none = Modifiers::NONE;

        if self.cropping.is_some() {
            if k(none, Key::Escape) {
                self.cancel_crop();
            } else if k(none, Key::Enter) || k(none, Key::C) {
                self.finish_crop();
            }
        } else if k(none, Key::C) {
            self.start_crop(ctx);
        }

        if k(cmd_shift, Key::Z) || k(cmd, Key::Y) {
            self.redo();
        } else if k(cmd, Key::Z) {
            self.undo();
        }
        if k(cmd_shift, Key::S) {
            self.save_board(ctx, true);
        } else if k(cmd, Key::S) {
            self.save_board(ctx, false);
        }
        if k(cmd_shift, Key::T) {
            self.toggle_on_top(ctx);
        } else if k(cmd, Key::T) {
            self.reset_transform();
        }
        if k(cmd, Key::O) {
            self.request(ctx, Pending::Open(None));
        }
        if k(cmd, Key::N) {
            self.request(ctx, Pending::New);
        }
        if k(cmd, Key::I) {
            self.import_dialog(ctx);
        }
        if k(cmd, Key::A) {
            self.select_all();
        }
        if k(cmd, Key::D) {
            self.duplicate();
        }
        if k(cmd, Key::P) {
            self.arrange();
        }
        if k(cmd, Key::Num0) {
            let c = self.canvas_rect.center();
            self.zoom_at(c, 1.0 / self.zoom);
        }
        if k(cmd, Key::Equals) || k(cmd, Key::Plus) {
            self.zoom_at(self.canvas_rect.center(), 1.25);
        }
        if k(cmd, Key::Minus) {
            self.zoom_at(self.canvas_rect.center(), 0.8);
        }
        if k(none, Key::Delete) || k(none, Key::Backspace) {
            self.delete_selected();
        }
        if k(none, Key::Escape) {
            if self.tool != Tool::Select {
                self.tool = Tool::Select;
            } else {
                self.deselect_all();
            }
        }
        if k(Modifiers::SHIFT, Key::H) {
            self.flip(false);
        } else if k(none, Key::H) {
            self.flip(true);
        }
        if k(none, Key::F) {
            self.fit();
        }
        if k(none, Key::S) || k(none, Key::V) {
            self.set_tool(Tool::Select);
        }
        if k(none, Key::B) || k(none, Key::P) {
            self.set_tool(Tool::Pen);
        }
        if k(none, Key::E) {
            self.set_tool(Tool::Eraser);
        }
        if k(none, Key::L) {
            self.set_tool(Tool::Link);
        }
        if k(none, Key::T) || k(none, Key::F2) {
            self.title_selected(ctx);
            // The T keypress also arrives as typed text; keep it out of the new title.
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Text(_))));
        }
        if k(none, Key::G) {
            self.grid = !self.grid;
        }
        if k(Modifiers::SHIFT, Key::N) {
            self.step_grid(-1);
        } else if k(none, Key::N) {
            self.step_grid(1);
        }
        if k(none, Key::Tab) {
            self.show_toolbar = !self.show_toolbar;
        }
        if k(none, Key::F1) {
            self.show_help = !self.show_help;
        }
        if k(none, Key::PageUp) {
            self.reorder(true);
        }
        if k(none, Key::PageDown) {
            self.reorder(false);
        }
        if k(none, Key::OpenBracket) {
            self.resize_brush(1.0 / 1.25);
        }
        if k(none, Key::CloseBracket) {
            self.resize_brush(1.25);
        }
    }

    fn resize_brush(&mut self, f: f32) {
        if self.tool == Tool::Eraser {
            self.eraser_radius = (self.eraser_radius * f).clamp(2.0, 200.0);
        } else {
            self.pen_width = (self.pen_width * f).clamp(0.5, 200.0);
        }
    }

    fn toggle_on_top(&mut self, ctx: &egui::Context) {
        self.always_on_top = !self.always_on_top;
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if self.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }));
    }

    fn begin_primary_drag(&mut self, origin: Pos2, mods: Modifiers) {
        let w = self.s2w(origin);
        if self.cropping.is_some() {
            if let (Some(grip), Some((idx, full))) = (self.hit_crop(origin), self.crop_target()) {
                let orig = self.items[idx].clone();
                let start_px = orig.world_to_px(full, w);
                self.drag = Drag::Crop {
                    grip,
                    orig,
                    start_px,
                };
                return;
            }
            // Dragging anywhere else applies the crop and behaves as usual.
            self.finish_crop();
        }
        match self.tool {
            Tool::Pen => {
                let (w, snapped) = if mods.shift {
                    (snap(w, self.grid_size), true)
                } else {
                    (w, false)
                };
                self.drag = Drag::Draw {
                    points: vec![w],
                    snapped: vec![snapped],
                    grid_pen: smooth::GridPen::default(),
                }
            }
            Tool::Eraser => {
                self.push_undo();
                let changed = self.erase_at(w, self.eraser_radius / self.zoom);
                self.drag = Drag::Erase { changed };
            }
            Tool::Link => {
                if let Some(i) = self.hit_item(w) {
                    self.drag = Drag::Link {
                        from: self.items[i].id,
                        at: w,
                    };
                }
            }
            Tool::Select => {
                if let Some(h) = self.hit_handle(origin, mods) {
                    let frame = self.selection_frame().expect("handle implies selection");
                    let corners = frame.corners();
                    self.push_undo();
                    let sel = || (0..self.items.len()).filter(|&i| self.items[i].selected);
                    self.drag = match h {
                        Handle::Scale(i) => Drag::Scale {
                            anchor: corners[(i + 2) % 4],
                            start: corners[i],
                            orig: sel()
                                .map(|i| (i, self.items[i].center, self.items[i].scale))
                                .collect(),
                        },
                        Handle::Rotate => {
                            let orig: Vec<_> = sel()
                                .map(|i| (i, self.items[i].center, self.items[i].rotation))
                                .collect();
                            Drag::Rotate {
                                pivot: frame.center,
                                a0: (w - frame.center).angle(),
                                base_rot: if orig.len() == 1 { orig[0].2 } else { 0.0 },
                                orig,
                            }
                        }
                    };
                } else if let Some(i) = self.hit_item(w) {
                    if !self.items[i].selected {
                        if !mods.shift && !mods.command {
                            self.deselect_all();
                        }
                        self.items[i].selected = true;
                    }
                    self.push_undo();
                    self.drag = Drag::Move {
                        start: w,
                        anchor: self.snap_anchor().unwrap_or(w),
                        orig: (0..self.items.len())
                            .filter(|&i| self.items[i].selected)
                            .map(|i| (i, self.items[i].center))
                            .collect(),
                    };
                } else {
                    let base = if mods.shift || mods.command {
                        self.selected().map(|i| i.id).collect()
                    } else {
                        self.deselect_all();
                        Vec::new()
                    };
                    self.drag = Drag::Rubber { start: w, base };
                }
            }
        }
    }

    fn continue_drag(&mut self, pos: Pos2, mods: Modifiers) {
        let w = self.s2w(pos);
        let zoom = self.zoom;
        let grid = self.grid_size;
        if let Drag::Crop {
            grip,
            orig,
            start_px,
        } = &self.drag
        {
            let (grip, orig, start_px) = (*grip, orig.clone(), *start_px);
            // With Shift, dragged crop edges land on grid lines.
            let w = if mods.shift && matches!(grip, CropGrip::Edges { .. }) {
                snap(w, self.grid_size)
            } else {
                w
            };
            self.drag_crop(grip, &orig, start_px, w);
            return;
        }
        match &mut self.drag {
            Drag::None | Drag::Crop { .. } => {}
            Drag::Pan { last } => {
                self.pan += pos - *last;
                *last = pos;
            }
            Drag::Move {
                start,
                anchor,
                orig,
            } => {
                let mut d = w - *start;
                if mods.shift {
                    d = snap(*anchor + d, grid) - *anchor;
                }
                for &(i, c) in orig.iter() {
                    if let Some(it) = self.items.get_mut(i) {
                        it.center = c + d;
                    }
                }
            }
            Drag::Scale {
                anchor,
                start,
                orig,
            } => {
                // With Shift, the dragged corner lands on a grid point.
                let w = if mods.shift { snap(w, grid) } else { w };
                let d = *start - *anchor;
                let s = ((w - *anchor).dot(d) / d.length_sq().max(1e-6)).max(0.02);
                for &(i, c, sc) in orig.iter() {
                    if let Some(it) = self.items.get_mut(i) {
                        it.center = *anchor + (c - *anchor) * s;
                        it.scale = sc * s;
                    }
                }
            }
            Drag::Rotate {
                pivot,
                a0,
                base_rot,
                orig,
            } => {
                let mut delta = (w - *pivot).angle() - *a0;
                if mods.shift {
                    delta = ((*base_rot + delta) / ROTATE_SNAP).round() * ROTATE_SNAP - *base_rot;
                }
                for &(i, c, r) in orig.iter() {
                    if let Some(it) = self.items.get_mut(i) {
                        it.center = *pivot + rotate(c - *pivot, delta);
                        it.rotation = r + delta;
                    }
                }
            }
            Drag::Rubber { start, base } => {
                let r = Rect::from_two_pos(*start, w);
                for it in &mut self.items {
                    it.selected = base.contains(&it.id) || r.intersects(it.world_aabb());
                }
            }
            Drag::Draw {
                points,
                snapped,
                grid_pen,
            } => {
                if mods.shift {
                    // 0/45/90° segments between grid points, KiCad style.
                    // Leaving a line takes half a grid step, or 6px on screen if that's more,
                    // so hand jitter on a fine grid doesn't add corners.
                    grid_pen.add(points, snapped, w, grid, (grid * 0.5).max(6.0 / zoom));
                } else if points.last().is_none_or(|l| l.distance(w) * zoom >= 1.0) {
                    grid_pen.release();
                    points.push(w);
                    snapped.push(false);
                }
            }
            Drag::Erase { changed } => {
                let c = *changed;
                let now = self.erase_at(w, self.eraser_radius / zoom);
                self.drag = Drag::Erase { changed: c || now };
            }
            Drag::Link { at, .. } => *at = w,
        }
    }

    fn end_drag(&mut self, ctx: &egui::Context) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Draw {
                points, snapped, ..
            } => {
                let points =
                    smooth::finish_mixed(&points, &snapped, self.smoothing, 1.0 / self.zoom);
                self.commit_stroke(points);
            }
            Drag::Erase { changed: false } => {
                self.undo.pop();
            }
            Drag::Link { from, at } => {
                if let Some(i) = self.hit_item(at) {
                    let to = self.items[i].id;
                    if to != from && !self.add_link(from, to) {
                        self.set_status(ctx, "Those two are already linked");
                    }
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, w: Pos2, mods: Modifiers) {
        if self.cropping.is_some() {
            if self.hit_crop(self.w2s(w)).is_some() {
                return;
            }
            self.finish_crop();
        }
        match self.tool {
            // Links are selected by clicking them, with the Select or the Link tool.
            Tool::Select | Tool::Link if self.hit_link_on_top(w).is_some() => {
                let l = self.hit_link_on_top(w).unwrap();
                if mods.shift || mods.command {
                    self.links[l].selected = !self.links[l].selected;
                } else {
                    self.deselect_all();
                    self.links[l].selected = true;
                }
            }
            Tool::Select | Tool::Link => match self.hit_item(w) {
                Some(i) if mods.shift || mods.command => {
                    self.items[i].selected = !self.items[i].selected;
                }
                Some(i) => {
                    self.deselect_all();
                    self.items[i].selected = true;
                }
                None => {
                    if !mods.shift && !mods.command {
                        self.deselect_all();
                    }
                }
            },
            Tool::Pen => {
                let w = if mods.shift { snap(w, self.grid_size) } else { w };
                self.commit_stroke(vec![w]);
            }
            Tool::Eraser => {
                self.push_undo();
                if !self.erase_at(w, self.eraser_radius / self.zoom) {
                    self.undo.pop();
                }
            }
        }
    }

    fn double_click(&mut self, w: Pos2) {
        if self.cropping.is_some() {
            self.finish_crop();
            return;
        }
        match self.hit_item(w) {
            Some(i) if self.items[i].hit_title(w) => self.start_title_edit(i),
            Some(i) => {
                let r = self.items[i].world_aabb();
                self.fit_rect(r);
            }
            None => self.fit(),
        }
    }

    // ----------------------------------------------------------------- canvas

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        self.canvas_rect = rect;
        if !self.view_init {
            self.pan = rect.center().to_vec2();
            self.view_init = true;
        }

        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let (mods, hover, scroll, zoom_delta, space, press_origin) = ctx.input(|i| {
            (
                i.modifiers,
                i.pointer.hover_pos(),
                i.raw_scroll_delta,
                i.zoom_delta(),
                i.key_down(Key::Space),
                i.pointer.press_origin(),
            )
        });
        let space = space && !ctx.wants_keyboard_input();

        // Zoom towards the cursor.
        if resp.hovered() {
            let factor = if zoom_delta != 1.0 {
                zoom_delta
            } else {
                (scroll.y * 0.0025).exp()
            };
            if factor != 1.0 {
                if let Some(p) = hover {
                    self.zoom_at(p, factor);
                }
            }
        }

        // Pointer interaction.
        let pointer = resp.interact_pointer_pos().or(hover);
        if resp.drag_started() {
            let origin = press_origin.or(pointer).unwrap_or(rect.center());
            if resp.dragged_by(PointerButton::Middle)
                || resp.dragged_by(PointerButton::Secondary)
                || (space && resp.dragged_by(PointerButton::Primary))
            {
                self.drag = Drag::Pan { last: origin };
            } else if resp.dragged_by(PointerButton::Primary) {
                self.begin_primary_drag(origin, mods);
            }
        }
        if resp.dragged() {
            if matches!(self.drag, Drag::Draw { .. }) {
                // Use every pointer sample, not just one per frame, so fast strokes keep their shape.
                let mut samples: Vec<Pos2> = ctx.input(|i| {
                    i.events
                        .iter()
                        .filter_map(|e| match e {
                            Event::PointerMoved(p) => Some(*p),
                            _ => None,
                        })
                        .collect()
                });
                if samples.is_empty() {
                    samples.extend(pointer);
                }
                for p in samples {
                    self.continue_drag(p, mods);
                }
            } else if let Some(p) = pointer {
                self.continue_drag(p, mods);
            }
        }
        if resp.drag_stopped() {
            self.end_drag(&ctx);
        }
        if let Some(p) = pointer {
            let w = self.s2w(p);
            if resp.double_clicked() && self.tool == Tool::Select {
                self.double_click(w);
            } else if resp.clicked() {
                self.click(w, mods);
            }
            if resp.secondary_clicked() {
                if let Some(l) = self.hit_link_on_top(w) {
                    if !self.links[l].selected {
                        self.deselect_all();
                        self.links[l].selected = true;
                    }
                } else if let Some(i) = self.hit_item(w) {
                    if !self.items[i].selected {
                        self.deselect_all();
                        self.items[i].selected = true;
                    }
                }
            }
        }
        resp.context_menu(|ui| self.context_menu(ui, &ctx));

        // Cursor.
        if resp.hovered() {
            let icon = if matches!(self.drag, Drag::Pan { .. }) {
                CursorIcon::Grabbing
            } else if space {
                CursorIcon::Grab
            } else {
                match self.tool {
                    Tool::Pen => CursorIcon::Crosshair,
                    Tool::Link => match hover.map(|p| self.s2w(p)) {
                        Some(w) if self.hit_item(w).is_some() => CursorIcon::Crosshair,
                        Some(w) if self.hit_link(w).is_some() => CursorIcon::PointingHand,
                        _ => CursorIcon::Default,
                    },
                    Tool::Eraser => CursorIcon::None,
                    Tool::Select if self.cropping.is_some() => {
                        match hover.and_then(|p| self.hit_crop(p)) {
                            Some(CropGrip::Edges {
                                left,
                                top,
                                right,
                                bottom,
                            }) => match (left || right, top || bottom) {
                                (true, true) if (left && top) || (right && bottom) => {
                                    CursorIcon::ResizeNwSe
                                }
                                (true, true) => CursorIcon::ResizeNeSw,
                                (true, false) => CursorIcon::ResizeHorizontal,
                                _ => CursorIcon::ResizeVertical,
                            },
                            Some(CropGrip::Slide) => CursorIcon::Move,
                            None => CursorIcon::Default,
                        }
                    }
                    Tool::Select => match hover.and_then(|p| self.hit_handle(p, mods)) {
                        Some(Handle::Scale(i)) => {
                            if i % 2 == 0 {
                                CursorIcon::ResizeNwSe
                            } else {
                                CursorIcon::ResizeNeSw
                            }
                        }
                        Some(Handle::Rotate) => CursorIcon::Alias,
                        None => match hover.and_then(|p| self.hit_item(self.s2w(p))) {
                            Some(i) if self.items[i].selected => CursorIcon::Move,
                            Some(_) => CursorIcon::PointingHand,
                            None => CursorIcon::Default,
                        },
                    },
                }
            };
            ctx.set_cursor_icon(icon);
        }

        // Paint.
        if self.grid {
            self.draw_grid(&painter, rect);
        }
        // Layers: images, then links between them, then pen strokes on top.
        let (strokes, others): (Vec<&Item>, Vec<&Item>) =
            self.items.iter().partition(|it| it.is_stroke());
        let visible = |it: &&&Item| {
            let aabb = it.world_aabb();
            Rect::from_two_pos(self.w2s(aabb.min), self.w2s(aabb.max)).intersects(rect)
        };
        for it in others.iter().filter(visible) {
            self.draw_item(&painter, it);
        }
        self.draw_links(&painter);
        for it in strokes.iter().filter(visible) {
            self.draw_item(&painter, it);
        }
        self.draw_selection(&painter);
        self.draw_overlays(&ctx, &painter, rect, hover);
    }

    fn draw_grid(&self, painter: &egui::Painter, rect: Rect) {
        // Draw every 1st/2nd/5th/10th/20th... grid point so dots never crowd together.
        let mut step = self.grid_size;
        let mut multiples = [2.0, 2.5, 2.0].iter().cycle();
        while step * self.zoom < MIN_DOT_SPACING_PX {
            step *= multiples.next().unwrap();
        }
        let tl = self.s2w(rect.min);
        let br = self.s2w(rect.max);
        let lum = self.background.r() as u32 + self.background.g() as u32 + self.background.b() as u32;
        let c = if lum > 382 {
            Color32::from_black_alpha(50)
        } else {
            Color32::from_white_alpha(28)
        };
        let mut x = (tl.x / step).floor() * step;
        while x <= br.x {
            let mut y = (tl.y / step).floor() * step;
            while y <= br.y {
                painter.rect_filled(Rect::from_center_size(self.w2s(pos2(x, y)), Vec2::splat(2.0)), 0.0, c);
                y += step;
            }
            x += step;
        }
    }

    fn draw_item(&self, painter: &egui::Painter, it: &Item) {
        match &it.content {
            Content::Image(a) => {
                if self.cropping.as_ref().is_some_and(|c| c.id == it.id) {
                    // Show the whole picture faded so the cropped-away parts stay visible.
                    let c = it.crop_px(a.size).center();
                    let full = Rect::from_min_size(Pos2::ZERO, a.size).translate(-c.to_vec2());
                    self.paint_image(painter, it, a, FULL_CROP, full, Color32::WHITE.gamma_multiply(0.3));
                }
                let visible = Rect::from_center_size(Pos2::ZERO, it.size);
                self.paint_image(painter, it, a, it.crop, visible, Color32::WHITE);
                self.draw_card(painter, it);
            }
            Content::Stroke(s) => {
                let pts: Vec<Pos2> = s
                    .points
                    .iter()
                    .map(|p| self.w2s(it.local_to_world(p.to_vec2())))
                    .collect();
                let w = (s.width * it.scale * self.zoom).max(0.75);
                draw_polyline(painter, pts, w, s.color);
            }
        }
    }

    /// Title bar and coloured border of a titled or labelled image.
    fn draw_card(&self, painter: &egui::Painter, it: &Item) {
        if it.title.is_empty() && it.tag.is_none() {
            return;
        }
        let tag = it.tag.unwrap_or(TAG_COLORS[0]);
        let card: Vec<Pos2> = it.card_corners().iter().map(|p| self.w2s(*p)).collect();
        let tb = it.title_height();
        if tb > 0.0 {
            let h = it.half();
            let to_screen = |x: f32, y: f32| self.w2s(it.center + rotate(vec2(x, y), it.rotation));
            let bar = vec![
                card[0],
                card[1],
                to_screen(h.x, -h.y),
                to_screen(-h.x, -h.y),
            ];
            painter.add(Shape::convex_polygon(bar, tag, Stroke::NONE));
            // Font sizes are stepped so zooming doesn't fill the font atlas with every size.
            let size = tb * 0.6 * self.zoom;
            let size = (2f32.powf((size.log2() * 6.0).round() / 6.0)).min(96.0);
            if size >= 5.0 {
                let lum = tag.r() as u32 * 3 + tag.g() as u32 * 6 + tag.b() as u32;
                let ink = if lum > 1500 { Color32::BLACK } else { Color32::WHITE };
                let pad = tb * 0.3;
                let mut job = LayoutJob::single_section(
                    it.title.clone(),
                    TextFormat::simple(FontId::proportional(size), ink),
                );
                job.wrap = TextWrapping {
                    max_width: ((h.x * 2.0 - pad * 2.0) * self.zoom).max(1.0),
                    max_rows: 1,
                    break_anywhere: true,
                    overflow_character: Some('…'),
                };
                let galley = painter.layout_job(job);
                let text_h = galley.size().y / self.zoom;
                let top_left = to_screen(-h.x + pad, -h.y - tb + (tb - text_h) * 0.5);
                painter.add(TextShape::new(top_left, galley, ink).with_angle(it.rotation));
            }
        }
        painter.add(Shape::closed_line(card, Stroke::new(1.5, tag)));
    }

    fn draw_links(&self, painter: &egui::Painter) {
        for (i, a, b) in self.link_lines() {
            let l = &self.links[i];
            let (a, b) = (self.w2s(a), self.w2s(b));
            if l.selected {
                draw_link(painter, a, b, l.arrow, LINK_PX + 4.0, ACCENT.gamma_multiply(0.6));
            }
            draw_link(painter, a, b, l.arrow, LINK_PX, l.color);
        }
    }

    /// Paint part of an image: `uv` of the texture onto the `local` rect (unscaled, relative to
    /// the item center).
    fn paint_image(
        &self,
        painter: &egui::Painter,
        it: &Item,
        asset: &ImageAsset,
        uv: Rect,
        local: Rect,
        tint: Color32,
    ) {
        let mut mesh = Mesh::with_texture(asset.texture.id());
        for (l, uv) in [
            (local.left_top(), uv.left_top()),
            (local.right_top(), uv.right_top()),
            (local.right_bottom(), uv.right_bottom()),
            (local.left_bottom(), uv.left_bottom()),
        ] {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: self.w2s(it.local_to_world(l.to_vec2())),
                uv,
                color: tint,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(Shape::mesh(mesh));
    }

    fn draw_crop_frame(&self, painter: &egui::Painter) {
        let Some((idx, full)) = self.crop_target() else {
            return;
        };
        let it = &self.items[idx];
        let r = it.crop_px(full);
        let frame: Vec<Pos2> = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()]
            .iter()
            .map(|p| self.w2s(it.px_to_world(full, *p)))
            .collect();
        painter.add(Shape::closed_line(frame, Stroke::new(1.5, Color32::WHITE)));
        for (p, grip) in self.crop_grips() {
            let corner = matches!(
                grip,
                CropGrip::Edges { left, top, right, bottom } if (left || right) && (top || bottom)
            );
            let size = if corner { vec2(11.0, 11.0) } else { vec2(9.0, 9.0) };
            let r = Rect::from_center_size(p, size);
            painter.rect_filled(r, 0.0, Color32::from_gray(20));
            painter.rect_stroke(r, 0.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Middle);
        }
    }

    fn draw_selection(&self, painter: &egui::Painter) {
        if self.cropping.is_some() {
            self.draw_crop_frame(painter);
            return;
        }
        let outline = Stroke::new(1.0, ACCENT);
        let mut count = 0;
        for it in self.selected() {
            count += 1;
            let c: Vec<Pos2> = it.corners().iter().map(|c| self.w2s(*c)).collect();
            painter.add(Shape::closed_line(c, outline));
        }
        if count == 0 || self.tool != Tool::Select {
            return;
        }
        let Some(frame) = self.selection_frame() else {
            return;
        };
        let corners = frame.corners().map(|c| self.w2s(c));
        if count > 1 {
            painter.add(Shape::closed_line(
                corners.to_vec(),
                Stroke::new(1.5, ACCENT.gamma_multiply(0.8)),
            ));
        }
        for c in corners {
            let r = Rect::from_center_size(c, Vec2::splat(9.0));
            painter.rect_filled(r, 1.0, Color32::WHITE);
            painter.rect_stroke(r, 1.0, Stroke::new(1.5, ACCENT), StrokeKind::Middle);
        }
    }

    fn draw_overlays(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: Rect,
        hover: Option<Pos2>,
    ) {
        match &self.drag {
            Drag::Rubber { start, .. } => {
                if let Some(p) = hover {
                    let r = Rect::from_two_pos(self.w2s(*start), p);
                    painter.rect_filled(r, 0.0, ACCENT.gamma_multiply(0.12));
                    painter.rect_stroke(r, 0.0, Stroke::new(1.0, ACCENT), StrokeKind::Inside);
                }
            }
            Drag::Link { from, at } => {
                // Rubber-band line from the source card to the pointer, or to the target's edge.
                if let Some(src) = self.item_by_id(*from) {
                    let target = self.hit_item(*at).map(|i| &self.items[i]).filter(|t| t.id != *from);
                    let ends = match target {
                        Some(t) => {
                            let c: Vec<Pos2> = t.card_corners().iter().map(|p| self.w2s(*p)).collect();
                            painter.add(Shape::closed_line(c, Stroke::new(2.0, ACCENT)));
                            link_ends(src, t)
                        }
                        None => src.edge_toward(*at).map(|e| (e, *at)),
                    };
                    if let Some((a, b)) = ends {
                        draw_link(painter, self.w2s(a), self.w2s(b), true, LINK_PX, self.pen_color);
                    }
                }
            }
            Drag::Draw {
                points, snapped, ..
            } => {
                // Preview freehand parts through the gap-filling spline so the live line is
                // already smooth; grid-snapped parts stay straight.
                let pts = smooth::finish_mixed(points, snapped, 0.0, 1.0 / self.zoom)
                    .into_iter()
                    .map(|p| self.w2s(p))
                    .collect();
                draw_polyline(painter, pts, (self.pen_width * self.zoom).max(0.75), self.pen_color);
            }
            _ => {}
        }
        if let Some(p) = hover.filter(|p| rect.contains(*p)) {
            match self.tool {
                Tool::Eraser => {
                    painter.circle_stroke(p, self.eraser_radius, Stroke::new(1.5, Color32::WHITE));
                    painter.circle_stroke(
                        p,
                        self.eraser_radius + 1.5,
                        Stroke::new(1.0, Color32::from_black_alpha(160)),
                    );
                }
                Tool::Pen if matches!(self.drag, Drag::None) => {
                    painter.circle_stroke(
                        p,
                        self.pen_width * self.zoom * 0.5 + 1.0,
                        Stroke::new(1.0, self.pen_color.gamma_multiply(0.8)),
                    );
                }
                _ => {}
            }
        }

        let dim = Color32::from_gray(130);
        if self.items.is_empty() && self.loading == 0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Drop images or PDFs here, or paste with Ctrl+V\n\
                 B: pen   E: eraser   C: crop   F1: all shortcuts\n\
                 Right-click for menu",
                FontId::proportional(18.0),
                dim,
            );
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            painter.rect_filled(rect.shrink(8.0), 12.0, ACCENT.gamma_multiply(0.15));
            painter.rect_stroke(rect.shrink(8.0), 12.0, Stroke::new(2.0, ACCENT), StrokeKind::Inside);
        }
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.set_min_width(200.0);
        let has_sel = self.any_selected() || self.any_link_selected();

        if ui.button("Paste\tCtrl+V").clicked() {
            self.paste(ctx, None);
            ui.close();
        }
        if ui.button("Import images / PDF…\tCtrl+I").clicked() {
            ui.close();
            self.import_dialog(ctx);
        }
        ui.separator();
        ui.add_enabled_ui(has_sel, |ui| {
            if ui.button("Copy\tCtrl+C").clicked() {
                self.copy(ctx);
                ui.close();
            }
            if ui.button("Duplicate\tCtrl+D").clicked() {
                self.duplicate();
                ui.close();
            }
            if ui.button("Delete\tDel").clicked() {
                self.delete_selected();
                ui.close();
            }
            if self.cropping.is_some() {
                if ui.button("Finish crop\tEnter").clicked() {
                    self.finish_crop();
                    ui.close();
                }
            } else if ui
                .add_enabled(
                    self.single_selected_image().is_some(),
                    egui::Button::new("Crop\tC"),
                )
                .clicked()
            {
                self.start_crop(ctx);
                ui.close();
            }
            if ui
                .add_enabled(
                    self.single_selected_image().is_some(),
                    egui::Button::new("Title…\tT"),
                )
                .clicked()
            {
                self.title_selected(ctx);
                ui.close();
            }
            ui.add_enabled_ui(self.has_selected_image(), |ui| {
                ui.menu_button("Label colour", |ui| {
                    ui.horizontal(|ui| {
                        for c in TAG_COLORS {
                            let on = self.selected().all(|it| it.is_stroke() || it.tag == Some(c));
                            if swatch(ui, c, on).clicked() {
                                self.set_tag(Some(c));
                                ui.close();
                            }
                        }
                    });
                    if ui.button("No label").clicked() {
                        self.set_tag(None);
                        ui.close();
                    }
                });
            });
            if self.any_link_selected() {
                ui.separator();
                if ui.button("Reverse link direction").clicked() {
                    self.reverse_links();
                    ui.close();
                }
                let mut arrow = self.links.iter().filter(|l| l.selected).all(|l| l.arrow);
                if ui.checkbox(&mut arrow, "Arrowhead").clicked() {
                    self.set_link_arrows(arrow);
                }
                ui.separator();
            }
            ui.menu_button("Transform", |ui| {
                if ui.button("Flip horizontal\tH").clicked() {
                    self.flip(true);
                    ui.close();
                }
                if ui.button("Flip vertical\tShift+H").clicked() {
                    self.flip(false);
                    ui.close();
                }
                if ui.button("Reset crop").clicked() {
                    self.reset_crop();
                    ui.close();
                }
                if ui.button("Reset transform\tCtrl+T").clicked() {
                    self.reset_transform();
                    ui.close();
                }
                if ui.button("Normalize sizes").clicked() {
                    self.normalize();
                    ui.close();
                }
            });
            ui.menu_button("Order", |ui| {
                if ui.button("Bring to front\tPgUp").clicked() {
                    self.reorder(true);
                    ui.close();
                }
                if ui.button("Send to back\tPgDn").clicked() {
                    self.reorder(false);
                    ui.close();
                }
            });
        });
        if ui.button("Arrange\tCtrl+P").clicked() {
            self.arrange();
            ui.close();
        }
        if ui.button("Fit view\tF").clicked() {
            self.fit();
            ui.close();
        }
        ui.separator();
        ui.menu_button("View", |ui| {
            for (name, c) in BACKGROUNDS {
                if ui.radio(self.background == c, name).clicked() {
                    self.background = c;
                    self.dirty = true;
                    ui.close();
                }
            }
            ui.separator();
            ui.checkbox(&mut self.grid, "Dot grid\tG");
            ui.checkbox(&mut self.show_toolbar, "Toolbar\tTab");
            let mut on_top = self.always_on_top;
            if ui.checkbox(&mut on_top, "Always on top\tCtrl+Shift+T").clicked() {
                self.toggle_on_top(ctx);
            }
        });
        ui.menu_button("File", |ui| {
            if ui.button("New\tCtrl+N").clicked() {
                ui.close();
                self.request(ctx, Pending::New);
            }
            if ui.button("Open…\tCtrl+O").clicked() {
                ui.close();
                self.request(ctx, Pending::Open(None));
            }
            if ui.button("Save\tCtrl+S").clicked() {
                ui.close();
                self.save_board(ctx, false);
            }
            if ui.button("Save as…\tCtrl+Shift+S").clicked() {
                ui.close();
                self.save_board(ctx, true);
            }
        });
        if ui.button("Shortcuts\tF1").clicked() {
            self.show_help = true;
            ui.close();
        }
    }

    // ------------------------------------------------------------------ panels

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::Area::new(Id::new("toolbar"))
            .anchor(Align2::LEFT_TOP, vec2(10.0, 10.0))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (tool, label, tip) in [
                            (Tool::Select, "Select", "Select / move / scale / rotate  (S)"),
                            (Tool::Pen, "Pen", "Freehand sketching  (B)"),
                            (Tool::Eraser, "Eraser", "Erase strokes and links  (E)"),
                            (Tool::Link, "Link", "Connect items: drag from one to another  (L)"),
                        ] {
                            if ui
                                .selectable_label(self.tool == tool && self.cropping.is_none(), label)
                                .on_hover_text(tip)
                                .clicked()
                            {
                                self.set_tool(tool);
                            }
                        }
                        let can_crop =
                            self.cropping.is_some() || self.single_selected_image().is_some();
                        if ui
                            .add_enabled(
                                can_crop,
                                egui::Button::selectable(self.cropping.is_some(), "Crop"),
                            )
                            .on_hover_text("Crop the selected image  (C)")
                            .on_disabled_hover_text("Select a single image to crop it")
                            .clicked()
                        {
                            self.toggle_crop(ctx);
                        }
                        ui.separator();
                        self.grid_controls(ui);
                        ui.separator();
                        for c in INK_COLORS {
                            if swatch(ui, c, c == self.pen_color)
                                .on_hover_text("Ink colour (recolours selected strokes and links)")
                                .clicked()
                            {
                                self.pen_color = c;
                                self.recolor_ink(c);
                            }
                        }
                        ui.separator();
                        if self.tool == Tool::Eraser {
                            ui.add(
                                egui::Slider::new(&mut self.eraser_radius, 2.0..=200.0)
                                    .logarithmic(true)
                                    .text("size"),
                            );
                        } else {
                            ui.add(
                                egui::Slider::new(&mut self.pen_width, 0.5..=200.0)
                                    .logarithmic(true)
                                    .text("width"),
                            )
                            .on_hover_text(
                                "Pen width in board pixels (equal to screen pixels at 100% zoom).\n\
                                 Strokes keep this size relative to images at any zoom.",
                            );
                            if self.tool == Tool::Pen {
                                ui.add(
                                    egui::Slider::new(&mut self.smoothing, 0.0..=1.0)
                                        .text("smooth")
                                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                        .custom_parser(|s| {
                                            s.trim_end_matches('%').trim().parse::<f64>().ok().map(|v| v / 100.0)
                                        }),
                                )
                                .on_hover_text(
                                    "Jitter smoothing applied when you lift the pen.\n\
                                     Strokes are always drawn as smooth curves through the mouse \
                                     samples; 0% keeps every sample exactly.",
                                );
                            }
                        }
                    });
                });
            });
    }

    fn grid_controls(&mut self, ui: &mut egui::Ui) {
        ui.toggle_value(&mut self.grid, "Grid")
            .on_hover_text("Show dot grid  (G)\nHold Shift while dragging to snap");
        let mut size = self.grid_size;
        egui::ComboBox::from_id_salt("grid-size")
            .selected_text(format!("{} px", fmt_grid(size)))
            .width(70.0)
            .show_ui(ui, |ui| {
                for g in GRID_PRESETS {
                    ui.selectable_value(&mut size, g, format!("{} px", fmt_grid(g)));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Custom:");
                    ui.add(
                        egui::DragValue::new(&mut size)
                            .range(1.0..=2000.0)
                            .speed(0.5)
                            .suffix(" px"),
                    );
                });
            })
            .response
            .on_hover_text("Grid spacing  (N / Shift+N)");
        self.set_grid_size(size);
    }

    fn hud(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.status.as_ref().is_some_and(|(_, until)| *until < now) {
            self.status = None;
        }
        let mut parts = vec![format!("{:.0}%", self.zoom * 100.0)];
        let snapping = ctx.input(|i| i.modifiers.shift) && !matches!(self.drag, Drag::None);
        if self.grid || snapping {
            let snap_note = if snapping { " (snapping)" } else { "" };
            parts.push(format!("grid {} px{snap_note}", fmt_grid(self.grid_size)));
        }
        if !self.items.is_empty() {
            parts.push(format!("{} items", self.items.len()));
        }
        if !self.links.is_empty() {
            parts.push(format!("{} links", self.links.len()));
        }
        if self.loading > 0 {
            parts.push("loading…".into());
        }
        if let Some((s, until)) = &self.status {
            parts.push(s.clone());
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((until - now).max(0.05)));
        }
        let lum = self.background.r() as u32 + self.background.g() as u32 + self.background.b() as u32;
        let color = if lum > 382 {
            Color32::from_gray(70)
        } else {
            Color32::from_gray(150)
        };
        egui::Area::new(Id::new("hud"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-10.0, -8.0))
            .order(Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(parts.join("  ·  ")).color(color).size(12.0))
                        .extend(),
                );
            });
    }

    /// Inline text field over the title bar of the image being titled.
    fn title_editor(&mut self, ctx: &egui::Context) {
        let Some(id) = self.title_edit.as_ref().map(|e| e.id) else {
            return;
        };
        let Some(it) = self.item_by_id(id) else {
            self.title_edit = None;
            return;
        };
        let h = it.half();
        let tb = it.title_bar_height();
        let top_left = self.w2s(it.center + rotate(vec2(-h.x, -h.y - tb), it.rotation));
        let width = (h.x * 2.0 * self.zoom).clamp(160.0, 520.0);
        let font = (tb * 0.6 * self.zoom).clamp(13.0, 28.0);
        let edit = self.title_edit.as_mut().unwrap();
        // Some(true): apply, Some(false): cancel.
        let mut done = None;
        egui::Area::new(Id::new("title-edit"))
            .fixed_pos(top_left)
            .order(Order::Foreground)
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut edit.text)
                        .desired_width(width)
                        .font(FontId::proportional(font))
                        .hint_text("Title (Enter to apply, Esc to cancel)"),
                );
                if std::mem::take(&mut edit.focus) {
                    r.request_focus();
                } else if r.lost_focus() {
                    done = Some(!ui.input(|i| i.key_pressed(Key::Escape)));
                }
            });
        match done {
            Some(true) => self.commit_title(),
            Some(false) => self.title_edit = None,
            None => {}
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        const ROWS: &[(&str, &str)] = &[
            ("Pan", "Middle / right drag, or Space + drag"),
            ("Zoom", "Scroll wheel, Ctrl +/-, Ctrl+0 = 100%"),
            ("Fit all / selection", "F, or double-click empty space"),
            ("Select tool", "S or V"),
            ("Pen / Eraser", "B / E   ( [ and ] change size; Shift: 0/45/90° lines on the grid )"),
            ("Select", "Click, Shift+click, drag a box"),
            ("Scale", "Drag a corner handle"),
            ("Rotate", "Drag just outside a corner (Shift snaps 15°)"),
            ("Paste / Copy / Cut", "Ctrl+V / Ctrl+C / Ctrl+X"),
            ("Duplicate / Delete", "Ctrl+D / Del"),
            ("Crop image", "C, drag the handles; Enter applies, Esc cancels"),
            ("Flip", "H horizontal, Shift+H vertical"),
            ("Reset transform", "Ctrl+T"),
            ("Arrange", "Ctrl+P"),
            ("Bring front / back", "PgUp / PgDn"),
            ("Undo / Redo", "Ctrl+Z / Ctrl+Shift+Z"),
            ("New / Open / Save", "Ctrl+N / Ctrl+O / Ctrl+S"),
            ("Import images / PDF pages", "Ctrl+I (or drag & drop)"),
            ("Always on top", "Ctrl+Shift+T"),
            ("Grid", "G show · N / Shift+N spacing · hold Shift while dragging to snap"),
            ("Title an image", "T or F2, or double-click its title bar · right-click → Label colour"),
            ("Link items", "L, then drag from one item to another · click a link to select it, Del removes"),
            ("Snap while moving", "Shift: a stroke's points (or an image's corner) land on the grid"),
            ("Toolbar", "Tab"),
        ];
        egui::Window::new("Shortcuts")
            .open(&mut self.show_help)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("help").striped(true).show(ui, |ui| {
                    for (a, b) in ROWS {
                        ui.strong(*a);
                        ui.label(*b);
                        ui.end_row();
                    }
                });
            });
    }

    fn unsaved_modal(&mut self, ctx: &egui::Context) {
        if self.pending.is_none() {
            return;
        }
        let mut choice = None;
        let resp = egui::Modal::new(Id::new("unsaved")).show(ctx, |ui| {
            ui.heading("Unsaved changes");
            ui.label("Save the current board before continuing?");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(0);
                }
                if ui.button("Don't save").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(2);
                }
            });
        });
        if resp.should_close() && choice.is_none() {
            choice = Some(2);
        }
        match choice {
            Some(0) => {
                if self.save_board(ctx, false) {
                    self.proceed(ctx);
                }
            }
            Some(1) => self.proceed(ctx),
            Some(_) => self.pending = None,
            None => {}
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        let title = format!("{name}{} — RifRef", if self.dirty { "*" } else { "" });
        if title != self.last_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let last = self
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        storage.set_string(LAST_BOARD_KEY, last);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_msg(ctx, msg);
        }
        if self.cropping.is_some() && self.crop_target().is_none() {
            // The image went away (deleted, undone) while it was being cropped.
            self.cropping = None;
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty && !self.allow_close {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.pending = Some(Pending::Close);
        }
        self.handle_dropped(ctx);
        self.handle_shortcuts(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(self.background))
            .show(ctx, |ui| self.canvas(ui));

        if self.show_toolbar {
            self.toolbar(ctx);
        }
        self.pdf_dialog(ctx);
        self.hud(ctx);
        self.title_editor(ctx);
        if self.show_help {
            self.help_window(ctx);
        }
        self.unsaved_modal(ctx);
        self.update_title(ctx);
    }
}

// ------------------------------------------------------------------- helpers

fn draw_polyline(painter: &egui::Painter, pts: Vec<Pos2>, width: f32, color: Color32) {
    let Some(&first) = pts.first() else {
        return;
    };
    if pts.len() == 1 || pts.iter().all(|p| p.distance(first) < 0.5) {
        painter.circle_filled(first, width * 0.5, color);
        return;
    }
    // egui joins thick line segments with miters, which stick out as square or chopped-off
    // points at sharp turns. So split the stroke at sharp corners, draw each smooth run as its
    // own line (gentle turns make invisible miters), and put a round dot on each corner and end.
    const SHARP_COS: f32 = 0.9; // turns sharper than ~25°
    let round = width > 2.0;
    let stroke = Stroke::new(width, color);
    let mut start = 0;
    for i in 1..pts.len() - 1 {
        let (d1, d2) = (pts[i] - pts[i - 1], pts[i + 1] - pts[i]);
        if d1.length_sq() == 0.0 || d2.length_sq() == 0.0 {
            continue;
        }
        if d1.normalized().dot(d2.normalized()) < SHARP_COS {
            painter.add(Shape::line(pts[start..=i].to_vec(), stroke));
            if round {
                painter.circle_filled(pts[i], width * 0.5, color);
            }
            start = i;
        }
    }
    let last = *pts.last().unwrap();
    painter.add(Shape::line(pts[start..].to_vec(), stroke));
    if round {
        painter.circle_filled(first, width * 0.5, color);
        painter.circle_filled(last, width * 0.5, color);
    }
}

/// A link line from `a` to `b` (screen), with an optional arrowhead at `b`.
fn draw_link(painter: &egui::Painter, a: Pos2, b: Pos2, arrow: bool, width: f32, color: Color32) {
    let d = b - a;
    let len = d.length();
    if len < 1.0 {
        return;
    }
    let dir = d / len;
    let head = (ARROW_PX + width * 0.5).min(len * 0.5);
    let end = if arrow { b - dir * head * 0.8 } else { b };
    painter.line_segment([a, end], Stroke::new(width, color));
    if arrow {
        let side = dir.rot90() * head * 0.45;
        let back = b - dir * head;
        painter.add(Shape::convex_polygon(vec![b, back + side, back - side], color, Stroke::NONE));
    }
}

fn swatch(ui: &mut egui::Ui, c: Color32, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let p = ui.painter();
    let r = rect.shrink(2.0);
    if c.a() == 0 {
        p.rect_stroke(r, 3.0, Stroke::new(1.0, Color32::from_gray(160)), StrokeKind::Inside);
        p.line_segment([r.left_bottom(), r.right_top()], Stroke::new(1.0, Color32::from_rgb(220, 80, 80)));
    } else {
        p.rect_filled(r, 3.0, c);
    }
    if selected {
        p.rect_stroke(rect, 4.0, Stroke::new(2.0, ACCENT), StrokeKind::Inside);
    } else if resp.hovered() {
        p.rect_stroke(rect, 4.0, Stroke::new(1.0, Color32::from_gray(200)), StrokeKind::Inside);
    }
    resp
}

/// Nearest grid point.
fn snap(p: Pos2, grid: f32) -> Pos2 {
    pos2((p.x / grid).round() * grid, (p.y / grid).round() * grid)
}

fn fmt_grid(g: f32) -> String {
    if g.fract() == 0.0 {
        format!("{g:.0}")
    } else {
        format!("{g:.1}")
    }
}

fn color(c: [u8; 4]) -> Color32 {
    Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3])
}

fn is_scene_path(p: &Path) -> bool {
    p.extension().is_some_and(|e| {
        scene::OPEN_EXTENSIONS
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_to_nearest_grid_point() {
        assert_eq!(snap(pos2(14.0, -17.0), 10.0), pos2(10.0, -20.0));
        assert_eq!(snap(pos2(33.0, 47.0), 32.0), pos2(32.0, 32.0));
        assert_eq!(snap(pos2(2.5, 7.4), 2.5), pos2(2.5, 7.5));
    }

    #[test]
    fn grid_labels() {
        assert_eq!(fmt_grid(32.0), "32");
        assert_eq!(fmt_grid(2.5), "2.5");
    }

    #[test]
    fn presets_are_sorted_and_include_default() {
        assert!(GRID_PRESETS.windows(2).all(|w| w[0] < w[1]));
        assert!(GRID_PRESETS.contains(&DEFAULT_GRID));
    }
}
