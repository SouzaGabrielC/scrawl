// Scrawl HUD Pill WGSL Shader
// Ported from src/native/pill/pill.metal for Linux (wgpu) & multi-platform support.

// ─── Structs & Bindings ─────────────────────────────────────────────────────────────────────

struct TileUniforms {
    atlas_size: vec2<f32>,   // in pixels
    canvas_size: vec2<f32>,  // in points (960, 256)
    scale: f32,              // Backing scale (1.0 or 2.0)
    _pad0: f32,
    _pad1: vec2<f32>,
};

// kind == 0: analytic rounded rect / circle (`rect` in canvas pt, `uv_rect.x` = corner radius)
// kind == 1: textured quad from `atlas` (`rect` in canvas pt, `uv_rect` = [u0, v0, u1, v1] in 0..1)
struct TilePrim {
    rect: vec4<f32>,      // x, y, w, h in canvas points
    uv_rect: vec4<f32>,   // u0, v0, u1, v1 (for kind == 1) or (radius, 0, 0, 0) (for kind == 0)
    color: vec4<f32>,     // premultiplied or straight RGBA (r, g, b, a)
    kind: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

struct BlurBox {
    tile_rect: vec4<f32>, // x, y, w, h in canvas points (960 × 256)
    sigma_pt: f32,        // blur sigma in points
    _pad0: f32,
    _pad1: vec2<f32>,
};

struct CompositeElem {
    screen_rect: vec4<f32>, // x, y, w, h in panel points (scaled)
    tile_rect: vec4<f32>,   // tile_x, tile_y, tile_w, tile_h in offscreen_b points
    base_size: vec2<f32>,   // unscaled (w, h) in points
    scale: f32,             // element scale (0.82 .. 1.0)
    alpha: f32,             // element opacity (0 .. 1)
    sigma_pt: f32,          // vertical blur sigma in points
    pad_pt: f32,            // tile padding in points (16.0)
    _pad0: vec2<f32>,
};

struct PillUniforms {
    panel_size: vec2<f32>,     // (640, 84) in points
    canvas_size: vec2<f32>,    // (960, 256) in points
    scale: f32,                // Backing scale (1.0 or 2.0)
    elem_count: u32,
    is_completed: u32,
    has_ghost: u32,
    shape_rect: vec4<f32>,     // l, t, r, b in panel points
    shape_meta: vec4<f32>,     // radius, alpha, blur, is_glass (1.0 = glass, 0.0 = black)
    ghost_rect: vec4<f32>,     // l, t, r, b in panel points
    ghost_meta: vec4<f32>,     // radius, alpha, 0, 0
    hint_rect: vec4<f32>,      // x, y, w, h of optional "Esc" badge below capsule (w == 0 when off)
    hint_uv: vec4<f32>,        // u0, v0, u1, v1 of "Esc" label in atlas
};

// ─── Resource Bindings (Uniforms, Buffers, Textures, Samplers) ──────────────────────────────

@group(0) @binding(0) var<uniform> tile_uniforms: TileUniforms;
@group(0) @binding(1) var<storage, read> tile_prims: array<TilePrim>;
@group(0) @binding(2) var<storage, read> blur_boxes: array<BlurBox>;
@group(0) @binding(3) var<uniform> pill_uniforms: PillUniforms;
@group(0) @binding(4) var<storage, read> comp_elems: array<CompositeElem>;

@group(0) @binding(5) var atlas_texture: texture_2d<f32>;
@group(0) @binding(6) var offscreen_a_texture: texture_2d<f32>;
@group(0) @binding(7) var offscreen_b_texture: texture_2d<f32>;
@group(0) @binding(8) var linear_sampler: sampler;

// ─── Pass 1: Render Elements into Offscreen Tile Canvas (960 × 256 pt) ──────────────────────

struct TileVertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) pt: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) rect: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) radius: f32,
    @location(5) @interpolate(flat) kind: u32,
};

@vertex
fn pill_tile_vertex(
    @builtin(vertex_index) vid: u32,
    @builtin(instance_index) iid: u32,
) -> TileVertexOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0)
    );
    let prim = tile_prims[iid];
    let c = corners[vid];
    let pad = select(0.0, 1.5, prim.kind == 0u);
    let p = prim.rect.xy - vec2<f32>(pad) + c * (prim.rect.zw + vec2<f32>(2.0 * pad));
    let ndc = vec2<f32>(
        (p.x / tile_uniforms.canvas_size.x) * 2.0 - 1.0,
        1.0 - (p.y / tile_uniforms.canvas_size.y) * 2.0
    );

    var out: TileVertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.pt = p;
    out.uv = mix(prim.uv_rect.xy, prim.uv_rect.zw, c);
    out.rect = prim.rect;
    out.color = prim.color;
    out.radius = prim.uv_rect.x;
    out.kind = prim.kind;
    return out;
}

