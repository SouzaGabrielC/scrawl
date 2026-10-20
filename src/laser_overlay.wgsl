// Scrawl Laser & Ink Overlay WGSL Shader
// Ported from src/laser_overlay.metal for Linux (wgpu) & multi-platform support.

struct LaserUniforms {
    resolution: vec2<f32>,
    scale: f32,
    now_sec: f32,
    tail_duration: f32,
    tip_active: f32,
    tip_pos: vec2<f32>,
    segment_count: u32,
    pin_count: u32,
    constrain_rect: f32,
    _pad1: f32,
    target_rect: vec4<f32>,
    // x: 1.0 when a full-screen recording started straight from the shortcut (no picker).
    glow: vec4<f32>,
}

// Each segment connects a -> b in logical screen points.
// kind == 0.0: tapered laser segment (w0..w1 = combined arc + time weight 0..1)
// kind == 1.0: persistent ink segment (w0..w1 = simulated pressure scale)
struct PathSegment {
    a: vec2<f32>,
    b: vec2<f32>,
    w0: f32,
    w1: f32,
    kind: f32,
    width: f32,
}

struct PinMarker {
    pos: vec2<f32>,
    radius: f32,
    _pad: f32,
}

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var<uniform> u: LaserUniforms;
@group(0) @binding(1) var<storage, read> segments: array<PathSegment>;
@group(0) @binding(2) var<storage, read> pins: array<PinMarker>;

