use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use eframe::egui::{
    self, epaint::TextShape, pos2, vec2, Align2, Color32, CursorIcon, Event, FontId, Id, Key,
    Margin, Mesh, Modifiers, Order, PointerButton, Pos2, Rect, Sense, Shape, Stroke, StrokeKind,
    TextureFilter, TextureOptions, Vec2, ViewportCommand,
};

use crate::item::{
    polyline_distance, rotate, segment_distance, stroke_geometry, Content, Frame as SelFrame,
    ImageAsset, Item, Note, StrokeData, FULL_CROP, NOTE_DEFAULT_FONT, NOTE_DEFAULT_WIDTH, NOTE_PAD,
};
use crate::loader::{self, Decoded, Layout, LoadMsg};
use crate::pdf;
use crate::scene::{self, SavedItem, SavedKind, SceneFile};
use crate::smooth;

const HANDLE_PX: f32 = 9.0;
const ROTATE_PX: f32 = 28.0;
const MIN_ZOOM: f32 = 0.005;
const MAX_ZOOM: f32 = 100.0;
const UNDO_LIMIT: usize = 200;
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

/// Note styles: (background, text). A transparent background uses the current ink colour for text.
const NOTE_COLORS: [(Color32, Color32); 6] = [
    (Color32::from_rgb(255, 238, 140), Color32::from_rgb(40, 35, 20)),
    (Color32::from_rgb(255, 196, 208), Color32::from_rgb(50, 25, 30)),
    (Color32::from_rgb(186, 220, 255), Color32::from_rgb(20, 35, 55)),
    (Color32::from_rgb(198, 240, 186), Color32::from_rgb(25, 45, 20)),
    (Color32::from_rgb(52, 52, 58), Color32::from_rgb(235, 235, 235)),
    (Color32::TRANSPARENT, Color32::WHITE),
];

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
    Note,
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
    Move {
        last: Pos2,
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
        /// Raw pointer samples (world).
        points: Vec<Pos2>,
    },
    Erase {
        changed: bool,
    },
}

enum Pending {
    Close,
    New,
    Open(Option<PathBuf>),
}

pub struct App {
    items: Vec<Item>,
    next_id: u64,

    pan: Vec2,
    zoom: f32,
    view_init: bool,
    canvas_rect: Rect,

    tool: Tool,
    drag: Drag,
    menu_pos: Pos2,

    undo: Vec<Vec<Item>>,
    redo: Vec<Vec<Item>>,

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
    pen_width: f32,
    /// Stroke smoothing strength, 0..=1.
    smoothing: f32,
    eraser_radius: f32,
    note_style: usize,

    editing: Option<u64>,
    edit_original: Option<String>,
    edit_focus_pending: bool,

    cropping: Option<CropState>,

