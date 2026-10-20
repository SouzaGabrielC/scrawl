//! Native AppKit NSPanel + CAMetalLayer (macOS) and X11 + wgpu (Linux) renderer for the Scrawl recording pill.
//!
//! Replaces the GPUI HudPillWindow with a single fixed-size panel (640 x 84 pt at y = 34
//! below the menu bar) that never resizes or moves during morphs.

pub mod atlas;
pub mod model;

use std::mem::size_of;

use self::atlas::IconKind;
use self::model::{
    CHIP_H, CHIP_PX, CX, ElemId, FrameState, PANEL_H, PANEL_W, PillModel, Pres, TOP,
};
use crate::ui::{SelectionMode, motion};

const OFFSCREEN_W_PT: f32 = 960.0;
const OFFSCREEN_H_PT: f32 = 256.0;
const TILE_PAD_PT: f32 = 20.0;

/// Capsule visual style (`pillStyle: "black" | "glass"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PillStyle {
    #[default]
    Black,
    Glass,
}

#[allow(dead_code)]
impl PillStyle {
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "glass" | "liquid" | "liquid-glass" | "1" | "true" => Some(Self::Glass),
            "black" | "dark" | "metal" | "0" | "false" => Some(Self::Black),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Glass => "glass",
        }
    }

    /// Resolves the active `PillStyle` from `SCRAWL_PILL_STYLE`, `tune.json`, or `config.json`.
    pub fn current() -> Self {
        if let Ok(env) = std::env::var("SCRAWL_PILL_STYLE")
            && let Some(style) = Self::from_str_opt(&env)
        {
            return style;
        }
        if let Some(style) = motion::tune().pill_style_override() {
            return style;
        }
        crate::handoff::pill_style()
    }
}

/// User actions emitted by clicking or pressing keys on the native pill.
#[derive(Clone, Copy, Debug)]
pub enum PillAction {
    SelectMode(SelectionMode),
    SwitchRecording(SelectionMode),
    RecordSelected,
    Undo,
    Cancel,
    Finish,
    Paste(Option<[f32; 2]>),
    Inspect,
    DismissCompleted,
}

