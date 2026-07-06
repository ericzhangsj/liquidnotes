// liquidnotes — liquid glass material.
// One unified physics pass (psglass) + separable Gaussian frost (psblur).
// All math fp32; output dithered; alpha premultiplied.
//
// Refraction is driven by the DISTANCE to the rounded-rectangle edge (an SDF),
// so the dome — and therefore the warp — is uniform all the way around,
// corners included (no separable-product "miter" seam). Blur increases toward
// the center on its own independent falloff. The rim is a refractive lip plus a
// Fresnel specular. The baked-in mouse pointer is never sampled: every backdrop
// lookup is steered out of the pointer's rect.

Texture2D srcTex  : register(t0);   // sharp desktop (full output)
Texture2D blurTex : register(t1);   // pre-blurred backdrop region (center frost)
Texture2D textTex : register(t2);   // per-note text layer (premultiplied)
SamplerState samp : register(s0);

cbuffer Params : register(b0) {
    float4 pane;   // w, h | originX, originY (inside srcTex, px)
    float4 src;    // deskW, deskH | 1/deskW, 1/deskH
    float4 shape;  // corner radius | band (dome shoulder px) | height px | glyph
    float4 refr;   // eta px | dome exponent q | border refract | border thickness px
    float4 frost;  // sigma | margin offset px | 1/blurTexW, 1/blurTexH
    float4 cursor; // minU, minV, maxU, maxV  (>=2 means "no pointer")
    float4 blur;   // sigma | radius texels | dir x, y   (psblur only)
    float4 light;  // intensity | angle rad | elevation rad | opacity (psglass only)
    float4 fx;     // reveal | snap glow | active (fill opacity bump) | spare (psglass only)
};

