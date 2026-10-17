//! Scene file format (`.rifref`): a JSON document with images embedded as base64.

use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const EXTENSION: &str = "rifref";
/// Extensions also accepted when opening (boards saved before the rename).
pub const OPEN_EXTENSIONS: [&str; 2] = [EXTENSION, "refb"];
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct SceneFile {
    pub version: u32,
    pub background: [u8; 4],
    #[serde(default)]
    pub grid: bool,
    /// Camera: pan offset (relative to window center) and zoom.
    pub view: Option<([f32; 2], f32)>,
    #[serde(default)]
    pub images: Vec<String>,
    pub items: Vec<SavedItem>,
}

#[derive(Serialize, Deserialize)]
pub struct SavedItem {
    pub center: [f32; 2],
    pub scale: f32,
    pub rotation: f32,
    #[serde(default)]
    pub flip_x: bool,
    #[serde(default)]
    pub flip_y: bool,
    pub kind: SavedKind,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedKind {
    Image {
        image: usize,
    },
    Note {
        text: String,
        width: f32,
        bg: [u8; 4],
        fg: [u8; 4],
        font_size: f32,
    },
    Stroke {
        points: Vec<[f32; 2]>,
        color: [u8; 4],
        width: f32,
        size: [f32; 2],
    },
}

impl SceneFile {
    pub fn new(background: [u8; 4], grid: bool, view: ([f32; 2], f32)) -> Self {
        Self {
            version: VERSION,
            background,
            grid,
            view: Some(view),
            images: Vec::new(),
            items: Vec::new(),
        }
    }

    pub fn add_image(&mut self, bytes: &[u8]) -> usize {
        self.images
            .push(base64::engine::general_purpose::STANDARD.encode(bytes));
        self.images.len() - 1
    }
}

pub fn write(path: &Path, scene: &SceneFile) -> Result<(), String> {
    let json = serde_json::to_vec(scene).map_err(|e| e.to_string())?;
    // Write to a temp file first so a crash never leaves a half-written scene.
    let tmp = path.with_extension(format!("{EXTENSION}.tmp"));
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Returns the scene plus the decoded (binary) image blobs, with `scene.images` emptied.
pub fn read(path: &Path) -> Result<(SceneFile, Vec<Vec<u8>>), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut scene: SceneFile = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    if scene.version > VERSION {
        return Err(format!("unsupported scene version {}", scene.version));
    }
    let blobs = std::mem::take(&mut scene.images)
        .iter()
        .map(|s| {
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((scene, blobs))
}