/// Coordinator state snapshot synced to `NativePill`.
#[derive(Clone, Debug, PartialEq)]
pub struct PillSnapshot {
    pub pres: Pres,
    pub clock: String,
    pub reduced_motion: bool,
    pub style: PillStyle,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PillUniforms {
    panel_size: [f32; 2],
    canvas_size: [f32; 2],
    scale: f32,
    elem_count: u32,
    is_completed: u32,
    has_ghost: u32,
    shape_rect: [f32; 4],
    shape_meta: [f32; 4],
    ghost_rect: [f32; 4],
    ghost_meta: [f32; 4],
    hint_rect: [f32; 4],
    hint_uv: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TileUniforms {
    atlas_size: [f32; 2],
    canvas_size: [f32; 2],
    scale: f32,
    _pad0: f32,
    _pad1: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TilePrim {
    rect: [f32; 4],
    uv_rect: [f32; 4],
    color: [f32; 4],
    kind: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlurBox {
    tile_rect: [f32; 4],
    sigma_pt: f32,
    _pad0: f32,
    _pad1: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CompositeElem {
    screen_rect: [f32; 4],
    tile_rect: [f32; 4],
    base_size: [f32; 2],
    scale: f32,
    alpha: f32,
    sigma_pt: f32,
    pad_pt: f32,
    _pad0: [f32; 2],
}


/// The panel's top-left (global points) on the screen holding point, else the primary one.
fn panel_origin(screens: &[crate::screens::Screen], point: Option<[f64; 2]>) -> Option<[f32; 2]> {
    let index = point
        .and_then(|p| crate::screens::index_at(screens, p))
        .or(if screens.is_empty() { None } else { Some(0) })?;
    let [x, y] = crate::screens::pill_origin(
        &screens[index],
        f64::from(PANEL_W),
        f64::from(model::PANEL_Y),
    );
    Some([x as f32, y as f32])
}

fn emit_elem_prims(
    atlas: &atlas::GlyphAtlas,
    is_glass: bool,
    elem: &model::FrameElem,
    [ox, oy]: [f32; 2],
    t: f32,
    out: &mut Vec<TilePrim>,
) {
    let [bw, bh] = elem.base_size;
    let push_shape = |out: &mut Vec<TilePrim>, rect: [f32; 4], radius: f32, color: [f32; 4]| {
        out.push(TilePrim {
            rect,
            uv_rect: [radius, 0.0, 0.0, 0.0],
            color,
            kind: 0,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
        });
    };
    let push_sprite = |out: &mut Vec<TilePrim>, rect: [f32; 4], uv: [f32; 4], color: [f32; 4]| {
        out.push(TilePrim {
            rect,
            uv_rect: uv,
            color,
            kind: 1,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
        });
    };
    let center_text = |sp: atlas::Sprite| -> [f32; 4] {
        [
            ox + (bw - sp.size_pt[0]) * 0.5,
            oy + (bh - sp.size_pt[1]) * 0.5,
            sp.size_pt[0],
            sp.size_pt[1],
        ]
    };


    match elem.id {
        ElemId::ChipArea | ElemId::ChipWindow | ElemId::ChipScreen | ElemId::ChipIphone => {
            let mode = match elem.id {
                ElemId::ChipArea => SelectionMode::Area,
                ElemId::ChipWindow => SelectionMode::Window,
                ElemId::ChipScreen => SelectionMode::Screen,
                ElemId::ChipIphone => SelectionMode::Iphone,
                _ => unreachable!(),
            };
            if is_glass {
                let smoke_a = 0.20 + 0.18 * elem.active + 0.10 * elem.hover;
                push_shape(out, [ox, oy, bw, bh], bh * 0.5, [0.06, 0.07, 0.10, smoke_a]);
            } else {
                let bg_a = 0.16 * elem.active
                    + 0.08 * elem.hover * (1.0 - elem.active)
                    + 0.06 * elem.press;
                if bg_a > 0.002 {
                    push_shape(out, [ox, oy, bw, bh], bh * 0.5, [1.0, 1.0, 1.0, bg_a]);
                }
            }
            let ic = atlas.icon(IconKind::from_mode(mode));
            let tx = atlas.label(mode.label());
            let fg = if is_glass {
                0.90 + 0.10 * elem.active.max(elem.hover)
            } else {
                0.608 + 0.353 * elem.active.max(elem.hover * 0.65)
            };
            push_sprite(
                out,
                [ox + CHIP_PX, oy + (CHIP_H - 14.0) * 0.5, 14.0, 14.0],
                ic.uv,
                [fg, fg, fg, 1.0],
            );
            push_sprite(
                out,
                [
                    ox + CHIP_PX + 14.0 + 6.0 - 2.0,
                    oy + (CHIP_H - tx.size_pt[1]) * 0.5,
                    tx.size_pt[0],
                    tx.size_pt[1],
                ],
                tx.uv,
                [fg, fg, fg, 1.0],
            );
        }
        ElemId::Record | ElemId::Finish | ElemId::Paste => {
            let label = match elem.id {
                ElemId::Record => "Record",
                ElemId::Finish => "Finish",
                ElemId::Paste => "Paste",
                _ => unreachable!(),
            };
            let tx = atlas.label(label);
            // `ACCENT_COLOR = 0x3b5cf6`, `ACCENT_HOVER = 0x4b6bff`
            let r = (0.231 + 0.063 * elem.hover - 0.03 * elem.press).clamp(0.0, 1.0);
            let g = (0.361 + 0.059 * elem.hover - 0.04 * elem.press).clamp(0.0, 1.0);
            let b = (0.965 + 0.035 * elem.hover - 0.04 * elem.press).clamp(0.0, 1.0);
            push_shape(out, [ox, oy, bw, bh], bh * 0.5, [r, g, b, 1.0]);
            push_sprite(out, center_text(tx), tx.uv, [0.95, 0.95, 0.96, 1.0]);
        }
        ElemId::Lead => {
            if is_glass {
                push_shape(out, [ox, oy, bw, bh], bh * 0.5, [0.06, 0.07, 0.10, 0.24]);
            }
            let tx = atlas.clock_sprite;
            let cycle = 1.6_f32;
            let ripple = (t / cycle).fract();
            let breath = 0.5 + 0.5 * (std::f32::consts::TAU * t / cycle).cos();
            let halo_r = 4.0 + 6.0 * ripple;
            let halo_a = 0.34 * (1.0 - ripple).powi(2);
            push_shape(
                out,
                [
                    ox + 16.0 - halo_r,
                    oy + 16.0 - halo_r,
                    halo_r * 2.0,
                    halo_r * 2.0,
                ],
                halo_r,
                [0.941, 0.267, 0.220, halo_a],
            );
            push_shape(
                out,
                [ox + 12.0, oy + 12.0, 8.0, 8.0],
                4.0,
                [0.941, 0.267, 0.220, 0.78 + 0.22 * breath],
            );
            push_sprite(
                out,
                [
                    ox + CHIP_H - 2.0,
                    oy + (CHIP_H - tx.size_pt[1]) * 0.5,
                    tx.size_pt[0],
                    tx.size_pt[1],
                ],
                tx.uv,
                [0.961, 0.961, 0.969, 1.0],
            );
        }
        ElemId::Undo | ElemId::Cancel | ElemId::Inspect => {
            let label = match elem.id {
                ElemId::Undo => "Undo",
                ElemId::Cancel => "Cancel",
                ElemId::Inspect => "Inspect",
                _ => unreachable!(),
            };
            if is_glass {
                let smoke_a = if elem.enabled {
                    0.20 + 0.12 * elem.hover + 0.06 * elem.press
                } else {
                    0.12
                };
                push_shape(out, [ox, oy, bw, bh], bh * 0.5, [0.06, 0.07, 0.10, smoke_a]);
            } else if elem.enabled && (elem.hover > 0.01 || elem.press > 0.01) {
                let bg_a = 0.08 * elem.hover + 0.06 * elem.press;
                push_shape(out, [ox, oy, bw, bh], bh * 0.5, [1.0, 1.0, 1.0, bg_a]);
            }
            let tx = atlas.label(label);
            let fg = if !elem.enabled {
                if is_glass { 0.78 } else { 0.56 }
            } else if is_glass {
                0.92 + 0.08 * elem.hover
            } else {
                0.74 + 0.22 * elem.hover
            };
            let fg_a = if !elem.enabled { 0.90 } else { 1.0 };
            push_sprite(out, center_text(tx), tx.uv, [fg, fg, fg, fg_a]);
        }
        ElemId::Close => {
            if is_glass {
                let smoke_a = 0.20 + 0.12 * elem.hover + 0.06 * elem.press;
                push_shape(
                    out,
                    [ox + 2.0, oy + 2.0, bh - 4.0, bh - 4.0],
                    (bh - 4.0) * 0.5,
                    [0.06, 0.07, 0.10, smoke_a],
                );
            } else if elem.hover > 0.01 || elem.press > 0.01 {
                let bg_a = 0.10 * elem.hover + 0.06 * elem.press;
                push_shape(
                    out,
                    [ox + 2.0, oy + 2.0, bh - 4.0, bh - 4.0],
                    (bh - 4.0) * 0.5,
                    [1.0, 1.0, 1.0, bg_a],
                );
            }
            let ic = atlas.icon(IconKind::Close);
            let fg = if is_glass {
                0.90 + 0.10 * elem.hover
            } else {
                0.61 + 0.35 * elem.hover
            };
            push_sprite(
                out,
                [ox + (CHIP_H - 12.0) * 0.5, oy + (CHIP_H - 12.0) * 0.5, 12.0, 12.0],
                ic.uv,
                [fg, fg, fg, 1.0],
            );
        }
        ElemId::DividerLead | ElemId::DividerClose => {
            push_shape(
                out,
                [ox + (bw - 1.0) * 0.5, oy + (bh - 16.0) * 0.5, 1.0, 16.0],
                0.5,
                [1.0, 1.0, 1.0, 0.11],
            );
        }
        ElemId::Processing => {
            let head = (t * 8.0) % 8.0;
            for k in 0..8 {
                let ang =
                    (k as f32) * std::f32::consts::FRAC_PI_4 - std::f32::consts::FRAC_PI_2;
                let dcx = ox + 16.0 + 6.2 * ang.cos();
                let dcy = oy + 16.0 + 6.2 * ang.sin();
                let phase_d = (head - k as f32).rem_euclid(8.0);
                let da = (1.0 - phase_d / 7.5).clamp(0.18, 1.0);
                push_shape(
                    out,
                    [dcx - 1.35, dcy - 1.35, 2.7, 2.7],
                    1.35,
                    [0.961, 0.961, 0.969, da],
                );
            }
            let tx = atlas.label("Processing…");
            push_sprite(
                out,
                [
                    ox + CHIP_H + 6.0 - 2.0,
                    oy + (CHIP_H - tx.size_pt[1]) * 0.5,
                    tx.size_pt[0],
                    tx.size_pt[1],
                ],
                tx.uv,
                [0.961, 0.961, 0.969, 1.0],
            );
        }
        ElemId::DoneLead => {
            push_shape(
                out,
                [ox + 5.0, oy + 5.0, 22.0, 22.0],
                11.0,
                [0.412, 0.847, 0.624, 0.22],
            );
            let ic = atlas.icon(IconKind::Check);
            push_sprite(
                out,
                [ox + (CHIP_H - 13.0) * 0.5, oy + (CHIP_H - 13.0) * 0.5, 13.0, 13.0],
                ic.uv,
                [0.412, 0.847, 0.624, 1.0],
            );
            let tx = atlas.completed_sprite;
            push_sprite(
                out,
                [
                    ox + CHIP_H + 8.0 - 2.0,
                    oy + (CHIP_H - tx.size_pt[1]) * 0.5,
                    tx.size_pt[0],
                    tx.size_pt[1],
                ],
                tx.uv,
                [0.961, 0.961, 0.969, 1.0],
            );
        }
    }
}

// ─── macOS AppKit & Metal Implementation ───────────────────────────────────────────────────

#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use core_graphics_types::geometry::CGSize;
#[cfg(target_os = "macos")]
use core_video::base::{CVOptionFlags, CVTimeStamp};
#[cfg(target_os = "macos")]
use core_video::display_link::{CVDisplayLink, CVDisplayLinkRef};
#[cfg(target_os = "macos")]
use core_video::r#return::CVReturn;
#[cfg(target_os = "macos")]
use metal::foreign_types::ForeignType;
#[cfg(target_os = "macos")]
use metal::{
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLStoreAction, MTLTextureUsage, MetalLayer,
    RenderPassDescriptor, RenderPipelineDescriptor, RenderPipelineState, TextureDescriptor,
};
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::{AnyClass, AnyObject, NSObjectProtocol};
#[cfg(target_os = "macos")]
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSPanel, NSScreen, NSStatusWindowLevel,
    NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
#[cfg(target_os = "macos")]
use objc2_quartz_core::{CALayer, CATransaction};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

#[cfg(target_os = "macos")]
struct PillViewIvars {
    action_tx: flume::Sender<PillAction>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = PillViewIvars]
    struct ScrawlPillView;

    unsafe impl NSObjectProtocol for ScrawlPillView {}

    impl ScrawlPillView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(canBecomeKeyView))]
        fn can_become_key_view(&self) -> bool {
            true
        }

        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().action_tx.try_send(PillAction::Cancel);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let key_code: u16 = unsafe { msg_send![event, keyCode] };
            if key_code == 53 {
                let _ = self.ivars().action_tx.try_send(PillAction::Cancel);
            }
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn accessibility_role(&self) -> Retained<NSString> {
            NSString::from_str("AXToolbar")
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn accessibility_label(&self) -> Retained<NSString> {
            NSString::from_str("Scrawl Recording Controls")
        }
    }
);

impl ScrawlPillView {
    fn new(mtm: MainThreadMarker, frame: NSRect, action_tx: flume::Sender<PillAction>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(PillViewIvars { action_tx });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

#[cfg(target_os = "macos")]
struct MetalPipelines {
    _device: metal::Device,
    queue: metal::CommandQueue,
    layer: MetalLayer,
    tile_pipeline: RenderPipelineState,
    blur_h_pipeline: RenderPipelineState,
    composite_pipeline: RenderPipelineState,
    offscreen_a: metal::Texture,
    offscreen_b: metal::Texture,
    atlas: GlyphAtlas,
    scale: f32,
}

#[cfg(target_os = "macos")]
impl MetalPipelines {
    fn new(scale: f32) -> Result<Self, String> {
        let device = metal::Device::system_default().ok_or("No Metal device available")?;
        let queue = device.new_command_queue();

        let metallib_bytes = include_bytes!(concat!(env!("OUT_DIR"), "/pill.metallib"));
        let library = device
            .new_library_with_data(metallib_bytes)
            .map_err(|e| format!("Failed to load pill.metallib: {e}"))?;

        let make_pipeline = |vs_name: &str, fs_name: &str, premul_blend: bool| -> Result<RenderPipelineState, String> {
            let vs = library
                .get_function(vs_name, None)
                .map_err(|e| format!("Missing vertex shader {vs_name}: {e}"))?;
            let fs = library
                .get_function(fs_name, None)
                .map_err(|e| format!("Missing fragment shader {fs_name}: {e}"))?;
            let desc = RenderPipelineDescriptor::new();
            desc.set_vertex_function(Some(&vs));
            desc.set_fragment_function(Some(&fs));
            let att = desc
                .color_attachments()
                .object_at(0)
                .ok_or("Missing color attachment 0")?;
            att.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            if premul_blend {
                att.set_blending_enabled(true);
                att.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
                att.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
                att.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
                att.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
                att.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
                att.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
            } else {
                att.set_blending_enabled(false);
            }
            device
                .new_render_pipeline_state(&desc)
                .map_err(|e| format!("Pipeline {vs_name}/{fs_name} error: {e}"))
        };

        let tile_pipeline = make_pipeline("pill_tile_vertex", "pill_tile_fragment", true)?;
        let blur_h_pipeline = make_pipeline("pill_blur_h_vertex", "pill_blur_h_fragment", false)?;
        let composite_pipeline = make_pipeline("pill_screen_vertex", "pill_composite_fragment", false)?;

        let make_offscreen = || {
            let desc = TextureDescriptor::new();
            desc.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            desc.set_width((OFFSCREEN_W_PT * scale).round() as u64);
            desc.set_height((OFFSCREEN_H_PT * scale).round() as u64);
            desc.set_usage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            device.new_texture(&desc)
        };
        let offscreen_a = make_offscreen();
        let offscreen_b = make_offscreen();

        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(true);
        layer.set_opaque(false);
        layer.set_contents_scale(f64::from(scale));
        layer.set_drawable_size(CGSize::new(
            f64::from(PANEL_W * scale),
            f64::from(PANEL_H * scale),
        ));

        let atlas = GlyphAtlas::new(&device, scale);

        Ok(Self {
            _device: device,
            queue,
            layer,
            tile_pipeline,
            blur_h_pipeline,
            composite_pipeline,
            offscreen_a,
            offscreen_b,
            atlas,
            scale,
        })
    }
}

#[cfg(target_os = "macos")]
struct PillRuntimeState {
    model: PillModel,
    pipelines: MetalPipelines,
    window: Retained<NSPanel>,
    glass_view: Option<Retained<NSView>>,
    screen_origin_top_left: [f32; 2],
    style: PillStyle,
    last_tick_now: f64,
    dirty: bool,
    ignores_mouse: bool,
    was_mouse_down: bool,
    press_down_elem: Option<ElemId>,
    paste_drag_start_screen: Option<[f32; 2]>,
    action_tx: flume::Sender<PillAction>,
}

// Accessed only on the main thread (guarded by `MainThreadMarker::new()` in `tick_main`).
#[cfg(target_os = "macos")]
unsafe impl Send for PillRuntimeState {}

#[cfg(target_os = "macos")]
struct SharedNativePill {
    state: Mutex<PillRuntimeState>,
    tick_queued: AtomicBool,
    alive: AtomicBool,
}

#[cfg(target_os = "macos")]
pub struct NativePill {
    shared: Arc<SharedNativePill>,
    display_link: Option<CVDisplayLink>,
    display_link_ctx: Option<*const SharedNativePill>,
    action_rx: flume::Receiver<PillAction>,
    window_num: u32,
}

#[cfg(target_os = "macos")]
impl NativePill {
    pub fn open() -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("NativePill::open requires the main thread")?;
        let screens = crate::screens::current();
        // Drawn at the sharpest scale of any display, so moving between them needs no new
        // textures (a 1x display just downsamples).
        let scale = screens
            .iter()
            .map(|s| s.scale as f32)
            .fold(2.0_f32, f32::max);
        let [top_left_x, top_left_y] = panel_origin(&screens, crate::screens::pointer())
            .ok_or("No screen available for NativePill")?;
        let panel_rect = panel_appkit_rect(mtm, [top_left_x, top_left_y]);

        let pipelines = MetalPipelines::new(scale)?;
        let metrics = atlas::measure_metrics();
        let model = PillModel::new(metrics);
        let (action_tx, action_rx) = flume::unbounded();

        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            panel_rect,
            NSWindowStyleMask::Borderless
                | NSWindowStyleMask::UtilityWindow
                | NSWindowStyleMask::NonactivatingPanel
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe {
            let sel = sel!(_setPreventsActivation:);
            let responds: bool = msg_send![&*window, respondsToSelector: sel];
            if responds {
                let _: () = msg_send![&*window, _setPreventsActivation: true];
            }
            window.setReleasedWhenClosed(false);
        }

        let local_bounds = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(f64::from(PANEL_W), f64::from(PANEL_H)),
        );
        let root_view = ScrawlPillView::new(mtm, local_bounds, action_tx.clone());
        root_view.setWantsLayer(true);

        // Optional AppKit Liquid Glass view (`NSGlassEffectView` on macOS 26+).
        let glass_view = create_glass_views(&root_view, local_bounds);

        let metal_view = NSView::initWithFrame(mtm.alloc(), local_bounds);
        metal_view.setWantsLayer(true);
        let cocoa_layer = unsafe { &*pipelines.layer.as_ptr().cast::<CALayer>() };
        cocoa_layer.setOpaque(false);
        metal_view.setLayer(Some(cocoa_layer));
        root_view.addSubview(&metal_view);

        window.setContentView(Some(&root_view));
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setHasShadow(false);
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        window.setIgnoresMouseEvents(true);
        window.setLevel(NSStatusWindowLevel + 5);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::CanJoinAllApplications
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setFrame_display(panel_rect, true);
        window.orderFrontRegardless();
        // Force one initial AppKit display/layout pass while `glass_view` is non-hidden (alpha 0)
        // so `NSGlassEffectView` constructs its `SDFLayer` + `CABackdropLayer` + `CASDFElementLayer`
        // hierarchy on the main thread.
        window.displayIfNeeded();
        if let Some(ref gv) = glass_view {
            gv.setHidden(true);
        }
        let window_num = window.windowNumber().max(0) as u32;

        let shared = Arc::new(SharedNativePill {
            state: Mutex::new(PillRuntimeState {
                model,
                pipelines,
                window: window.clone(),
                glass_view,
                screen_origin_top_left: [top_left_x, top_left_y],
                style: PillStyle::current(),
                last_tick_now: motion::now(),
                dirty: true,
                ignores_mouse: true,
                was_mouse_down: false,
                press_down_elem: None,
                paste_drag_start_screen: None,
                action_tx,
            }),
            tick_queued: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        });

        // Draw initial transparent frame synchronously.
        shared.tick_main();

        let mut display_link = None;
        let mut display_link_ctx = None;
        if let Ok(dl) = CVDisplayLink::from_active_cg_displays() {
            let ctx = Arc::into_raw(shared.clone());
            if unsafe {
                dl.set_output_callback(
                    pill_display_link_cb,
                    ctx.cast_mut().cast::<c_void>(),
                )
            }
            .is_ok()
            {
                let _ = dl.start();
                display_link = Some(dl);
                display_link_ctx = Some(ctx);
            } else {
                unsafe { drop(Arc::from_raw(ctx)) };
            }
        }

        Ok(Self {
            shared,
            display_link,
            display_link_ctx,
            action_rx,
            window_num,
        })
    }

    pub fn window_number(&self) -> u32 {
        self.window_num
    }

    /// Moves the capsule to the screen holding `point` (global points, top-left origin), below
    /// that screen's menu bar and notch. A no-op when it's already there.
    pub fn place_on(&self, point: [f64; 2]) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let screens = crate::screens::current();
        let Some(origin) = panel_origin(&screens, Some(point)) else {
            return;
        };
        let Ok(mut st) = self.shared.state.lock() else {
            return;
        };
        if (st.screen_origin_top_left[0] - origin[0]).abs() < 0.5
            && (st.screen_origin_top_left[1] - origin[1]).abs() < 0.5
        {
            return;
        }
        st.screen_origin_top_left = origin;
        st.window
            .setFrame_display(panel_appkit_rect(mtm, origin), true);
        // On a 1x screen AppKit resets the layer's contentsScale; keep the scale it's drawn at.
        let scale = st.pipelines.scale;
        st.pipelines.layer.set_contents_scale(f64::from(scale));
        st.pipelines.layer.set_drawable_size(CGSize::new(
            f64::from(PANEL_W * scale),
            f64::from(PANEL_H * scale),
        ));
        st.dirty = true;
    }

    /// Synchronizes coordinator state into the native pill model; the next `CVDisplayLink` tick
    /// advances springs and renders on the display refresh cadence (with main-loop fallback if
    /// `CVDisplayLink` is paused).
    pub fn sync(&self, snapshot: PillSnapshot) {
        let mut needs_fallback_tick = self.display_link.is_none();
        if let Ok(mut st) = self.shared.state.lock() {
            if st.style != snapshot.style {
                st.style = snapshot.style;
                st.dirty = true;
            }
            st.model.reduced = snapshot.reduced_motion;
            if st.model.clock != snapshot.clock {
                st.pipelines.atlas.update_clock(&snapshot.clock);
                st.model.set_clock(&snapshot.clock);
                st.dirty = true;
            }
            if let Pres::Completed { ref label } = snapshot.pres {
                let sp = st.pipelines.atlas.update_completed(label);
                st.model.set_completed_label(label.clone(), sp.size_pt[0]);
            }
            if st.model.pres != snapshot.pres {
                let was_hidden = st.model.is_fully_hidden();
                st.model.set_pres(snapshot.pres);
                st.dirty = true;
                if was_hidden {
                    st.window.setLevel(NSStatusWindowLevel + 5);
                    st.window.orderFrontRegardless();
                }
            }
            if (motion::now() - st.last_tick_now) >= f64::from(0.045 * motion::time_scale()) {
                needs_fallback_tick = true;
            }
        }
        if needs_fallback_tick {
            self.shared.tick_main();
        }
    }

    pub fn drain_actions(&self) -> Vec<PillAction> {
        self.action_rx.try_iter().collect()
    }

    pub fn try_recv_action(&self) -> Option<PillAction> {
        self.action_rx.try_recv().ok()
    }

    pub fn is_fully_hidden(&self) -> bool {
        self.shared
            .state
            .lock()
            .map_or(true, |st| st.model.is_fully_hidden())
    }

    pub fn pointer_on_pill(&self) -> bool {
        self.shared
            .state
            .lock()
            .map_or(false, |st| !st.ignores_mouse)
    }
}

#[cfg(target_os = "macos")]
impl Drop for NativePill {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        if let Some(ref dl) = self.display_link {
            let _ = dl.stop();
        }
        if let Ok(st) = self.shared.state.lock() {
            st.window.orderOut(None);
            st.window.close();
        }
        if let Some(ctx) = self.display_link_ctx {
            unsafe { drop(Arc::from_raw(ctx)) };
        }
    }
}



