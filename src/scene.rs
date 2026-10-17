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
    /// Grid spacing in board pixels.
    #[serde(default)]
    pub grid_size: Option<f32>,
    /// Camera: pan offset (relative to window center) and zoom.
    pub view: Option<([f32; 2], f32)>,
    #[serde(default)]
    pub images: Vec<String>,
    pub items: Vec<SavedItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<SavedLink>,
}

/// A connection between `items[from]` and `items[to]`.
#[derive(Serialize, Deserialize)]
pub struct SavedLink {
    pub from: usize,
    pub to: usize,
    pub color: [u8; 4],
    #[serde(default = "yes")]
    pub arrow: bool,
}

fn yes() -> bool {
    true
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<[u8; 4]>,
    pub kind: SavedKind,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedKind {
    Image {
        image: usize,
        /// Visible region as normalized [min_x, min_y, max_x, max_y]; absent when uncropped.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        crop: Option<[f32; 4]>,
    },
    Stroke {
        points: Vec<[f32; 2]>,
        color: [u8; 4],
        width: f32,
        size: [f32; 2],
    },
    /// Any item type this version doesn't know (e.g. text notes from older boards);
    /// skipped on load.
    #[serde(other)]
    Unsupported,
}

impl SceneFile {
    pub fn new(background: [u8; 4], grid: bool, grid_size: f32, view: ([f32; 2], f32)) -> Self {
        Self {
            version: VERSION,
            background,
            grid,
            grid_size: Some(grid_size),
            view: Some(view),
            images: Vec::new(),
            items: Vec::new(),
            links: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_links_round_trip_and_stay_optional() {
        let mut scene = SceneFile::new([0, 0, 0, 255], false, 32.0, ([0.0, 0.0], 1.0));
        for title in ["Suspect", ""] {
            scene.items.push(SavedItem {
                center: [0.0, 0.0],
                scale: 1.0,
                rotation: 0.0,
                flip_x: false,
                flip_y: false,
                title: title.into(),
                tag: (!title.is_empty()).then_some([200, 80, 80, 255]),
                kind: SavedKind::Unsupported,
            });
        }
        scene.links.push(SavedLink { from: 0, to: 1, color: [255; 4], arrow: false });
        let json = serde_json::to_string(&scene).unwrap();
        let back: SceneFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.items[0].title, "Suspect");
        assert_eq!(back.items[0].tag, Some([200, 80, 80, 255]));
        assert!(back.items[1].title.is_empty() && back.items[1].tag.is_none());
        assert_eq!((back.links[0].from, back.links[0].to, back.links[0].arrow), (0, 1, false));

        // Boards written before titles and links still load.
        let old = r#"{"version":1,"background":[0,0,0,255],"view":null,
            "items":[{"center":[0,0],"scale":1,"rotation":0,"kind":{"type":"note"}}]}"#;
        let old: SceneFile = serde_json::from_str(old).unwrap();
        assert!(old.links.is_empty() && old.items[0].title.is_empty());
    }
}
