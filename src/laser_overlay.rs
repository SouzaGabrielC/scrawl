use std::mem::size_of;
use std::sync::Arc;

use crate::capture::SharedCaptureTelemetry;

pub const TAIL_DURATION_SEC: f32 = 0.42;
pub const MAX_SEGMENTS: usize = 1536;
pub const MAX_PINS: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(target_os = "linux", derive(bytemuck::Pod, bytemuck::Zeroable))]
pub struct LaserUniforms {
    pub resolution: [f32; 2],
    pub scale: f32,
    pub now_sec: f32,
    pub tail_duration: f32,
    pub tip_active: f32,
    pub tip_pos: [f32; 2],
    pub segment_count: u32,
    pub pin_count: u32,
    pub constrain_rect: f32,
    pub _pad1: f32,
    pub target_rect: [f32; 4],
    pub glow: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(target_os = "linux", derive(bytemuck::Pod, bytemuck::Zeroable))]
pub struct PathSegment {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub w0: f32,
    pub w1: f32,
    pub kind: f32,
    pub width: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(target_os = "linux", derive(bytemuck::Pod, bytemuck::Zeroable))]
pub struct PinMarker {
    pub pos: [f32; 2],
    pub radius: f32,
    pub _pad: f32,
}

pub fn same_rect(a: [f64; 4], b: [f64; 4]) -> bool {
    (0..4).all(|i| (a[i] - b[i]).abs() < 1.0)
}

// ============================================================================
// macOS Implementation (MetalLayer + CVDisplayLink)
// ============================================================================

#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "macos")]
use std::sync::Mutex;

#[cfg(target_os = "macos")]
use core_graphics_types::geometry::CGSize;
#[cfg(target_os = "macos")]
use core_video::base::{CVOptionFlags, CVTimeStamp};
#[cfg(target_os = "macos")]
use core_video::display_link::{CVDisplayLink, CVDisplayLinkRef};
#[cfg(target_os = "macos")]
use core_video::r#return::CVReturn;
#[cfg(target_os = "macos")]
use metal::{
    MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLResourceOptions, MTLStoreAction,
    MetalLayer, RenderPassDescriptor, RenderPipelineDescriptor, RenderPipelineState,
};
#[cfg(target_os = "macos")]
use objc2::MainThreadMarker;
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSScreen, NSStatusWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSPoint, NSRect};
#[cfg(target_os = "macos")]
use objc2_quartz_core::CALayer;

#[cfg(target_os = "macos")]
use crate::sketch::{slice_laser_window, smooth_ink_points, smooth_laser_points};

#[cfg(target_os = "macos")]
pub struct MetalLaserOverlay {
    window: Retained<NSPanel>,
    display_link: CVDisplayLink,
    display_link_context: *const SharedMetalOverlay,
    /// The screen it covers, global points.
    screen: [f64; 4],
}

#[cfg(target_os = "macos")]
fn pick_screen(
    mtm: MainThreadMarker,
    on: Option<[f64; 4]>,
) -> Option<(crate::screens::Screen, NSRect)> {
    let screens = crate::screens::current();
    let index = on
        .and_then(|r| crate::screens::index_for_rect(&screens, r))
        .or_else(|| crate::screens::pointer().and_then(|p| crate::screens::index_at(&screens, p)))
        .or(if screens.is_empty() { None } else { Some(0) })?;
    let screen = screens[index];
    let primary_h = NSScreen::screens(mtm).firstObject()?.frame().size.height;
    let [x, y, w, h] = crate::screens::flip(screen.frame, primary_h);
    Some((
        screen,
        NSRect::new(NSPoint::new(x, y), objc2_foundation::NSSize::new(w, h)),
    ))
}

#[cfg(target_os = "macos")]
impl MetalLaserOverlay {
    /// Opens over the screen holding the centre of `on` (global points, top-left origin, like
    /// capture targets), else the screen under the pointer.
    pub fn open(
        telemetry: Arc<SharedCaptureTelemetry>,
        on: Option<[f64; 4]>,
    ) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("Metal overlay requires the main thread")?;
        let (screen, frame) =
            pick_screen(mtm, on).ok_or("No screen available for Metal overlay")?;
        let scale = (screen.scale as f32).max(1.0);
        let [ox, oy, _, _] = screen.frame;