/// The panel's AppKit frame for a top-left origin in global points.
#[cfg(target_os = "macos")]
fn panel_appkit_rect(mtm: MainThreadMarker, [x, y]: [f32; 2]) -> NSRect {
    let primary_h = NSScreen::screens(mtm)
        .firstObject()
        .map_or(900.0, |s| s.frame().size.height);
    let [ax, ay, w, h] = crate::screens::flip(
        [
            f64::from(x),
            f64::from(y),
            f64::from(PANEL_W),
            f64::from(PANEL_H),
        ],
        primary_h,
    );
    NSRect::new(NSPoint::new(ax, ay), NSSize::new(w, h))
}

/// Creates `NSGlassEffectView` dynamically on macOS 26+.
#[cfg(target_os = "macos")]
fn create_glass_views(root_view: &NSView, bounds: NSRect) -> Option<Retained<NSView>> {
    let glass_cls = AnyClass::get(c"NSGlassEffectView")?;
    unsafe {
        let raw_glass: *mut NSView = msg_send![glass_cls, alloc];
        if raw_glass.is_null() {
            return None;
        }
        let init_frame = NSRect::new(
            NSPoint::new((bounds.size.width - 200.0) * 0.5, 22.0),
            NSSize::new(200.0, 40.0),
        );
        let raw_glass: *mut NSView = msg_send![raw_glass, initWithFrame: init_frame];
        let glass_view = Retained::from_raw(raw_glass)?;
        glass_view.setWantsLayer(true);
        glass_view.setAlphaValue(0.0);

        if msg_send![&*glass_view, respondsToSelector: sel!(setStyle:)] {
            // 0 = NSGlassEffectViewStyleRegular (refractive Liquid Glass bottle + specular rim)
            let _: () = msg_send![&*glass_view, setStyle: 0_isize];
        }
        if msg_send![&*glass_view, respondsToSelector: sel!(setCornerRadius:)] {
            let _: () = msg_send![&*glass_view, setCornerRadius: 20.0_f64];
        }
        if msg_send![&*glass_view, respondsToSelector: sel!(setTintColor:)] {
            let tint = NSColor::colorWithSRGBRed_green_blue_alpha(0.06, 0.07, 0.10, 0.22);
            let _: () = msg_send![&*glass_view, setTintColor: Some(&*tint)];
        }

        root_view.addSubview(&glass_view);
        Some(glass_view)
    }
}