@fragment
fn pill_tile_fragment(in: TileVertexOut) -> @location(0) vec4<f32> {
    if (in.kind == 0u) {
        let center = in.rect.xy + in.rect.zw * 0.5;
        let half_sz = in.rect.zw * 0.5;
        let r = min(in.radius, min(half_sz.x, half_sz.y));
        let q = abs(in.pt - center) - (half_sz - vec2<f32>(r));
        let sdf = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        let cov = clamp(0.5 - sdf * tile_uniforms.scale, 0.0, 1.0);
        let a = in.color.a * cov;
        return vec4<f32>(in.color.rgb * a, a);
    } else {
        let tex = textureSample(atlas_texture, linear_sampler, in.uv);
        let cov = tex.a;
        let a = in.color.a * cov;
        return vec4<f32>(in.color.rgb * a, a);
    }
}

// ─── Pass 2: 1D Horizontal Gaussian Blur (offscreen_a -> offscreen_b) ──────────────────────

struct BlurVertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) pt: vec2<f32>,
    @location(1) tile_rect: vec4<f32>,
    @location(2) sigma_pt: f32,
};

@vertex
fn pill_blur_h_vertex(
    @builtin(vertex_index) vid: u32,
    @builtin(instance_index) iid: u32,
) -> BlurVertexOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0)
    );
    let b = blur_boxes[iid];
    let c = corners[vid];
    let p = b.tile_rect.xy + c * b.tile_rect.zw;
    let ndc = vec2<f32>(
        (p.x / tile_uniforms.canvas_size.x) * 2.0 - 1.0,
        1.0 - (p.y / tile_uniforms.canvas_size.y) * 2.0
    );

    var out: BlurVertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.pt = p;
    out.tile_rect = b.tile_rect;
    out.sigma_pt = b.sigma_pt;
    return out;
}

@fragment
fn pill_blur_h_fragment(in: BlurVertexOut) -> @location(0) vec4<f32> {
    let inv_canvas = 1.0 / tile_uniforms.canvas_size;
    let sigma = in.sigma_pt;
    if (sigma < 0.05) {
        return textureSample(offscreen_a_texture, linear_sampler, in.pt * inv_canvas);
    }
    let min_x = in.tile_rect.x + 0.5;
    let max_x = in.tile_rect.x + in.tile_rect.z - 0.5;
    let radius = min(24i, i32(ceil(sigma * 3.0 * tile_uniforms.scale)));
    let step_pt = 1.0 / tile_uniforms.scale;
    let inv_2s2 = 0.5 / max(sigma * sigma, 1e-4);

    var acc = vec4<f32>(0.0);
    var w_sum = 0.0;
    for (var k = -radius; k <= radius; k = k + 1) {
        let dx = f32(k) * step_pt;
        let w = exp(-(dx * dx) * inv_2s2);
        let sx = clamp(in.pt.x + dx, min_x, max_x);
        acc = acc + textureSample(offscreen_a_texture, linear_sampler, vec2<f32>(sx, in.pt.y) * inv_canvas) * w;
        w_sum = w_sum + w;
    }
    return acc / max(w_sum, 1e-5);
}

// ─── Pass 3: Analytic SDF Capsule + 1D Vertical Blur Composite + Hairline ──────────────────

struct ScreenVertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn pill_screen_vertex(@builtin(vertex_index) vid: u32) -> ScreenVertexOut {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0)
    );
    let p = pos[vid];
    var out: ScreenVertexOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = p * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5);
    return out;
}

fn rrect_sdf(p: vec2<f32>, rect: vec4<f32>, r: f32) -> f32 {
    let center = (rect.xy + rect.zw) * 0.5;
    let half_sz = max((rect.zw - rect.xy) * 0.5, vec2<f32>(0.0));
    let cr = min(r, min(half_sz.x, half_sz.y));
    let q = abs(p - center) - (half_sz - vec2<f32>(cr));
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - cr;
}

fn blend_over(dst: vec4<f32>, src: vec4<f32>) -> vec4<f32> {
    return src + dst * (1.0 - src.a);
}