        let shared = Arc::new(SharedMetalOverlay::new(
            telemetry,
            [ox as f32, oy as f32],
            frame.size.width as f32,
            frame.size.height as f32,
            scale,
        )?);

        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            frame,
            NSWindowStyleMask::Borderless
                | NSWindowStyleMask::UtilityWindow
                | NSWindowStyleMask::NonactivatingPanel
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        );
        let view =
            NSView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        view.setWantsLayer(true);
        let metal_layer = shared.layer_ptr();
        let cocoa_layer = unsafe { &*metal_layer.cast::<CALayer>() };
        cocoa_layer.setOpaque(false);
        view.setLayer(Some(cocoa_layer));
        window.setContentView(Some(&view));
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setIgnoresMouseEvents(true);
        window.setHasShadow(false);
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        window.setLevel(NSStatusWindowLevel + 1);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::CanJoinAllApplications
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        unsafe { window.setReleasedWhenClosed(false) };
        window.setFrame_display(frame, true);
        // Present Frame 0 synchronously so the cutout and border are already on screen
        // before TargetPickerWindow closes (zero 1-frame blink).
        shared.draw();
        window.orderFrontRegardless();

        let display_link = CVDisplayLink::from_active_cg_displays()
            .map_err(|status| format!("Could not create CVDisplayLink: {status}"))?;
        if screen.id != 0 {
            let _ = display_link.set_current_cg_display(screen.id);
        }
        let display_link_context = Arc::into_raw(shared);
        unsafe {
            display_link.set_output_callback(
                display_link_callback,
                display_link_context.cast_mut().cast::<c_void>(),
            )
        }
        .map_err(|status| format!("Could not configure CVDisplayLink callback: {status}"))?;
        let _ = display_link.start();

        Ok(Self {
            window,
            display_link,
            display_link_context,
            screen: screen.frame,
        })
    }

    pub fn spawn(telemetry: Arc<SharedCaptureTelemetry>) -> Option<Self> {
        Self::open(telemetry, None).ok()
    }

    pub fn window_number(&self) -> u32 {
        self.window.windowNumber().max(0) as u32
    }

    pub fn window_id(&self) -> u32 {
        self.window_number()
    }

    /// The screen it covers, global points.
    pub fn screen(&self) -> [f64; 4] {
        self.screen
    }

    /// Moves to the screen `rect` (global points) belongs to, taking its size, scale and origin.
    /// Returns whether it moved.
    pub fn move_to(&mut self, rect: [f64; 4]) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let Some((screen, frame)) = pick_screen(mtm, Some(rect)) else {
            return false;
        };
        if same_rect(screen.frame, self.screen) {
            return false;
        }
        // Window first: AppKit sets the layer's contentsScale for the new screen as it moves,
        // and `place` then sets the size and scale the shader draws at.
        self.window.setFrame_display(frame, true);
        let shared = unsafe { &*self.display_link_context };
        shared.place(
            [screen.frame[0] as f32, screen.frame[1] as f32],
            frame.size.width as f32,
            frame.size.height as f32,
            (screen.scale as f32).max(1.0),
        );
        if screen.id != 0 {
            let _ = self.display_link.set_current_cg_display(screen.id);
        }
        self.screen = screen.frame;
        shared.draw();
        true
    }
}

#[cfg(target_os = "macos")]
impl Drop for MetalLaserOverlay {
    fn drop(&mut self) {
        let _ = self.display_link.stop();
        unsafe { drop(Arc::from_raw(self.display_link_context)) };
        self.window.orderOut(None);
        self.window.close();
    }
}

#[cfg(target_os = "macos")]
extern "C" fn display_link_callback(
    _display_link: CVDisplayLinkRef,
    _now: *const CVTimeStamp,
    _output_time: *const CVTimeStamp,
    _flags_in: CVOptionFlags,
    _flags_out: *mut CVOptionFlags,
    context: *mut c_void,
) -> CVReturn {
    if !context.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let overlay = unsafe { &*context.cast::<SharedMetalOverlay>() };
            overlay.draw();
        }));
    }
    0
}

#[cfg(target_os = "macos")]
struct SharedMetalOverlay {
    telemetry: Arc<SharedCaptureTelemetry>,
    renderer: Mutex<MetalLaserRenderer>,
}

#[cfg(target_os = "macos")]
unsafe impl Send for SharedMetalOverlay {}
#[cfg(target_os = "macos")]
unsafe impl Sync for SharedMetalOverlay {}

#[cfg(target_os = "macos")]
struct MetalLaserRenderer {
    layer: MetalLayer,
    device: metal::Device,
    command_queue: metal::CommandQueue,
    pipeline: RenderPipelineState,
    /// The screen's top-left in global points: telemetry is global, the shader draws locally.
    origin: [f32; 2],
    width: f32,
    height: f32,
    scale: f32,
}

#[cfg(target_os = "macos")]
impl SharedMetalOverlay {
    fn new(
        telemetry: Arc<SharedCaptureTelemetry>,
        origin: [f32; 2],
        width: f32,
        height: f32,
        scale: f32,
    ) -> Result<Self, String> {
        let device = metal::Device::system_default().ok_or("Metal is unavailable")?;
        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_opaque(false);
        layer.set_presents_with_transaction(false);
        layer.set_display_sync_enabled(true);
        layer.set_contents_scale(f64::from(scale));
        layer.set_drawable_size(CGSize::new(
            f64::from(width * scale),
            f64::from(height * scale),
        ));

        let pipeline = overlay_pipeline(&device)?;

        Ok(Self {
            telemetry,
            renderer: Mutex::new(MetalLaserRenderer {
                command_queue: device.new_command_queue(),
                device,
                layer,
                pipeline,
                origin,
                width,
                height,
                scale,
            }),
        })
    }

    /// Takes another screen's origin, size and scale (`MetalLaserOverlay::move_to`).
    fn place(&self, origin: [f32; 2], width: f32, height: f32, scale: f32) {
        let mut renderer = self
            .renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        renderer.origin = origin;
        renderer.width = width;
        renderer.height = height;
        renderer.scale = scale;
        renderer.layer.set_contents_scale(f64::from(scale));
        renderer.layer.set_drawable_size(CGSize::new(
            f64::from(width * scale),
            f64::from(height * scale),
        ));
    }

    fn origin(&self) -> [f32; 2] {
        self.renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .origin
    }

