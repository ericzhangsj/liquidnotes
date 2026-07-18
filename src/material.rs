use std::path::PathBuf;

use crate::store::{parse_number_fields, store_dir};

/// Base glass material. Zero disables the corresponding optical effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlassMaterial {
    /// How strongly the backdrop warps, in pixels of normal-driven displacement.
    pub refraction: f32,
    /// 0..1 dome control: 0 hugs the border, 1 reaches the note centre.
    pub depth: f32,
    /// Gaussian sigma in pixels. 0 skips both frost passes.
    pub frost: f32,
    pub corner_radius: f32,
    /// Width of the refractive rim zone, in pixels.
    pub border_thickness: f32,
    /// Additional displacement at the rim.
    pub border_refract: f32,
    /// Fresnel rim and highlight intensity.
    pub lighting: f32,
    /// Direction of the screen-space highlight, in degrees.
    pub light_angle: f32,
    /// Adaptive glass tint, from clear (0) to solid (1).
    pub opacity: f32,
}

impl Default for GlassMaterial {
    fn default() -> Self {
        Self {
            refraction: 60.0,
            depth: 0.5,
            frost: 1.0,
            corner_radius: 32.0,
            border_thickness: 2.7,
            border_refract: 0.7,
            lighting: 0.35,
            light_angle: 135.0,
            opacity: 0.25,
        }
    }
}

impl GlassMaterial {
    /// Load `%APPDATA%\liquidnotes\material.json`, creating a documented
    /// template on first use. Environment variables remain the final temporary
    /// override for diagnostics and development builds.
    pub fn load() -> Self {
        let path = config_path();
        let mut material = match std::fs::read_to_string(&path) {
            Ok(json) => Self::from_json(&json).unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let default = Self::default();
                let _ = std::fs::write(&path, config_json(default));
                default
            }
            Err(_) => Self::default(),
        };
        material.apply_env();
        material.sanitized()
    }

    /// Defaults plus environment overrides, retained for small isolated tools.
    pub fn from_env() -> Self {
        let mut material = Self::default();
        material.apply_env();
        material.sanitized()
    }

    fn apply_env(&mut self) {
        let get = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse::<f32>().ok())
        };
        if let Some(value) = get("LN_REFRACT") {
            self.refraction = value;
        }
        if let Some(value) = get("LN_DEPTH") {
            self.depth = value;
        }
        if let Some(value) = get("LN_FROST") {
            self.frost = value;
        }
        if let Some(value) = get("LN_CORNER") {
            self.corner_radius = value;
        }
        if let Some(value) = get("LN_BORDER") {
            self.border_thickness = value;
        }
        if let Some(value) = get("LN_BREFRACT") {
            self.border_refract = value;
        }
        if let Some(value) = get("LN_LIGHT") {
            self.lighting = value;
        }
        if let Some(value) = get("LN_LANGLE") {
            self.light_angle = value;
        }
        if let Some(value) = get("LN_OPACITY") {
            self.opacity = value;
        }
    }

    fn from_json(json: &str) -> Option<Self> {
        let mut material = Self::default();
        for (key, value) in parse_number_fields(json)? {
            let value = value as f32;
            match key.as_str() {
                "refraction" => material.refraction = value,
                "depth" => material.depth = value,
                "blur" | "frost" => material.frost = value,
                "corner_radius" => material.corner_radius = value,
                "rim_width" | "border_thickness" => material.border_thickness = value,
                "rim_refraction" | "border_refract" => material.border_refract = value,
                "lighting" => material.lighting = value,
                "light_angle" => material.light_angle = value,
                // Opacity is intentionally owned by the in-app slider.
                // Ignore legacy config files that still contain this key.
                "opacity" => {}
                _ => {}
            }
        }
        Some(material.sanitized())
    }

    fn sanitized(mut self) -> Self {
        let defaults = Self::default();
        self.refraction = bounded(self.refraction, defaults.refraction, 0.0, 160.0);
        self.depth = bounded(self.depth, defaults.depth, 0.0, 1.0);
        self.frost = bounded(self.frost, defaults.frost, 0.0, 12.0);
        self.corner_radius = bounded(self.corner_radius, defaults.corner_radius, 0.0, 128.0);
        self.border_thickness =
            bounded(self.border_thickness, defaults.border_thickness, 0.0, 20.0);
        self.border_refract = bounded(self.border_refract, defaults.border_refract, 0.0, 3.0);
        self.lighting = bounded(self.lighting, defaults.lighting, 0.0, 2.0);
        self.light_angle = bounded(self.light_angle, defaults.light_angle, 0.0, 360.0);
        self.opacity = bounded(self.opacity, defaults.opacity, 0.0, 1.0);
        self
    }

    pub fn zero() -> Self {
        Self {
            refraction: 0.0,
            depth: 0.0,
            frost: 0.0,
            border_refract: 0.0,
            lighting: 0.0,
            ..Self::default()
        }
    }
}

fn bounded(value: f32, fallback: f32, min: f32, max: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

/// The user-editable material file next to notes.json.
pub fn config_path() -> PathBuf {
    store_dir().join("material.json")
}

fn config_json(material: GlassMaterial) -> String {
    format!(
        r#"{{
  "_instructions": "Edit the numeric values, save, then restart LiquidNotes.",
  "_ranges": {{
    "refraction": "0..160 px; 0 disables backdrop bending",
    "depth": "0..1; how far the curved dome reaches into the note",
    "blur": "0..12 px Gaussian sigma; 0 disables both blur passes",
    "corner_radius": "0..128 px",
    "rim_width": "0..20 px",
    "rim_refraction": "0..3; extra bending at the edge",
    "lighting": "0..2; rim and highlight strength",
    "light_angle": "0..360 degrees"
  }},
  "refraction": {},
  "depth": {},
  "blur": {},
  "corner_radius": {},
  "rim_width": {},
  "rim_refraction": {},
  "lighting": {},
  "light_angle": {}
}}
"#,
        material.refraction,
        material.depth,
        material.frost,
        material.corner_radius,
        material.border_thickness,
        material.border_refract,
        material.lighting,
        material.light_angle,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_json_is_partial_alias_aware_and_bounded() {
        let material = GlassMaterial::from_json(
            r#"{
                "blur": 2.25,
                "refraction": 999,
                "rim_width": -5,
                "opacity": 0.0,
                "unknown_future_control": 12
            }"#,
        )
        .unwrap();
        assert_eq!(material.frost, 2.25);
        assert_eq!(material.refraction, 160.0);
        assert_eq!(material.border_thickness, 0.0);
        assert_eq!(material.opacity, GlassMaterial::default().opacity);
        assert_eq!(material.depth, GlassMaterial::default().depth);
    }

    #[test]
    fn generated_material_config_round_trips_defaults() {
        let defaults = GlassMaterial::default();
        let parsed = GlassMaterial::from_json(&config_json(defaults)).unwrap();
        assert_eq!(parsed, defaults);
    }

    #[test]
    fn invalid_material_json_fails_cleanly() {
        assert!(GlassMaterial::from_json("not json").is_none());
        assert!(GlassMaterial::from_json("[]").is_none());
    }
}
