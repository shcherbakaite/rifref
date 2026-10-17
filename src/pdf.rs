//! PDF support: rasterizing pages (pure-Rust `hayro`) and the page-picker dialog.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, pos2, vec2, Color32, ColorImage, Pos2, Rect, Sense, Stroke, StrokeKind};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, PixmapSettings, RenderCache, RenderSettings};

use crate::loader::{self, Layout, LoadMsg};

const THUMB_W: f32 = 120.0;
const THUMB_H: f32 = 160.0;
const LABEL_H: f32 = 18.0;
/// Rendered pages never exceed this many pixels on their longest side.
const MAX_RENDER_SIDE: f32 = 10_000.0;
const ACCENT: Color32 = Color32::from_rgb(80, 160, 255);

pub const DPI_CHOICES: [(u32, &str); 4] = [
    (72, "Low (72 dpi)"),
    (150, "Medium (150 dpi)"),
    (220, "High (220 dpi)"),
    (300, "Print (300 dpi)"),
];

pub fn is_pdf_path(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

fn parse(data: &Arc<Vec<u8>>) -> Result<Pdf, String> {
    Pdf::new(data.clone()).map_err(|e| match e {
        LoadPdfError::Decryption(_) => "password-protected PDFs are not supported".into(),
        _ => "not a valid PDF".into(),
    })
}

type Rgba = (usize, usize, Vec<u8>);

/// Render `pages` on several threads. `scale_for(width_pt, height_pt)` picks the scale for each
/// page; `on_page(position, page_index, result)` is called as soon as a page is done.
fn render_parallel(
    data: &Arc<Vec<u8>>,
    pages: &[usize],
    cancel: &AtomicBool,
    scale_for: impl Fn(f32, f32) -> f32 + Sync,
    on_page: impl Fn(usize, usize, Result<Rgba, String>) + Sync,
) {
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(pages.len())
        .max(1);
    std::thread::scope(|s| {
        for t in 0..threads {
            let (scale_for, on_page) = (&scale_for, &on_page);
            s.spawn(move || {
                let mine = pages.iter().copied().enumerate().skip(t).step_by(threads);
                // Each worker parses its own copy: hayro documents are not shareable across threads.
                let pdf = match parse(data) {
                    Ok(p) => p,
                    Err(e) => {
                        for (pos, index) in mine {
                            on_page(pos, index, Err(e.clone()));
                        }
                        return;
                    }
                };
                let cache = RenderCache::new();
                let settings = InterpreterSettings::default();
                for (pos, index) in mine {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = match pdf.pages().get(index) {
                        None => Err("page out of range".to_string()),
                        Some(page) => {
                            let (w, h) = page.render_dimensions();
                            let scale = scale_for(w, h);
                            let pixmap_settings = PixmapSettings {
                                x_scale: scale,
                                y_scale: scale,
                                bg_color: WHITE,
                            };
                            std::panic::catch_unwind(AssertUnwindSafe(|| {
                                render(
                                    page,
                                    &cache,
                                    &settings,
                                    &RenderSettings::default(),
                                    &pixmap_settings,
                                )
                            }))
                            .map_err(|_| "the PDF renderer crashed on this page".to_string())
                            .map(|pix| {
                                // Opaque white background, so premultiplied == straight alpha.
                                (
                                    pix.width() as usize,
                                    pix.height() as usize,
                                    pix.data_as_u8_slice().to_vec(),
                                )
                            })
                        }
                    };
                    on_page(pos, index, result);
                }
            });
        }
    });
}

/// Read and parse a PDF in the background; replies with `LoadMsg::PdfOpened`.
pub fn open(path: PathBuf, at: Pos2, tx: Sender<LoadMsg>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let result = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                let data = Arc::new(bytes);
                let pages = parse(&data)?
                    .pages()
                    .iter()
                    .map(|p| {
                        let (w, h) = p.render_dimensions();
                        vec2(w, h)
                    })
                    .collect::<Vec<_>>();
                if pages.is_empty() {
                    return Err("the PDF has no pages".to_string());
                }
                Ok((data, pages))
            });
        let msg = match result {
            Ok((data, pages)) => LoadMsg::PdfOpened {
                path,
                data,
                pages,
                at,
            },
            Err(e) => LoadMsg::Error(format!("{}: {e}", path.display())),
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
}