    fn layer_ptr(&self) -> *const metal::MetalLayerRef {
        self.renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .layer
            .as_ref()
    }

    fn draw(&self) {
        let is_active = self.telemetry.active.load(Ordering::Acquire);
        let is_selecting = self.telemetry.selecting.load(Ordering::Acquire);
        let is_device_pending = self.telemetry.capture_mode.load(Ordering::Acquire) == 3;
        if !is_active && !is_selecting && !is_device_pending {
            return;
        }
        let now_ms = self.telemetry.elapsed_ms();
        let now_sec = if is_selecting {
            0.0
        } else {
            now_ms as f32 / 1_000.0
        };
        let picker_fade = if is_selecting {
            let elapsed = self
                .telemetry
                .picker_started_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .elapsed()
                .as_secs_f32();
            let t = (elapsed / 0.15).clamp(0.0, 1.0);
            1.0 - (1.0 - t).powi(3)
        } else {
            1.0
        };
        let tail_ms = (TAIL_DURATION_SEC * 1_000.0) as u64;
        let min_laser_ms = now_ms.saturating_sub(tail_ms);

        let picker_sel = if is_selecting {
            *self
                .telemetry
                .picker_rect
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        } else {
            None
        };
        let target_frame = *self
            .telemetry
            .target_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tx, ty, tw, th, is_constrained, mode_code) = if is_selecting {
            if let Some(([rx, ry, rw, rh], kind)) = picker_sel {
                (rx, ry, rw.max(1.0), rh.max(1.0), true, f32::from(kind))
            } else {
                (0.0, 0.0, 1.0, 1.0, false, -picker_fade.max(0.001))
            }
        } else {
            let tx = target_frame.x as f32;
            let ty = target_frame.y as f32;
            let tw = (target_frame.width as f32).max(1.0);
            let th = (target_frame.height as f32).max(1.0);
            let mode = self.telemetry.capture_mode.load(Ordering::Acquire) as f32;
            let constrained = mode > 0.5 && tx > -10_000.0;
            (tx, ty, tw, th, constrained, mode)
        };
        let [ox, oy] = self.origin();
        let (tx, ty) = (tx - ox, ty - oy);
        let to_screen = |nx: f32, ny: f32| -> [f32; 2] { [tx + nx * tw, ty + ny * th] };

        let committed_strokes = self
            .telemetry
            .strokes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let active_laser = self
            .telemetry
            .active_laser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let active_ink = self
            .telemetry
            .live_ink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        let mut strokes: Vec<(crate::model::Stroke, bool)> =
            committed_strokes.into_iter().map(|s| (s, false)).collect();
        if let Some(laser) = active_laser {
            strokes.push((laser, true));
        }
        if let Some(ink) = active_ink {
            strokes.push((ink, true));
        }

        let mut segments = Vec::with_capacity(256);
        let mut pins = Vec::new();

        for (stroke, is_live) in &strokes {
            if stroke.pin.is_some() {
                if let Some([nx, ny]) = stroke.normalized_points().first().copied()
                    && pins.len() < MAX_PINS
                {
                    pins.push(PinMarker {
                        pos: to_screen(nx, ny),
                        radius: 11.5,
                        _pad: 0.0,
                    });
                }
                continue;
            }
            if stroke.laser {
                if stroke.t1 < min_laser_ms {
                    continue;
                }
                let raw = slice_laser_window(&stroke.timed_points(), now_ms, tail_ms, to_screen);
                let smoothed = smooth_laser_points(&raw);
                for pair in smoothed.windows(2) {
                    if segments.len() >= MAX_SEGMENTS {
                        break;
                    }
                    let (a, w0) = pair[0];
                    let (b, w1) = pair[1];
                    segments.push(PathSegment {
                        a,
                        b,
                        w0,
                        w1,
                        kind: 0.0,
                        width: 3.0,
                    });
                }
            } else {
                // Keep live Shift strokes at 100% while held, then hold for 650ms and fade out over 650ms.
                let fade_alpha = if *is_live {
                    1.0
                } else {
                    let age_ms = now_ms.saturating_sub(stroke.t1);
                    if age_ms >= 1_300 {
                        continue;
                    } else if age_ms <= 650 {
                        1.0
                    } else {
                        let t = ((age_ms - 650) as f32 / 650.0).clamp(0.0, 1.0);
                        (1.0 - t) * (1.0 - t)
                    }
                };
                let pts: Vec<[f32; 2]> = stroke
                    .normalized_points()
                    .into_iter()
                    .map(|[nx, ny]| to_screen(nx, ny))
                    .collect();
                let smoothed = smooth_ink_points(&pts);
                for pair in smoothed.windows(2) {
                    if segments.len() >= MAX_SEGMENTS {
                        break;
                    }
                    segments.push(PathSegment {
                        a: pair[0].0,
                        b: pair[1].0,
                        w0: pair[0].1,
                        w1: pair[1].1,
                        kind: 1.0,
                        width: fade_alpha,
                    });
                }
            }
        }

        let shift_held = self.telemetry.shift_held.load(Ordering::Acquire);
        let pointer = {
            let [px, py] = *self
                .telemetry
                .pointer_screen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            [px - ox, py - oy]
        };