extern "C" fn pill_display_link_cb(
    _display_link: CVDisplayLinkRef,
    _now: *const CVTimeStamp,
    _output_time: *const CVTimeStamp,
    _flags_in: CVOptionFlags,
    _flags_out: *mut CVOptionFlags,
    context: *mut c_void,
) -> CVReturn {
    if !context.is_null() {
        let shared = unsafe { &*context.cast::<SharedNativePill>() };
        if shared.alive.load(Ordering::Relaxed)
            && !shared.tick_queued.swap(true, Ordering::AcqRel)
        {
            // Retain an Arc reference for the main-queue callback.
            unsafe {
                Arc::increment_strong_count(context.cast::<SharedNativePill>());
                dispatch_async_f(
                    &_dispatch_main_q,
                    context,
                    pill_main_queue_trampoline,
                );
            }
        }
    }
    0
}

extern "C" fn pill_main_queue_trampoline(context: *mut c_void) {
    let shared = unsafe { Arc::from_raw(context.cast::<SharedNativePill>()) };
    if shared.alive.load(Ordering::Relaxed) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.tick_main();
        }));
    }
    shared.tick_queued.store(false, Ordering::Release);
}

#[cfg(target_os = "macos")]
impl SharedNativePill {
    fn tick_main(&self) {
        if MainThreadMarker::new().is_none() {
            return;
        }
        motion::poll_tune();
        let Ok(mut st) = self.state.lock() else {
            return;
        };

        let live_style = PillStyle::current();
        if st.style != live_style {
            st.style = live_style;
            st.dirty = true;
        }

        // Poll mouse cursor and button state for hover, press squish, click, and Paste drag.
        st.poll_pointer_and_input();

        let now = motion::now();
        let raw_dt = (now - st.last_tick_now).max(0.0) as f32;
        let min_dt = 0.008 * motion::time_scale();
        if raw_dt < min_dt {
            return;
        }
        st.last_tick_now = now;
        let dt = raw_dt.min(0.025 * motion::time_scale());

        let animating = !st.model.is_at_rest()
            || matches!(st.model.pres, Pres::Processing);
        if !animating && !st.dirty {
            if motion::tracing() && !st.model.is_fully_hidden() {
                let frame = st.model.frame();
                st.emit_trace(&frame);
            }
            return;
        }

        if dt > 0.0 {
            st.model.advance(dt);
        }
        st.dirty = false;

        let frame = st.model.frame();
        if motion::tracing() && (!st.model.is_fully_hidden() || animating) {
            st.emit_trace(&frame);
        }
        st.render_frame(&frame);
    }
}

#[cfg(target_os = "macos")]
impl PillRuntimeState {
    fn poll_pointer_and_input(&mut self) {
        if self.model.is_fully_hidden() {
            if !self.ignores_mouse {
                self.ignores_mouse = true;
                self.window.setIgnoresMouseEvents(true);
            }
            return;
        }

        let Some(cls) = AnyClass::get(c"NSEvent") else {
            return;
        };
        let mouse_ns: NSPoint = unsafe { msg_send![cls, mouseLocation] };
        let pressed_mask: usize = unsafe { msg_send![cls, pressedMouseButtons] };
        let mouse_down = (pressed_mask & 1) != 0;

        let win_frame = self.window.frame();
        let local_x = (mouse_ns.x - win_frame.origin.x) as f32;
        let local_y = (win_frame.size.height - (mouse_ns.y - win_frame.origin.y)) as f32;
        let screen_pt = [
            self.screen_origin_top_left[0] + local_x,
            self.screen_origin_top_left[1] + local_y,
        ];

        let inside_island = self.model.hit_island(local_x, local_y);
        let should_ignore = !inside_island && self.press_down_elem.is_none();
        if self.ignores_mouse != should_ignore {
            self.ignores_mouse = should_ignore;
            self.window.setIgnoresMouseEvents(should_ignore);
        }

        let prev_hover = self.model.hovered;
        let prev_press = self.model.pressed;
        let pos_opt = inside_island.then_some([local_x, local_y]);
        self.model.set_pointer(pos_opt, mouse_down);
        if self.model.hovered != prev_hover || self.model.pressed != prev_press {
            self.dirty = true;
        }

        if mouse_down && !self.was_mouse_down {
            let hit = pos_opt.and_then(|[x, y]| self.model.hit_element(x, y));
            self.press_down_elem = hit;
            if hit == Some(ElemId::Paste) {
                self.paste_drag_start_screen = Some(screen_pt);
            } else {
                self.paste_drag_start_screen = None;
            }
        } else if !mouse_down && self.was_mouse_down {
            if let Some(down_elem) = self.press_down_elem.take() {
                let up_elem = pos_opt.and_then(|[x, y]| self.model.hit_element(x, y));
                if down_elem == ElemId::Paste {
                    if up_elem == Some(ElemId::Paste) {
                        let _ = self.action_tx.try_send(PillAction::Paste(None));
                    } else if let Some(start) = self.paste_drag_start_screen.take() {
                        let dist = (screen_pt[0] - start[0]).hypot(screen_pt[1] - start[1]);
                        if dist >= 24.0 {
                            let _ = self.action_tx.try_send(PillAction::Paste(Some(screen_pt)));
                        }
                    }
                } else if up_elem == Some(down_elem) {
                    self.dispatch_click(down_elem);
                }
            }
            self.paste_drag_start_screen = None;
        }
        self.was_mouse_down = mouse_down;
    }

    fn dispatch_click(&self, elem: ElemId) {
        let is_selecting = matches!(self.model.pres, Pres::Selecting { .. });
        let action = match elem {
            ElemId::ChipArea => {
                if is_selecting {
                    PillAction::SelectMode(SelectionMode::Area)
                } else {
                    PillAction::SwitchRecording(SelectionMode::Area)
                }
            }
            ElemId::ChipWindow => {
                if is_selecting {
                    PillAction::SelectMode(SelectionMode::Window)
                } else {
                    PillAction::SwitchRecording(SelectionMode::Window)
                }
            }
            ElemId::ChipScreen => {
                if is_selecting {
                    PillAction::SelectMode(SelectionMode::Screen)
                } else {
                    PillAction::SwitchRecording(SelectionMode::Screen)
                }
            }
            ElemId::ChipIphone => {
                if is_selecting {
                    PillAction::SelectMode(SelectionMode::Iphone)
                } else {
                    PillAction::SwitchRecording(SelectionMode::Iphone)
                }
            }
            ElemId::Record => PillAction::RecordSelected,
            ElemId::Undo => PillAction::Undo,
            ElemId::Cancel => PillAction::Cancel,
            ElemId::Finish => PillAction::Finish,
            ElemId::Close => {
                if matches!(self.model.pres, Pres::Completed { .. }) {
                    PillAction::DismissCompleted
                } else {
                    PillAction::Cancel
                }
            }
            ElemId::Paste => PillAction::Paste(None),
            ElemId::Inspect => PillAction::Inspect,
            _ => return,
        };
        let _ = self.action_tx.try_send(action);
    }

    fn emit_trace(&self, frame: &FrameState) {
        motion::trace_frame();
        let [ox, oy] = self.screen_origin_top_left;
        let win = [ox, oy, PANEL_W, PANEL_H];
        let sh = &frame.shape;
        let shell = [ox + sh.l, oy + sh.t, sh.width(), sh.height()];
        motion::trace_element("window", win, 0.0, 0.0, None, None);
        motion::trace_element("shell", shell, sh.alpha, sh.radius, None, None);
        motion::trace_channel("content.blur", "level", frame.content_blur);
        motion::trace_channel("content.alpha", "level", frame.content_alpha);
        motion::trace_channel(
            "shell.width",
            "position",
            (self.model.r.x - self.model.l.x).max(0.0),
        );

        for elem in &frame.elems {
            let [ex, ey, ew, eh] = elem.rect;
            let screen_rect = if matches!(elem.id, ElemId::DividerLead | ElemId::DividerClose) {
                let dw = 1.0 * elem.scale;
                let dh = 16.0 * elem.scale;
                [
                    ox + ex + (ew - dw) * 0.5,
                    oy + ey + (eh - dh) * 0.5,
                    dw,
                    dh,
                ]
            } else {
                [ox + ex, oy + ey, ew, eh]
            };
            motion::trace_element(
                elem.id.trace_key(),
                screen_rect,
                elem.alpha,
                0.0,
                Some("row"),
                Some(shell),
            );
        }
    }

