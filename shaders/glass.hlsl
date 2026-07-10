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
    float4 light;  // fresnel rim intensity | screen-light azimuth rad (per-note) | (unused) | opacity
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
    // Anti-fold cap: at the steep rim the raw displacement can exceed the
    // distance to the edge, which folds the backdrop mapping back on itself and
    // shows an inverted (upside-down) sliver at the border. Soft-limit |disp|
    // (tanh) to a contour-uniform cap so the mapping stays monotonic — the
    // gentle center displacement is untouched, only the runaway rim is tamed.
    float dl = length(disp);
    if (dl > 1e-4) {
        float cap = min(0.45 * band, 0.75 * (rad + depth));
        disp *= (max(cap, 1.0) * tanh(dl / max(cap, 1.0))) / dl;
    }

    // Single-tap refraction (one eta for all channels -> white refracts white),
    // steered out of the pointer rect.
    float2 baseUV = (pane.zw + i.uv * size) * src.zw;
    float2 sampUV = avoidCursor(baseUV - disp * src.zw, cursor);
    float3 col = srcTex.Sample(samp, sampUV).rgb;

    // Frost the interior; ALSO strongly blur the BORDER (rim). blurTex is
    // already pointer-free (psblur avoids the cursor).
    float interiorFrost = smoother(0.0, max(bw, 1.0), depth);
    float blurMix = (frost.x > 0.05) ? max(interiorFrost, rimZone) : 0.0;
    if (blurMix > 0.0001) {
        float2 buv = (i.uv * size + frost.yy) * frost.zw - disp * frost.zw;
        float3 bcol = blurTex.Sample(samp, buv).rgb;
        // Super blur at the rim: a wide extra-tap average of the already-frosted
        // backdrop, weighted in by rimZone, so the border reads heavily blurred.
        if (rimZone > 0.01) {
            float2 px = frost.zw;
            float R = 10.0;
            float3 acc = bcol
                + blurTex.Sample(samp, buv + float2(R, 0.0) * px).rgb
                + blurTex.Sample(samp, buv - float2(R, 0.0) * px).rgb
                + blurTex.Sample(samp, buv + float2(0.0, R) * px).rgb
                + blurTex.Sample(samp, buv - float2(0.0, R) * px).rgb
                + blurTex.Sample(samp, buv + float2(R, R) * px).rgb
                + blurTex.Sample(samp, buv - float2(R, R) * px).rgb
                + blurTex.Sample(samp, buv + float2(R, -R) * px).rgb
                + blurTex.Sample(samp, buv - float2(R, -R) * px).rgb;
            bcol = lerp(bcol, acc / 9.0, rimZone);
        }
        col = lerp(col, bcol, blurMix);
    }

    // Adaptive card fill that MATCHES the backdrop darkness: a dark box over a
    // dark desktop, a light box over a light one. The dark<->light decision is
    // made on the CPU (a luminance threshold with a debounce) and delivered as
    // `mix` in fx.w, already eased over time — so a genuine timed fade plays
    // when the backdrop crosses the threshold, instead of a per-pixel flip.
    float mix = saturate(fx.w); // 0 = dark scheme, 1 = light scheme
    float3 fillCol = lerp(float3(0.10, 0.10, 0.12),   // dark box
                          float3(0.95, 0.95, 0.97),   // light box
                          mix);
    float active = fx.z;
    float op = saturate(light.w + 0.30 * active);
    if (op > 0.0001) {
        col = lerp(col, fillCol, op);
    }

    // Razor-thin white Fresnel rim from the view-angle reflectance
    // F = (1 - N.V)^p with V = +z, so N.V = N.z. It hugs the steep rim
    // uniformly all the way around the border (N.z is symmetric, so there is
    // NO directional or per-corner bias), and the high exponent compresses it
    // into the outermost sliver. Intensity = the `lighting` knob.
    if (light.x > 0.0001) {
        // Rim mask from the TRUE signed distance to the border (`-d`, positive
        // inward), confined to a thin fixed-width band — a bright rim LINE that
        // does not bleed into the glass. `d` is exactly 0 along the whole
        // border, rounded corners included, so the rim is uniform there (unlike
        // the dome's approximate `depth` field, which never reaches 0 on the
        // corner arcs and so dimmed them).
        float rimW = 3.0; // rim thickness in px
        float rimMask = 1.0 - smoother(0.0, rimW, -d);
        // Weighted toward a screen-space light: brightest on the edge FACING
        // the light, faint on the far edge. `light.y` is the per-note azimuth to
        // the light (from the note's screen position), so the bright arc slides
        // around the border as the note moves — nothing corner-baked.
        float2 Ldir = float2(cos(light.y), sin(light.y));
        float2 En = normalize(N.xy + float2(1e-5, 0.0)); // outward edge direction
        float dirW = 0.5 + 0.5 * dot(En, Ldir);          // 1 facing light, 0 opposite
        col = saturate(col + light.x * rimMask * (0.2 + 0.8 * dirW));
    }

    // Text ink CONTRASTS the box (which itself matches the backdrop): white
    // font on a dark box, near-black font on a light box. Same `mix` band as
    // the fill so the ink cross-fades in lockstep with the box colour.
    float4 txt = textTex.Sample(samp, i.uv);
    if (txt.a > 0.001) {
        float3 ink = lerp(float3(0.97, 0.97, 0.98),   // white on dark box
                          float3(0.08, 0.08, 0.10),   // near-black on light box
                          mix);
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

// Minimal soft drop shadow for a note's companion window (sized note + 2*margin).
// A note-shaped rounded rect inset by the margin (so full strength sits UNDER
// the note, hidden by it) with a gentle symmetric falloff outward over the
// margin — a faint halo around every border, no directional bias.
// shape = [corner radius | margin px | opacity | -]. Output premultiplied black.
float4 psshadow(VSO i) : SV_Target {
    float2 size = pane.xy;
    float2 p = (i.uv - 0.5) * size;
    float2 halfsz = 0.5 * size;
    float S = max(shape.y, 1.0);
    float2 hz = max(halfsz - S, float2(1.0, 1.0));
    float rr = min(shape.x, min(hz.x, hz.y));
    float d = sdrb(p, hz, rr);                     // 0 at the note edge, + outward
    // Gradient shadow: brightest right at the note border, fading out to
    // nothing by the outer edge. A soft Gaussian blur times a linear ramp that
    // reaches 0 at the margin edge (so it truly fades to zero instead of
    // clipping into a faint band at the window boundary).
    float sigma = S * 0.8;
    float a = exp(-d * d / (2.0 * sigma * sigma)) * (1.0 - saturate(d / S)) * shape.z;
    float n = frac(52.9829189 * frac(dot(i.pos.xy, float2(0.06711056, 0.00583715))));
    a = saturate(a + (n - 0.5) / 255.0);
    return float4(0.0, 0.0, 0.0, a);               // premultiplied translucent black
}