@fragment
fn pill_composite_fragment(in: ScreenVertexOut) -> @location(0) vec4<f32> {
    let p = in.uv * pill_uniforms.panel_size;
    var out = vec4<f32>(0.0);

    let shape_alpha = pill_uniforms.shape_meta.y;
    if (shape_alpha <= 0.001 && pill_uniforms.has_ghost == 0u) {
        return out;
    }

    let r = pill_uniforms.shape_meta.x;
    let is_glass = pill_uniforms.shape_meta.w;
    let sdf = rrect_sdf(p, pill_uniforms.shape_rect, r);

    // 1. Soft drop shadow behind the capsule in black mode.
    if (is_glass < 0.5) {
        let sdf_shadow = rrect_sdf(p - vec2<f32>(0.0, 4.0), pill_uniforms.shape_rect, r);
        if (sdf_shadow < 22.0) {
            let sh_d = max(sdf_shadow, 0.0);
            let sh_u = sh_d / 7.0;
            let sh_a = 0.16 * shape_alpha * exp(-0.5 * sh_u * sh_u);
            out = blend_over(out, vec4<f32>(0.0, 0.0, 0.0, sh_a));
        }
    }

    // 2. Optional Reduce Motion ghost silhouette.
    let bg_rgb = vec3<f32>(20.0 / 255.0, 20.0 / 255.0, 22.0 / 255.0);
    if (is_glass < 0.5 && pill_uniforms.has_ghost != 0u && pill_uniforms.ghost_meta.y > 0.001) {
        let g_sdf = rrect_sdf(p, pill_uniforms.ghost_rect, pill_uniforms.ghost_meta.x);
        let g_cov = clamp(0.5 - g_sdf * pill_uniforms.scale, 0.0, 1.0);
        let g_a = 0.96 * pill_uniforms.ghost_meta.y * g_cov;
        out = blend_over(out, vec4<f32>(bg_rgb * g_a, g_a));
    }

    // 3. Main capsule silhouette fill.
    // Note WGSL smoothstep requirement (low < high): downward smoothstep(high, low, x) becomes (1.0 - smoothstep(low, high, x)).
    let half_band = max(0.5 / pill_uniforms.scale, pill_uniforms.shape_meta.z * 1.75);
    let in_cov = 1.0 - smoothstep(-half_band, half_band, sdf);
    if (in_cov > 0.001) {
        if (is_glass < 0.5) {
            let fill_a = 0.96 * shape_alpha * in_cov;
            out = blend_over(out, vec4<f32>(bg_rgb * fill_a, fill_a));
        }

        // 4. Composite per-element quads with 1D Vertical Gaussian blur from offscreen_b.
        let inv_canvas = 1.0 / pill_uniforms.canvas_size;
        let count = min(pill_uniforms.elem_count, 16u);
        for (var i = 0u; i < count; i = i + 1u) {
            let e = comp_elems[i];
            let exp_pad = e.pad_pt * e.scale;
            let min_p = e.screen_rect.xy - vec2<f32>(exp_pad);
            let max_p = e.screen_rect.xy + e.screen_rect.zw + vec2<f32>(exp_pad);
            if (p.x < min_p.x || p.x > max_p.x || p.y < min_p.y || p.y > max_p.y) {
                continue;
            }
            let local_pt = (p - e.screen_rect.xy) / max(e.scale, 0.05) + vec2<f32>(e.pad_pt);
            let tile_pt = e.tile_rect.xy + local_pt;
            let min_ty = e.tile_rect.y + 0.5;
            let max_ty = e.tile_rect.y + e.tile_rect.w - 0.5;
            if (tile_pt.x < e.tile_rect.x || tile_pt.x > e.tile_rect.x + e.tile_rect.z) {
                continue;
            }

            var sample_col = vec4<f32>(0.0);
            let sigma = e.sigma_pt;
            if (sigma < 0.05) {
                if (tile_pt.y >= min_ty && tile_pt.y <= max_ty) {
                    sample_col = textureSample(offscreen_b_texture, linear_sampler, tile_pt * inv_canvas);
                }
            } else {
                let radius = min(24i, i32(ceil(sigma * 3.0 * pill_uniforms.scale)));
                let step_pt = 1.0 / pill_uniforms.scale;
                let inv_2s2 = 0.5 / max(sigma * sigma, 1e-4);
                var w_sum = 0.0;
                for (var k = -radius; k <= radius; k = k + 1) {
                    let dy = f32(k) * step_pt;
                    let w = exp(-(dy * dy) * inv_2s2);
                    let sy = clamp(tile_pt.y + dy, min_ty, max_ty);
                    sample_col = sample_col + textureSample(offscreen_b_texture, linear_sampler, vec2<f32>(tile_pt.x, sy) * inv_canvas) * w;
                    w_sum = w_sum + w;
                }
                sample_col = sample_col / max(w_sum, 1e-5);
                // Compensate for 2D Gaussian peak-alpha dilution on thin (1.5 pt) glyph/icon strokes
                if (sample_col.a > 1e-4) {
                    let blur_boost = 1.0 + min(sigma, 8.0) * 0.35;
                    let boosted_a = min(1.0, sample_col.a * blur_boost);
                    sample_col = sample_col * (boosted_a / sample_col.a);
                }
            }

            // In Liquid Glass mode, add backing shadow
            if (is_glass >= 0.5 && sigma < 3.5) {
                let sh_y = clamp(tile_pt.y - 0.85, min_ty, max_ty);
                let sh_a = textureSample(offscreen_b_texture, linear_sampler, vec2<f32>(tile_pt.x, sh_y) * inv_canvas).a;
                let backing_a = min(1.0, sh_a * 0.64) * e.alpha * in_cov;
                out = blend_over(out, vec4<f32>(0.0, 0.0, 0.0, backing_a));
            }
            let eff_a = e.alpha * in_cov;
            out = blend_over(out, sample_col * eff_a);
        }
    }

    // 5. Analytic 1px specular hairline border on capsule rim.
    if (is_glass < 0.5 || pill_uniforms.is_completed != 0u) {
        let border_sharp = clamp(1.0 - pill_uniforms.shape_meta.z * 0.22, 0.0, 1.0);
        let border_cov = clamp(1.0 + 0.5 / pill_uniforms.scale + sdf, 0.0, 1.0)
                       * clamp(0.5 / pill_uniforms.scale - sdf, 0.0, 1.0)
                       * border_sharp;
        if (border_cov > 0.001) {
            let border_rgb = select(
                vec3<f32>(1.0, 1.0, 1.0),
                vec3<f32>(105.0 / 255.0, 216.0 / 255.0, 159.0 / 255.0),
                pill_uniforms.is_completed != 0u
            );
            let border_max_a = select(0.12, 0.42, pill_uniforms.is_completed != 0u);
            let ba = border_cov * border_max_a * shape_alpha;
            out = blend_over(out, vec4<f32>(border_rgb * ba, ba));
        }
    }

    // 6. Optional "Esc" shortcut hint pill below hovered cancel chip.
    if (pill_uniforms.hint_rect.z > 1.0) {
        let hr = vec4<f32>(pill_uniforms.hint_rect.xy, pill_uniforms.hint_rect.xy + pill_uniforms.hint_rect.zw);
        let h_sdf = rrect_sdf(p, hr, pill_uniforms.hint_rect.w * 0.5);
        let h_cov = clamp(0.5 - h_sdf * pill_uniforms.scale, 0.0, 1.0);
        if (h_cov > 0.001) {
            let ha = 0.92 * shape_alpha * h_cov;
            out = blend_over(out, vec4<f32>(vec3<f32>(0.11, 0.11, 0.12) * ha, ha));
            // Text inside hint pill (padded by 6 pt horizontally, 2.5 pt vertically).
            let t_min = pill_uniforms.hint_rect.xy + vec2<f32>(6.0, 2.5);
            let t_max = pill_uniforms.hint_rect.xy + pill_uniforms.hint_rect.zw - vec2<f32>(6.0, 2.5);
            if (p.x >= t_min.x && p.x <= t_max.x && p.y >= t_min.y && p.y <= t_max.y) {
                let t_uv = (p - t_min) / max(t_max - t_min, vec2<f32>(1.0));
                let uv = mix(pill_uniforms.hint_uv.xy, pill_uniforms.hint_uv.zw, t_uv);
                let txt_a = textureSample(atlas_texture, linear_sampler, uv).a * shape_alpha * h_cov;
                let txt_rgb = vec3<f32>(0.78, 0.78, 0.80);
                out = blend_over(out, vec4<f32>(txt_rgb * txt_a, txt_a));
            }
        }
        let hb_cov = clamp(1.0 + 0.5 / pill_uniforms.scale + h_sdf, 0.0, 1.0)
                   * clamp(0.5 / pill_uniforms.scale - h_sdf, 0.0, 1.0);
        if (hb_cov > 0.001) {
            let hba = hb_cov * 0.16 * shape_alpha;
            out = blend_over(out, vec4<f32>(vec3<f32>(1.0) * hba, hba));
        }
    }

    return out;
}