    fn render_frame(&mut self, frame: &FrameState) {
        let use_glass = self.style == PillStyle::Glass && self.glass_view.is_some();

        let mut prims: Vec<TilePrim> = Vec::with_capacity(frame.elems.len() * 3);
        let mut blur_boxes: Vec<BlurBox> = Vec::with_capacity(frame.elems.len());
        let mut comp_elems: Vec<CompositeElem> = Vec::with_capacity(frame.elems.len());

        for (i, elem) in frame.elems.iter().take(16).enumerate() {
            let slot_x = (i % 5) as f32 * 192.0 + 4.0;
            let slot_y = (i / 5) as f32 * 112.0 + 4.0;
            let tile_w = (elem.base_size[0] + 2.0 * TILE_PAD_PT).min(184.0);
            let tile_h = (elem.base_size[1] + 2.0 * TILE_PAD_PT).min(104.0);
            let tile_rect = [slot_x, slot_y, tile_w, tile_h];

            self.emit_elem_prims(
                elem,
                [slot_x + TILE_PAD_PT, slot_y + TILE_PAD_PT],
                frame.t,
                &mut prims,
            );

            blur_boxes.push(BlurBox {
                tile_rect,
                sigma_pt: elem.blur,
                _pad0: 0.0,
                _pad1: [0.0; 2],
            });

            comp_elems.push(CompositeElem {
                screen_rect: elem.rect,
                tile_rect,
                base_size: elem.base_size,
                scale: elem.scale,
                alpha: elem.alpha,
                sigma_pt: elem.blur,
                pad_pt: TILE_PAD_PT,
                _pad0: [0.0; 2],
            });
        }

        // Optional "Esc" hint pill when hovering Cancel or Close in Selecting / Recording.
        let mut hint_rect = [0.0_f32; 4];
        let mut hint_uv = [0.0_f32; 4];
        if !matches!(self.model.pres, Pres::Completed { .. })
            && let Some(hov_id @ (ElemId::Cancel | ElemId::Close)) = self.model.hovered
            && let Some(hov_elem) = frame.elems.iter().find(|e| e.id == hov_id && e.alpha > 0.6)
        {
            let esc_sp = self.pipelines.atlas.label("Esc");
            let hw = 30.0_f32;
            let hh = 18.0_f32;
            let hx = hov_elem.rect[0] + (hov_elem.rect[2] - hw) * 0.5;
            let hy = (frame.shape.b + 4.0).min(PANEL_H - hh - 1.0);
            hint_rect = [hx, hy, hw, hh];
            hint_uv = esc_sp.uv;
        }

        let (has_ghost, ghost_rect, ghost_meta) = if let Some(ref g) = frame.ghost {
            (
                1_u32,
                [g.l, g.t, g.r, g.b],
                [g.radius, g.alpha, 0.0, 0.0],
            )
        } else {
            (0_u32, [0.0; 4], [0.0; 4])
        };

        let tile_uniforms = TileUniforms {
            atlas_size: [1024.0, 512.0],
            canvas_size: [OFFSCREEN_W_PT, OFFSCREEN_H_PT],
            scale: self.pipelines.scale,
            _pad0: 0.0,
            _pad1: [0.0; 2],
        };

        let uniforms = PillUniforms {
            panel_size: [PANEL_W, PANEL_H],
            canvas_size: [OFFSCREEN_W_PT, OFFSCREEN_H_PT],
            scale: self.pipelines.scale,
            elem_count: comp_elems.len() as u32,
            is_completed: u32::from(frame.is_completed),
            has_ghost,
            shape_rect: [
                frame.shape.l,
                frame.shape.t,
                frame.shape.r,
                frame.shape.b,
            ],
            shape_meta: [
                frame.shape.radius,
                frame.shape.alpha,
                frame.shape.blur,
                if use_glass { 1.0 } else { 0.0 },
            ],
            ghost_rect,
            ghost_meta,
            hint_rect,
            hint_uv,
        };

        let prim_count = prims.len() as u64;
        let box_count = blur_boxes.len() as u64;
        if prims.is_empty() {
            prims.push(TilePrim::default());
        }
        if blur_boxes.is_empty() {
            blur_boxes.push(BlurBox::default());
        }
        if comp_elems.is_empty() {
            comp_elems.push(CompositeElem::default());
        }

        let Some(drawable) = self.pipelines.layer.next_drawable() else {
            return;
        };

        let cmd = self.pipelines.queue.new_command_buffer();

        // Pass 1: Render crisp unblurred element primitives into `offscreen_a`.
        {
            let pass = RenderPassDescriptor::new();
            let att = pass.color_attachments().object_at(0).unwrap();
            att.set_texture(Some(&self.pipelines.offscreen_a));
            att.set_load_action(MTLLoadAction::Clear);
            att.set_clear_color(metal::MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
            att.set_store_action(MTLStoreAction::Store);
            let enc = cmd.new_render_command_encoder(&pass);
            if prim_count > 0 {
                enc.set_render_pipeline_state(&self.pipelines.tile_pipeline);
                enc.set_vertex_bytes(
                    0,
                    size_of::<TileUniforms>() as u64,
                    (&raw const tile_uniforms).cast::<c_void>(),
                );
                enc.set_vertex_bytes(
                    1,
                    (prims.len() * size_of::<TilePrim>()) as u64,
                    prims.as_ptr().cast::<c_void>(),
                );
                enc.set_fragment_bytes(
                    0,
                    size_of::<TileUniforms>() as u64,
                    (&raw const tile_uniforms).cast::<c_void>(),
                );
                enc.set_fragment_texture(0, Some(&self.pipelines.atlas.texture));
                enc.draw_primitives_instanced(MTLPrimitiveType::Triangle, 0, 6, prim_count);
            }
            enc.end_encoding();
        }

        // Pass 2: 1D Horizontal Gaussian blur from `offscreen_a` into `offscreen_b`.
        {
            let pass = RenderPassDescriptor::new();
            let att = pass.color_attachments().object_at(0).unwrap();
            att.set_texture(Some(&self.pipelines.offscreen_b));
            att.set_load_action(MTLLoadAction::Clear);
            att.set_clear_color(metal::MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
            att.set_store_action(MTLStoreAction::Store);
            let enc = cmd.new_render_command_encoder(&pass);
            if box_count > 0 {
                enc.set_render_pipeline_state(&self.pipelines.blur_h_pipeline);
                enc.set_vertex_bytes(
                    0,
                    size_of::<TileUniforms>() as u64,
                    (&raw const tile_uniforms).cast::<c_void>(),
                );
                enc.set_vertex_bytes(
                    1,
                    (blur_boxes.len() * size_of::<BlurBox>()) as u64,
                    blur_boxes.as_ptr().cast::<c_void>(),
                );
                enc.set_fragment_bytes(
                    0,
                    size_of::<TileUniforms>() as u64,
                    (&raw const tile_uniforms).cast::<c_void>(),
                );
                enc.set_fragment_texture(0, Some(&self.pipelines.offscreen_a));
                enc.draw_primitives_instanced(MTLPrimitiveType::Triangle, 0, 6, box_count);
            }
            enc.end_encoding();
        }

        // Pass 3: Capsule SDF silhouette + 1D Vertical Gaussian blur + hairline rim into drawable.
        {
            let pass = RenderPassDescriptor::new();
            let att = pass.color_attachments().object_at(0).unwrap();
            att.set_texture(Some(drawable.texture()));
            att.set_load_action(MTLLoadAction::Clear);
            att.set_clear_color(metal::MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
            att.set_store_action(MTLStoreAction::Store);
            let enc = cmd.new_render_command_encoder(&pass);
            enc.set_render_pipeline_state(&self.pipelines.composite_pipeline);
            enc.set_fragment_bytes(
                0,
                size_of::<PillUniforms>() as u64,
                (&raw const uniforms).cast::<c_void>(),
            );
            enc.set_fragment_bytes(
                1,
                (comp_elems.len() * size_of::<CompositeElem>()) as u64,
                comp_elems.as_ptr().cast::<c_void>(),
            );
            enc.set_fragment_texture(0, Some(&self.pipelines.offscreen_b));
            enc.set_fragment_texture(1, Some(&self.pipelines.atlas.texture));
            enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
            enc.end_encoding();
        }

        // Synchronize `NSGlassEffectView` geometry and Metal drawable presentation in one CATransaction.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if let Some(ref gv) = self.glass_view {
            if use_glass && frame.shape.alpha > 0.005 {
                gv.setHidden(false);
                let gx = f64::from(frame.shape.l);
                let gy = f64::from(PANEL_H - frame.shape.b);
                let gw = f64::from(frame.shape.width());
                let gh = f64::from(frame.shape.height());
                gv.setFrame(NSRect::new(NSPoint::new(gx, gy), NSSize::new(gw, gh)));
                gv.setAlphaValue(f64::from(frame.shape.alpha));
                unsafe {
                    if msg_send![&**gv, respondsToSelector: sel!(setCornerRadius:)] {
                        let _: () =
                            msg_send![&**gv, setCornerRadius: f64::from(frame.shape.radius)];
                    }
                    if msg_send![&**gv, respondsToSelector: sel!(setTintColor:)] {
                        let tint = if frame.is_completed {
                            NSColor::colorWithSRGBRed_green_blue_alpha(0.04, 0.14, 0.09, 0.28)
                        } else {
                            NSColor::colorWithSRGBRed_green_blue_alpha(0.06, 0.07, 0.10, 0.22)
                        };
                        let _: () = msg_send![&**gv, setTintColor: Some(&*tint)];
                    }
                }
                gv.setNeedsLayout(true);
                gv.layoutSubtreeIfNeeded();
            } else {
                gv.setHidden(true);
            }
        }
        cmd.commit();
        cmd.wait_until_scheduled();
        drawable.present();
        CATransaction::commit();
    }

    fn emit_elem_prims(
        &self,
        elem: &model::FrameElem,
        pt: [f32; 2],
        t: f32,
        out: &mut Vec<TilePrim>,
    ) {
        let is_glass = self.style == PillStyle::Glass && self.glass_view.is_some();
        emit_elem_prims(&self.pipelines.atlas, is_glass, elem, pt, t, out);
    }
}

// ─── Linux X11 + wgpu Implementation ───────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use x11rb::connection::Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ColormapAlloc, ConfigureWindowAux, CreateWindowAux, EventMask,
    PropMode, Rectangle, WindowClass,
};
#[cfg(target_os = "linux")]
use x11rb::protocol::shape;
#[cfg(target_os = "linux")]
use x11rb::protocol::Event;

#[cfg(target_os = "linux")]
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(display_name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn XCloseDisplay(display: *mut std::ffi::c_void) -> std::ffi::c_int;
}

#[cfg(target_os = "linux")]
struct XlibDisplayGuard(*mut std::ffi::c_void);

#[cfg(target_os = "linux")]
impl Drop for XlibDisplayGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                XCloseDisplay(self.0);
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct XlibWindowSource {
    window: std::ffi::c_ulong,
    display: *mut std::ffi::c_void,
    screen: i32,
}

#[cfg(target_os = "linux")]
impl raw_window_handle::HasWindowHandle for XlibWindowSource {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let handle = raw_window_handle::XlibWindowHandle::new(self.window);
        unsafe {
            Ok(raw_window_handle::WindowHandle::borrow_raw(
                raw_window_handle::RawWindowHandle::Xlib(handle),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
impl raw_window_handle::HasDisplayHandle for XlibWindowSource {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let display_ptr = std::ptr::NonNull::new(self.display)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = raw_window_handle::XlibDisplayHandle::new(Some(display_ptr), self.screen);
        unsafe {
            Ok(raw_window_handle::DisplayHandle::borrow_raw(
                raw_window_handle::RawDisplayHandle::Xlib(handle),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn block_on<F: std::future::Future>(mut future: F) -> F::Output {
    unsafe fn noop_clone(_: *const ()) -> std::task::RawWaker {
        std::task::RawWaker::new(std::ptr::null(), &NOOP_VTABLE)
    }
    unsafe fn noop(_: *const ()) {}
    static NOOP_VTABLE: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(noop_clone, noop, noop, noop);

    let waker = unsafe {
        std::task::Waker::from_raw(std::task::RawWaker::new(std::ptr::null(), &NOOP_VTABLE))
    };
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = unsafe { std::pin::Pin::new_unchecked(&mut future) };
    loop {
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(res) => return res,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[cfg(target_os = "linux")]
fn as_u8_slice<T: Copy>(slice: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            slice.as_ptr().cast::<u8>(),
            std::mem::size_of_val(slice),
        )
    }
}

#[cfg(target_os = "linux")]
fn as_u8_val<T: Copy>(val: &T) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            (val as *const T).cast::<u8>(),
            std::mem::size_of::<T>(),
        )
    }
}

#[cfg(target_os = "linux")]
struct WgpuPipelines {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    scale: f32,
    atlas: atlas::GlyphAtlas,
    tile_pipeline: wgpu::RenderPipeline,
    blur_h_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,
    tile_bind_group_layout: wgpu::BindGroupLayout,
    blur_h_bind_group_layout: wgpu::BindGroupLayout,
    composite_bind_group_layout: wgpu::BindGroupLayout,
    tile_uniforms_buf: wgpu::Buffer,
    tile_prims_buf: wgpu::Buffer,
    blur_boxes_buf: wgpu::Buffer,
    pill_uniforms_buf: wgpu::Buffer,
    comp_elems_buf: wgpu::Buffer,
    offscreen_a: wgpu::Texture,
    offscreen_a_view: wgpu::TextureView,
    offscreen_b: wgpu::Texture,
    offscreen_b_view: wgpu::TextureView,
    linear_sampler: wgpu::Sampler,
}

#[cfg(target_os = "linux")]
impl WgpuPipelines {
    fn new(
        device: &Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        scale: f32,
        surface_format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pill_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("pill.wgsl").into()),
        });

        let offscreen_w = (OFFSCREEN_W_PT * scale).round() as u32;
        let offscreen_h = (OFFSCREEN_H_PT * scale).round() as u32;

        let make_offscreen = |label: &'static str| {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: offscreen_w,
                    height: offscreen_h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            (tex, view)
        };
        let (offscreen_a, offscreen_a_view) = make_offscreen("pill_offscreen_a");
        let (offscreen_b, offscreen_b_view) = make_offscreen("pill_offscreen_b");

        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pill_linear_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let tile_uniforms_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tile_uniforms_buf"),
            size: std::mem::size_of::<TileUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tile_prims_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tile_prims_buf"),
            size: (128 * std::mem::size_of::<TilePrim>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blur_boxes_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blur_boxes_buf"),
            size: (16 * std::mem::size_of::<BlurBox>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let pill_uniforms_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pill_uniforms_buf"),
            size: std::mem::size_of::<PillUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let comp_elems_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("comp_elems_buf"),
            size: (16 * std::mem::size_of::<CompositeElem>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let tile_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tile_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let blur_h_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blur_h_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 6,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let composite_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let tile_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tile_pipeline_layout"),
            bind_group_layouts: &[&tile_bind_group_layout],
            push_constant_ranges: &[],
        });
        let tile_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tile_pipeline"),
            layout: Some(&tile_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("pill_tile_vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("pill_tile_fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let blur_h_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blur_h_pipeline_layout"),
            bind_group_layouts: &[&blur_h_bind_group_layout],
            push_constant_ranges: &[],
        });
        let blur_h_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blur_h_pipeline"),
            layout: Some(&blur_h_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("pill_blur_h_vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("pill_blur_h_fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let composite_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite_pipeline_layout"),
            bind_group_layouts: &[&composite_bind_group_layout],
            push_constant_ranges: &[],
        });
        let composite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("composite_pipeline"),
            layout: Some(&composite_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("pill_screen_vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("pill_composite_fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let atlas = atlas::GlyphAtlas::new(device, Arc::clone(&queue), scale);

        Ok(Self {
            device: Arc::clone(device),
            queue,
            scale,
            atlas,
            tile_pipeline,
            blur_h_pipeline,
            composite_pipeline,
            tile_bind_group_layout,
            blur_h_bind_group_layout,
            composite_bind_group_layout,
            tile_uniforms_buf,
            tile_prims_buf,
            blur_boxes_buf,
            pill_uniforms_buf,
            comp_elems_buf,
            offscreen_a,
            offscreen_a_view,
            offscreen_b,
            offscreen_b_view,
            linear_sampler,
        })
    }

    fn render(
        &mut self,
        surface: &wgpu::Surface<'_>,
        frame: &model::FrameState,
        model: &model::PillModel,
        style: PillStyle,
    ) -> Result<(), String> {
        let is_glass = style == PillStyle::Glass;

        let mut prims: Vec<TilePrim> = Vec::with_capacity(frame.elems.len() * 3);
        let mut blur_boxes: Vec<BlurBox> = Vec::with_capacity(frame.elems.len());
        let mut comp_elems: Vec<CompositeElem> = Vec::with_capacity(frame.elems.len());

        for (i, elem) in frame.elems.iter().take(16).enumerate() {
            let slot_x = (i % 5) as f32 * 192.0 + 4.0;
            let slot_y = (i / 5) as f32 * 112.0 + 4.0;
            let tile_w = (elem.base_size[0] + 2.0 * TILE_PAD_PT).min(184.0);
            let tile_h = (elem.base_size[1] + 2.0 * TILE_PAD_PT).min(104.0);
            let tile_rect = [slot_x, slot_y, tile_w, tile_h];

            emit_elem_prims(
                &self.atlas,
                is_glass,
                elem,
                [slot_x + TILE_PAD_PT, slot_y + TILE_PAD_PT],
                frame.t,
                &mut prims,
            );

            blur_boxes.push(BlurBox {
                tile_rect,
                sigma_pt: elem.blur,
                _pad0: 0.0,
                _pad1: [0.0; 2],
            });

            comp_elems.push(CompositeElem {
                screen_rect: elem.rect,
                tile_rect,
                base_size: elem.base_size,
                scale: elem.scale,
                alpha: elem.alpha,
                sigma_pt: elem.blur,
                pad_pt: TILE_PAD_PT,
                _pad0: [0.0; 2],
            });
        }

        let tile_u = TileUniforms {
            atlas_size: [atlas::ATLAS_W as f32, atlas::ATLAS_H as f32],
            canvas_size: [OFFSCREEN_W_PT, OFFSCREEN_H_PT],
            scale: self.scale,
            _pad0: 0.0,
            _pad1: [0.0; 2],
        };
        self.queue.write_buffer(&self.tile_uniforms_buf, 0, as_u8_val(&tile_u));

        if !prims.is_empty() {
            self.queue.write_buffer(&self.tile_prims_buf, 0, as_u8_slice(&prims));
        }
        if !blur_boxes.is_empty() {
            self.queue.write_buffer(&self.blur_boxes_buf, 0, as_u8_slice(&blur_boxes));
        }
        if !comp_elems.is_empty() {
            self.queue.write_buffer(&self.comp_elems_buf, 0, as_u8_slice(&comp_elems));
        }

        let mut hint_rect = [0.0_f32; 4];
        let mut hint_uv = [0.0_f32; 4];
        if !matches!(model.pres, Pres::Completed { .. })
            && let Some(hov_id @ (ElemId::Cancel | ElemId::Close)) = model.hovered
            && let Some(hov_elem) = frame.elems.iter().find(|e| e.id == hov_id && e.alpha > 0.6)
        {
            let esc_sp = self.atlas.label("Esc");
            let hw = 30.0_f32;
            let hh = 18.0_f32;
            let hx = hov_elem.rect[0] + (hov_elem.rect[2] - hw) * 0.5;
            let hy = (frame.shape.b + 4.0).min(PANEL_H - hh - 1.0);
            hint_rect = [hx, hy, hw, hh];
            hint_uv = esc_sp.uv;
        }
        let pill_u = PillUniforms {
            panel_size: [PANEL_W, PANEL_H],
            canvas_size: [OFFSCREEN_W_PT, OFFSCREEN_H_PT],
            scale: self.scale,
            elem_count: comp_elems.len() as u32,
            is_completed: if frame.is_completed { 1 } else { 0 },
            has_ghost: if frame.ghost.is_some() { 1 } else { 0 },
            shape_rect: [
                frame.shape.l,
                frame.shape.t,
                frame.shape.r,
                frame.shape.b,
            ],
            shape_meta: [
                frame.shape.radius,
                frame.shape.alpha,
                frame.shape.blur,
                if is_glass { 1.0 } else { 0.0 },
            ],
            ghost_rect: frame.ghost.map_or([0.0; 4], |g| [g.l, g.t, g.r, g.b]),
            ghost_meta: frame.ghost.map_or([0.0; 4], |g| [g.radius, g.alpha, 0.0, 0.0]),
            hint_rect,
            hint_uv,
        };
        self.queue.write_buffer(&self.pill_uniforms_buf, 0, as_u8_val(&pill_u));

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pill_frame_encoder"),
        });

        // Pass 1: Tiles
        {
            let tile_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tile_bind_group"),
                layout: &self.tile_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.tile_uniforms_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: self.tile_prims_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&self.atlas.texture_view) },
                    wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(&self.linear_sampler) },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tile_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.offscreen_a_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.tile_pipeline);
            pass.set_bind_group(0, &tile_bind_group, &[]);
            pass.draw(0..6, 0..prims.len() as u32);
        }

