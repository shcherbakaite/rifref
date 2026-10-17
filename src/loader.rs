//! Background image decoding so the UI never freezes on large files.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use eframe::egui::{self, ColorImage, Pos2};
use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::scene::SceneFile;

pub struct Decoded {
    pub bytes: Arc<Vec<u8>>,
    pub image: ColorImage,
    pub orig_size: [usize; 2],
}

/// How a batch of new images is placed around the drop point.
#[derive(Clone, Copy)]
pub enum Layout {
    Row,
    Grid,
}

pub enum LoadMsg {
    Images {
        images: Vec<Decoded>,
        at: Pos2,
        errors: Vec<String>,
        layout: Layout,
    },
    PdfOpened {
        path: PathBuf,
        data: Arc<Vec<u8>>,
        /// Page sizes in points.
        pages: Vec<egui::Vec2>,
        at: Pos2,
    },
    PdfThumb {
        doc: u64,
        page: usize,
        image: ColorImage,
    },
    Scene {
        path: PathBuf,
        scene: SceneFile,
        images: Vec<Option<Decoded>>,
    },
    Error(String),
}

/// Decode an encoded image at full resolution, honouring EXIF orientation.
pub fn decode_dynamic(bytes: &[u8]) -> Result<DynamicImage, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img)
}

pub fn decode(bytes: Vec<u8>, max_side: usize) -> Result<Decoded, String> {
    let mut img = decode_dynamic(&bytes)?;
    let orig_size = [img.width() as usize, img.height() as usize];
    let max_side = max_side.max(256) as u32;
    if img.width() > max_side || img.height() > max_side {
        img = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    let rgba = img.to_rgba8();
    let image =
        ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], &rgba);
    Ok(Decoded {
        bytes: Arc::new(bytes),
        image,
        orig_size,
    })
}

/// Wrap raw RGBA pixels: PNG-encode them (so they can be saved) and build the texture image.
pub fn from_rgba(width: usize, height: usize, rgba: Vec<u8>, max_side: usize) -> Result<Decoded, String> {
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| "bad image buffer".to_string())?;
    let mut png = Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let max_side = max_side.max(256) as u32;
    let img = if img.width() > max_side || img.height() > max_side {
        DynamicImage::ImageRgba8(img)
            .resize(max_side, max_side, image::imageops::FilterType::Triangle)
            .to_rgba8()
    } else {
        img
    };
    let image =
        ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], &img);
    Ok(Decoded {
        bytes: Arc::new(png.into_inner()),
        image,
        orig_size: [width, height],
    })
}

/// Decode several byte buffers in parallel, preserving order.
fn decode_all(inputs: Vec<Result<Vec<u8>, String>>, max_side: usize) -> Vec<Result<Decoded, String>> {
    std::thread::scope(|s| {
        let handles: Vec<_> = inputs
            .into_iter()
            .map(|input| s.spawn(move || input.and_then(|b| decode(b, max_side))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("decoder panicked".into())))
            .collect()
    })
}

fn finish_images(
    tx: &Sender<LoadMsg>,
    ctx: &egui::Context,
    labels: Vec<String>,
    inputs: Vec<Result<Vec<u8>, String>>,
    at: Pos2,
    max_side: usize,
) {
    let mut images = Vec::new();
    let mut errors = Vec::new();
    for (label, r) in labels.into_iter().zip(decode_all(inputs, max_side)) {
        match r {
            Ok(d) => images.push(d),
            Err(e) => errors.push(format!("{label}: {e}")),
        }
    }
    let _ = tx.send(LoadMsg::Images {
        images,
        at,
        errors,
        layout: Layout::Row,
    });
    ctx.request_repaint();
}

pub fn load_files(
    paths: Vec<PathBuf>,
    at: Pos2,
    max_side: usize,
    tx: Sender<LoadMsg>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let labels = paths.iter().map(|p| p.display().to_string()).collect();
        let inputs = paths
            .iter()
            .map(|p| std::fs::read(p).map_err(|e| e.to_string()))
            .collect();
        finish_images(&tx, &ctx, labels, inputs, at, max_side);
    });
}

/// A raw RGBA clipboard image.
pub fn load_rgba(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    at: Pos2,
    max_side: usize,
    tx: Sender<LoadMsg>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let (images, errors) = match from_rgba(width, height, rgba, max_side) {
            Ok(d) => (vec![d], vec![]),
            Err(e) => (vec![], vec![format!("clipboard: {e}")]),
        };
        let _ = tx.send(LoadMsg::Images {
            images,
            at,
            errors,
            layout: Layout::Row,
        });
        ctx.request_repaint();
    });
}

pub fn load_scene(path: PathBuf, max_side: usize, tx: Sender<LoadMsg>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let msg = match crate::scene::read(&path) {
            Ok((scene, blobs)) => {
                let images = decode_all(blobs.into_iter().map(Ok).collect(), max_side)
                    .into_iter()
                    .map(Result::ok)
                    .collect();
                LoadMsg::Scene {
                    path,
                    scene,
                    images,
                }
            }
            Err(e) => LoadMsg::Error(format!("Could not open {}: {e}", path.display())),
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
}
