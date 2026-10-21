//! `scrawl export`: a bundle's recording as one shareable `.mp4` (H.264 or HEVC, AAC audio).
//!
//! Rust decides everything about the picture and the helper (`helper/Export.swift`) renders it:
//!
//! - `crop`: the part of the recording kept (`--crop`, default the activity region);
//! - `camera`: with `--zoom`, a camera rectangle per frame from the bundle's events, so the
//!   helper only interpolates and draws;
//! - `ink`: the bundle's strokes, pins, laser and click ripples as a time-stamped vector plan,
//!   drawn in the recording's own clock;
//! - audio: `audio.wav` (or an iPhone movie's own track) trimmed to start where the video does.
//!
//! The helper reads the video with AVAssetReader, composites with Core Image, draws the
//! annotations with Core Graphics into the same pixel buffers, and hardware-encodes with
//! AVAssetWriter. Nothing is decoded into Rust.

pub mod camera;
pub mod crop;
pub mod ink;

#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "macos")]
use std::io::{BufRead, BufReader};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::autocrop::Rect;
use crate::model::{InteractionEvent, ScrawlBundle};

use self::camera::{CameraKey, Focus};
use self::crop::CropArg;

#[derive(Clone, Debug)]
pub struct Options {
    pub out: Option<PathBuf>,
    pub crop: CropArg,
    /// Long edge cap in pixels.
    pub size: u32,
    pub zoom: bool,
    pub ink: bool,
    pub clicks: bool,
    pub hevc: bool,
    pub fps: Option<u32>,
    /// Run the encode at utility priority (the app, so a recording in progress isn't hurt).
    pub background: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            out: None,
            crop: CropArg::Auto,
            size: crop::DEFAULT_LONG_EDGE,
            zoom: false,
            ink: true,
            clicks: true,
            hevc: false,
            fps: None,
            background: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Exported {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub duration_ms: u64,
    pub crop: Rect,
    pub audio: bool,
    pub camera_moves: bool,
    pub bytes: u64,
}

/// Where `~/Movies/Scrawl/<bundle>.mp4` goes for a bundle (or `~/Videos/Scrawl` on Linux).
pub fn default_output(bundle_dir: &Path) -> PathBuf {
    let name = bundle_dir
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("scrawl");
    #[cfg(target_os = "linux")]
    {
        let video_dir = dirs::video_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
            .unwrap_or_else(|| PathBuf::from("."));
        video_dir.join("Scrawl").join(format!("{name}.mp4"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Movies/Scrawl")
            .join(format!("{name}.mp4"))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Plan<'a> {
    video: &'a Path,
    out: &'a Path,
    audio: Option<&'a Path>,
    audio_start_ms: u64,
    audio_length_ms: u64,
    width: u32,
    height: u32,
    fps: u32,
    codec: &'a str,
    duration_ms: u64,
    crop: Rect,
    camera: Option<Vec<[f64; 5]>>,
    ink: &'a ink::InkPlan,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanOwned {
    video: PathBuf,
    out: PathBuf,
    audio: Option<PathBuf>,
    audio_start_ms: u64,
    audio_length_ms: u64,
    width: u32,
    height: u32,
    fps: u32,
    codec: String,
    duration_ms: u64,
    crop: Rect,
    camera: Option<Vec<[f64; 5]>>,
    ink: ink::InkPlan,
}

/// What the helper's `export` reports when it is done.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Done {
    duration_ms: u64,
    bytes: u64,
}

/// Camera focuses for a bundle, on the video's clock: clicks, drags, drawn marks and laser
/// points. Activity before the video began is dropped.
pub fn focuses(bundle: &ScrawlBundle) -> Vec<Focus> {
    let offset = bundle.video_offset_ms;
    let mut out = Vec::new();
    let mut push = |t0: u64, t1: u64, rect: Rect| {
        if t1 >= offset {
            out.push(Focus {
                t0: t0.saturating_sub(offset),
                t1: t1.saturating_sub(offset),
                rect,
            });
        }
    };
    let bbox = |points: &mut dyn Iterator<Item = [f32; 2]>| -> Option<Rect> {
        points.fold(None, |acc: Option<Rect>, p| {
            Some(acc.map_or([p[0], p[1], p[0], p[1]], |r| {
                [
                    r[0].min(p[0]),
                    r[1].min(p[1]),
                    r[2].max(p[0]),
                    r[3].max(p[1]),
                ]
            }))
        })
    };
    for event in &bundle.events {
        match event {
            InteractionEvent::Click { t, x, y, .. } | InteractionEvent::Point { t, x, y, .. } => {
                push(*t, *t, [*x, *y, *x, *y]);
            }
            InteractionEvent::Drag {
                t,
                t1,
                x0,
                y0,
                x1,
                y1,
                path,
                ..
            } => {
                let mut points = [[*x0, *y0], [*x1, *y1]]
                    .into_iter()
                    .chain(path.iter().copied());
                if let Some(rect) = bbox(&mut points) {
                    push(*t, *t1, rect);
                }
            }
            _ => {}
        }
    }
    for stroke in &bundle.strokes {
        let mut points = stroke.normalized_points().into_iter();
        if let Some(rect) = bbox(&mut points) {
            push(stroke.t0, stroke.t1, rect);
        }
    }
    out
}

/// The camera path for `bundle` inside `crop`, as `[t, x0, y0, x1, y1]` rows.
pub fn camera_rows(
    bundle: &ScrawlBundle,
    crop: Rect,
    video_px: (u32, u32),
    duration_ms: u64,
) -> Vec<CameraKey> {
    let offset = bundle.video_offset_ms;
    let focuses = focuses(bundle);
    let cursor: Vec<(u64, f32, f32)> = bundle
        .cursor
        .iter()
        .filter(|c| c.0 >= offset)
        .map(|c| (c.0 - offset, c.1, c.2))
        .collect();
    camera::plan(&camera::Input {
        duration_ms,
        crop,
        aspect: video_px.0 as f32 / video_px.1.max(1) as f32,
        focuses: &focuses,
        cursor: &cursor,
    })
}

/// Exports `bundle` (in `dir`), reporting progress `0.0..=1.0`.
pub fn export(
    dir: &Path,
    bundle: &ScrawlBundle,
    options: &Options,
    progress: &mut dyn FnMut(f32),
) -> Result<Exported> {
    let video = match &bundle.video {
        Some(name) => dir.join(name),
        None => dir.join("recording.mp4"),
    };
    if !video.is_file() {
        bail!(
            "{}",
            crate::retention::missing_media_reason(dir)
                .unwrap_or_else(|| format!("This bundle has no video ({})", video.display()))
        );
    }
    let probe = crate::helper::probe(&video).context("Could not read the recording's video")?;
    ensure!(
        probe.width > 0 && probe.height > 0 && probe.duration_ms > 0,
        "The recording has no video"
    );
    let video_px = (probe.width, probe.height);

    let crop = crop::resolve(options.crop, bundle, video_px)?;
    let crop_px = (
        (crop[2] - crop[0]) * video_px.0 as f32,
        (crop[3] - crop[1]) * video_px.1 as f32,
    );
    let (width, height) = crop::output_size(crop_px, options.size)?;
    let nominal = if probe.fps > 0.0 { probe.fps } else { 30.0 };
    let fps = match options.fps {
        Some(fps) => crop::validate_fps(fps)?,
        None if nominal >= 45.0 => 60,
        None => 30,
    };

    // The audio: an iPhone movie's own track (full quality, on the movie's clock), else
    // `audio.wav`, which Scrawl records from the bundle's start, `video_offset_ms` before the
    // video's.
    let wav = bundle
        .audio
        .as_ref()
        .map(|name| dir.join(name))
        .filter(|p| p.is_file());
    let (audio, audio_start, audio_length) =
        if bundle.source.as_deref() == Some("iphone") && probe.audio_tracks > 0 {
            (Some(video.clone()), 0, probe.duration_ms)
        } else if let Some(wav) = wav {
            let wav_ms = wav_duration_ms(&wav)?;
            match crop::audio_window(bundle.video_offset_ms, probe.duration_ms, wav_ms) {
                Some((start, length)) => (Some(wav), start, length),
                None => (None, 0, 0),
            }
        } else {
            (None, 0, 0)
        };

    let camera = options
        .zoom
        .then(|| camera_rows(bundle, crop, video_px, probe.duration_ms))
        .filter(|keys| !keys.iter().all(|k| k.rect == crop));
    let layers = ink::Layers {
        ink: options.ink,
        clicks: options.clicks,
    };
    let ink = ink::plan(bundle, video_px, layers);

    let out = options.out.clone().unwrap_or_else(|| default_output(dir));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
    }
    let partial = out.with_extension("partial.mp4");
    let plan = Plan {
        video: &video,
        out: &partial,
        audio: audio.as_deref(),
        audio_start_ms: audio_start,
        audio_length_ms: audio_length,
        width,
        height,
        fps,
        codec: if options.hevc { "hevc" } else { "h264" },
        duration_ms: probe.duration_ms,
        crop,
        camera: camera.as_ref().map(|keys| {
            keys.iter()
                .map(|k| {
                    [
                        (k.t_ms * 10.0).round() / 10.0,
                        k.rect[0] as f64,
                        k.rect[1] as f64,
                        k.rect[2] as f64,
                        k.rect[3] as f64,
                    ]
                })
                .collect()
        }),
        ink: &ink,
    };
    let plan_path =
        std::env::temp_dir().join(format!("scrawl-export-{}.json", crate::make_bundle_id()));
    std::fs::write(&plan_path, serde_json::to_vec(&plan)?)?;
    let result = run_helper(&plan_path, options.background, progress);
    let _ = std::fs::remove_file(&plan_path);
    let done = match result {
        Ok(done) => done,
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
    };
    std::fs::rename(&partial, &out)
        .with_context(|| format!("Could not write {}", out.display()))?;
    progress(1.0);
    Ok(Exported {
        path: out,
        width,
        height,
        fps,
        duration_ms: done.duration_ms,
        crop,
        audio: audio.is_some(),
        camera_moves: camera.is_some(),
        bytes: done.bytes,
    })
}

fn wav_duration_ms(path: &Path) -> Result<u64> {
    let reader = hound::WavReader::open(path)
        .with_context(|| format!("Could not read {}", path.display()))?;
    let spec = reader.spec();
    ensure!(
        spec.sample_rate > 0 && spec.channels > 0,
        "Invalid audio file"
    );
    Ok(u64::from(reader.duration()) * 1_000 / u64::from(spec.sample_rate))
}

/// Runs `scrawl-helper export PLAN`, reading its JSON lines: `{"progress": f}` and a final
/// `{"done": {...}}`.
#[cfg(target_os = "macos")]
fn run_helper(plan: &Path, background: bool, progress: &mut dyn FnMut(f32)) -> Result<Done> {
    let helper = crate::helper::path()?;
    let mut command = if background {
        let mut c = Command::new("/usr/sbin/taskpolicy");
        c.args(["-c", "utility"]).arg(&helper);
        c
    } else {
        Command::new(&helper)
    };
    let mut child = command
        .arg("export")
        .arg(plan)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start scrawl-helper")?;
    let stdout = child.stdout.take().context("No helper stdout")?;
    let mut stderr = child.stderr.take().context("No helper stderr")?;
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut done = None;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(fraction) = value.get("progress").and_then(serde_json::Value::as_f64) {
            progress(fraction as f32);
        } else if let Some(summary) = value.get("done") {
            done = serde_json::from_value::<Done>(summary.clone()).ok();
        }
    }
    let status = child.wait()?;
    let message = errors.join().unwrap_or_default();
    if !status.success() {
        bail!("{}", message.trim());
    }
    done.context("scrawl-helper finished without a result")
}

#[cfg(not(target_os = "macos"))]
fn run_helper(plan_path: &Path, background: bool, progress: &mut dyn FnMut(f32)) -> Result<Done> {
    let bytes = std::fs::read(plan_path)?;
    let plan: PlanOwned = serde_json::from_slice(&bytes)
        .context("Failed to parse export plan JSON")?;

    let probe = crate::helper::probe(&plan.video)?;
    let total_frames = ((plan.duration_ms as f64 * plan.fps as f64) / 1000.0).round() as u64;
    ensure!(total_frames > 0, "No frames to export");

    let out_w = plan.width;
    let out_h = plan.height;
    let fps = plan.fps;

    let mut enc_cmd = if background {
        let mut c = Command::new("nice");
        c.args(["-n", "19", "ionice", "-c", "3", "ffmpeg"]);
        c
    } else {
        Command::new("ffmpeg")
    };
    enc_cmd
        .args(["-loglevel", "error", "-y"])
        .args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{out_w}x{out_h}"),
            "-r",
            &fps.to_string(),
            "-i",
            "-",
        ]);

    if let Some(ref audio_path) = plan.audio {
        let start_sec = format!("{:.3}", plan.audio_start_ms as f64 / 1000.0);
        let len_sec = format!("{:.3}", plan.audio_length_ms as f64 / 1000.0);
        enc_cmd.args(["-ss", &start_sec, "-t", &len_sec, "-i"])
            .arg(audio_path);
    }

    if plan.codec == "hevc" {
        enc_cmd.args(["-c:v", "libx265", "-crf", "24"]);
    } else {
        enc_cmd.args(["-c:v", "libx264", "-crf", "20"]);
    }
    enc_cmd.args(["-pix_fmt", "yuv420p", "-preset", "veryfast"]);

    if plan.audio.is_some() {
        enc_cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    enc_cmd.arg(&plan.out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut enc_child = enc_cmd.spawn()
        .context("Failed to start ffmpeg encoder")?;
    let mut enc_stdin = enc_child.stdin.take().context("No encoder stdin")?;
    let mut enc_stderr = enc_child.stderr.take().context("No encoder stderr")?;
    let enc_errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = enc_stderr.read_to_string(&mut text);
        text
    });

    let mut dec_cmd = if background {
        let mut c = Command::new("nice");
        c.args(["-n", "19", "ionice", "-c", "3", "ffmpeg"]);
        c
    } else {
        Command::new("ffmpeg")
    };
    dec_cmd
        .args(["-loglevel", "error", "-i"])
        .arg(&plan.video)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut dec_child = dec_cmd.spawn()
        .context("Failed to start ffmpeg decoder")?;
    let mut dec_stdout = dec_child.stdout.take().context("No decoder stdout")?;

    let src_w = probe.width;
    let src_h = probe.height;
    let src_frame_bytes = (src_w as usize) * (src_h as usize) * 4;
    let out_frame_bytes = (out_w as usize) * (out_h as usize) * 4;

    let mut src_buf = vec![0u8; src_frame_bytes];
    let mut out_buf = vec![0u8; out_frame_bytes];

    for frame_idx in 0..total_frames {
        let t_ms = (frame_idx as f64 * 1000.0 / fps as f64).round() as u64;
        match dec_stdout.read_exact(&mut src_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }

        let camera_rect = if let Some(ref keys) = plan.camera {
            let after = keys.partition_point(|k| (k[0] as u64) <= t_ms);
            if after == 0 {
                [keys[0][1] as f32, keys[0][2] as f32, keys[0][3] as f32, keys[0][4] as f32]
            } else if after >= keys.len() {
                let last = &keys[keys.len() - 1];
                [last[1] as f32, last[2] as f32, last[3] as f32, last[4] as f32]
            } else {
                let prev = &keys[after - 1];
                let next = &keys[after];
                let dt = next[0] - prev[0];
                let f = if dt > 1e-4 { ((t_ms as f64 - prev[0]) / dt) as f32 } else { 0.0 };
                [
                    prev[1] as f32 + (next[1] as f32 - prev[1] as f32) * f,
                    prev[2] as f32 + (next[2] as f32 - prev[2] as f32) * f,
                    prev[3] as f32 + (next[3] as f32 - prev[3] as f32) * f,
                    prev[4] as f32 + (next[4] as f32 - prev[4] as f32) * f,
                ]
            }
        } else {
            plan.crop
        };

        resample_crop_into(&src_buf, src_w, src_h, camera_rect, &mut out_buf, out_w, out_h);
        #[cfg(target_os = "linux")]
        if let Some(mut pixmap) = tiny_skia::PixmapMut::from_bytes(&mut out_buf, out_w, out_h) {
            ink::render_frame_ink(&mut pixmap, &plan.ink, camera_rect, t_ms);
        }
        enc_stdin.write_all(&out_buf)?;

        if total_frames > 0 && frame_idx % 15 == 0 {
            progress((frame_idx as f32) / (total_frames as f32));
        }
    }

    drop(enc_stdin);
    drop(dec_stdout);
    let _ = dec_child.kill();
    let _ = dec_child.wait();
    let enc_status = enc_child.wait()?;
    let enc_msg = enc_errors.join().unwrap_or_default();
    if !enc_status.success() {
        bail!("ffmpeg encoder failed: {}", enc_msg.trim());
    }

    let bytes = std::fs::metadata(&plan.out)?.len();
    progress(1.0);
    Ok(Done {
        duration_ms: plan.duration_ms,
        bytes,
    })
}