        // Pass 2: Horizontal Blur
        {
            let blur_h_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("blur_h_bind_group"),
                layout: &self.blur_h_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.tile_uniforms_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: self.blur_boxes_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&self.offscreen_a_view) },
                    wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(&self.linear_sampler) },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blur_h_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.offscreen_b_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.blur_h_pipeline);
            pass.set_bind_group(0, &blur_h_bind_group, &[]);
            pass.draw(0..6, 0..blur_boxes.len() as u32);
        }

        // Pass 3: Composite to Surface
        let surface_tex = surface.get_current_texture().map_err(|e| format!("get_current_texture failed: {e}"))?;
        let surface_view = surface_tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        {
            let composite_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("composite_bind_group"),
                layout: &self.composite_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 3, resource: self.pill_uniforms_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 4, resource: self.comp_elems_buf.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&self.offscreen_b_view) },
                    wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&self.atlas.texture_view) },
                    wgpu::BindGroupEntry { binding: 8, resource: wgpu::BindingResource::Sampler(&self.linear_sampler) },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.composite_pipeline);
            pass.set_bind_group(0, &composite_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        surface_tex.present();

        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct LinuxPillSharedState {
    snapshot: PillSnapshot,
    snapshot_changed: bool,
    origin: [f32; 2],
    origin_changed: bool,
    dirty: bool,
}

#[cfg(target_os = "linux")]
pub struct NativePill {
    conn: Arc<x11rb::rust_connection::RustConnection>,
    window: u32,
    shared: Arc<Mutex<LinuxPillSharedState>>,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    action_rx: flume::Receiver<PillAction>,
    pointer_on_pill: Arc<AtomicBool>,
    is_hidden: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl NativePill {
    pub fn open() -> Result<Self, String> {
        let screens = crate::screens::current();
        let scale = screens
            .iter()
            .map(|s| s.scale as f32)
            .fold(1.0_f32, f32::max);
        let [top_left_x, top_left_y] = panel_origin(&screens, crate::screens::pointer())
            .ok_or("No screen available for NativePill")?;

        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| format!("X11 connect failed: {e}"))?;
        let conn = Arc::new(conn);

        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;

        let mut depth = screen.root_depth;
        let mut visual_id = screen.root_visual;
        let mut colormap = 0u32;

        for d in &screen.allowed_depths {
            if d.depth == 32 {
                for vis in &d.visuals {
                    depth = 32;
                    visual_id = vis.visual_id;
                    if let Ok(cm) = conn.generate_id() {
                        if conn
                            .create_colormap(
                                ColormapAlloc::NONE,
                                cm,
                                root,
                                visual_id,
                            )
                            .is_ok()
                        {
                            colormap = cm;
                        }
                    }
                    break;
                }
            }
        }

        let window = conn.generate_id().map_err(|e| format!("generate_id failed: {e}"))?;
        let win_w = (PANEL_W * scale).max(1.0) as u16;
        let win_h = (PANEL_H * scale).max(1.0) as u16;

        let mut values = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::POINTER_MOTION
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::LEAVE_WINDOW,
            );
        if colormap != 0 {
            values = values.colormap(colormap);
        }

        conn.create_window(
            depth,
            window,
            root,
            top_left_x as i16,
            top_left_y as i16,
            win_w,
            win_h,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &values,
        )
        .map_err(|e| format!("create_window failed: {e}"))?;

        // EWMH atoms: _NET_WM_WINDOW_TYPE = DOCK / UTILITY, _NET_WM_STATE = ABOVE, STICKY
        let net_wm_type = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);
        let net_wm_type_dock = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_DOCK")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);
        let net_wm_type_utility = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_UTILITY")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);
        let net_wm_state = conn
            .intern_atom(false, b"_NET_WM_STATE")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);
        let net_wm_state_above = conn
            .intern_atom(false, b"_NET_WM_STATE_ABOVE")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);
        let net_wm_state_sticky = conn
            .intern_atom(false, b"_NET_WM_STATE_STICKY")
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.atom);

        if let (Some(prop), Some(dock)) = (net_wm_type, net_wm_type_dock) {
            let mut types = vec![dock];
            if let Some(util) = net_wm_type_utility {
                types.push(util);
            }
            let _ = conn.change_property32(
                PropMode::REPLACE,
                window,
                prop,
                AtomEnum::ATOM,
                &types,
            );
        }

        if let (Some(prop), Some(above)) = (net_wm_state, net_wm_state_above) {
            let mut states = vec![above];
            if let Some(sticky) = net_wm_state_sticky {
                states.push(sticky);
            }
            let _ = conn.change_property32(
                PropMode::REPLACE,
                window,
                prop,
                AtomEnum::ATOM,
                &states,
            );
        }

        let _ = conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Scrawl Dynamic Island",
        );

        // Initial input shape: empty (click-through until hovered)
        let _ = shape::ConnectionExt::shape_rectangles(
            &*conn,
            shape::So::SET,
            shape::Sk::INPUT,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        );

        conn.map_window(window).map_err(|e| format!("map_window failed: {e}"))?;
        conn.flush().map_err(|e| format!("flush failed: {e}"))?;

        let (action_tx, action_rx) = flume::unbounded();
        let running = Arc::new(AtomicBool::new(true));
        let pointer_on_pill = Arc::new(AtomicBool::new(false));
        let is_hidden = Arc::new(AtomicBool::new(true));

        let shared = Arc::new(Mutex::new(LinuxPillSharedState {
            snapshot: PillSnapshot {
                pres: Pres::Hidden,
                clock: "0:00".into(),
                reduced_motion: false,
                style: PillStyle::current(),
            },
            snapshot_changed: false,
            origin: [top_left_x, top_left_y],
            origin_changed: false,
            dirty: true,
        }));

        let thread_conn = Arc::clone(&conn);
        let thread_running = Arc::clone(&running);
        let thread_shared = Arc::clone(&shared);
        let thread_action_tx = action_tx;
        let thread_pointer_on_pill = Arc::clone(&pointer_on_pill);
        let thread_is_hidden = Arc::clone(&is_hidden);

        let thread = std::thread::Builder::new()
            .name("scrawl-hud-pill".into())
            .spawn(move || {
                run_linux_pill_thread(
                    thread_conn,
                    window,
                    screen_num as i32,
                    scale,
                    [top_left_x, top_left_y],
                    thread_shared,
                    thread_action_tx,
                    thread_running,
                    thread_pointer_on_pill,
                    thread_is_hidden,
                );
            })
            .ok();

        Ok(Self {
            conn,
            window,
            shared,
            running,
            thread,
            action_rx,
            pointer_on_pill,
            is_hidden,
        })
    }

    pub fn window_number(&self) -> u32 {
        self.window
    }

    pub fn place_on(&self, point: [f64; 2]) {
        let screens = crate::screens::current();
        let Some(origin) = panel_origin(&screens, Some(point)) else {
            return;
        };
        if let Ok(mut st) = self.shared.lock() {
            if (st.origin[0] - origin[0]).abs() < 0.5 && (st.origin[1] - origin[1]).abs() < 0.5 {
                return;
            }
            st.origin = origin;
            st.origin_changed = true;
            st.dirty = true;
        }
    }

    pub fn sync(&self, snapshot: PillSnapshot) {
        if let Ok(mut st) = self.shared.lock() {
            st.snapshot = snapshot;
            st.snapshot_changed = true;
            st.dirty = true;
        }
    }

    pub fn drain_actions(&self) -> Vec<PillAction> {
        self.action_rx.try_iter().collect()
    }

    pub fn try_recv_action(&self) -> Option<PillAction> {
        self.action_rx.try_recv().ok()
    }

    pub fn is_fully_hidden(&self) -> bool {
        self.is_hidden.load(Ordering::Acquire)
    }

    pub fn pointer_on_pill(&self) -> bool {
        self.pointer_on_pill.load(Ordering::Acquire)
    }
}