    background: Color32,
    grid: bool,
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
            next_id: 1,
            pan: Vec2::ZERO,
            zoom: 1.0,
            view_init: false,
            canvas_rect: Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0)),
            tool: Tool::Select,
            drag: Drag::None,
            menu_pos: Pos2::ZERO,
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
            note_style: 0,
            editing: None,
            edit_original: None,
            edit_focus_pending: false,
            cropping: None,
            background: BACKGROUNDS[0].1,
            grid: false,
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

    fn push_undo(&mut self) {
        self.undo.push(self.items.clone());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.dirty = true;
    }

    fn undo(&mut self) {
        self.finish_editing();
        self.finish_crop();
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.items, prev));
            self.dirty = true;
        }
    }

    fn redo(&mut self) {
        self.finish_editing();
        self.finish_crop();
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.items, next));
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
        self.finish_editing();
        self.cropping = None;
        self.items.clear();
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
            ([offset.x, offset.y], self.zoom),
        );
        let mut image_index: HashMap<*const ImageAsset, usize> = HashMap::new();
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
                Content::Note(n) => SavedKind::Note {
                    text: n.text.clone(),
                    width: it.size.x,
                    bg: n.bg.to_array(),
                    fg: n.fg.to_array(),
                    font_size: n.font_size,
                },
                Content::Stroke(s) => SavedKind::Stroke {
                    points: s.points.iter().map(|p| [p.x, p.y]).collect(),
                    color: s.color.to_array(),
                    width: s.width,
                    size: [it.size.x, it.size.y],
                },
            };
            scene.items.push(SavedItem {
                center: [it.center.x, it.center.y],
                scale: it.scale,
                rotation: it.rotation,
                flip_x: it.flip_x,
                flip_y: it.flip_y,
                kind,
            });
        }
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
        for s in scene.items {
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
                SavedKind::Note {
                    text,
                    width,
                    bg,
                    fg,
                    font_size,
                } => (
                    Content::Note(Note {
                        text,
                        bg: color(bg),
                        fg: color(fg),
                        font_size,
                    }),
                    vec2(width, font_size + 2.0 * NOTE_PAD),
                ),
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
            self.items.push(it);
        }
        self.background = color(scene.background);
        self.grid = scene.grid;
        match scene.view {
            Some((offset, zoom)) => {
                self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                self.pan = self.canvas_rect.center().to_vec2() + vec2(offset[0], offset[1]);
            }
            None => self.fit(),
        }
        if missing > 0 {
            self.set_status(ctx, format!("{missing} image(s) could not be decoded"));
        } else {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.set_status(ctx, format!("Opened {name}"));
        }
        self.path = Some(path);
        self.dirty = false;
    }

    /// Returns true if the board was saved.
    fn save_board(&mut self, ctx: &egui::Context, save_as: bool) -> bool {
        self.finish_editing();
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
        if self.any_selected() {
            self.push_undo();
            self.items.retain(|i| !i.selected);
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
        for mut c in copies {
            c.id = self.alloc_id();
            self.items.push(c);
        }
    }

    fn flip(&mut self, horizontal: bool) {
        if !self.selected().any(|i| !i.is_note()) {
            return;
        }
        self.push_undo();
        for it in self.items.iter_mut().filter(|i| i.selected && !i.is_note()) {
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
        let applies = |it: &Item| match &it.content {
            Content::Stroke(_) => true,
            Content::Note(n) => n.bg.a() == 0,
            _ => false,
        };
        if !self.selected().any(applies) {
            return;
        }
        self.push_undo();
        for it in self.items.iter_mut().filter(|i| i.selected) {
            match &mut it.content {
                Content::Stroke(s) => s.color = c,
                Content::Note(n) if n.bg.a() == 0 => n.fg = c,
                _ => {}
            }
        }
    }

    fn note_colors(&self, style: usize) -> (Color32, Color32) {
        let (bg, fg) = NOTE_COLORS[style];
        if bg.a() == 0 {
            (bg, self.pen_color)
        } else {
            (bg, fg)
        }
    }

    fn restyle_notes(&mut self, style: usize) {
        if !self.selected().any(Item::is_note) {
            return;
        }
        self.push_undo();
        let (bg, fg) = self.note_colors(style);
        for it in self.items.iter_mut().filter(|i| i.selected) {
            if let Some(n) = it.note_mut() {
                n.bg = bg;
                n.fg = fg;
            }
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        self.finish_editing();
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
        self.finish_editing();
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

    // ------------------------------------------------------------------ notes

    fn add_note(&mut self, at: Pos2, text: String) -> usize {
        let (bg, fg) = self.note_colors(self.note_style);
        let id = self.alloc_id();
        // Notes keep a constant on-screen size when created, whatever the zoom.
        let mut it = Item::new(
            id,
            Content::Note(Note {
                text,
                bg,
                fg,
                font_size: NOTE_DEFAULT_FONT,
            }),
            at,
            vec2(NOTE_DEFAULT_WIDTH, NOTE_DEFAULT_FONT + 2.0 * NOTE_PAD),
        );
        it.scale = 1.0 / self.zoom;
        // Anchor the top-left corner at the click position.
        it.center = at + it.half();
        self.deselect_all();
        it.selected = true;
        self.items.push(it);
        self.items.len() - 1
    }

    fn create_note(&mut self, at: Pos2) {
        self.finish_editing();
        self.push_undo();
        let idx = self.add_note(at, String::new());
        self.editing = Some(self.items[idx].id);
        self.edit_original = None;
        self.edit_focus_pending = true;
    }

    fn start_editing(&mut self, idx: usize) {
        self.finish_editing();
        let Some(text) = self.items[idx].note_mut().map(|n| n.text.clone()) else {
            return;
        };
        self.push_undo();
        self.deselect_all();
        self.items[idx].selected = true;
        self.editing = Some(self.items[idx].id);
        self.edit_original = Some(text);
        self.edit_focus_pending = true;
    }

    fn finish_editing(&mut self) {
        let Some(id) = self.editing.take() else {
            return;
        };
        let original = self.edit_original.take();
        let Some(idx) = self.items.iter().position(|i| i.id == id) else {
            return;
        };
        let text = self.items[idx]
            .note_mut()
            .map(|n| n.text.clone())
            .unwrap_or_default();
        if text.trim().is_empty() {
            self.items.remove(idx);
            if original.is_none() {
                // A freshly created note that was never filled in: nothing happened.
                self.undo.pop();
            }
        } else if original.as_deref() == Some(text.as_str()) {
            self.undo.pop();
        }
    }

    /// Keep note heights in sync with their wrapped text.
    fn layout_notes(&mut self, painter: &egui::Painter) {
        for it in &mut self.items {
            if let Content::Note(n) = &it.content {
                let wrap = (it.size.x - 2.0 * NOTE_PAD).max(10.0);
                let g = painter.layout(
                    n.text.clone(),
                    FontId::proportional(n.font_size),
                    n.fg,
                    wrap,
                );
                it.size.y = g.size().y.max(n.font_size) + 2.0 * NOTE_PAD;
            }
        }
    }

    // ---------------------------------------------------------------- sketching

    fn commit_stroke(&mut self, points: Vec<Pos2>) {
        if points.is_empty() {
            return;
        }
        let width = self.pen_width / self.zoom;
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
        let mut changed = false;
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
            self.push_undo();
            self.add_note(at, text.trim_end().to_owned());
        }
    }

    /// Copies the first selected image (or note text) to the system clipboard.
    fn copy(&mut self, ctx: &egui::Context) -> bool {
        let Some(it) = self
            .selected()
            .find(|i| matches!(i.content, Content::Image(_)))
            .or_else(|| self.selected().find(|i| i.is_note()))
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
            Content::Note(n) => {
                let text = n.text.clone();
                self.clipboard
                    .as_mut()
                    .ok_or("no clipboard".to_string())
                    .and_then(|cb| cb.set_text(text).map_err(|e| e.to_string()))
            }
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
        if k(none, Key::T) || k(none, Key::N) {
            self.set_tool(Tool::Note);
        }
        if k(none, Key::G) {
            self.grid = !self.grid;
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
            self.pen_width = (self.pen_width * f).clamp(1.0, 60.0);
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
            Tool::Pen => self.drag = Drag::Draw { points: vec![w] },
            Tool::Eraser => {
                self.push_undo();
                let changed = self.erase_at(w, self.eraser_radius / self.zoom);
                self.drag = Drag::Erase { changed };
            }
            Tool::Select | Tool::Note => {
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
                    self.drag = Drag::Move { last: w };
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
        if let Drag::Crop {
            grip,
            orig,
            start_px,
        } = &self.drag
        {
            let (grip, orig, start_px) = (*grip, orig.clone(), *start_px);
            self.drag_crop(grip, &orig, start_px, w);
            return;
        }
        match &mut self.drag {
            Drag::None | Drag::Crop { .. } => {}
            Drag::Pan { last } => {
                self.pan += pos - *last;
                *last = pos;
            }
            Drag::Move { last } => {
                let d = w - *last;
                *last = w;
                for it in self.items.iter_mut().filter(|i| i.selected) {
                    it.center += d;
                }
            }
            Drag::Scale {
                anchor,
                start,
                orig,
            } => {
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
            Drag::Draw { points } => {
                if points.last().is_none_or(|l| l.distance(w) * zoom >= 1.0) {
                    points.push(w);
                }
            }
            Drag::Erase { changed } => {
                let c = *changed;
                let now = self.erase_at(w, self.eraser_radius / zoom);
                self.drag = Drag::Erase { changed: c || now };
            }
        }
    }

    fn end_drag(&mut self) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Draw { points } => {
                let points = smooth::finish_stroke(&points, self.smoothing, 1.0 / self.zoom);
                self.commit_stroke(points);
            }
            Drag::Erase { changed: false } => {
                self.undo.pop();
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
            Tool::Select => match self.hit_item(w) {
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
            Tool::Note => {
                match self.hit_item(w) {
                    Some(i) if self.items[i].is_note() => self.start_editing(i),
                    _ => self.create_note(w),
                }
                self.tool = Tool::Select;
            }
            Tool::Pen => self.commit_stroke(vec![w]),
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
            Some(i) if self.items[i].is_note() => self.start_editing(i),
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
            self.finish_editing();
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
            self.end_drag();
        }
        if let Some(p) = pointer {
            let w = self.s2w(p);
            if resp.double_clicked() && self.tool == Tool::Select {
                self.double_click(w);
            } else if resp.clicked() {
                self.finish_editing();
                self.click(w, mods);
            }
            if resp.secondary_clicked() {
                self.finish_editing();
                self.menu_pos = w;
                if let Some(i) = self.hit_item(w) {
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
                    Tool::Eraser => CursorIcon::None,
                    Tool::Note => CursorIcon::Text,
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
        self.layout_notes(&painter);
        // Pen strokes always sit above images and notes.
        let (strokes, others): (Vec<&Item>, Vec<&Item>) =
            self.items.iter().partition(|it| it.is_stroke());
        for it in others.into_iter().chain(strokes) {
            let aabb = it.world_aabb();
            let screen = Rect::from_two_pos(self.w2s(aabb.min), self.w2s(aabb.max));
            if screen.intersects(rect) {
                self.draw_item(&painter, it);
            }
        }
        self.draw_selection(&painter);
        self.draw_overlays(&ctx, &painter, rect, hover);
    }

    fn draw_grid(&self, painter: &egui::Painter, rect: Rect) {
        let mut step = 32.0;
        while step * self.zoom < 24.0 {
            step *= 2.0;
        }
        while step * self.zoom > 96.0 {
            step /= 2.0;
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
        let editing = self.editing == Some(it.id);
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
            }
            Content::Note(n) => {
                // While editing, the note is shown upright so it lines up with the text editor.
                let corners: Vec<Pos2> = if editing {
                    let h = it.half();
                    let r = Rect::from_min_max(self.w2s(it.center - h), self.w2s(it.center + h));
                    vec![r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()]
                } else {
                    it.corners().iter().map(|c| self.w2s(*c)).collect()
                };
                if n.bg.a() > 0 {
                    // Soft drop shadow.
                    let off = vec2(2.0, 3.0);
                    painter.add(Shape::convex_polygon(
                        corners.iter().map(|c| *c + off).collect(),
                        Color32::from_black_alpha(60),
                        Stroke::NONE,
                    ));
                    painter.add(Shape::convex_polygon(corners, n.bg, Stroke::NONE));
                } else if editing || n.text.is_empty() {
                    painter.add(Shape::closed_line(
                        corners,
                        Stroke::new(1.0, Color32::from_gray(120)),
                    ));
                }
                if !editing {
                    let fs = n.font_size * it.scale * self.zoom;
                    if fs >= 2.0 {
                        let wrap = (it.size.x - 2.0 * NOTE_PAD) * it.scale * self.zoom;
                        let galley = painter.layout(
                            n.text.clone(),
                            FontId::proportional(fs.min(400.0)),
                            n.fg,
                            wrap.max(1.0),
                        );
                        let tl = it.local_to_world(vec2(
                            -it.size.x * 0.5 + NOTE_PAD,
                            -it.size.y * 0.5 + NOTE_PAD,
                        ));
                        painter.add(
                            TextShape::new(self.w2s(tl), galley, n.fg).with_angle(it.rotation),
                        );
                    }
                }
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
        if count == 0 || !matches!(self.tool, Tool::Select | Tool::Note) {
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
            Drag::Draw { points } => {
                // Preview through the gap-filling spline so the live line is already smooth.
                let pts = smooth::densify(points, 1.0 / self.zoom)
                    .into_iter()
                    .map(|p| self.w2s(p))
                    .collect();
                draw_polyline(painter, pts, self.pen_width, self.pen_color);
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
                        self.pen_width * 0.5 + 1.0,
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
                 B: pen   E: eraser   T: note   F1: all shortcuts\n\
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
        let has_sel = self.any_selected();
        let at = self.menu_pos;

        if ui.button("Paste\tCtrl+V").clicked() {
            self.paste(ctx, None);
            ui.close();
        }
        if ui.button("Import images / PDF…\tCtrl+I").clicked() {
            ui.close();
            self.import_dialog(ctx);
        }
        if ui.button("Add note here\tT").clicked() {
            self.create_note(at);
            ui.close();
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

    fn note_editor(&mut self, ctx: &egui::Context) {
        let Some(id) = self.editing else {
            return;
        };
        let Some(idx) = self.items.iter().position(|i| i.id == id) else {
            self.editing = None;
            return;
        };
        let focus_now = std::mem::take(&mut self.edit_focus_pending);
        let zoom = self.zoom;
        let it = &self.items[idx];
        let pos = self.w2s(it.center - it.half() + Vec2::splat(NOTE_PAD * it.scale));
        let (scale, width) = (it.scale, it.size.x);
        let Some(n) = self.items[idx].note_mut() else {
            return;
        };
        let fs = (n.font_size * scale * zoom).clamp(4.0, 400.0);
        let wrap = ((width - 2.0 * NOTE_PAD) * scale * zoom).max(10.0);
        let fg = n.fg;
        let mut done = false;
        egui::Area::new(Id::new("note_editor"))
            .order(Order::Foreground)
            .fixed_pos(pos)
            .constrain(false)
            .show(ctx, |ui| {
                ui.visuals_mut().text_cursor.stroke.color = fg;
                let r = ui.add(
                    egui::TextEdit::multiline(&mut n.text)
                        .font(FontId::proportional(fs))
                        .text_color(fg)
                        .desired_width(wrap)
                        .desired_rows(1)
                        .frame(false)
                        .margin(Margin::ZERO)
                        .hint_text("Type a note…"),
                );
                if focus_now {
                    r.request_focus();
                }
                if r.lost_focus() {
                    done = true;
                }
            });
        if done {
            self.finish_editing();
        }
    }

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
                            (Tool::Eraser, "Eraser", "Erase strokes  (E)"),
                            (Tool::Note, "Note", "Click to place a text note  (T)"),
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
                        for c in INK_COLORS {
                            if swatch(ui, c, c == self.pen_color)
                                .on_hover_text("Ink colour (recolours selected strokes)")
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
                                egui::Slider::new(&mut self.pen_width, 1.0..=60.0)
                                    .logarithmic(true)
                                    .text("width"),
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
                        ui.separator();
                        for (i, (bg, _)) in NOTE_COLORS.iter().enumerate() {
                            if swatch(ui, *bg, i == self.note_style)
                                .on_hover_text("Note colour (restyles selected notes)")
                                .clicked()
                            {
                                self.note_style = i;
                                self.restyle_notes(i);
                            }
                        }
                    });
                });
            });
    }

    fn hud(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.status.as_ref().is_some_and(|(_, until)| *until < now) {
            self.status = None;
        }
        let mut parts = vec![format!("{:.0}%", self.zoom * 100.0)];
        if !self.items.is_empty() {
            parts.push(format!("{} items", self.items.len()));
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

    fn help_window(&mut self, ctx: &egui::Context) {
        const ROWS: &[(&str, &str)] = &[
            ("Pan", "Middle / right drag, or Space + drag"),
            ("Zoom", "Scroll wheel, Ctrl +/-, Ctrl+0 = 100%"),
            ("Fit all / selection", "F, or double-click empty space"),
            ("Select tool", "S or V"),
            ("Pen / Eraser", "B / E   ( [ and ] change size )"),
            ("Note", "T, then click; double-click a note to edit"),
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
            ("Grid / Toolbar", "G / Tab"),
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

        self.note_editor(ctx);
        if self.show_toolbar {
            self.toolbar(ctx);
        }
        self.pdf_dialog(ctx);
        self.hud(ctx);
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
    // Round caps (and joins, for thick lines) so strokes look like ink.
    let last = *pts.last().unwrap();
    if width > 2.0 {
        painter.circle_filled(first, width * 0.5, color);
        painter.circle_filled(last, width * 0.5, color);
    }
    if width > 6.0 {
        for p in &pts[1..pts.len() - 1] {
            painter.circle_filled(*p, width * 0.5, color);
        }
    }
    painter.add(Shape::line(pts, Stroke::new(width, color)));
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