/// Render small previews for the picker, streaming each one back as `LoadMsg::PdfThumb`.
pub fn thumbnails(
    doc: u64,
    data: Arc<Vec<u8>>,
    count: usize,
    cancel: Arc<AtomicBool>,
    tx: Sender<LoadMsg>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let pages: Vec<usize> = (0..count).collect();
        let tx = Mutex::new(tx);
        let ppp = ctx.pixels_per_point();
        render_parallel(
            &data,
            &pages,
            &cancel,
            |w, h| (THUMB_W / w).min(THUMB_H / h) * ppp,
            |_, page, result| {
                if let Ok((w, h, rgba)) = result {
                    let image = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let _ = tx.lock().unwrap().send(LoadMsg::PdfThumb { doc, page, image });
                    ctx.request_repaint();
                }
            },
        );
    });
}

/// Render the chosen pages at `dpi` and send them to the board as regular images.
pub fn import(
    name: String,
    data: Arc<Vec<u8>>,
    pages: Vec<usize>,
    dpi: u32,
    at: Pos2,
    max_side: usize,
    tx: Sender<LoadMsg>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let results: Mutex<Vec<Option<Result<loader::Decoded, String>>>> =
            Mutex::new((0..pages.len()).map(|_| None).collect());
        let base = dpi as f32 / 72.0;
        render_parallel(
            &data,
            &pages,
            &AtomicBool::new(false),
            |w, h| base.min(MAX_RENDER_SIDE / w.max(h)),
            |pos, _, result| {
                let decoded = result.and_then(|(w, h, rgba)| loader::from_rgba(w, h, rgba, max_side));
                results.lock().unwrap()[pos] = Some(decoded);
            },
        );
        let mut images = Vec::new();
        let mut errors = Vec::new();
        for (r, page) in results.into_inner().unwrap().into_iter().zip(&pages) {
            match r {
                Some(Ok(d)) => images.push(d),
                Some(Err(e)) => errors.push(format!("{name} page {}: {e}", page + 1)),
                None => errors.push(format!("{name} page {}: not rendered", page + 1)),
            }
        }
        let _ = tx.send(LoadMsg::Images {
            images,
            at,
            errors,
            layout: Layout::Grid,
        });
        ctx.request_repaint();
    });
}

/// Parse "1-3, 5, 8-" (1-based, open-ended ranges allowed) into a selection mask.
pub fn parse_ranges(text: &str, count: usize) -> Option<Vec<bool>> {
    let mut sel = vec![false; count];
    for part in text.split([',', ';', ' ']).map(str::trim).filter(|p| !p.is_empty()) {
        let num = |s: &str, default: usize| -> Option<usize> {
            let s = s.trim();
            if s.is_empty() {
                Some(default)
            } else {
                s.parse::<usize>().ok().filter(|&n| n >= 1)
            }
        };
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (num(a, 1)?, num(b, count)?),
            None => {
                let n = num(part, 0)?;
                (n, n)
            }
        };
        let (a, b) = (a.min(b), a.max(b));
        if a > count {
            return None;
        }
        for s in &mut sel[a - 1..b.min(count)] {
            *s = true;
        }
    }
    Some(sel)
}

/// Format a selection mask back into compact ranges, e.g. "1-3, 5".
fn format_ranges(sel: &[bool]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sel.len() {
        if sel[i] {
            let start = i;
            while i + 1 < sel.len() && sel[i + 1] {
                i += 1;
            }
            parts.push(if start == i {
                format!("{}", start + 1)
            } else {
                format!("{}-{}", start + 1, i + 1)
            });
        }
        i += 1;
    }
    parts.join(", ")
}

pub enum DialogResult {
    Import { pages: Vec<usize>, dpi: u32 },
    Cancel,
}

pub struct ImportDialog {
    pub doc: u64,
    pub name: String,
    pub data: Arc<Vec<u8>>,
    pub at: Pos2,
    page_sizes: Vec<egui::Vec2>,
    thumbs: Vec<Option<egui::TextureHandle>>,
    selected: Vec<bool>,
    range_text: String,
    range_error: bool,
    last_clicked: Option<usize>,
    cancel: Arc<AtomicBool>,
}

