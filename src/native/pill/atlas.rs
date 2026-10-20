//! CoreText & CoreGraphics (macOS) and pure-Rust fontdb + ab_glyph + tiny-skia (Linux)
//! Retina glyph and icon atlas for the native recording pill.

use std::collections::HashMap;

use super::model::{HUD_TEXT, Metrics};
use crate::ui::SelectionMode;

pub const ATLAS_W: u32 = 1024;
pub const ATLAS_H: u32 = 512;

pub const WEIGHT_MEDIUM: f64 = 0.23;
pub const WEIGHT_SEMIBOLD: f64 = 0.30;

#[derive(Clone, Copy, Debug, Default)]
pub struct Sprite {
    /// Width and height in logical points.
    pub size_pt: [f32; 2],
    /// UV bounds `[u0, v0, u1, v1]` in `[0.0 .. 1.0]`.
    pub uv: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconKind {
    Area,
    Window,
    Screen,
    Iphone,
    Close,
    Check,
}

impl IconKind {
    pub const fn from_mode(mode: SelectionMode) -> Self {
        match mode {
            SelectionMode::Area => Self::Area,
            SelectionMode::Window => Self::Window,
            SelectionMode::Screen => Self::Screen,
            SelectionMode::Iphone => Self::Iphone,
        }
    }
}

// ─── macOS CoreText & CoreGraphics Implementation ──────────────────────────────────────────

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
use core_foundation::{
    attributed_string::CFMutableAttributedString,
    base::{CFRange, TCFType},
    boolean::CFBoolean,
    string::CFString,
};
#[cfg(target_os = "macos")]
use core_graphics::{
    base::kCGImageAlphaPremultipliedLast,
    color_space::CGColorSpace,
    context::{CGContext, CGLineCap, CGLineJoin, CGTextDrawingMode},
};
#[cfg(target_os = "macos")]
use core_text::{
    font::{self, CTFont},
    line::CTLine,
    string_attributes::{kCTFontAttributeName, kCTForegroundColorFromContextAttributeName},
};
#[cfg(target_os = "macos")]
use metal::{MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize, MTLTextureUsage, TextureDescriptor};
#[cfg(target_os = "macos")]
use objc2::msg_send;
#[cfg(target_os = "macos")]
use objc2::runtime::{AnyClass, AnyObject};

/// Creates a `CTFont` from AppKit's `NSFont` (SF Pro, optionally with monospaced digits for the timer).
#[cfg(target_os = "macos")]
fn system_ct_font(size_px: f64, weight: f64, monospaced_digits: bool) -> Option<CTFont> {
    if let Some(cls) = AnyClass::get(c"NSFont") {
        let ptr: *mut AnyObject = unsafe {
            if monospaced_digits {
                msg_send![cls, monospacedDigitSystemFontOfSize: size_px, weight: weight]
            } else {
                msg_send![cls, systemFontOfSize: size_px, weight: weight]
            }
        };
        if !ptr.is_null() {
            let ct = unsafe { CTFont::wrap_under_get_rule(ptr.cast()) };
            return Some(ct);
        }
    }
    font::new_from_name(".AppleSystemUIFont", size_px).ok()
}

#[cfg(target_os = "macos")]
fn shape_ct_line(
    text: &str,
    size_px: f64,
    weight: f64,
    monospaced_digits: bool,
) -> Option<(CTFont, CTLine)> {
    let font = system_ct_font(size_px, weight, monospaced_digits)?;
    let mut attr = CFMutableAttributedString::new();
    attr.replace_str(&CFString::new(text), CFRange::init(0, 0));
    unsafe {
        attr.set_attribute(
            CFRange::init(0, attr.char_len()),
            kCTFontAttributeName,
            &font,
        );
        attr.set_attribute(
            CFRange::init(0, attr.char_len()),
            kCTForegroundColorFromContextAttributeName,
            &CFBoolean::true_value(),
        );
    }
    let line = CTLine::new_with_attributed_string(attr.as_concrete_TypeRef());
    Some((font, line))
}

/// Measures `text` in logical points at 13 pt (or `size_pt`).
#[cfg(target_os = "macos")]
pub fn measure_text_pt(text: &str, size_pt: f32, weight: f64, monospaced_digits: bool) -> f32 {
    if let Some((_, line)) = shape_ct_line(text, f64::from(size_pt), weight, monospaced_digits) {
        (line.get_typographic_bounds().width as f32).ceil()
    } else {
        (text.chars().count() as f32 * size_pt * 0.58).ceil()
    }
}

/// Measures all standard capsule strings with CoreText to build a `Metrics` table.
#[cfg(target_os = "macos")]
pub fn measure_metrics() -> Metrics {
    Metrics {
        area: measure_text_pt("Area", HUD_TEXT, WEIGHT_MEDIUM, false),
        window: measure_text_pt("Window", HUD_TEXT, WEIGHT_MEDIUM, false),
        screen: measure_text_pt("Screen", HUD_TEXT, WEIGHT_MEDIUM, false),
        iphone: measure_text_pt("iPhone", HUD_TEXT, WEIGHT_MEDIUM, false),
        record: measure_text_pt("Record", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        undo: measure_text_pt("Undo", HUD_TEXT, WEIGHT_MEDIUM, false),
        cancel: measure_text_pt("Cancel", HUD_TEXT, WEIGHT_MEDIUM, false),
        finish: measure_text_pt("Finish", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        processing: measure_text_pt("Processing…", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        paste: measure_text_pt("Paste", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        inspect: measure_text_pt("Inspect", HUD_TEXT, WEIGHT_MEDIUM, false),
        clock_000: measure_text_pt("0:00", HUD_TEXT, WEIGHT_SEMIBOLD, true),
        clock_0000: measure_text_pt("00:00", HUD_TEXT, WEIGHT_SEMIBOLD, true),
    }
}

/// Rasterizes a text label at `scale` into premultiplied white RGBA8 pixels.
#[cfg(target_os = "macos")]
fn rasterize_text(
    text: &str,
    size_pt: f32,
    weight: f64,
    monospaced_digits: bool,
    scale: f32,
) -> (u32, u32, [f32; 2], Vec<u8>) {
    let size_px = f64::from(size_pt * scale);
    let Some((font, line)) = shape_ct_line(text, size_px, weight, monospaced_digits) else {
        return (4, 4, [2.0, 2.0], vec![0; 64]);
    };
    let bounds = line.get_typographic_bounds();
    let pad_px = (2.0 * scale).ceil() as f64;
    let w_px = (bounds.width.ceil() + 2.0 * pad_px).max(4.0) as usize;
    let ascent = font.ascent();
    let descent = font.descent();
    let h_px = ((ascent + descent).ceil() + 2.0 * pad_px).max(4.0) as usize;

    let space = CGColorSpace::create_device_rgb();
    let mut ctx = CGContext::create_bitmap_context(
        None,
        w_px,
        h_px,
        8,
        w_px * 4,
        &space,
        kCGImageAlphaPremultipliedLast,
    );
    ctx.data().fill(0);
    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_should_smooth_fonts(false);
    ctx.set_rgb_fill_color(1.0, 1.0, 1.0, 1.0);
    ctx.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
    ctx.set_text_position(pad_px, pad_px + descent);
    line.draw(&ctx);
    ctx.flush();

    let size_pt = [w_px as f32 / scale, h_px as f32 / scale];
    (w_px as u32, h_px as u32, size_pt, ctx.data().to_vec())
}

/// Rasterizes one of Scrawl's line icons into premultiplied white RGBA8 pixels at `scale`.
#[cfg(target_os = "macos")]
fn rasterize_icon(kind: IconKind, size_pt: f32, scale: f32) -> (u32, u32, [f32; 2], Vec<u8>) {
    let w_px = (size_pt * scale).round().max(8.0) as usize;
    let h_px = w_px;
    let space = CGColorSpace::create_device_rgb();
    let mut ctx = CGContext::create_bitmap_context(
        None,
        w_px,
        h_px,
        8,
        w_px * 4,
        &space,
        kCGImageAlphaPremultipliedLast,
    );
    ctx.data().fill(0);
    ctx.set_should_antialias(true);
    ctx.set_allows_antialiasing(true);
    ctx.set_rgb_stroke_color(1.0, 1.0, 1.0, 1.0);
    ctx.set_line_cap(CGLineCap::CGLineCapRound);
    ctx.set_line_join(CGLineJoin::CGLineJoinRound);

    // Map 24×24 SVG coordinates (top-left origin) to CoreGraphics context (bottom-left origin).
    let s = w_px as f64 / 24.0;
    let map = |x: f64, y: f64| -> (f64, f64) { (x * s, (24.0 - y) * s) };
    let line_seg = |ctx: &CGContext, x0: f64, y0: f64, x1: f64, y1: f64| {
        let (ax, ay) = map(x0, y0);
        let (bx, by) = map(x1, y1);
        ctx.begin_path();
        ctx.move_to_point(ax, ay);
        ctx.add_line_to_point(bx, by);
        ctx.stroke_path();
    };
    let rrect = |ctx: &CGContext, x: f64, y: f64, w: f64, h: f64, r: f64| {
        let (x0, y1) = map(x, y);
        let (x1, y0) = map(x + w, y + h);
        let cr = (r * s).min((x1 - x0) * 0.5).min((y1 - y0) * 0.5);
        let c = cr * 0.552_284_75;
        ctx.begin_path();
        ctx.move_to_point(x0 + cr, y0);
        ctx.add_line_to_point(x1 - cr, y0);
        ctx.add_curve_to_point(x1 - cr + c, y0, x1, y0 + cr - c, x1, y0 + cr);
        ctx.add_line_to_point(x1, y1 - cr);
        ctx.add_curve_to_point(x1, y1 - cr + c, x1 - cr + c, y1, x1 - cr, y1);
        ctx.add_line_to_point(x0 + cr, y1);
        ctx.add_curve_to_point(x0 + cr - c, y1, x0, y1 - cr + c, x0, y1 - cr);
        ctx.add_line_to_point(x0, y0 + cr);
        ctx.add_curve_to_point(x0, y0 + cr - c, x0 + cr - c, y0, x0 + cr, y0);
        ctx.close_path();
        ctx.stroke_path();
    };

    ctx.set_line_width(2.5 * s);
    match kind {
        IconKind::Area => {
            let corner = |ctx: &CGContext, ax: f64, ay: f64, cx: f64, cy: f64, bx: f64, by: f64| {
                let (p0x, p0y) = map(ax, ay);
                let (pcx, pcy) = map(cx, cy);
                let (p1x, p1y) = map(bx, by);
                ctx.begin_path();
                ctx.move_to_point(p0x, p0y);
                ctx.add_line_to_point(pcx, pcy);
                ctx.add_line_to_point(p1x, p1y);
                ctx.stroke_path();
            };
            corner(&ctx, 3.0, 8.0, 3.0, 3.0, 8.0, 3.0);
            corner(&ctx, 16.0, 3.0, 21.0, 3.0, 21.0, 8.0);
            corner(&ctx, 21.0, 16.0, 21.0, 21.0, 16.0, 21.0);
            corner(&ctx, 8.0, 21.0, 3.0, 21.0, 3.0, 16.0);
        }
        IconKind::Window => {
            rrect(&ctx, 2.5, 4.0, 19.0, 16.0, 2.5);
            line_seg(&ctx, 2.5, 9.5, 21.5, 9.5);
        }
        IconKind::Screen => {
            rrect(&ctx, 2.0, 3.0, 20.0, 14.0, 2.0);
            line_seg(&ctx, 8.0, 21.0, 16.0, 21.0);
            line_seg(&ctx, 12.0, 17.0, 12.0, 21.0);
        }
        IconKind::Iphone => {
            rrect(&ctx, 5.5, 2.0, 13.0, 20.0, 3.0);
            line_seg(&ctx, 10.5, 6.0, 13.5, 6.0);
        }
        IconKind::Close => {
            ctx.set_line_width(2.4 * s);
            line_seg(&ctx, 6.5, 6.5, 17.5, 17.5);
            line_seg(&ctx, 17.5, 6.5, 6.5, 17.5);
        }
        IconKind::Check => {
            ctx.set_line_width(2.6 * s);
            let (ax, ay) = map(5.5, 12.5);
            let (bx, by) = map(10.0, 17.0);
            let (cx, cy) = map(18.5, 7.5);
            ctx.begin_path();
            ctx.move_to_point(ax, ay);
            ctx.add_line_to_point(bx, by);
            ctx.add_line_to_point(cx, cy);
            ctx.stroke_path();
        }
    }
    ctx.flush();
    (
        w_px as u32,
        h_px as u32,
        [size_pt, size_pt],
        ctx.data().to_vec(),
    )
}

#[cfg(target_os = "macos")]
pub fn rasterize_text_rgba(
    text: &str,
    size_pt: f32,
    weight: f64,
    monospaced_digits: bool,
    scale: f32,
) -> (u32, u32, [f32; 2], Vec<u8>) {
    rasterize_text(text, size_pt, weight, monospaced_digits, scale)
}

#[cfg(target_os = "macos")]
pub fn rasterize_icon_rgba(kind: IconKind, size_pt: f32, scale: f32) -> (u32, u32, [f32; 2], Vec<u8>) {
    rasterize_icon(kind, size_pt, scale)
}

#[cfg(target_os = "macos")]
pub struct GlyphAtlas {
    pub texture: metal::Texture,
    pub scale: f32,
    icons: HashMap<IconKind, Sprite>,
    labels: HashMap<String, Sprite>,
    cursor_x: u32,
    cursor_y: u32,
    row_h: u32,
    /// Reserved slot for the 1 Hz timer string (`y = 400`, `x = 0..240`).
    clock_text: String,
    pub clock_sprite: Sprite,
    /// Reserved slot for the completed label string (`y = 400`, `x = 256..960`).
    completed_text: String,
    pub completed_sprite: Sprite,
}

#[cfg(target_os = "macos")]
impl GlyphAtlas {
    pub fn new(device: &metal::DeviceRef, scale: f32) -> Self {
        let desc = TextureDescriptor::new();
        desc.set_pixel_format(MTLPixelFormat::RGBA8Unorm);
        desc.set_width(u64::from(ATLAS_W));
        desc.set_height(u64::from(ATLAS_H));
        desc.set_usage(MTLTextureUsage::ShaderRead);
        let texture = device.new_texture(&desc);

        // Zero-initialize the atlas texture.
        let zeros = vec![0_u8; (ATLAS_W * ATLAS_H * 4) as usize];
        texture.replace_region(
            MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize {
                    width: u64::from(ATLAS_W),
                    height: u64::from(ATLAS_H),
                    depth: 1,
                },
            },
            0,
            zeros.as_ptr().cast::<c_void>(),
            u64::from(ATLAS_W * 4),
        );

        let mut atlas = Self {
            texture,
            scale,
            icons: HashMap::new(),
            labels: HashMap::new(),
            cursor_x: 2,
            cursor_y: 2,
            row_h: 0,
            clock_text: String::new(),
            clock_sprite: Sprite::default(),
            completed_text: String::new(),
            completed_sprite: Sprite::default(),
        };

        for (kind, size_pt) in [
            (IconKind::Area, 14.0),
            (IconKind::Window, 14.0),
            (IconKind::Screen, 14.0),
            (IconKind::Iphone, 14.0),
            (IconKind::Close, 12.0),
            (IconKind::Check, 13.0),
        ] {
            let (w, h, size_pt, bytes) = rasterize_icon(kind, size_pt, scale);
            let sprite = atlas.pack(w, h, size_pt, &bytes);
            atlas.icons.insert(kind, sprite);
        }

        for (text, size_pt, weight) in [
            ("Area", HUD_TEXT, WEIGHT_MEDIUM),
            ("Window", HUD_TEXT, WEIGHT_MEDIUM),
            ("Screen", HUD_TEXT, WEIGHT_MEDIUM),
            ("iPhone", HUD_TEXT, WEIGHT_MEDIUM),
            ("Record", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Undo", HUD_TEXT, WEIGHT_MEDIUM),
            ("Cancel", HUD_TEXT, WEIGHT_MEDIUM),
            ("Finish", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Processing…", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Paste", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Inspect", HUD_TEXT, WEIGHT_MEDIUM),
            ("Esc", 11.0, WEIGHT_MEDIUM),
        ] {
            let (w, h, size_pt, bytes) = rasterize_text(text, size_pt, weight, false, scale);
            let sprite = atlas.pack(w, h, size_pt, &bytes);
            atlas.labels.insert(text.to_owned(), sprite);
        }

        atlas.update_clock("0:00");
        atlas.update_completed("Copied");
        atlas
    }

    fn pack(&mut self, w: u32, h: u32, size_pt: [f32; 2], bytes: &[u8]) -> Sprite {
        if self.cursor_x + w + 2 > ATLAS_W {
            self.cursor_x = 2;
            self.cursor_y += self.row_h + 2;
            self.row_h = 0;
        }
        let x = self.cursor_x;
        let y = self.cursor_y;
        self.cursor_x += w + 2;
        self.row_h = self.row_h.max(h);
        self.upload_at(x, y, w, h, bytes);
        Sprite {
            size_pt,
            uv: [
                x as f32 / ATLAS_W as f32,
                y as f32 / ATLAS_H as f32,
                (x + w) as f32 / ATLAS_W as f32,
                (y + h) as f32 / ATLAS_H as f32,
            ],
        }
    }

    fn upload_at(&self, x: u32, y: u32, w: u32, h: u32, bytes: &[u8]) {
        if x + w > ATLAS_W || y + h > ATLAS_H || w == 0 || h == 0 {
            return;
        }
        self.texture.replace_region(
            MTLRegion {
                origin: MTLOrigin {
                    x: u64::from(x),
                    y: u64::from(y),
                    z: 0,
                },
                size: MTLSize {
                    width: u64::from(w),
                    height: u64::from(h),
                    depth: 1,
                },
            },
            0,
            bytes.as_ptr().cast::<c_void>(),
            u64::from(w * 4),
        );
    }

    pub fn icon(&self, kind: IconKind) -> Sprite {
        self.icons.get(&kind).copied().unwrap_or_default()
    }

    pub fn label(&self, text: &str) -> Sprite {
        self.labels.get(text).copied().unwrap_or_default()
    }

    /// Updates the monospaced timer sprite only when `clock` changes (1 Hz during recording).
    pub fn update_clock(&mut self, clock: &str) -> Sprite {
        if self.clock_text == clock {
            return self.clock_sprite;
        }
        self.clock_text.clear();
        self.clock_text.push_str(clock);
        let (w, h, size_pt, bytes) =
            rasterize_text(clock, HUD_TEXT, WEIGHT_SEMIBOLD, true, self.scale);
        let (x, y) = (4_u32, 380_u32);
        let w = w.min(240);
        let h = h.min(120);
        self.upload_at(x, y, w, h, &bytes);
        self.clock_sprite = Sprite {
            size_pt,
            uv: [
                x as f32 / ATLAS_W as f32,
                y as f32 / ATLAS_H as f32,
                (x + w) as f32 / ATLAS_W as f32,
                (y + h) as f32 / ATLAS_H as f32,
            ],
        };
        self.clock_sprite
    }

    /// Updates the Completed lead label sprite only when `text` changes.
    pub fn update_completed(&mut self, text: &str) -> Sprite {
        if self.completed_text == text {
            return self.completed_sprite;
        }
        self.completed_text.clear();
        self.completed_text.push_str(text);
        let (w, h, size_pt, bytes) =
            rasterize_text(text, HUD_TEXT, WEIGHT_SEMIBOLD, false, self.scale);
        let (x, y) = (256_u32, 380_u32);
        let w = w.min(ATLAS_W - x - 4);
        let h = h.min(120);
        self.upload_at(x, y, w, h, &bytes);
        self.completed_sprite = Sprite {
            size_pt,
            uv: [
                x as f32 / ATLAS_W as f32,
                y as f32 / ATLAS_H as f32,
                (x + w) as f32 / ATLAS_W as f32,
                (y + h) as f32 / ATLAS_H as f32,
            ],
        };
        self.completed_sprite
    }
}

// ─── Linux Pure-Rust fontdb + ab_glyph + tiny-skia Implementation ──────────────────────────

#[cfg(target_os = "linux")]
use std::sync::{Arc, OnceLock};

#[cfg(target_os = "linux")]
use ab_glyph::{Font, Glyph, PxScale, ScaleFont};
#[cfg(target_os = "linux")]
use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

#[cfg(target_os = "linux")]
static FONT_DB: OnceLock<fontdb::Database> = OnceLock::new();

#[cfg(target_os = "linux")]
fn get_font_db() -> &'static fontdb::Database {
    FONT_DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

#[cfg(target_os = "linux")]
fn find_font_face<F, R>(monospaced: bool, weight: f64, f: F) -> Option<R>
where
    F: FnOnce(&ab_glyph::FontRef<'_>) -> R,
{
    let db = get_font_db();
    let query = if monospaced {
        fontdb::Query {
            families: &[
                fontdb::Family::Monospace,
                fontdb::Family::Name("Ubuntu Mono"),
                fontdb::Family::Name("DejaVu Sans Mono"),
                fontdb::Family::Name("Liberation Mono"),
                fontdb::Family::SansSerif,
            ],
            weight: fontdb::Weight(if weight >= WEIGHT_SEMIBOLD { 600 } else { 400 }),
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        }
    } else {
        fontdb::Query {
            families: &[
                fontdb::Family::SansSerif,
                fontdb::Family::Name("Ubuntu"),
                fontdb::Family::Name("DejaVu Sans"),
                fontdb::Family::Name("Liberation Sans"),
                fontdb::Family::Name("Noto Sans"),
            ],
            weight: fontdb::Weight(if weight >= WEIGHT_SEMIBOLD { 600 } else { 400 }),
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        }
    };
    let face_id = db.query(&query).or_else(|| db.faces().next().map(|face| face.id))?;
    db.with_face_data(face_id, |data, face_index| {
        let font_ref = ab_glyph::FontRef::try_from_slice_and_index(data, face_index).ok()?;
        Some(f(&font_ref))
    })?
}

/// Measures `text` in logical points at `size_pt` using pure-Rust font metrics.
#[cfg(target_os = "linux")]
pub fn measure_text_pt(text: &str, size_pt: f32, weight: f64, monospaced_digits: bool) -> f32 {
    let measured = find_font_face(monospaced_digits, weight, |font| {
        let px_scale = PxScale::from(size_pt);
        let scaled = font.as_scaled(px_scale);
        let mut width = 0.0_f32;
        let mut prev = None;
        for ch in text.chars() {
            let gid = font.glyph_id(ch);
            if let Some(p) = prev {
                width += scaled.kern(p, gid);
            }
            width += scaled.h_advance(gid);
            prev = Some(gid);
        }
        width.ceil()
    });
    measured.unwrap_or_else(|| {
        (text.chars().count() as f32 * size_pt * 0.58).ceil()
    })
}

/// Measures all standard capsule strings using fontdb to build a `Metrics` table.
#[cfg(target_os = "linux")]
pub fn measure_metrics() -> Metrics {
    Metrics {
        area: measure_text_pt("Area", HUD_TEXT, WEIGHT_MEDIUM, false),
        window: measure_text_pt("Window", HUD_TEXT, WEIGHT_MEDIUM, false),
        screen: measure_text_pt("Screen", HUD_TEXT, WEIGHT_MEDIUM, false),
        iphone: measure_text_pt("iPhone", HUD_TEXT, WEIGHT_MEDIUM, false),
        record: measure_text_pt("Record", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        undo: measure_text_pt("Undo", HUD_TEXT, WEIGHT_MEDIUM, false),
        cancel: measure_text_pt("Cancel", HUD_TEXT, WEIGHT_MEDIUM, false),
        finish: measure_text_pt("Finish", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        processing: measure_text_pt("Processing…", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        paste: measure_text_pt("Paste", HUD_TEXT, WEIGHT_SEMIBOLD, false),
        inspect: measure_text_pt("Inspect", HUD_TEXT, WEIGHT_MEDIUM, false),
        clock_000: measure_text_pt("0:00", HUD_TEXT, WEIGHT_SEMIBOLD, true),
        clock_0000: measure_text_pt("00:00", HUD_TEXT, WEIGHT_SEMIBOLD, true),
    }
}

/// Rasterizes a text label at `scale` into premultiplied white RGBA8 pixels using ab_glyph.
#[cfg(target_os = "linux")]
pub fn rasterize_text_rgba(
    text: &str,
    size_pt: f32,
    weight: f64,
    monospaced_digits: bool,
    scale: f32,
) -> (u32, u32, [f32; 2], Vec<u8>) {
    let size_px = size_pt * scale;
    let pad_px = (2.0 * scale).ceil();
    let res = find_font_face(monospaced_digits, weight, |font| {
        let px_scale = PxScale::from(size_px);
        let scaled = font.as_scaled(px_scale);
        let ascent = scaled.ascent();
        let descent = scaled.descent();
        let line_h = (ascent - descent).ceil().max(size_px);

        let mut glyphs = Vec::with_capacity(text.len());
        let mut pen_x = pad_px;
        let baseline_y = pad_px + ascent;
        let mut prev = None;
        for ch in text.chars() {
            let gid = font.glyph_id(ch);
            if let Some(p) = prev {
                pen_x += scaled.kern(p, gid);
            }
            let glyph = gid.with_scale_and_position(px_scale, ab_glyph::Point { x: pen_x, y: baseline_y });
            glyphs.push(glyph);
            pen_x += scaled.h_advance(gid);
            prev = Some(gid);
        }

        let w_px = ((pen_x + pad_px).ceil().max(4.0) as u32).max(4);
        let h_px = ((line_h + 2.0 * pad_px).ceil().max(4.0) as u32).max(4);
        let mut buf = vec![0_u8; (w_px * h_px * 4) as usize];

        for glyph in glyphs {
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let min_x = bounds.min.x.floor() as i32;
                let min_y = bounds.min.y.floor() as i32;
                outlined.draw(|gx, gy, coverage| {
                    let px = min_x + gx as i32;
                    let py = min_y + gy as i32;
                    if px >= 0 && px < w_px as i32 && py >= 0 && py < h_px as i32 {
                        let idx = ((py as u32 * w_px + px as u32) * 4) as usize;
                        let alpha = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
                        let cur_a = buf[idx + 3];
                        let new_a = cur_a.max(alpha);
                        buf[idx] = new_a;
                        buf[idx + 1] = new_a;
                        buf[idx + 2] = new_a;
                        buf[idx + 3] = new_a;
                    }
                });
            }
        }
        let size_pt = [w_px as f32 / scale, h_px as f32 / scale];
        (w_px, h_px, size_pt, buf)
    });

    res.unwrap_or_else(|| {
        let w_pt = measure_text_pt(text, size_pt, weight, monospaced_digits);
        let h_pt = size_pt * 1.2;
        let w_px = (w_pt * scale).ceil().max(4.0) as u32;
        let h_px = (h_pt * scale).ceil().max(4.0) as u32;
        (w_px, h_px, [w_pt, h_pt], vec![0; (w_px * h_px * 4) as usize])
    })
}

/// Rasterizes one of Scrawl's line icons into premultiplied white RGBA8 pixels using tiny-skia.
#[cfg(target_os = "linux")]
pub fn rasterize_icon_rgba(kind: IconKind, size_pt: f32, scale: f32) -> (u32, u32, [f32; 2], Vec<u8>) {
    let w_px = (size_pt * scale).round().max(8.0) as u32;
    let h_px = w_px;
    let mut pixmap = match Pixmap::new(w_px, h_px) {
        Some(p) => p,
        None => return (4, 4, [2.0, 2.0], vec![0; 64]),
    };

    let s = w_px as f32 / 24.0;
    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 255);
    paint.anti_alias = true;

    let stroke = Stroke {
        width: 2.5 * s,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };

    let line_seg = |pb: &mut PathBuilder, x0: f32, y0: f32, x1: f32, y1: f32| {
        pb.move_to(x0 * s, y0 * s);
        pb.line_to(x1 * s, y1 * s);
    };

    let rrect = |pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32, r: f32| {
        let x0 = x * s;
        let y0 = y * s;
        let rw = w * s;
        let rh = h * s;
        let cr = (r * s).min(rw * 0.5).min(rh * 0.5);
        let c = cr * 0.552_284_75;
        let x1 = x0 + rw;
        let y1 = y0 + rh;

        pb.move_to(x0 + cr, y0);
        pb.line_to(x1 - cr, y0);
        pb.cubic_to(x1 - cr + c, y0, x1, y0 + cr - c, x1, y0 + cr);
        pb.line_to(x1, y1 - cr);
        pb.cubic_to(x1, y1 - cr + c, x1 - cr + c, y1, x1 - cr, y1);
        pb.line_to(x0 + cr, y1);
        pb.cubic_to(x0 + cr - c, y1, x0, y1 - cr + c, x0, y1 - cr);
        pb.line_to(x0, y0 + cr);
        pb.cubic_to(x0, y0 + cr - c, x0 + cr - c, y0, x0 + cr, y0);
        pb.close();
    };

    let mut pb = PathBuilder::new();
    let mut custom_stroke = stroke;

    match kind {
        IconKind::Area => {
            let mut corner = |pb: &mut PathBuilder, ax: f32, ay: f32, cx: f32, cy: f32, bx: f32, by: f32| {
                pb.move_to(ax * s, ay * s);
                pb.line_to(cx * s, cy * s);
                pb.line_to(bx * s, by * s);
            };
            corner(&mut pb, 3.0, 8.0, 3.0, 3.0, 8.0, 3.0);
            corner(&mut pb, 16.0, 3.0, 21.0, 3.0, 21.0, 8.0);
            corner(&mut pb, 21.0, 16.0, 21.0, 21.0, 16.0, 21.0);
            corner(&mut pb, 8.0, 21.0, 3.0, 21.0, 3.0, 16.0);
        }
        IconKind::Window => {
            rrect(&mut pb, 2.5, 4.0, 19.0, 16.0, 2.5);
            line_seg(&mut pb, 2.5, 9.5, 21.5, 9.5);
        }
        IconKind::Screen => {
            rrect(&mut pb, 2.0, 3.0, 20.0, 14.0, 2.0);
            line_seg(&mut pb, 8.0, 21.0, 16.0, 21.0);
            line_seg(&mut pb, 12.0, 17.0, 12.0, 21.0);
        }
        IconKind::Iphone => {
            rrect(&mut pb, 5.5, 2.0, 13.0, 20.0, 3.0);
            line_seg(&mut pb, 10.5, 6.0, 13.5, 6.0);
        }
        IconKind::Close => {
            custom_stroke.width = 2.4 * s;
            line_seg(&mut pb, 6.5, 6.5, 17.5, 17.5);
            line_seg(&mut pb, 17.5, 6.5, 6.5, 17.5);
        }
        IconKind::Check => {
            custom_stroke.width = 2.6 * s;
            pb.move_to(5.5 * s, 12.5 * s);
            pb.line_to(10.0 * s, 17.0 * s);
            pb.line_to(18.5 * s, 7.5 * s);
        }
    }

    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &custom_stroke, Transform::identity(), None);
    }

    (
        w_px,
        h_px,
        [size_pt, size_pt],
        pixmap.take(),
    )
}

#[cfg(target_os = "linux")]
pub struct GlyphAtlas {
    pub texture: wgpu::Texture,
    pub texture_view: wgpu::TextureView,
    pub scale: f32,
    queue: Arc<wgpu::Queue>,
    icons: HashMap<IconKind, Sprite>,
    labels: HashMap<String, Sprite>,
    cursor_x: u32,
    cursor_y: u32,
    row_h: u32,
    clock_text: String,
    pub clock_sprite: Sprite,
    completed_text: String,
    pub completed_sprite: Sprite,
}

#[cfg(target_os = "linux")]
impl GlyphAtlas {
    pub fn new(device: &wgpu::Device, queue: Arc<wgpu::Queue>, scale: f32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pill_glyph_atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let zeros = vec![0_u8; (ATLAS_W * ATLAS_H * 4) as usize];
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &zeros,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(ATLAS_W * 4),
                rows_per_image: Some(ATLAS_H),
            },
            wgpu::Extent3d {
                width: ATLAS_W,
                height: ATLAS_H,
                depth_or_array_layers: 1,
            },
        );

        let mut atlas = Self {
            texture,
            texture_view,
            scale,
            queue,
            icons: HashMap::new(),
            labels: HashMap::new(),
            cursor_x: 2,
            cursor_y: 2,
            row_h: 0,
            clock_text: String::new(),
            clock_sprite: Sprite::default(),
            completed_text: String::new(),
            completed_sprite: Sprite::default(),
        };

        for (kind, size_pt) in [
            (IconKind::Area, 14.0),
            (IconKind::Window, 14.0),
            (IconKind::Screen, 14.0),
            (IconKind::Iphone, 14.0),
            (IconKind::Close, 12.0),
            (IconKind::Check, 13.0),
        ] {
            let (w, h, size_pt, bytes) = rasterize_icon_rgba(kind, size_pt, scale);
            let sprite = atlas.pack(w, h, size_pt, &bytes);
            atlas.icons.insert(kind, sprite);
        }

        for (text, size_pt, weight) in [
            ("Area", HUD_TEXT, WEIGHT_MEDIUM),
            ("Window", HUD_TEXT, WEIGHT_MEDIUM),
            ("Screen", HUD_TEXT, WEIGHT_MEDIUM),
            ("iPhone", HUD_TEXT, WEIGHT_MEDIUM),
            ("Record", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Undo", HUD_TEXT, WEIGHT_MEDIUM),
            ("Cancel", HUD_TEXT, WEIGHT_MEDIUM),
            ("Finish", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Processing…", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Paste", HUD_TEXT, WEIGHT_SEMIBOLD),
            ("Inspect", HUD_TEXT, WEIGHT_MEDIUM),
            ("Esc", 11.0, WEIGHT_MEDIUM),
        ] {
            let (w, h, size_pt, bytes) = rasterize_text_rgba(text, size_pt, weight, false, scale);
            let sprite = atlas.pack(w, h, size_pt, &bytes);
            atlas.labels.insert(text.to_owned(), sprite);
        }

        atlas.update_clock("0:00");
        atlas.update_completed("Copied");
        atlas
    }

    fn pack(&mut self, w: u32, h: u32, size_pt: [f32; 2], bytes: &[u8]) -> Sprite {
        if self.cursor_x + w + 2 > ATLAS_W {
            self.cursor_x = 2;
            self.cursor_y += self.row_h + 2;
            self.row_h = 0;
        }
        let x = self.cursor_x;
        let y = self.cursor_y;
        self.cursor_x += w + 2;
        self.row_h = self.row_h.max(h);
        self.upload_at(x, y, w, h, bytes);
        Sprite {
            size_pt,
            uv: [
                x as f32 / ATLAS_W as f32,
                y as f32 / ATLAS_H as f32,
                (x + w) as f32 / ATLAS_W as f32,
                (y + h) as f32 / ATLAS_H as f32,
            ],
        }
    }

    fn upload_at(&self, x: u32, y: u32, w: u32, h: u32, bytes: &[u8]) {
        if x + w > ATLAS_W || y + h > ATLAS_H || w == 0 || h == 0 {
            return;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn icon(&self, kind: IconKind) -> Sprite {
        self.icons.get(&kind).copied().unwrap_or_default()
    }

    pub fn label(&self, text: &str) -> Sprite {
        self.labels.get(text).copied().unwrap_or_default()
    }

    /// Updates the monospaced timer sprite only when `clock` changes (1 Hz during recording).
    pub fn update_clock(&mut self, clock: &str) -> Sprite {
        if self.clock_text == clock {
            return self.clock_sprite;
        }
        self.clock_text.clear();
        self.clock_text.push_str(clock);
        let (w, h, size_pt, bytes) =
            rasterize_text_rgba(clock, HUD_TEXT, WEIGHT_SEMIBOLD, true, self.scale);
        let (x, y) = (4_u32, 380_u32);
        let w = w.min(240);
        let h = h.min(120);
        self.upload_at(x, y, w, h, &bytes);
        self.clock_sprite = Sprite {
            size_pt,
            uv: [
                x as f32 / ATLAS_W as f32,
                y as f32 / ATLAS_H as f32,
                (x + w) as f32 / ATLAS_W as f32,
                (y + h) as f32 / ATLAS_H as f32,
            ],
        };
        self.clock_sprite
    }

    /// Updates the Completed lead label sprite only when `text` changes.
    pub fn update_completed(&mut self, text: &str) -> Sprite {
        if self.completed_text == text {
            return self.completed_sprite;
        }
        self.completed_text.clear();
        self.completed_text.push_str(text);
        let (w, h, size_pt, bytes) =
            rasterize_text_rgba(text, HUD_TEXT, WEIGHT_SEMIBOLD, false, self.scale);
        let (x, y) = (256_u32, 380_u32);
        let w = w.min(ATLAS_W - x - 4);
        let h = h.min(120);
        self.upload_at(x, y, w, h, &bytes);
        self.completed_sprite = Sprite {
            size_pt,
            uv: [
                x as f32 / ATLAS_W as f32,
                y as f32 / ATLAS_H as f32,
                (x + w) as f32 / ATLAS_W as f32,
                (y + h) as f32 / ATLAS_H as f32,
            ],
        };
        self.completed_sprite
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn coretext_metrics_and_monospaced_clock_digits_are_stable() {
        let m = measure_metrics();
        assert!(m.area >= 20.0 && m.area <= 45.0, "area={}", m.area);
        assert!(m.window > m.area, "window={} area={}", m.window, m.area);
        // Monospaced digits must give identical widths for 0:00, 1:11, and 4:59.
        let w_000 = measure_text_pt("0:00", HUD_TEXT, WEIGHT_SEMIBOLD, true);
        let w_111 = measure_text_pt("1:11", HUD_TEXT, WEIGHT_SEMIBOLD, true);
        let w_459 = measure_text_pt("4:59", HUD_TEXT, WEIGHT_SEMIBOLD, true);
        assert_eq!(w_000, w_111);
        assert_eq!(w_000, w_459);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn icons_rasterize_with_non_empty_ink() {
        for kind in [
            IconKind::Area,
            IconKind::Window,
            IconKind::Screen,
            IconKind::Iphone,
            IconKind::Close,
            IconKind::Check,
        ] {
            let (w, h, _, rgba) = rasterize_icon(kind, 14.0, 2.0);
            assert_eq!(w, 28);
            assert_eq!(h, 28);
            assert!(
                rgba.chunks_exact(4).any(|px| px[3] > 180),
                "{kind:?} produced no opaque pixels"
            );
        }
    }

    #[test]
    fn pure_rust_rasterizer_produces_non_empty_premultiplied_glyphs_and_icons() {
        let metrics = measure_metrics();
        assert!(metrics.record > 20.0 && metrics.clock_000 > 15.0);

        let (w, h, size_pt, rgba) = rasterize_text_rgba("0:05", HUD_TEXT, WEIGHT_SEMIBOLD, true, 2.0);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(size_pt[0] > 10.0 && size_pt[1] > 8.0);
        assert!(rgba.chunks_exact(4).any(|px| px[3] > 128), "glyph alpha pixels rendered");

        for kind in [
            IconKind::Area,
            IconKind::Window,
            IconKind::Screen,
            IconKind::Close,
            IconKind::Check,
        ] {
            let (iw, ih, _, bytes) = rasterize_icon_rgba(kind, 14.0, 2.0);
            assert_eq!(bytes.len(), (iw * ih * 4) as usize);
            assert!(bytes.chunks_exact(4).any(|px| px[3] > 180), "{kind:?} icon has opaque strokes");
        }
    }
}