#[cfg(target_os = "linux")]
impl Drop for NativePill {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(th) = self.thread.take() {
            let _ = th.join();
        }
        let _ = self.conn.unmap_window(self.window);
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

#[cfg(target_os = "linux")]
fn run_linux_pill_thread(
    conn: Arc<x11rb::rust_connection::RustConnection>,
    window: u32,
    screen_num: i32,
    scale: f32,
    initial_origin: [f32; 2],
    shared: Arc<Mutex<LinuxPillSharedState>>,
    action_tx: flume::Sender<PillAction>,
    running: Arc<AtomicBool>,
    pointer_on_pill_shared: Arc<AtomicBool>,
    is_hidden_shared: Arc<AtomicBool>,
) {
    let xlib_display = unsafe { XOpenDisplay(std::ptr::null()) };
    if xlib_display.is_null() {
        return;
    }
    let _display_guard = XlibDisplayGuard(xlib_display);

    let source = XlibWindowSource {
        window: window as std::ffi::c_ulong,
        display: xlib_display,
        screen: screen_num,
    };

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        flags: wgpu::InstanceFlags::default(),
        dx12_shader_compiler: Default::default(),
        gles_minor_version: Default::default(),
    });

    let surface = match instance.create_surface(&source) {
        Ok(s) => s,
        Err(_) => {
            let Some(display_ptr) = std::ptr::NonNull::new(xlib_display) else {
                return;
            };
            let rwh = raw_window_handle::RawWindowHandle::Xlib(
                raw_window_handle::XlibWindowHandle::new(window as std::ffi::c_ulong),
            );
            let rdh = raw_window_handle::RawDisplayHandle::Xlib(
                raw_window_handle::XlibDisplayHandle::new(Some(display_ptr), screen_num),
            );
            let target = wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: rdh,
                raw_window_handle: rwh,
            };
            match unsafe { instance.create_surface_unsafe(target) } {
                Ok(s) => s,
                Err(_) => return,
            }
        }
    };

    let adapter = match block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    })) {
        Some(a) => a,
        None => return,
    };

    let (device, queue) = match block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("pill_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: Default::default(),
        },
        None,
    )) {
        Ok(pair) => pair,
        Err(_) => return,
    };

    let device = Arc::new(device);
    let queue = Arc::new(queue);

    let surface_caps = surface.get_capabilities(&adapter);
    let alpha_mode = if surface_caps
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else if surface_caps
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
    {
        wgpu::CompositeAlphaMode::PostMultiplied
    } else {
        wgpu::CompositeAlphaMode::Auto
    };

    let format = surface_caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(surface_caps.formats[0]);

    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: (PANEL_W * scale).max(1.0) as u32,
        height: (PANEL_H * scale).max(1.0) as u32,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let mut pipelines = match WgpuPipelines::new(&device, Arc::clone(&queue), scale, format) {
        Ok(p) => p,
        Err(_) => return,
    };

    let mut model = PillModel::new(atlas::measure_metrics());
    let mut last_tick = motion::now();
    let mut last_shape_rect: Option<[i16; 4]> = None;
    let mut last_ignores_mouse = true;
    let mut press_down_elem: Option<ElemId> = None;
    let mut paste_drag_start_screen: Option<[f32; 2]> = None;
    let mut mouse_down = false;
    let mut mouse_pos = [CX, TOP];
    let mut current_origin = initial_origin;
    let mut current_style = PillStyle::current();

    while running.load(Ordering::Acquire) {
        // 1. Sync state from shared
        let (snapshot_opt, origin_opt, dirty) = if let Ok(mut st) = shared.lock() {
            let snap = if st.snapshot_changed {
                st.snapshot_changed = false;
                Some(st.snapshot.clone())
            } else {
                None
            };
            let orig = if st.origin_changed {
                st.origin_changed = false;
                Some(st.origin)
            } else {
                None
            };
            let d = st.dirty;
            st.dirty = false;
            (snap, orig, d)
        } else {
            (None, None, false)
        };

        if let Some(origin) = origin_opt {
            current_origin = origin;
            let aux = ConfigureWindowAux::new()
                .x(origin[0] as i32)
                .y(origin[1] as i32);
            let _ = conn.configure_window(window, &aux);
            let _ = conn.flush();
        }

        if let Some(snapshot) = snapshot_opt {
            current_style = snapshot.style;
            model.reduced = snapshot.reduced_motion;
            if model.clock != snapshot.clock {
                pipelines.atlas.update_clock(&snapshot.clock);
                model.set_clock(&snapshot.clock);
            }
            if let Pres::Completed { ref label } = snapshot.pres {
                let sp = pipelines.atlas.update_completed(label);
                model.set_completed_label(label.clone(), sp.size_pt[0]);
            }
            if model.pres != snapshot.pres {
                model.set_pres(snapshot.pres);
            }
        }

        // 2. Poll X11 events
        while let Ok(Some(event)) = conn.poll_for_event() {
            match event {
                Event::MotionNotify(ev) => {
                    let lx = ev.event_x as f32 / scale;
                    let ly = ev.event_y as f32 / scale;
                    mouse_pos = [lx, ly];
                    let inside = model.hit_island(lx, ly);
                    let pos_opt = inside.then_some([lx, ly]);
                    model.set_pointer(pos_opt, mouse_down);
                }
                Event::ButtonPress(ev) if ev.detail == 1 => {
                    mouse_down = true;
                    let lx = ev.event_x as f32 / scale;
                    let ly = ev.event_y as f32 / scale;
                    mouse_pos = [lx, ly];
                    let inside = model.hit_island(lx, ly);
                    let pos_opt = inside.then_some([lx, ly]);
                    model.set_pointer(pos_opt, true);
                    let hit = pos_opt.and_then(|[x, y]| model.hit_element(x, y));
                    press_down_elem = hit;
                    if hit == Some(ElemId::Paste) {
                        paste_drag_start_screen = Some([current_origin[0] + lx, current_origin[1] + ly]);
                    } else {
                        paste_drag_start_screen = None;
                    }
                }
                Event::ButtonRelease(ev) if ev.detail == 1 => {
                    mouse_down = false;
                    let lx = ev.event_x as f32 / scale;
                    let ly = ev.event_y as f32 / scale;
                    mouse_pos = [lx, ly];
                    let inside = model.hit_island(lx, ly);
                    let pos_opt = inside.then_some([lx, ly]);
                    model.set_pointer(pos_opt, false);

                    if let Some(down_elem) = press_down_elem.take() {
                        let up_elem = pos_opt.and_then(|[x, y]| model.hit_element(x, y));
                        let screen_pt = [current_origin[0] + lx, current_origin[1] + ly];
                        if down_elem == ElemId::Paste {
                            if up_elem == Some(ElemId::Paste) {
                                let _ = action_tx.try_send(PillAction::Paste(None));
                            } else if let Some(start) = paste_drag_start_screen.take() {
                                let dist = (screen_pt[0] - start[0]).hypot(screen_pt[1] - start[1]);
                                if dist >= 24.0 {
                                    let _ = action_tx.try_send(PillAction::Paste(Some(screen_pt)));
                                }
                            }
                        } else if up_elem == Some(down_elem) {
                            dispatch_click_linux(down_elem, &model, &action_tx);
                        }
                    }
                    paste_drag_start_screen = None;
                }
                Event::LeaveNotify(_) => {
                    model.set_pointer(None, false);
                }
                _ => {}
            }
        }

        // 3. Step model animation
        let now = motion::now();
        let dt = (now - last_tick).max(0.001).min(0.033) as f32;
        last_tick = now;
        let animating = !model.is_at_rest() || matches!(model.pres, Pres::Processing);
        if animating || dirty {
            model.advance(dt);
        }
        let frame = model.frame();

        // 4. Update X11 input shape
        let inside_island = model.hit_island(mouse_pos[0], mouse_pos[1]);
        let should_ignore = model.is_fully_hidden() || (!inside_island && press_down_elem.is_none());
        let cur_shape_rect = if !should_ignore && frame.shape.alpha > 0.01 {
            Some([
                (frame.shape.l * scale).round() as i16,
                (frame.shape.t * scale).round() as i16,
                (frame.shape.width() * scale).round().max(1.0) as i16,
                (frame.shape.height() * scale).round().max(1.0) as i16,
            ])
        } else {
            None
        };

        if cur_shape_rect != last_shape_rect || should_ignore != last_ignores_mouse {
            last_shape_rect = cur_shape_rect;
            last_ignores_mouse = should_ignore;
            if let Some([sx, sy, sw, sh]) = cur_shape_rect {
                let rect = Rectangle {
                    x: sx,
                    y: sy,
                    width: sw as u16,
                    height: sh as u16,
                };
                let _ = shape::ConnectionExt::shape_rectangles(
                    &*conn,
                    shape::So::SET,
                    shape::Sk::INPUT,
                    ClipOrdering::UNSORTED,
                    window,
                    0,
                    0,
                    &[rect],
                );
            } else {
                let _ = shape::ConnectionExt::shape_rectangles(
                    &*conn,
                    shape::So::SET,
                    shape::Sk::INPUT,
                    ClipOrdering::UNSORTED,
                    window,
                    0,
                    0,
                    &[],
                );
            }
            let _ = conn.flush();
        }

        pointer_on_pill_shared.store(!should_ignore, Ordering::Release);
        is_hidden_shared.store(model.is_fully_hidden(), Ordering::Release);

        // 5. Render frame if needed
        if !model.is_fully_hidden() && (animating || dirty || frame.shape.alpha > 0.01) {
            let _ = pipelines.render(&surface, &frame, &model, current_style);
        }

        // 6. Cadence sleep: 16ms (~60fps) or 8ms when animating
        std::thread::sleep(Duration::from_millis(if animating { 8 } else { 16 }));
    }
}