impl Drop for ImportDialog {
    fn drop(&mut self) {
        // Stop rendering thumbnails nobody will look at.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl ImportDialog {
    pub fn new(
        doc: u64,
        path: &std::path::Path,
        data: Arc<Vec<u8>>,
        page_sizes: Vec<egui::Vec2>,
        at: Pos2,
    ) -> (Self, Arc<AtomicBool>) {
        let n = page_sizes.len();
        // Preselect everything for short documents; let the user choose for long ones.
        let selected = vec![n <= 12; n];
        let cancel = Arc::new(AtomicBool::new(false));
        let dialog = Self {
            doc,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "PDF".into()),
            data,
            at,
            thumbs: vec![None; n],
            range_text: format_ranges(&selected),
            selected,
            page_sizes,
            range_error: false,
            last_clicked: None,
            cancel: cancel.clone(),
        };
        (dialog, cancel)
    }

    pub fn set_thumb(&mut self, ctx: &egui::Context, page: usize, image: ColorImage) {
        if let Some(slot) = self.thumbs.get_mut(page) {
            *slot = Some(ctx.load_texture(
                format!("pdf-{}-{page}", self.doc),
                image,
                egui::TextureOptions::LINEAR,
            ));
        }
    }

    fn sync_text(&mut self) {
        self.range_text = format_ranges(&self.selected);
        self.range_error = false;
    }