        if segments.is_empty() {
            segments.push(PathSegment::default());
        }
        if pins.is_empty() {
            pins.push(PinMarker::default());
        }

        let renderer = self
            .renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        objc2::rc::autoreleasepool(|_| {
            let Some(drawable) = renderer.layer.next_drawable() else {
                return;
            };
            let pass = RenderPassDescriptor::new();
            let attachment = pass.color_attachments().object_at(0).unwrap();
            attachment.set_texture(Some(drawable.texture()));
            attachment.set_load_action(MTLLoadAction::Clear);
            attachment.set_clear_color(metal::MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
            attachment.set_store_action(MTLStoreAction::Store);

            let uniforms = LaserUniforms {
                resolution: [
                    renderer.width * renderer.scale,
                    renderer.height * renderer.scale,
                ],
                scale: renderer.scale,
                now_sec,
                tail_duration: picker_fade,
                tip_active: if is_active && shift_held { 1.0 } else { 0.0 },
                tip_pos: pointer,
                segment_count: if !is_active || (segments.len() == 1 && segments[0].width == 0.0) {
                    0
                } else {
                    segments.len().min(MAX_SEGMENTS) as u32
                },
                pin_count: if !is_active || (pins.len() == 1 && pins[0].radius == 0.0) {
                    0
                } else {
                    pins.len().min(MAX_PINS) as u32
                },
                constrain_rect: if is_constrained { 1.0 } else { 0.0 },
                _pad1: mode_code,
                target_rect: [tx, ty, tw, th],
                glow: [
                    f32::from(u8::from(
                        is_active && self.telemetry.start_glow.load(Ordering::Acquire),
                    )),
                    0.0,
                    0.0,
                    0.0,
                ],
            };

            let cmd = renderer.command_queue.new_command_buffer();
            encode_overlay(
                cmd,
                pass,
                &renderer.device,
                &renderer.pipeline,
                &uniforms,
                &segments,
                &pins,
            );
            cmd.present_drawable(drawable);
            cmd.commit();
        });
    }
}

#[cfg(target_os = "macos")]
fn overlay_pipeline(device: &metal::DeviceRef) -> Result<RenderPipelineState, String> {
    let library = device
        .new_library_with_data(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/laser_overlay.metallib"
        )))
        .map_err(|e| format!("Could not load laser shader library: {e}"))?;
    let vertex = library
        .get_function("laser_vertex", None)
        .map_err(|e| format!("Could not load laser_vertex: {e}"))?;
    let fragment = library
        .get_function("laser_fragment", None)
        .map_err(|e| format!("Could not load laser_fragment: {e}"))?;

    let descriptor = RenderPipelineDescriptor::new();
    descriptor.set_vertex_function(Some(&vertex));
    descriptor.set_fragment_function(Some(&fragment));
    let attachment = descriptor
        .color_attachments()
        .object_at(0)
        .ok_or("Missing color attachment")?;
    attachment.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    attachment.set_blending_enabled(false);

    device
        .new_render_pipeline_state(&descriptor)
        .map_err(|e| format!("Could not create laser pipeline: {e}"))
}

#[cfg(target_os = "macos")]
fn encode_overlay(
    cmd: &metal::CommandBufferRef,
    pass: &metal::RenderPassDescriptorRef,
    device: &metal::DeviceRef,
    pipeline: &RenderPipelineState,
    uniforms: &LaserUniforms,
    segments: &[PathSegment],
    pins: &[PinMarker],
) {
    let enc = cmd.new_render_command_encoder(pass);
    enc.set_render_pipeline_state(pipeline);
    enc.set_fragment_bytes(
        0,
        size_of::<LaserUniforms>() as u64,
        std::ptr::from_ref(uniforms).cast(),
    );
    // Up to MAX_SEGMENTS * 32 B = 48 KB, past the 4 KB `set_fragment_bytes` limit (128
    // segments), so segments go in a fresh buffer each frame. The command buffer retains it
    // until the GPU is done, so a frame never overwrites one still being read.
    let segment_buffer = device.new_buffer_with_data(
        segments.as_ptr().cast(),
        (segments.len() * size_of::<PathSegment>()) as u64,
        MTLResourceOptions::StorageModeShared,
    );
    enc.set_fragment_buffer(1, Some(&segment_buffer), 0);
    enc.set_fragment_bytes(
        2,
        (pins.len() * size_of::<PinMarker>()) as u64,
        pins.as_ptr().cast(),
    );
    enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
    enc.end_encoding();
}

// ============================================================================
// Linux Implementation (wgpu + X11 click-through override-redirect ARGB window)
// ============================================================================

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use x11rb::connection::Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::shape::{self, ConnectionExt as _};
#[cfg(target_os = "linux")]
use x11rb::wrapper::ConnectionExt as _;
#[cfg(target_os = "linux")]
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
    EventMask, PropMode, VisualClass, WindowClass,
};

#[cfg(target_os = "linux")]
use crate::sketch::{slice_laser_window, smooth_ink_points, smooth_laser_points};

