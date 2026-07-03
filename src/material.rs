/// Every parameter of the glass. Zero means OFF — exactly:
/// all-zero material + zero frost = pixel-identical passthrough of the
/// sharp desktop (modulo nothing: no hidden DWM blur exists in this engine).
#[derive(Clone, Copy, Debug)]
pub struct GlassMaterial {
    /// MATERIAL_REFRACTIVE_INDEX — how violently the backdrop warps, in px of
    /// normal-driven displacement. 0.0 = refraction off.
    pub refractive_index: f32,
    /// SURFACE_TENSION_FALLOFF — dimensionless restriction of the dome.
    /// The curved shoulder spans `min_half_extent / falloff` pixels, so
    /// LOWER values bleed the curve deeper into the center (1.0 = the dome
    /// reaches the exact center) and HIGHER values confine the liquid look
    /// to the outer border. Exactly 0 = flat glass, no curvature at all.
    pub surface_tension_falloff: f32,
    /// CHROMATIC_DISPERSION_AMOUNT — R<->B separation in px along the warp
    /// (Cauchy-weighted: eta_R < eta_G < eta_B). 0.0 = zero color separation.
    pub chromatic_dispersion: f32,
    /// FROST_BLUR_RADIUS — Gaussian sigma (px) applied to the backdrop BEFORE
    /// the physics. 0.0 = the blur pass is skipped entirely.
    pub frost_blur_radius: f32,

    pub corner_radius: f32,
    /// Peak height of the slab in px — scales the normals' tilt.
    pub height_scale: f32,
    /// Superellipse exponent of the dome profile: 2 = circular arc,
    /// higher = flatter top with a steeper rim.
    pub dome_exponent: f32,
    pub light_dir: (f32, f32),
    pub specular_exponent: f32,
    pub specular_intensity: f32,
    pub rim_exponent: f32,
    pub rim_intensity: f32,
    pub tint_color: (f32, f32, f32),
    pub tint_amount: f32,
}

impl Default for GlassMaterial {
    fn default() -> Self {
        Self {
            refractive_index: 30.0,
            surface_tension_falloff: 1.0,
            chromatic_dispersion: 10.0,
            frost_blur_radius: 0.0,
            corner_radius: 14.0,
            height_scale: 42.0,
            dome_exponent: 3.0,
            light_dir: (-0.55, -0.75),
            specular_exponent: 42.0,
            specular_intensity: 0.32,
            rim_exponent: 2.6,
            rim_intensity: 0.16,
            tint_color: (1.0, 1.0, 1.0),
            tint_amount: 0.0,
        }
    }
}

impl GlassMaterial {
    /// Per-channel eta in px. Cauchy eta(lambda) = A + B/lambda^2 evaluated at
    /// 650/510/475nm, normalized so `chromatic_dispersion` is the full R<->B
    /// spread: eta_R = A - 0.716*B, eta_G = A, eta_B = A + 0.284*B.
    pub fn etas(&self) -> (f32, f32, f32) {
        let a = self.refractive_index;
        let b = self.chromatic_dispersion;
        (a - 0.716 * b, a, a + 0.284 * b)
    }

    /// Env-var overrides for quick experiments, e.g. `LN_FROST=8 liquidnotes`.
    pub fn from_env() -> Self {
        let mut m = Self::default();
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f32>().ok());
        if let Some(v) = get("LN_REFRACT") {
            m.refractive_index = v;
        }
        if let Some(v) = get("LN_TENSION") {
            m.surface_tension_falloff = v;
        }
        if let Some(v) = get("LN_DISPERSION") {
            m.chromatic_dispersion = v;
        }
        if let Some(v) = get("LN_FROST") {
            m.frost_blur_radius = v;
        }
        if let Some(v) = get("LN_HEIGHT") {
            m.height_scale = v;
        }
        if let Some(v) = get("LN_DOME") {
            m.dome_exponent = v;
        }
        if let Some(v) = get("LN_SPEC") {
            m.specular_intensity = v;
        }
        if let Some(v) = get("LN_RIM") {
            m.rim_intensity = v;
        }
        if let Some(v) = get("LN_TINT") {
            m.tint_amount = v;
        }
        m
    }

    pub fn zero() -> Self {
        Self {
            refractive_index: 0.0,
            surface_tension_falloff: 0.0,
            chromatic_dispersion: 0.0,
            frost_blur_radius: 0.0,
            height_scale: 0.0,
            specular_intensity: 0.0,
            rim_intensity: 0.0,
            tint_amount: 0.0,
            ..Self::default()
        }
    }
}