    pub fn show(&mut self, ctx: &egui::Context, dpi: &mut u32) -> Option<DialogResult> {
        let mut result = None;
        let mut open = true;
        let n = self.page_sizes.len();
        let count = self.selected.iter().filter(|s| **s).count();
        let pages: Vec<usize> = (0..n).filter(|&i| self.selected[i]).collect();

        egui::Window::new(format!("Import PDF pages — {}", self.name))
            .id(egui::Id::new(("pdf-import", self.doc)))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([760.0, 600.0])
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("{n} pages · {count} selected"));
                    ui.separator();
                    if ui.button("All").clicked() {
                        self.selected.fill(true);
                        self.sync_text();
                    }
                    if ui.button("None").clicked() {
                        self.selected.fill(false);
                        self.sync_text();
                    }
                    if ui.button("Invert").clicked() {
                        for s in &mut self.selected {
                            *s = !*s;
                        }
                        self.sync_text();
                    }
                    ui.separator();
                    ui.label("Pages:");
                    let mut edit = egui::TextEdit::singleline(&mut self.range_text)
                        .hint_text("e.g. 1-3, 7, 10-")
                        .desired_width(160.0);
                    if self.range_error {
                        edit = edit.text_color(Color32::from_rgb(255, 110, 110));
                    }
                    if ui.add(edit).changed() {
                        match parse_ranges(&self.range_text, n) {
                            Some(sel) => {
                                self.selected = sel;
                                self.range_error = false;
                            }
                            None => self.range_error = true,
                        }
                    }
                });
                ui.label(
                    egui::RichText::new("Click to toggle a page, Shift+click to select a range.")
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);

                let footer_h = 36.0;
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .max_height((ui.available_height() - footer_h).max(100.0))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
                            for i in 0..n {
                                let resp = self.page_cell(ui, i);
                                if resp.clicked() {
                                    let shift = ui.input(|inp| inp.modifiers.shift);
                                    match self.last_clicked.filter(|_| shift) {
                                        Some(last) => {
                                            let (a, b) = (last.min(i), last.max(i));
                                            self.selected[a..=b].fill(true);
                                        }
                                        None => self.selected[i] = !self.selected[i],
                                    }
                                    self.last_clicked = Some(i);
                                    self.sync_text();
                                }
                            }
                        });
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Resolution:");
                    let label = DPI_CHOICES
                        .iter()
                        .find(|(d, _)| d == dpi)
                        .map_or("Custom", |(_, l)| l);
                    egui::ComboBox::from_id_salt("pdf-dpi")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            for (d, l) in DPI_CHOICES {
                                ui.selectable_value(dpi, d, l);
                            }
                        });
                    if let Some(&first) = pages.first() {
                        let s = self.page_sizes[first] * (*dpi as f32 / 72.0);
                        let s = s * (MAX_RENDER_SIDE / s.max_elem()).min(1.0);
                        ui.label(
                            egui::RichText::new(format!("≈ {:.0}×{:.0} px per page", s.x, s.y))
                                .weak(),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let text = match count {
                            1 => "Import 1 page".to_string(),
                            c => format!("Import {c} pages"),
                        };
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui
                            .add_enabled(count > 0, egui::Button::new(text))
                            .clicked()
                            || (enter && count > 0 && !ctx.wants_keyboard_input())
                        {
                            result = Some(DialogResult::Import {
                                pages: pages.clone(),
                                dpi: *dpi,
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            result = Some(DialogResult::Cancel);
                        }
                    });
                });
            });
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(DialogResult::Cancel);
        }
        result
    }

    fn page_cell(&self, ui: &mut egui::Ui, i: usize) -> egui::Response {
        let (rect, resp) =
            ui.allocate_exact_size(vec2(THUMB_W + 8.0, THUMB_H + 8.0 + LABEL_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return resp;
        }
        let p = ui.painter();
        let selected = self.selected[i];
        let area = Rect::from_min_size(rect.min + vec2(4.0, 4.0), vec2(THUMB_W, THUMB_H));
        // Fit the page (with its real aspect ratio) inside the cell.
        let size = self.page_sizes[i];
        let fit = (THUMB_W / size.x).min(THUMB_H / size.y);
        let page_rect = Rect::from_center_size(area.center(), size * fit);

        if resp.hovered() || selected {
            p.rect_filled(rect, 6.0, ACCENT.gamma_multiply(if selected { 0.18 } else { 0.08 }));
        }
        match &self.thumbs[i] {
            Some(tex) => {
                p.image(
                    tex.id(),
                    page_rect,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                p.rect_filled(page_rect, 0.0, Color32::from_gray(235));
                p.text(
                    page_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "…",
                    egui::FontId::proportional(18.0),
                    Color32::from_gray(150),
                );
            }
        }
        let border = if selected {
            Stroke::new(3.0, ACCENT)
        } else {
            Stroke::new(1.0, Color32::from_gray(90))
        };
        p.rect_stroke(page_rect, 0.0, border, StrokeKind::Outside);
        if selected {
            let c = page_rect.right_top() + vec2(-11.0, 11.0);
            p.circle_filled(c, 9.0, ACCENT);
            p.text(
                c,
                egui::Align2::CENTER_CENTER,
                "✔",
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
        p.text(
            pos2(rect.center().x, rect.bottom() - LABEL_H * 0.5),
            egui::Align2::CENTER_CENTER,
            format!("{}", i + 1),
            egui::FontId::proportional(13.0),
            if selected {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().text_color()
            },
        );
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_round_trip() {
        let sel = parse_ranges("1-3, 5, 8-", 10).unwrap();
        assert_eq!(
            sel,
            [true, true, true, false, true, false, false, true, true, true]
        );
        assert_eq!(format_ranges(&sel), "1-3, 5, 8-10");
        assert_eq!(parse_ranges("", 3).unwrap(), vec![false; 3]);
        assert_eq!(parse_ranges("-2", 3).unwrap(), vec![true, true, false]);
        assert!(parse_ranges("4", 3).is_none());
        assert!(parse_ranges("abc", 3).is_none());
        assert_eq!(parse_ranges("2-9", 3).unwrap(), vec![false, true, true]);
    }

    /// Renders real pages end to end. Skipped when the sample PDF is not installed.
    #[test]
    fn import_renders_selected_pages() {
        let path = std::path::Path::new("/usr/share/doc/fig2dev/manual.pdf");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = egui::Context::default();
        import("manual.pdf".into(), Arc::new(bytes), vec![0, 2], 144, Pos2::ZERO, 4096, tx, ctx);
        let msg = rx.recv_timeout(std::time::Duration::from_secs(60)).unwrap();
        let LoadMsg::Images { images, errors, .. } = msg else {
            panic!("unexpected message");
        };
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(images.len(), 2);
        for d in &images {
            // Letter / A4 at 144 dpi is roughly 1190x1680 px.
            assert!(d.orig_size[0] > 1000 && d.orig_size[1] > 1400, "{:?}", d.orig_size);
            assert!(d.bytes.starts_with(b"\x89PNG"));
            // Not a blank page: some pixels must be dark (text).
            let dark = d.image.pixels.iter().filter(|p| p.r() < 100).count();
            assert!(dark > 1000, "page looks blank");
        }
    }
}