#[cfg(target_os = "linux")]
pub struct MetalLaserOverlay {
    conn: Arc<x11rb::rust_connection::RustConnection>,
    window: u32,
    screen: [f64; 4],
    running: Arc<AtomicBool>,
    state: Arc<Mutex<OverlayState>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
struct OverlayState {
    screen: [f64; 4],
    scale: f32,
    origin: [f32; 2],
    size: [f32; 2],
    resized: bool,
}

#[cfg(target_os = "linux")]
fn pick_screen_linux(on: Option<[f64; 4]>) -> Option<crate::screens::Screen> {
    let screens = crate::screens::current();
    let index = on
        .and_then(|r| crate::screens::index_for_rect(&screens, r))
        .or_else(|| crate::screens::pointer().and_then(|p| crate::screens::index_at(&screens, p)))
        .or(if screens.is_empty() { None } else { Some(0) })?;
    screens.get(index).cloned()
}

#[cfg(target_os = "linux")]
impl MetalLaserOverlay {
    /// Opens over the screen holding the centre of `on` (global points, top-left origin, like
    /// capture targets), else the screen under the pointer.
    pub fn open(
        telemetry: Arc<SharedCaptureTelemetry>,
        on: Option<[f64; 4]>,
    ) -> Result<Self, String> {
        let screen = pick_screen_linux(on).unwrap_or(crate::screens::Screen {
            id: 0,
            frame: [0.0, 0.0, 1920.0, 1080.0],
            visible: [0.0, 0.0, 1920.0, 1080.0],
            scale: 1.0,
            safe_top: 0.0,
        });
        let [ox, oy, sw, sh] = screen.frame;
        let scale = (screen.scale as f32).max(1.0);

        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("X11 connect failed: {e}"))?;
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen_num].root;

        let mut visual_id = conn.setup().roots[screen_num].root_visual;
        let mut depth = conn.setup().roots[screen_num].root_depth;
        let mut colormap = conn.setup().roots[screen_num].default_colormap;

        // Search for 32-bit ARGB TrueColor visual for transparent background
        for d in &conn.setup().roots[screen_num].allowed_depths {
            if d.depth == 32 {
                for v in &d.visuals {
                    if v.class == VisualClass::TRUE_COLOR {
                        visual_id = v.visual_id;
                        depth = 32;
                        if let Ok(cm) = conn.generate_id() {
                            if conn
                                .create_colormap(ColormapAlloc::NONE, cm, root, visual_id)
                                .is_ok()
                            {
                                colormap = cm;
                            }
                        }
                        break;
                    }
                }
            }
        }

        let window = conn.generate_id().map_err(|e| format!("generate_id failed: {e}"))?;
        let values = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);

        conn.create_window(
            depth,
            window,
            root,
            ox as i16,
            oy as i16,
            (sw * screen.scale).max(1.0) as u16,
            (sh * screen.scale).max(1.0) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &values,
        )
        .map_err(|e| format!("create_window failed: {e}"))?;

        // 1. Shape extension: set empty input shape (click-through)
        let _ = shape::ConnectionExt::shape_rectangles(
            &*conn,
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        );

        // 2. XFixes extension: set empty input shape region (click-through)
        if let Ok(region) = conn.generate_id() {
            if xfixes::ConnectionExt::xfixes_create_region(&*conn, region, &[]).is_ok() {
                let _ = xfixes::ConnectionExt::xfixes_set_window_shape_region(
                    &*conn,
                    window,
                    shape::SK::INPUT,
                    0,
                    0,
                    region,
                );
                let _ = xfixes::ConnectionExt::xfixes_destroy_region(&*conn, region);
            }
        }

        // Set EWMH atoms: _NET_WM_WINDOW_TYPE = DOCK / UTILITY, _NET_WM_STATE = ABOVE, STICKY
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

        // Set name
        let _ = conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"Scrawl Laser Overlay",
        );

        conn.map_window(window).map_err(|e| format!("map_window failed: {e}"))?;
        conn.flush().map_err(|e| format!("flush failed: {e}"))?;

        let running = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(OverlayState {
            screen: screen.frame,
            scale,
            origin: [ox as f32, oy as f32],
            size: [sw as f32, sh as f32],
            resized: false,
        }));

        let thread_running = Arc::clone(&running);
        let thread_state = Arc::clone(&state);
        let thread_telemetry = Arc::clone(&telemetry);
        let thread = std::thread::Builder::new()
            .name("scrawl-laser-overlay".into())
            .spawn(move || {
                run_render_thread(
                    window,
                    screen_num as i32,
                    thread_telemetry,
                    thread_state,
                    thread_running,
                );
            })
            .ok();

        Ok(Self {
            conn,
            window,
            screen: screen.frame,
            running,
            state,
            thread,
        })
    }

    pub fn spawn(telemetry: Arc<SharedCaptureTelemetry>) -> Option<Self> {
        Self::open(telemetry, None).ok()
    }

    pub fn window_number(&self) -> u32 {
        self.window
    }

    pub fn window_id(&self) -> u32 {
        self.window
    }

    pub fn screen(&self) -> [f64; 4] {
        self.screen
    }

    pub fn move_to(&mut self, rect: [f64; 4]) -> bool {
        let Some(screen) = pick_screen_linux(Some(rect)) else {
            return false;
        };
        if same_rect(screen.frame, self.screen) {
            return false;
        }
        let [x, y, w, h] = screen.frame;
        let scale = (screen.scale as f32).max(1.0);

        let aux = ConfigureWindowAux::new()
            .x(x as i32)
            .y(y as i32)
            .width((w * screen.scale).max(1.0) as u32)
            .height((h * screen.scale).max(1.0) as u32);
        let _ = self.conn.configure_window(self.window, &aux);
        let _ = self.conn.flush();

        if let Ok(mut state) = self.state.lock() {
            state.screen = screen.frame;
            state.scale = scale;
            state.origin = [x as f32, y as f32];
            state.size = [w as f32, h as f32];
            state.resized = true;
        }

        self.screen = screen.frame;
        true
    }
}

