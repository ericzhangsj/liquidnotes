/// Base glass: the knobs that matter. Zero means OFF — exactly:
/// all-zero material = pixel-identical passthrough of the sharp desktop.
#[derive(Clone, Copy, Debug)]
pub struct GlassMaterial {
    /// eta — how violently the backdrop warps, in px of normal-driven
    /// displacement. 0.0 = refraction off.
    pub refraction: f32,
    /// 0..1 single dome knob: 0 = shallow shoulder confined near the border,
    /// 1 = the dome reaches the center.
    pub depth: f32,
    /// Gaussian sigma (px) of the frost blur. 0.0 = blur pass skipped.
    pub frost: f32,
    pub corner_radius: f32,
    /// Rim zone width px.
    pub border_thickness: f32,
    /// Extra refraction at the rim (0 = none).
    pub border_refract: f32,
    /// Blinn-Phong rim glint intensity (0.0 = off).
    pub lighting: f32,
    /// Light azimuth in degrees (where the rim glint sits).
    pub light_angle: f32,
    /// Adaptive card-fill amount, 0..1 (0.0 = clear glass, 1.0 = solid card).
    /// The fill colour auto-opposes the desktop: dark grey over light, white
    /// over dark; the text ink then contrasts the fill.
    pub opacity: f32,
}

impl Default for GlassMaterial {
    fn default() -> Self {
        Self {
            refraction: 60.0,
            depth: 0.5,
            frost: 3.9,
            corner_radius: 32.0,
            border_thickness: 2.3,
            border_refract: 1.0,
            lighting: 0.0,
            light_angle: 135.0,
            opacity: 0.2,
        }
    }
}

impl GlassMaterial {
    /// Env-var overrides for quick experiments, e.g. `LN_FROST=12 liquidnotes`.
    pub fn from_env() -> Self {
        let mut m = Self::default();
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f32>().ok());
        if let Some(v) = get("LN_REFRACT") {
            m.refraction = v;
        }
        if let Some(v) = get("LN_DEPTH") {
            m.depth = v;
        }
        if let Some(v) = get("LN_FROST") {
            m.frost = v;
        }
        if let Some(v) = get("LN_CORNER") {
            m.corner_radius = v;
        }
        if let Some(v) = get("LN_BORDER") {
            m.border_thickness = v;
        }
        if let Some(v) = get("LN_BREFRACT") {
            m.border_refract = v;
        }   
        if let Some(v) = get("LN_LIGHT") {
            m.lighting = v;
        }
        if let Some(v) = get("LN_LANGLE") {
            m.light_angle = v;
        }
        if let Some(v) = get("LN_OPACITY") {
            m.opacity = v;
        }
        m
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
