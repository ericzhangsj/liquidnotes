// liquidnotes — liquid glass material.
// One unified physics pass (psglass) + separable Gaussian frost (psblur).
// All math fp32; output dithered; alpha premultiplied.

Texture2D srcTex : register(t0);
SamplerState samp : register(s0);

cbuffer Params : register(b0) {
    float4 pane;   // xy: viewport size px | zw: viewport origin inside srcTex, px
    float4 src;    // xy: srcTex size px  | zw: 1 / srcTex size
    float4 shape;  // x: corner radius | y: SURFACE_TENSION_FALLOFF (band px)
                   // z: height scale  | w: glyph (0 none, 1 plus)
    float4 refr;   // xyz: eta per channel, px of displacement (R < G < B)
    float4 light;  // xy: light dir | z: specular exponent | w: specular intensity
    float4 rim;    // x: rim exponent | y: rim intensity
    float4 tint;   // rgb: tint color | w: tint amount (0 = untinted)
    float4 blur;   // x: sigma | y: radius texels | zw: blur direction
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

// 2.5D height field z = f(x,y): flat slab with a meniscus that curls down
// across the edge band. Quintic smootherstep profile: C2-continuous at BOTH
// ends of the band — a circular-arc profile is only C1 and its curvature
// jump shades as a visible ring.
float hgt(float2 p, float2 halfsz, float r, float w) {
    float d = sdrb(p, halfsz, r);
    float t = saturate(-d / max(w, 1e-3));
    return t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
}

float4 psglass(VSO i) : SV_Target {
    float2 size = pane.xy;
    float2 p = (i.uv - 0.5) * size;
    float2 halfsz = 0.5 * size;
    float rad = min(shape.x, min(halfsz.x, halfsz.y));
    float band = shape.y;
    float hs = shape.z;

    float d = sdrb(p, halfsz, rad);

    // N = normalize([-df/dx, -df/dy, 1]), central differences, eps = 1px
    float e = 1.0;
    float hx = hgt(p + float2(e, 0), halfsz, rad, band)
             - hgt(p - float2(e, 0), halfsz, rad, band);
    float hy = hgt(p + float2(0, e), halfsz, rad, band)
             - hgt(p - float2(0, e), halfsz, rad, band);
    float3 N = normalize(float3(-hx * hs / (2.0 * e), -hy * hs / (2.0 * e), 1.0));

    // Snell approximation with Cauchy dispersion: three taps, one channel each.
    float2 baseUV = (pane.zw + i.uv * size) * src.zw;
    float3 col;
    col.r = srcTex.Sample(samp, baseUV + N.xy * refr.x * src.zw).r;
    col.g = srcTex.Sample(samp, baseUV + N.xy * refr.y * src.zw).g;
    col.b = srcTex.Sample(samp, baseUV + N.xy * refr.z * src.zw).b;

    col = lerp(col, tint.rgb, tint.w);

    // Blinn-Phong specular + view-dependent rim (meniscus lighting).
    float3 L = normalize(float3(light.xy, 0.65));
    float3 V = float3(0.0, 0.0, 1.0);
    float3 H = normalize(L + V);
    float spec = pow(saturate(dot(N, H)), light.z) * light.w;
    float rimf = pow(1.0 - saturate(dot(N, V)), rim.x) * rim.y;
    col = saturate(col + spec.xxx + rimf.xxx);

    // Plus glyph for the spawn button.
    if (shape.w > 0.5) {
        float th = 0.055 * min(size.x, size.y);
        float ln = 0.30 * min(halfsz.x, halfsz.y);
        float bar = min(sdrb(p, float2(ln, th), th),
                        sdrb(p, float2(th, ln), th));
        float ga = 1.0 - smoothstep(-0.75, 0.75, bar);
        col = lerp(col, float3(0.13, 0.13, 0.16), ga * 0.85);
    }

    // Analytic antialiased coverage cuts the rounded corners; premultiply.
    float a = 1.0 - smoothstep(-0.75, 0.75, d);

    // Interleaved gradient noise, +-0.5 LSB: smooth ramps cannot band.
    float n = frac(52.9829189 * frac(dot(i.pos.xy, float2(0.06711056, 0.00583715))));
    col = saturate(col + (n - 0.5) / 255.0);

    return float4(col * a, a);
}

// Separable Gaussian; run twice with blur.zw = (1,0) then (0,1).
// Skipped entirely by the CPU side when FROST_BLUR_RADIUS == 0.
float4 psblur(VSO i) : SV_Target {
    float2 basePx = pane.zw + i.uv * pane.xy;
    float sigma = max(blur.x, 0.01);
    int radius = (int)blur.y;
    float2 dir = blur.zw;
    float3 acc = 0.0;
    float wsum = 0.0;
    for (int k = -radius; k <= radius; ++k) {
        float wgt = exp(-0.5 * float(k * k) / (sigma * sigma));
        acc += srcTex.Sample(samp, (basePx + dir * float(k)) * src.zw).rgb * wgt;
        wsum += wgt;
    }
    return float4(acc / wsum, 1.0);
}