#[cfg(target_os = "linux")]
impl Drop for MetalLaserOverlay {
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
unsafe impl Send for XlibWindowSource {}
#[cfg(target_os = "linux")]
unsafe impl Sync for XlibWindowSource {}

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
fn run_render_thread(
    window: u32,
    screen_num: i32,
    telemetry: Arc<SharedCaptureTelemetry>,
    state: Arc<Mutex<OverlayState>>,
    running: Arc<AtomicBool>,
) {
    let xlib_display = unsafe { XOpenDisplay(std::ptr::null()) };
    if xlib_display.is_null() {
        return;
    }
    // Declared first so it drops AFTER `instance`, `surface`, `device`, and `queue`.
    let _display_guard = XlibDisplayGuard(xlib_display);

    let source = XlibWindowSource {
        window: window as std::ffi::c_ulong,
        display: xlib_display,
        screen: screen_num,
    };

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        flags: wgpu::InstanceFlags::default(),
        backend_options: Default::default(),
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
            label: Some("laser_device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: Default::default(),
        },
        None,
    )) {
        Ok(pair) => pair,
        Err(_) => return,
    };

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
    } else if surface_caps
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::Inherit)
    {
        wgpu::CompositeAlphaMode::Inherit
    } else {
        wgpu::CompositeAlphaMode::Auto
    };

    let format = surface_caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(surface_caps.formats[0]);

    let (mut width, mut height, mut scale, mut origin) = {
        let s = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            ((s.size[0] * s.scale).max(1.0)) as u32,
            ((s.size[1] * s.scale).max(1.0)) as u32,
            s.scale,
            s.origin,
        )
    };

    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width,
        height,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("laser_shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("laser_overlay.wgsl").into()),
    });

    let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("laser_uniform_buf"),
        size: size_of::<LaserUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let segment_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("laser_segment_buf"),
        size: (MAX_SEGMENTS * size_of::<PathSegment>()) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let pin_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("laser_pin_buf"),
        size: (MAX_PINS * size_of::<PinMarker>()) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("laser_bind_group_layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("laser_bind_group"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: segment_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: pin_buf.as_entire_binding(),
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("laser_pipeline_layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("laser_render_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("laser_vertex"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("laser_fragment"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
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

    while running.load(Ordering::Acquire) {
        // Check for window resize/move
        if let Ok(mut s) = state.lock() {
            if s.resized {
                s.resized = false;
                width = ((s.size[0] * s.scale).max(1.0)) as u32;
                height = ((s.size[1] * s.scale).max(1.0)) as u32;
                scale = s.scale;
                origin = s.origin;
                config.width = width;
                config.height = height;
                surface.configure(&device, &config);
            }
        }

        let is_active = telemetry.active.load(Ordering::Acquire);
        let is_selecting = telemetry.selecting.load(Ordering::Acquire);
        let is_device_pending = telemetry.capture_mode.load(Ordering::Acquire) == 3;

        let (uniforms, segments, pins) = build_linux_overlay_frame(
            &telemetry,
            origin,
            width as f32,
            height as f32,
            scale,
            is_active,
            is_selecting,
            is_device_pending,
        );

        queue.write_buffer(&uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        queue.write_buffer(&segment_buf, 0, bytemuck::cast_slice(&segments));
        queue.write_buffer(&pin_buf, 0, bytemuck::cast_slice(&pins));

        let output = match surface.get_current_texture() {
            Ok(tex) => tex,
            Err(wgpu::SurfaceError::Lost) => {
                surface.configure(&device, &config);
                continue;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => break,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(16));
                continue;
            }
        };

        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("laser_encoder"),
        });

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("laser_render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_pipeline(&pipeline);
            rpass.set_bind_group(0, &bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        queue.submit(std::iter::once(encoder.finish()));
        output.present();

        std::thread::sleep(Duration::from_millis(16));
    }
}

#[cfg(target_os = "linux")]
fn build_linux_overlay_frame(
    telemetry: &SharedCaptureTelemetry,
    origin: [f32; 2],
    width: f32,
    height: f32,
    scale: f32,
    is_active: bool,
    is_selecting: bool,
    _is_device_pending: bool,
) -> (LaserUniforms, Vec<PathSegment>, Vec<PinMarker>) {
    let now_ms = telemetry.elapsed_ms();
    let now_sec = if is_selecting {
        0.0
    } else {
        now_ms as f32 / 1_000.0
    };
    let picker_fade = if is_selecting {
        let elapsed = telemetry
            .picker_started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            .as_secs_f32();
        let t = (elapsed / 0.15).clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    } else {
        1.0
    };
    let tail_ms = (TAIL_DURATION_SEC * 1_000.0) as u64;
    let min_laser_ms = now_ms.saturating_sub(tail_ms);

    let picker_sel = if is_selecting {
        *telemetry
            .picker_rect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    } else {
        None
    };
    let target_frame = *telemetry
        .target_frame
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (tx, ty, tw, th, is_constrained, mode_code) = if is_selecting {
        if let Some(([rx, ry, rw, rh], kind)) = picker_sel {
            (rx, ry, rw.max(1.0), rh.max(1.0), true, f32::from(kind))
        } else {
            (0.0, 0.0, 1.0, 1.0, false, -picker_fade.max(0.001))
        }
    } else {
        let tx = target_frame.x as f32;
        let ty = target_frame.y as f32;
        let tw = (target_frame.width as f32).max(1.0);
        let th = (target_frame.height as f32).max(1.0);
        let mode = telemetry.capture_mode.load(Ordering::Acquire) as f32;
        let constrained = mode > 0.5 && tx > -10_000.0;
        (tx, ty, tw, th, constrained, mode)
    };

    let [ox, oy] = origin;
    let (tx, ty) = (tx - ox, ty - oy);
    let to_screen = |nx: f32, ny: f32| -> [f32; 2] { [tx + nx * tw, ty + ny * th] };

    let committed_strokes = telemetry
        .strokes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let active_laser = telemetry
        .active_laser
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let active_ink = telemetry
        .live_ink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    let mut strokes: Vec<(crate::model::Stroke, bool)> =
        committed_strokes.into_iter().map(|s| (s, false)).collect();
    if let Some(laser) = active_laser {
        strokes.push((laser, true));
    }
    if let Some(ink) = active_ink {
        strokes.push((ink, true));
    }

    let mut segments = Vec::with_capacity(256);
    let mut pins = Vec::new();

    for (stroke, is_live) in &strokes {
        if stroke.pin.is_some() {
            if let Some([nx, ny]) = stroke.normalized_points().first().copied() {
                if pins.len() < MAX_PINS {
                    pins.push(PinMarker {
                        pos: to_screen(nx, ny),
                        radius: 11.5,
                        _pad: 0.0,
                    });
                }
            }
            continue;
        }
        if stroke.laser {
            if stroke.t1 < min_laser_ms {
                continue;
            }
            let raw = slice_laser_window(&stroke.timed_points(), now_ms, tail_ms, to_screen);
            let smoothed = smooth_laser_points(&raw);
            for pair in smoothed.windows(2) {
                if segments.len() >= MAX_SEGMENTS {
                    break;
                }
                let (a, w0) = pair[0];
                let (b, w1) = pair[1];
                segments.push(PathSegment {
                    a,
                    b,
                    w0,
                    w1,
                    kind: 0.0,
                    width: 3.0,
                });
            }
        } else {
            let fade_alpha = if *is_live {
                1.0
            } else {
                let age_ms = now_ms.saturating_sub(stroke.t1);
                if age_ms >= 1_300 {
                    continue;
                } else if age_ms <= 650 {
                    1.0
                } else {
                    let t = ((age_ms - 650) as f32 / 650.0).clamp(0.0, 1.0);
                    (1.0 - t) * (1.0 - t)
                }
            };
            let pts: Vec<[f32; 2]> = stroke
                .normalized_points()
                .into_iter()
                .map(|[nx, ny]| to_screen(nx, ny))
                .collect();
            let smoothed = smooth_ink_points(&pts);
            for pair in smoothed.windows(2) {
                if segments.len() >= MAX_SEGMENTS {
                    break;
                }
                segments.push(PathSegment {
                    a: pair[0].0,
                    b: pair[1].0,
                    w0: pair[0].1,
                    w1: pair[1].1,
                    kind: 1.0,
                    width: fade_alpha,
                });
            }
        }
    }

    let shift_held = telemetry.shift_held.load(Ordering::Acquire);
    let pointer = {
        let [px, py] = *telemetry
            .pointer_screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        [px - ox, py - oy]
    };

    let uniforms = LaserUniforms {
        resolution: [width * scale, height * scale],
        scale,
        now_sec,
        tail_duration: picker_fade,
        tip_active: if is_active && shift_held { 1.0 } else { 0.0 },
        tip_pos: pointer,
        segment_count: if !is_active || (segments.len() == 1 && segments[0].width == 0.0) {
            0
        } else {
            segments.len().min(MAX_SEGMENTS) as u32
        },
        pin_count: if !is_active || (pins.len() == 1 && pins[0].radius == 0.0) {
            0
        } else {
            pins.len().min(MAX_PINS) as u32
        },
        constrain_rect: if is_constrained { 1.0 } else { 0.0 },
        _pad1: mode_code,
        target_rect: [tx, ty, tw, th],
        glow: [
            f32::from(u8::from(
                is_active && telemetry.start_glow.load(Ordering::Acquire),
            )),
            0.0,
            0.0,
            0.0,
        ],
    };

    if segments.is_empty() {
        segments.push(PathSegment::default());
    }
    if pins.is_empty() {
        pins.push(PinMarker::default());
    }

    (uniforms, segments, pins)
}

// ============================================================================
// Non-macOS / Non-Linux Fallback
// ============================================================================

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct MetalLaserOverlay {
    screen: [f64; 4],
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl MetalLaserOverlay {
    pub fn open(
        _telemetry: Arc<SharedCaptureTelemetry>,
        _on: Option<[f64; 4]>,
    ) -> Result<Self, String> {
        Err("MetalLaserOverlay is unsupported on this platform".into())
    }

    pub fn spawn(_telemetry: Arc<SharedCaptureTelemetry>) -> Option<Self> {
        None
    }

    pub fn window_number(&self) -> u32 {
        0
    }

    pub fn window_id(&self) -> u32 {
        0
    }

    pub fn screen(&self) -> [f64; 4] {
        self.screen
    }

    pub fn move_to(&mut self, _rect: [f64; 4]) -> bool {
        false
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgsl_laser_shader_parses_and_matches_uniform_struct_alignments() {
        let wgsl = include_str!("laser_overlay.wgsl");
        assert!(wgsl.contains("fn laser_vertex"));
        assert!(wgsl.contains("fn laser_fragment"));
        // WGSL uniform buffer alignment requires 16-byte multiple for LaserUniforms:
        assert_eq!(size_of::<LaserUniforms>() % 16, 0);
        assert_eq!(size_of::<PathSegment>(), 32);
        assert_eq!(size_of::<PinMarker>(), 16);
    }

    /// `setFragmentBytes` aborts in the driver past 4 KB.
    const SET_BYTES_LIMIT: usize = 4096;

    /// Uniforms and pins still go through `set_fragment_bytes`; segments don't fit and use a buffer.
    #[test]
    fn inline_fragment_bytes_fit_the_metal_limit() {
        assert!(size_of::<LaserUniforms>() <= SET_BYTES_LIMIT);
        assert!(MAX_PINS * size_of::<PinMarker>() <= SET_BYTES_LIMIT);
    }

    /// Renders the real shader offscreen with `MAX_SEGMENTS` segments (48 KB, which aborted in
    /// `setFragmentBytes`). Only the last segment is on screen, so the ink shows only if the
    /// shader read the whole array.
    #[cfg(target_os = "macos")]
    #[test]
    fn renders_a_full_segment_buffer() {
        let Some(device) = metal::Device::system_default() else {
            return;
        };
        let pipeline = overlay_pipeline(&device).expect("pipeline");
        let size = 64;
        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        descriptor.set_width(size);
        descriptor.set_height(size);
        descriptor.set_usage(metal::MTLTextureUsage::RenderTarget);
        descriptor.set_storage_mode(metal::MTLStorageMode::Shared);
        let texture = device.new_texture(&descriptor);

        let off_screen = PathSegment {
            a: [-500.0, -500.0],
            b: [-400.0, -500.0],
            w0: 1.0,
            w1: 1.0,
            kind: 1.0,
            width: 1.0,
        };
        let mut segments = vec![off_screen; MAX_SEGMENTS];
        segments[MAX_SEGMENTS - 1] = PathSegment {
            a: [0.0, 32.0],
            b: [64.0, 32.0],
            ..off_screen
        };
        let uniforms = LaserUniforms {
            resolution: [size as f32, size as f32],
            scale: 1.0,
            now_sec: 1.0,
            tail_duration: 1.0,
            tip_active: 0.0,
            tip_pos: [0.0, 0.0],
            segment_count: MAX_SEGMENTS as u32,
            pin_count: 0,
            constrain_rect: 0.0,
            _pad1: 0.0,
            target_rect: [0.0, 0.0, size as f32, size as f32],
            glow: [0.0; 4],
        };

        let pass = RenderPassDescriptor::new();
        let attachment = pass.color_attachments().object_at(0).unwrap();
        attachment.set_texture(Some(&texture));
        attachment.set_load_action(MTLLoadAction::Clear);
        attachment.set_clear_color(metal::MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
        attachment.set_store_action(MTLStoreAction::Store);
        let queue = device.new_command_queue();
        let cmd = queue.new_command_buffer();
        encode_overlay(
            cmd,
            pass,
            &device,
            &pipeline,
            &uniforms,
            &segments,
            &[PinMarker::default()],
        );
        cmd.commit();
        cmd.wait_until_completed();

        let mut pixels = vec![0u8; (size * size * 4) as usize];
        texture.get_bytes(
            pixels.as_mut_ptr().cast(),
            size * 4,
            metal::MTLRegion::new_2d(0, 0, size, size),
            0,
        );
        let bgra = |x: u64, y: u64| {
            let i = ((y * size + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        let [b, g, r, a] = bgra(32, 32);
        assert!(a > 200 && r > 200 && g < 100 && b < 100, "stroke pixel {:?}", [b, g, r, a]);
        assert_eq!(bgra(32, 4)[3], 0, "nothing drawn away from the stroke");
    }

    /// The shader must load on the oldest macOS Scrawl supports (`LSMinimumSystemVersion` 26.0).
    /// Without `-mmacosx-version-min`, `xcrun metal` targets the build machine's macOS and the
    /// overlay (ink, laser, edge glow) silently fails to open on older systems.
    #[cfg(target_os = "macos")]
    #[test]
    fn shader_targets_the_minimum_supported_macos() {
        let library = include_bytes!(concat!(env!("OUT_DIR"), "/laser_overlay.metallib"));
        let text = String::from_utf8_lossy(library);
        let marker = "-apple-macosx";
        let at = text.find(marker).expect("metallib names its target");
        let major: u32 = text[at + marker.len()..]
            .split('.')
            .next()
            .and_then(|v| v.parse().ok())
            .expect("target has a major version");
        assert!(major <= 26, "laser shader targets macOS {major}, newer than 26.0");
    }
}