#[allow(dead_code)]
fn resample_crop_into(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    crop: Rect,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    let sx0 = (crop[0] * src_w as f32).clamp(0.0, src_w as f32);
    let sy0 = (crop[1] * src_h as f32).clamp(0.0, src_h as f32);
    let sx1 = (crop[2] * src_w as f32).clamp(sx0, src_w as f32);
    let sy1 = (crop[3] * src_h as f32).clamp(sy0, src_h as f32);
    let span_x = sx1 - sx0;
    let span_y = sy1 - sy0;

    for dy in 0..dst_h {
        let sy = sy0 + (dy as f32 + 0.5) / dst_h as f32 * span_y;
        let iy = (sy.floor() as usize).min(src_h as usize - 1);
        let src_row = iy * src_w as usize * 4;
        let dst_row = dy as usize * dst_w as usize * 4;

        for dx in 0..dst_w {
            let sx = sx0 + (dx as f32 + 0.5) / dst_w as f32 * span_x;
            let ix = (sx.floor() as usize).min(src_w as usize - 1);
            let src_px = src_row + ix * 4;
            let dst_px = dst_row + dx as usize * 4;
            dst[dst_px..dst_px + 4].copy_from_slice(&src[src_px..src_px + 4]);
        }
    }
}

/// What `scrawl export` prints.
pub fn describe(exported: &Exported) -> String {
    format!(
        "{} · {}×{} @ {} fps · {} · {:.1} MB{}{}",
        exported.path.display(),
        exported.width,
        exported.height,
        exported.fps,
        crate::model::format_clock(exported.duration_ms),
        exported.bytes as f64 / 1_048_576.0,
        if exported.audio {
            " · audio"
        } else {
            " · no audio"
        },
        if exported.camera_moves {
            " · auto-zoom"
        } else {
            ""
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> ScrawlBundle {
        serde_json::from_value(serde_json::json!({
            "version": 1, "id": "t", "createdAtMs": 0, "durationMs": 20_000,
            "target": {"mode": "display", "displayId": 0,
                "frame": {"x": 0.0, "y": 0.0, "width": 2560.0, "height": 1440.0, "scale": 2.0}},
            "transcript": {"source": "test", "text": "", "segments": []},
            "strokes": [{"t0": 4000, "t1": 4300, "fw": 2560, "fh": 1440,
                "pts": [2000, 3000, 0, 2600, 3400, 300]}],
            "marks": [], "keyframes": [], "contactSheet": "c.png", "videoOffsetMs": 100,
            "events": [
                {"kind": "click", "t": 50, "x": 0.1, "y": 0.1},
                {"kind": "click", "t": 2100, "x": 0.5, "y": 0.5},
                {"kind": "drag", "t": 8000, "t1": 8500, "x0": 0.7, "y0": 0.6, "x1": 0.75, "y1": 0.65},
                {"kind": "scroll", "t": 9000, "x": 0.1, "y": 0.1, "dx": 0.0, "dy": 3.0}
            ],
            "cursor": [[50, 0.1, 0.1], [2100, 0.5, 0.5]]
        }))
        .unwrap()
    }

    #[test]
    fn camera_focuses_are_on_the_video_clock_and_skip_what_came_before_it() {
        let f = focuses(&bundle());
        assert_eq!(
            f.len(),
            3,
            "click, drag, stroke; the early click and scroll don't count"
        );
        assert_eq!((f[0].t0, f[0].rect), (2_000, [0.5, 0.5, 0.5, 0.5]));
        assert_eq!((f[1].t0, f[1].t1), (7_900, 8_400));
        assert_eq!(f[1].rect, [0.7, 0.6, 0.75, 0.65]);
        assert_eq!(f[2].t0, 3_900);
        assert!((f[2].rect[0] - 0.2).abs() < 1e-6 && (f[2].rect[3] - 0.34).abs() < 1e-6);
    }

    #[test]
    fn the_camera_path_for_a_bundle_zooms_toward_its_activity() {
        let b = bundle();
        let keys = camera_rows(&b, [0.0, 0.0, 1.0, 1.0], (2560, 1440), 15_000);
        assert!(keys.iter().any(|k| k.rect[2] - k.rect[0] < 0.6), "zooms in");
        assert_eq!(keys.last().unwrap().rect, [0.0, 0.0, 1.0, 1.0], "ends wide");
    }

    #[test]
    fn default_output_lands_in_movies_scrawl() {
        let path = default_output(Path::new("/x/scrawl-17-9.scrawl"));
        #[cfg(target_os = "linux")]
        let folder = "Scrawl/scrawl-17-9.mp4";
        #[cfg(not(target_os = "linux"))]
        let folder = "Movies/Scrawl/scrawl-17-9.mp4";
        assert!(
            path.ends_with(folder),
            "{}",
            path.display()
        );
    }
}