struct VSO {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

VSO vsmain(uint vid : SV_VertexID) {
    VSO o;
    float2 t = float2((vid << 1) & 2, vid & 2);
    o.pos = float4(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0);
    o.uv = t;
    return o;
}

// Signed distance to a rounded rectangle (negative inside).
float sdrb(float2 p, float2 b, float r) {
    float2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

// Ken Perlin's C2 smootherstep (ease-in-ease-out, zero 1st & 2nd derivative at
// both ends) — smoother than smoothstep, no visible kink at the plateau.
float smoother(float a, float b, float x) {
    float t = saturate((x - a) / max(b - a, 1e-4));
    return t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
}

// 1D superellipse shoulder: 0 at the rim rising to 1, steep at the rim and
// easing C2-smoothly into the plateau (q: 2 = circular arc, 3+ = flatter top).
float prof(float t, float q) {
    float u = 1.0 - saturate(t);
    return pow(saturate(1.0 - pow(u, q)), 1.0 / q);
}

// Polynomial smooth-min (C1, crease-free): blends a and b over a k-wide zone.
float sminp(float a, float b, float k) {
    float h = saturate(0.5 + 0.5 * (b - a) / max(k, 1e-4));
    return lerp(b, a, h) - k * h * (1.0 - h);
}

// Depth in from the pane edge: on the flat edges it is exactly the distance to
// the nearer edge (identical to the rounded-box field there), and at corners
// the two edge distances are blended by a smooth-min whose smoothing width is
// the SILHOUETTE corner radius — so corner roundness is set by corner_radius
// alone, independent of the dome band.
float edgeDepth(float2 p, float2 halfsz, float rad) {
    float dx = halfsz.x - abs(p.x);
    float dy = halfsz.y - abs(p.y);
    return sminp(dx, dy, max(rad, 1e-3));
}

// Dome height z = f(x,y): a function of the depth in from the pane edge, so
// iso-height contours run parallel to the border and the refraction is
// identical at a given depth whether that depth is reached at a flat edge or
// a corner. `rad` is the silhouette corner radius (shape.x clamped).
float domeH(float2 p, float2 halfsz, float band, float q, float rad) {
    float depth = edgeDepth(p, halfsz, rad);
    return prof(depth / max(band, 1e-3), q);
}

// Steer a backdrop sample out of the pointer's rect so the baked-in cursor is
// never read (push to the nearest edge of the rect). No-op when there is no
// pointer (cursor >= 2, outside the [0,1] uv range).
float2 avoidCursor(float2 uv, float4 cr) {
    if (uv.x > cr.x && uv.x < cr.z && uv.y > cr.y && uv.y < cr.w) {
        float dl = uv.x - cr.x;
        float dr = cr.z - uv.x;
        float dt = uv.y - cr.y;
        float db = cr.w - uv.y;
        float m = min(min(dl, dr), min(dt, db));
        if (m == dl) uv.x = cr.x;
        else if (m == dr) uv.x = cr.z;
        else if (m == dt) uv.y = cr.y;
        else uv.y = cr.w;
    }
    return uv;
}

float4 psglass(VSO i) : SV_Target {
    float2 size = pane.xy;
    float2 p = (i.uv - 0.5) * size;
    float2 halfsz = 0.5 * size;
    float eta = refr.x;
    float dome = max(refr.y, 1.05);
    float bref = refr.z;
    float bw = refr.w;
    float band = shape.y;
    float hs = shape.z;
    float rad = min(shape.x, min(halfsz.x, halfsz.y));

    float d = sdrb(p, halfsz, rad);   // container SDF, for alpha + rim mask

    // Depth in from the edge: smooth-min edge field, corner smoothing = the
    // silhouette corner radius only (decoupled from the dome band).
    float depth = edgeDepth(p, halfsz, rad);

    // N = normalize([-df/dx, -df/dy, 1]), central differences, eps = 1px.
    float e = 1.0;
    float hx = domeH(p + float2(e, 0), halfsz, band, dome, rad)
             - domeH(p - float2(e, 0), halfsz, band, dome, rad);
    float hy = domeH(p + float2(0, e), halfsz, band, dome, rad)
             - domeH(p - float2(0, e), halfsz, band, dome, rad);
    float3 N = normalize(float3(-hx * hs / (2.0 * e), -hy * hs / (2.0 * e), 1.0));

    // Mechanism A (bevel shift): within the rim zone the shader squeezes the
    // backdrop harder, so elements passing behind the border compress — the
    // border reads as a thicker, distinct lens. Confined to `border_width` px.
    float rimZone = 1.0 - smoother(0.0, max(bw, 1.0), depth);
    float2 disp = N.xy * eta * (1.0 + bref * rimZone);

    // Single-tap refraction (one eta for all channels -> white refracts white),
    // steered out of the pointer rect.
    float2 baseUV = (pane.zw + i.uv * size) * src.zw;
    float2 sampUV = avoidCursor(baseUV - disp * src.zw, cursor);
    float3 col = srcTex.Sample(samp, sampUV).rgb;

    // Frost eased in over the rim zone (sharp rim, frosted interior).
    // blurTex is already pointer-free (psblur avoids the cursor).
    float blurMix = (frost.x > 0.05) ? smoother(0.0, max(bw, 1.0), depth) : 0.0;
    if (blurMix > 0.0001) {
        float2 buv = (i.uv * size + frost.yy) * frost.zw - disp * frost.zw;
        col = lerp(col, blurTex.Sample(samp, buv).rgb, blurMix);
    }

    // Adaptive card fill: average the backdrop luminance across the whole note
    // (coarse 4x4 grid, pointer-avoided) once, then tint the glass to OPPOSE the
    // desktop so the note always stands out — dark VS Code grey over a light
    // desktop, near-white over a dark one. Amount is the opacity knob (light.w),
    // bumped +20% while the note is proximity-active (fx.z) so a hovered note
    // simply reads a touch more solid; an idle note is pixel-identical to before.
    float bgLum = 0.0;
    [unroll] for (int gy = 0; gy < 4; ++gy) {
        [unroll] for (int gx = 0; gx < 4; ++gx) {
            float2 guv = (float2(gx, gy) + 0.5) * 0.25;
            float2 buv = avoidCursor((pane.zw + guv * size) * src.zw, cursor);
            bgLum += dot(srcTex.Sample(samp, buv).rgb, float3(0.2126, 0.7152, 0.0722));
        }
    }
    bgLum *= 0.0625; // / 16
    float3 fillCol = (bgLum > 0.5) ? float3(0.118, 0.118, 0.118)   // VS Code dark
                                   : float3(0.97, 0.97, 0.98);      // near white
    float active = fx.z;
    float op = saturate(light.w + 0.20 * active);
    if (op > 0.0001) {
        col = lerp(col, fillCol, op);
    }

    // Blinn-Phong rim glint: a white specular highlight concentrated on the
    // tilted rim walls (rim ~0 on the flat center, so the interior stays
    // clear). light.y spins where the glint sits around the border.
    if (light.x > 0.0001) {
        float ang = light.y, el = light.z;
        float3 L = normalize(float3(cos(ang) * cos(el), sin(ang) * cos(el), sin(el)));
        float3 H = normalize(L + float3(0.0, 0.0, 1.0));
        float spec = pow(saturate(dot(N, H)), 60.0);      // tight glossy glint
        float rim  = 1.0 - saturate(N.z);                 // ~0 flat center, ->1 steep rim
        float glint = light.x * spec * rim;               // angle-controllable white glint
        glint += light.x * 0.15 * pow(rim, 5.0);          // faint always-on wet-edge fresnel
        col = saturate(col + glint);
    }

    // Text ink chosen to contrast the note's OWN surface: base the decision on
    // the effective luminance behind the glyphs (backdrop blended toward the
    // fill colour by opacity), so ink stays legible whether the note is glassy
    // or an opaque card. The text texture holds white coverage (alpha) only.
    float4 txt = textTex.Sample(samp, i.uv);
    if (txt.a > 0.001) {
        float fillLum = dot(fillCol, float3(0.2126, 0.7152, 0.0722));
        float effLum = lerp(bgLum, fillLum, saturate(light.w));
        // Bias hard toward white ink: the surface must be *very* bright (top
        // ~25% of luminance) before the text flips to dark, so black ink only
        // appears over near-white backdrops.
        float3 ink = (effLum < 0.75) ? float3(0.97, 0.97, 0.98)
                                     : float3(0.10, 0.10, 0.13);
        col = lerp(col, ink, txt.a);
    }

    // Analytic antialiased coverage cuts the rounded corners; premultiply.
    float a = 1.0 - smoothstep(-0.75, 0.75, d);

    // Interleaved gradient noise, +-0.5 LSB: smooth ramps cannot band.
    float n = frac(52.9829189 * frac(dot(i.pos.xy, float2(0.06711056, 0.00583715))));
    col = saturate(col + (n - 0.5) / 255.0);

    float reveal = fx.x;
    // blue snap-glow rim: brightest right at the border, fading inward over ~10px
    if (fx.y > 0.001) {
        float rim = 1.0 - smoother(0.0, 10.0, -d);      // -d = depth inside; 1 at edge
        float3 glowc = float3(0.25, 0.55, 1.0);
        col = lerp(col, glowc, fx.y * rim * 0.85);
    }
    a *= reveal;

    return float4(col * a, a);
}

// Separable Gaussian; run twice with blur.zw = (1,0) then (0,1).
// Skipped entirely by the CPU side when the center frost is off.
float4 psblur(VSO i) : SV_Target {
    float2 basePx = pane.zw + i.uv * pane.xy;
    float sigma = max(blur.x, 0.01);
    int radius = (int)blur.y;
    float2 dir = blur.zw;
    float3 acc = 0.0;
    float wsum = 0.0;
    for (int k = -radius; k <= radius; ++k) {
        float wgt = exp(-0.5 * float(k * k) / (sigma * sigma));
        // Steer each tap out of the pointer rect, and average in LINEAR light
        // (gamma ~2 approx) so blurring high-contrast content does not darken.
        float2 uv = avoidCursor((basePx + dir * float(k)) * src.zw, cursor);
        float3 s = srcTex.Sample(samp, uv).rgb;
        acc += s * s * wgt;
        wsum += wgt;
    }
    return float4(sqrt(acc / wsum), 1.0);
}