#[cfg(target_os = "linux")]
fn dispatch_click_linux(elem: ElemId, model: &model::PillModel, action_tx: &flume::Sender<PillAction>) {
    let is_selecting = matches!(model.pres, Pres::Selecting { .. });
    let action = match elem {
        ElemId::ChipArea => {
            if is_selecting {
                PillAction::SelectMode(SelectionMode::Area)
            } else {
                PillAction::SwitchRecording(SelectionMode::Area)
            }
        }
        ElemId::ChipWindow => {
            if is_selecting {
                PillAction::SelectMode(SelectionMode::Window)
            } else {
                PillAction::SwitchRecording(SelectionMode::Window)
            }
        }
        ElemId::ChipScreen => {
            if is_selecting {
                PillAction::SelectMode(SelectionMode::Screen)
            } else {
                PillAction::SwitchRecording(SelectionMode::Screen)
            }
        }
        ElemId::ChipIphone => {
            if is_selecting {
                PillAction::SelectMode(SelectionMode::Iphone)
            } else {
                PillAction::SwitchRecording(SelectionMode::Iphone)
            }
        }
        ElemId::Record => PillAction::RecordSelected,
        ElemId::Undo => PillAction::Undo,
        ElemId::Cancel => PillAction::Cancel,
        ElemId::Finish => PillAction::Finish,
        ElemId::Close => {
            if matches!(model.pres, Pres::Completed { .. }) {
                PillAction::DismissCompleted
            } else {
                PillAction::Cancel
            }
        }
        ElemId::Paste => PillAction::Paste(None),
        ElemId::Inspect => PillAction::Inspect,
        _ => return,
    };
    let _ = action_tx.try_send(action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn metal_pipelines_compile_and_initialize() {
        let pipelines = MetalPipelines::new(2.0).expect("MetalPipelines::new failed");
        assert_eq!(pipelines.scale, 2.0);
    }

    #[test]
    fn pill_uniform_struct_sizes_and_alignments() {
        assert_eq!(size_of::<TileUniforms>(), 32);
        assert_eq!(size_of::<TilePrim>(), 64);
        assert_eq!(size_of::<BlurBox>(), 32);
        assert_eq!(size_of::<CompositeElem>(), 64);
        assert_eq!(size_of::<PillUniforms>(), 128);
    }
}
