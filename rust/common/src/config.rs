use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct Config {
    pub roots: Vec<String>,
    pub exclude: Vec<String>,
    pub extensions: Vec<String>,
    #[serde(rename = "MaxCacheMB")]
    pub max_cache_mb: u64,
    pub background_workers: usize,
    #[serde(rename = "MaxFileMB")]
    pub max_file_mb: u64,
    pub import_timeout_seconds: u64,
    pub renderer: String,
    pub transparent: bool,
    pub background: String,
    pub source_colours: bool,
    pub ambient_occlusion: bool,
    #[serde(rename = "AOStrength")]
    pub ao_strength: f32,
    #[serde(rename = "AORadius")]
    pub ao_radius: f32,
    pub edges: bool,
    pub edge_strength: f32,
    pub edge_width: f32,
    pub antialias: bool,
    pub ambient: f32,
    pub light_intensity: f32,
    pub light_direction: String,
    pub light_colour: String,
    pub lighting_preset: String,
    pub start_with_windows: bool,
}
impl Default for Config {
    fn default() -> Self {
        let roots=(b'C'..=b'D').map(|c|format!("{}:\\",c as char)).filter(|p| unsafe{windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(crate::wide(p).as_ptr())}==3).collect();
        let mut exclude = Vec::new();
        for env in ["WINDIR", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(p) = std::env::var(env) {
                exclude.push(p)
            }
        }
        exclude.push(crate::data_dir().to_string_lossy().into());
        Self {
            roots,
            exclude,
            extensions: vec![".step".into(), ".stp".into()],
            max_cache_mb: 2048,
            background_workers: 1,
            max_file_mb: 0,
            import_timeout_seconds: 30,
            renderer: "look".into(),
            transparent: true,
            background: "#ffffff".into(),
            source_colours: true,
            ambient_occlusion: true,
            ao_strength: 1.0,
            ao_radius: 14.0,
            edges: true,
            edge_strength: 0.75,
            edge_width: 1.0,
            antialias: true,
            ambient: 0.18,
            light_intensity: 1.2,
            light_direction: "-1,-2,-3".into(),
            light_colour: "#ffffff".into(),
            lighting_preset: "technical".into(),
            start_with_windows: true,
        }
    }
}
impl Config {
    pub fn load() -> Result<Self> {
        let path = crate::data_dir().join("settings.json");
        if path.exists() {
            let bytes = fs::read(path)?;
            let value: Self =
                serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))?;
            value.validate()?;
            Ok(value)
        } else {
            let value = Self::default();
            value.save()?;
            Ok(value)
        }
    }
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        fs::create_dir_all(crate::data_dir())?;
        crate::cache::atomic_write(
            &crate::data_dir().join("settings.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn validate(&self) -> Result<()> {
        if self.roots.is_empty() {
            bail!("Add at least one monitored folder or drive.")
        }
        if !matches!(self.renderer.as_str(), "look" | "f3d") {
            bail!("Choose Look or F3D.")
        }
        if !matches!(self.lighting_preset.as_str(), "technical" | "f3d-match") {
            bail!("Choose technical or f3d-match lighting.");
        }
        if self.max_cache_mb > 1_048_576 {
            bail!("Cache limit is too large.");
        }
        if !(1..=32).contains(&self.background_workers) {
            bail!("Background workers must be between 1 and 32.");
        }
        if self.max_file_mb > 1_048_576 {
            bail!("Maximum model size is too large.");
        }
        if !(1..=300).contains(&self.import_timeout_seconds) {
            bail!("Import timeout must be between 1 and 300 seconds.");
        }
        if self
            .extensions
            .iter()
            .any(|v| !crate::hooks::EXTENSIONS.contains(&v.as_str()))
        {
            bail!("Unsupported file extension.")
        }
        for colour in [&self.background, &self.light_colour] {
            if colour.len() != 7
                || !colour.starts_with('#')
                || u32::from_str_radix(&colour[1..], 16).is_err()
            {
                bail!("Colours must use #RRGGBB format.")
            }
        }
        let direction = self.light_vector()?;
        if direction == [0.0; 3] {
            bail!("Light direction must be non-zero.")
        }
        if !self.ao_strength.is_finite()
            || !(0.0..=3.0).contains(&self.ao_strength)
            || !(1.0..=100.0).contains(&self.ao_radius)
            || !(0.0..=1.0).contains(&self.edge_strength)
            || !(1.0..=3.0).contains(&self.edge_width)
            || !(0.0..=2.0).contains(&self.ambient)
            || !(0.0..=10.0).contains(&self.light_intensity)
        {
            bail!("Check lighting, edge and AO values.")
        }
        Ok(())
    }
    pub fn light_vector(&self) -> Result<[f32; 3]> {
        let values = self
            .light_direction
            .split(',')
            .map(|v| v.trim().parse::<f32>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if values.len() != 3 || values.iter().any(|v| !v.is_finite()) {
            bail!("Light direction must contain three numbers, for example -1,-2,-3.")
        }
        Ok([values[0], values[1], values[2]])
    }
    pub fn supports(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| {
            self.extensions
                .iter()
                .any(|v| v[1..].eq_ignore_ascii_case(&e.to_string_lossy()))
        })
    }
    pub fn worker_count(&self) -> usize {
        self.background_workers
            .min(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1),
            )
            .max(1)
    }
    pub fn included(&self, path: &Path) -> bool {
        self.roots.iter().any(|p| crate::below(path, Path::new(p)))
            && !self
                .exclude
                .iter()
                .any(|p| crate::below(path, Path::new(p)))
            && !crate::below(path, &crate::data_dir())
    }
    pub fn profile(&self) -> String {
        let value = serde_json::json!([
            "rust-look-effects-v1",
            self.renderer,
            self.transparent,
            self.background,
            self.source_colours,
            self.ambient_occlusion,
            self.ao_strength,
            self.ao_radius,
            self.edges,
            self.edge_strength,
            self.edge_width,
            self.antialias,
            self.ambient,
            self.light_intensity,
            self.light_direction,
            self.light_colour,
            self.lighting_preset
        ]);
        blake3::hash(value.to_string().as_bytes())
            .to_hex()
            .to_string()
    }
    pub fn roots_available(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect()
    }
}