@vertex
fn laser_vertex(@builtin(vertex_index) vid: u32) -> VertexOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let p = positions[vid];
    var out: VertexOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = p * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn laser_fragment(in: VertexOut) -> @location(0) vec4<f32> {
    // Pixel position in logical screen points (top-left origin).
    let p = (in.uv * u.resolution) / max(u.scale, 1.0);

    let laser_red = vec3<f32>(1.0, 0.16, 0.19);
    let laser_hot = vec3<f32>(1.0, 0.97, 0.97);

    var laser_core_cov: f32 = 0.0;
    var laser_body_cov: f32 = 0.0;
    var laser_glow_cov: f32 = 0.0;
    var ink_halo_cov: f32 = 0.0;
    var ink_red_cov: f32 = 0.0;

    let count = min(u.segment_count, 1536u);
    for (var i = 0u; i < count; i = i + 1u) {
        let seg = segments[i];
        let ab = seg.b - seg.a;
        let len2 = max(dot(ab, ab), 0.0001);
        let h = clamp(dot(p - seg.a, ab) / len2, 0.0, 1.0);
        let closest = seg.a + ab * h;
        let d = length(p - closest);
        let w = clamp(mix(seg.w0, seg.w1, h), 0.0, 1.25);

        if (seg.kind < 0.5) {
            if (w <= 0.002) {
                continue;
            }
            let glow_sigma = max(1.4, 6.2 * (0.28 + 0.72 * w));
            if (d < glow_sigma * 3.0) {
                let body_r = 2.45 * pow(max(w, 0.0001), 0.68);
                let core_r = 0.90 * pow(max(w, 0.0001), 0.82);
                let body = (1.0 - smoothstep(max(0.0, body_r - 0.85), body_r + 0.85, d)) * pow(max(w, 0.0001), 0.40);
                let core = (1.0 - smoothstep(max(0.0, core_r - 0.75), core_r + 0.75, d)) * pow(max(w, 0.0001), 0.72);
                let glow = exp(-(d * d) / (glow_sigma * glow_sigma)) * pow(max(w, 0.0001), 0.92);
                laser_body_cov = max(laser_body_cov, body);
                laser_core_cov = max(laser_core_cov, core);
                laser_glow_cov = max(laser_glow_cov, glow);
            }
        } else {
            // Simple tldraw-style thin red marker stroke (~4.5px wide, smooth anti-aliased edge, fades out after release).
            let r = 2.25 * clamp(w, 0.55, 1.15);
            if (d < r + 2.0) {
                let fill = (1.0 - smoothstep(max(0.0, r - 0.70), r + 0.70, d)) * clamp(seg.width, 0.0, 1.0);
                ink_red_cov = max(ink_red_cov, fill);
            }
        }
    }

    // Pin markers
    let pcount = min(u.pin_count, 64u);
    for (var i = 0u; i < pcount; i = i + 1u) {
        let pin = pins[i];
        let d = length(p - pin.pos);
        if (d < pin.radius + 3.5) {
            let halo = 1.0 - smoothstep(pin.radius + 0.8, pin.radius + 2.0, d);
            let fill = 1.0 - smoothstep(pin.radius - 0.75, pin.radius + 0.75, d);
            ink_halo_cov = max(ink_halo_cov, halo);
            ink_red_cov = max(ink_red_cov, fill);
        }
    }

    // Subtle red marker dot at the cursor tip while Shift is held.
    if (u.tip_active > 0.01) {
        let d_tip = length(p - u.tip_pos);
        if (d_tip < 5.0) {
            let tip_fill = (1.0 - smoothstep(1.4, 2.6, d_tip)) * u.tip_active;
            ink_red_cov = max(ink_red_cov, tip_fill);
        }
    }

    var out_rgb: vec3<f32> = vec3<f32>(0.0);
    var out_a: f32 = 0.0;

    // Smooth 240ms transition from the picker's exact border/dim geometry into active recording.
    // `u.tail_duration` carries `picker_fade` (0..1 during the initial 150ms picker open, 1.0 during recording).
    let picker_fade = clamp(u.tail_duration, 0.0, 1.0);
    let t_intro = clamp(u.now_sec / 0.24, 0.0, 1.0);
    let intro_ease = 1.0 - pow(max(1.0 - t_intro, 0.0), 3.0);
    let intro_fade = 1.0 - intro_ease;

    if (u.constrain_rect > 0.5) {
        let is_device = u._pad1 > 2.5;
        let is_window = u._pad1 > 1.5 && !is_device;
        var corner_r: f32 = 0.0;
        if (is_device) {
            corner_r = min(u.target_rect.z, u.target_rect.w) * 0.135;
        } else if (is_window) {
            corner_r = 10.0;
        }
        let box_min = u.target_rect.xy;
        let box_max = u.target_rect.xy + u.target_rect.zw;
        let box_center = (box_min + box_max) * 0.5;
        let half_size = max((box_max - box_min) * 0.5 - vec2<f32>(corner_r), vec2<f32>(0.0));
        let q = abs(p - box_center) - half_size;
        let sdf = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - corner_r;

        // 1. Outside the target rect (`sdf > 0.0`): 0.32 dim during picker -> eases to 0.22 during recording.
        let out_cov = clamp(sdf + 0.5, 0.0, 1.0);
        if (out_cov > 0.001) {
            let dim_a = out_cov * mix(0.32, 0.22, intro_ease) * picker_fade;
            out_rgb = vec3<f32>(0.0);
            out_a = dim_a;
        }

        // 2. Inside the selected window (`sdf <= 0.0`): dissolve the picker's blue tint (`0x3b5cf621`).
        let in_cov = 1.0 - out_cov;
        if (in_cov > 0.001 && is_window && intro_fade > 0.002) {
            let fill_a = in_cov * 0.13 * intro_fade * picker_fade;
            let fill_col = vec3<f32>(0.231, 0.361, 0.965);
            out_rgb = fill_col * fill_a + out_rgb * (1.0 - fill_a);
            out_a = fill_a + out_a * (1.0 - fill_a);
        }

        // 3. Inset border (`sdf in [-border_w, 0.0]`): rendered by this exact same shader layer during
        //    BOTH Selecting (`now_sec == 0`) and Recording (`now_sec > 0`), so there is zero window swap or jump.
        //    (`is_device` already renders its own titanium PhoneCaseView bezel.)
        if (!is_device) {
            var border_w: f32 = 1.0;
            if (is_window) {
                border_w = mix(2.0, 1.0, intro_ease);
            }
            let border_cov = clamp(border_w + 0.5 + sdf, 0.0, 1.0) * clamp(0.5 - sdf, 0.0, 1.0);
            if (border_cov > 0.001) {
                var picker_border = vec3<f32>(1.0, 1.0, 1.0);
                if (is_window) {
                    picker_border = vec3<f32>(0.357, 0.486, 0.980);
                }
                let border_col = mix(picker_border, vec3<f32>(1.0, 1.0, 1.0), intro_ease);
                var base_alpha: f32 = 0.92;
                if (is_window) {
                    base_alpha = 0.93;
                }
                let border_alpha = border_cov * mix(base_alpha, 0.28, intro_ease) * picker_fade;
                out_rgb = border_col * border_alpha + out_rgb * (1.0 - border_alpha);
                out_a = border_alpha + out_a * (1.0 - border_alpha);
            }
        }
    } else if (u._pad1 < -0.0001) {
        // Picker open before any drag starts: uniform 0.32 * picker_fade dim.
        out_rgb = vec3<f32>(0.0);
        out_a = 0.32 * clamp(-u._pad1, 0.0, 1.0);
    } else if (u.glow.x > 0.5) {
        // Full-screen recording started from the shortcut: a soft red glow along the display's
        // edges rises in 0.12s and is gone by 1.1s, so the start is visible without the picker.
        let env = smoothstep(0.0, 0.12, u.now_sec) * (1.0 - smoothstep(0.35, 1.1, u.now_sec));
        if (env > 0.002) {
            let lo = u.target_rect.xy;
            let hi = lo + u.target_rect.zw;
            let d = max(min(min(p.x - lo.x, hi.x - p.x), min(p.y - lo.y, hi.y - p.y)), 0.0);
            out_rgb = vec3<f32>(1.0, 0.23, 0.19);
            out_a = exp(-d / 22.0) * 0.45 * env;
        }
    } else if (intro_fade > 0.002) {
        // Full-screen recording: simple 0.15s dim fade out.
        out_rgb = vec3<f32>(0.0);
        out_a = 0.32 * intro_fade;
    }

    // 1. Composite persistent ink (white halo + red fill)
    if (ink_halo_cov > 0.001) {
        let a = ink_halo_cov * 0.82;
        out_rgb = vec3<f32>(1.0) * a + out_rgb * (1.0 - a);
        out_a = a + out_a * (1.0 - a);
    }
    if (ink_red_cov > 0.001) {
        let ink_col = vec3<f32>(1.0, 0.23, 0.19);
        let a = ink_red_cov;
        out_rgb = ink_col * a + out_rgb * (1.0 - a);
        out_a = a + out_a * (1.0 - a);
    }

    // 2. Composite optical laser (soft atmospheric bloom + anti-aliased ruby ribbon + white-hot core)
    let laser_alpha = clamp(laser_glow_cov * 0.42 + laser_body_cov * 0.88 + laser_core_cov * 0.95, 0.0, 1.0);
    if (laser_alpha > 0.002) {
        let col = mix(laser_red, laser_hot, clamp(laser_core_cov * 0.92, 0.0, 1.0));
        out_rgb = col * laser_alpha + out_rgb * (1.0 - laser_alpha);
        out_a = laser_alpha + out_a * (1.0 - laser_alpha);
    }

    return vec4<f32>(out_rgb * out_a, out_a);
}
