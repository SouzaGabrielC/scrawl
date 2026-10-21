//! `scrawl-helper`, the Swift executable shipped beside `scrawl` (see `helper/main.swift`).
//!
//! It owns the Apple frameworks that are Swift-only or absent from the objc2 bindings Scrawl
//! uses: SpeechAnalyzer transcription, CoreMediaIO/AVFoundation iPhone capture with a live
//! mirror window, and AVFoundation movie decoding.

#[cfg(target_os = "macos")]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

#[cfg(target_os = "macos")]
use crate::model::{TranscriptSegment, WordTiming};
use crate::model::{RgbaFrame, TranscriptData};

#[cfg(target_os = "macos")]
pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SCRAWL_HELPER") {
        return Ok(PathBuf::from(path));
    }
    let raw_exe = std::env::current_exe().context("Could not locate scrawl")?;
    let exe = std::fs::canonicalize(&raw_exe).unwrap_or(raw_exe);
    // Beside the app binary, or one level up from `target/*/deps` for tests.
    exe.ancestors()
        .skip(1)
        .take(2)
        .map(|dir| dir.join("scrawl-helper"))
        .find(|path| path.is_file())
        .context("scrawl-helper is missing; rebuild Scrawl")
}

#[cfg(target_os = "linux")]
pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SCRAWL_HELPER") {
        return Ok(PathBuf::from(path));
    }
    let raw_exe = std::env::current_exe().context("Could not locate scrawl")?;
    let exe = std::fs::canonicalize(&raw_exe).unwrap_or(raw_exe);
    if let Some(dir) = exe.parent() {
        let beside = dir.join("scrawl-helper");
        if beside.is_file() {
            return Ok(beside);
        }
    }
    Ok(PathBuf::from("scrawl-helper"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn path() -> Result<PathBuf> {
    Ok(PathBuf::from("scrawl-helper"))
}

/// Runs a helper command to completion and parses its JSON output.
#[cfg(target_os = "macos")]
fn run<T: for<'de> Deserialize<'de>>(args: &[&std::ffi::OsStr], timeout: Duration) -> Result<T> {
    run_program(&path()?, args, timeout)
}

#[cfg(target_os = "macos")]
fn run_program<T: for<'de> Deserialize<'de>>(
    program: &Path,
    args: &[&std::ffi::OsStr],
    timeout: Duration,
) -> Result<T> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Could not start scrawl-helper")?;
    let mut stdout = child.stdout.take().context("No helper stdout")?;
    let mut stderr = child.stderr.take().context("No helper stderr")?;
    let caller = std::thread::current();
    let eof = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_eof = std::sync::Arc::clone(&eof);
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        reader_eof.store(true, std::sync::atomic::Ordering::Release);
        caller.unpark();
        out
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if eof.load(std::sync::atomic::Ordering::Acquire) {
            break child.wait()?;
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("scrawl-helper {:?} timed out", args.first());
        }
        std::thread::park_timeout(deadline - now);
    };
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        let mut message = String::new();
        let _ = stderr.read_to_string(&mut message);
        bail!("{}", message.trim());
    }
    serde_json::from_slice(&out).context("Invalid scrawl-helper output")
}

#[cfg(target_os = "macos")]
fn os(s: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(s)
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PhoneDevice {
    pub id: String,
    pub name: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub duration_ms: u64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub audio_tracks: u32,
    /// The video track's nominal frame rate (0 when unknown).
    #[serde(default)]
    pub fps: f64,
}

pub enum FrameTimes<'a> {
    /// Nearest decodable frame every N ms, stamped with its actual time.
    Every(u64),
    /// Exactly these times.
    At(&'a [u64]),
}

/// Parses standard `ffprobe -print_format json -show_streams -show_format` output into `Probe`.
pub fn parse_ffprobe_json(json: &str) -> Result<Probe> {
    #[derive(Deserialize)]
    struct RawProbe {
        #[serde(default)]
        streams: Vec<RawStream>,
        #[serde(default)]
        format: Option<RawFormat>,
    }

    #[derive(Deserialize)]
    struct RawStream {
        #[serde(default)]
        codec_type: Option<String>,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
        #[serde(default)]
        r_frame_rate: Option<String>,
        #[serde(default)]
        duration: Option<String>,
    }

    #[derive(Deserialize)]
    struct RawFormat {
        #[serde(default)]
        duration: Option<String>,
    }

    let parsed: RawProbe =
        serde_json::from_str(json).context("Invalid ffprobe JSON output")?;

    let mut width = 0;
    let mut height = 0;
    let mut fps = 0.0;
    let mut audio_tracks = 0;
    let mut video_duration_sec: Option<f64> = None;

    for s in &parsed.streams {
        if s.codec_type.as_deref() == Some("video") {
            if width == 0 && height == 0 {
                width = s.width.unwrap_or(0);
                height = s.height.unwrap_or(0);
            }
            if fps == 0.0 {
                if let Some(ref r_fps) = s.r_frame_rate {
                    if let Some((num, den)) = r_fps.split_once('/') {
                        if let (Ok(n), Ok(d)) = (num.trim().parse::<f64>(), den.trim().parse::<f64>())
                        {
                            if d > 0.0 {
                                fps = n / d;
                            }
                        }
                    } else if let Ok(val) = r_fps.trim().parse::<f64>() {
                        fps = val;
                    }
                }
            }
            if video_duration_sec.is_none() {
                if let Some(ref d_str) = s.duration {
                    if let Ok(d) = d_str.trim().parse::<f64>() {
                        video_duration_sec = Some(d);
                    }
                }
            }
        } else if s.codec_type.as_deref() == Some("audio") {
            audio_tracks += 1;
        }
    }

    let format_duration_sec = parsed
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.trim().parse::<f64>().ok());

    let duration_sec = format_duration_sec.or(video_duration_sec).unwrap_or(0.0);
    let duration_ms = (duration_sec * 1000.0).round() as u64;

    Ok(Probe {
        duration_ms,
        width,
        height,
        audio_tracks,
        fps,
    })
}

// MARK: macOS scrawl-helper implementations

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct Transcript {
    locale: String,
    text: String,
    segments: Vec<TranscriptSegment>,
    words: Vec<WordTiming>,
}

#[cfg(target_os = "macos")]
pub fn permissions()
-> Result<std::collections::BTreeMap<crate::onboarding::Permission, crate::onboarding::Status>> {
    run(&[os("permissions")], Duration::from_secs(5))
}

#[cfg(target_os = "macos")]
pub fn devices() -> Result<Vec<PhoneDevice>> {
    run(&[os("devices")], Duration::from_secs(5))
}

#[cfg(target_os = "macos")]
pub fn transcribe(audio: &Path) -> Result<TranscriptData> {
    let transcript: Transcript = run(
        &[os("transcribe"), audio.as_os_str()],
        Duration::from_secs(180),
    )?;
    Ok(TranscriptData {
        source: "speech-analyzer".into(),
        model: Some(format!("SpeechTranscriber {}", transcript.locale)),
        text: transcript.text,
        segments: transcript.segments,
        words: transcript.words,
        estimated: false,
    })
}

#[cfg(target_os = "macos")]
pub fn probe(movie: &Path) -> Result<Probe> {
    run(&[os("probe"), movie.as_os_str()], Duration::from_secs(30))
}

#[cfg(target_os = "macos")]
pub fn shrink(movie: &Path, out: &Path, max_edge: u32) -> Result<crate::retention::Shrunk> {
    let edge = max_edge.to_string();
    let helper = path()?;
    let args = [
        helper.as_os_str(),
        os("shrink"),
        movie.as_os_str(),
        os("--out"),
        out.as_os_str(),
        os("--max-edge"),
        os(&edge),
    ];
    run_program(
        Path::new("/usr/sbin/taskpolicy"),
        &[&[os("-c"), os("utility")], &args[..]].concat(),
        Duration::from_secs(1800),
    )
}

#[cfg(target_os = "macos")]
pub fn export_audio(movie: &Path, wav: &Path) -> Result<()> {
    let _: serde_json::Value = run(
        &[os("audio"), movie.as_os_str(), wav.as_os_str()],
        Duration::from_secs(120),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct FrameFile {
    t: u64,
    actual: u64,
    width: u32,
    height: u32,
    file: PathBuf,
}

#[cfg(target_os = "macos")]
pub fn frames(movie: &Path, times: FrameTimes, max_edge: u32) -> Result<Vec<RgbaFrame>> {
    let dir = std::env::temp_dir().join(format!("scrawl-frames-{}", crate::make_bundle_id()));
    let (flag, value, sampled) = match times {
        FrameTimes::Every(ms) => ("--every", ms.max(1).to_string(), true),
        FrameTimes::At(times) => {
            if times.is_empty() {
                return Ok(Vec::new());
            }
            let list = times.iter().map(u64::to_string).collect::<Vec<_>>();
            ("--at", list.join(","), false)
        }
    };
    let edge = max_edge.to_string();
    let result: Result<Vec<FrameFile>> = run(
        &[
            os("frames"),
            movie.as_os_str(),
            os("--out-dir"),
            dir.as_os_str(),
            os(flag),
            os(&value),
            os("--max-edge"),
            os(&edge),
        ],
        Duration::from_secs(300),
    );
    let frames = result.and_then(|files| {
        if files.len() <= 1 {
            files
                .into_iter()
                .map(|file| {
                    let rgba = std::fs::read(&file.file)?;
                    ensure!(
                        rgba.len() == file.width as usize * file.height as usize * 4,
                        "Truncated helper frame"
                    );
                    let t = if sampled { file.actual } else { file.t };
                    Ok(RgbaFrame::new(t, file.width, file.height, rgba))
                })
                .collect()
        } else {
            let chunk_size = files.len().div_ceil(6).max(1);
            std::thread::scope(|scope| -> Result<Vec<RgbaFrame>> {
                let workers: Vec<_> = files
                    .chunks(chunk_size)
                    .map(|chunk| {
                        scope.spawn(move || -> Result<Vec<RgbaFrame>> {
                            chunk
                                .iter()
                                .map(|file| {
                                    let rgba = std::fs::read(&file.file)?;
                                    ensure!(
                                        rgba.len()
                                            == file.width as usize * file.height as usize * 4,
                                        "Truncated helper frame"
                                    );
                                    let t = if sampled { file.actual } else { file.t };
                                    Ok(RgbaFrame::new(t, file.width, file.height, rgba))
                                })
                                .collect()
                        })
                    })
                    .collect();
                let mut out = Vec::with_capacity(files.len());
                for worker in workers {
                    out.extend(
                        worker
                            .join()
                            .map_err(|_| anyhow::anyhow!("Frame read worker panicked"))??,
                    );
                }
                Ok(out)
            })
        }
    });
    std::thread::spawn(move || {
        let _ = std::fs::remove_dir_all(dir);
    });
    frames
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct QrOutput {
    qr: String,
}

#[cfg(target_os = "macos")]
pub fn qr(text: &str) -> Result<String> {
    let out: QrOutput = run(&[os("qr"), os(text)], Duration::from_secs(5))?;
    Ok(out.qr)
}

// MARK: Linux ffmpeg / ffprobe implementations

#[cfg(target_os = "linux")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn permissions()
-> Result<std::collections::BTreeMap<crate::onboarding::Permission, crate::onboarding::Status>> {
    use crate::onboarding::{Permission, Status};
    let mut map = std::collections::BTreeMap::new();
    map.insert(Permission::ScreenRecording, Status::Granted);
    map.insert(Permission::Microphone, Status::Granted);
    map.insert(Permission::Accessibility, Status::Granted);
    map.insert(Permission::Camera, Status::Granted);
    Ok(map)
}

#[cfg(target_os = "linux")]
pub fn devices() -> Result<Vec<PhoneDevice>> {
    Ok(Vec::new())
}

#[cfg(target_os = "linux")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn transcribe(_audio: &Path) -> Result<TranscriptData> {
    bail!("Apple SpeechAnalyzer is not available on Linux; use Hex or Parakeet")
}

#[cfg(target_os = "linux")]
pub fn probe(movie: &Path) -> Result<Probe> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(movie)
        .output()
        .context("Failed to run ffprobe")?;
    ensure!(
        output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json_str = String::from_utf8(output.stdout).context("Invalid UTF-8 from ffprobe")?;
    parse_ffprobe_json(&json_str)
}

#[cfg(target_os = "linux")]
pub fn shrink(movie: &Path, out: &Path, max_edge: u32) -> Result<crate::retention::Shrunk> {
    let before = movie.metadata()?.len();
    let start = Instant::now();
    let edge_str = format!("scale=-2:{}", max_edge.min(720));
    let mut cmd = Command::new("nice");
    cmd.args(["-n", "19", "ionice", "-c", "3", "ffmpeg", "-y", "-i"])
        .arg(movie)
        .args([
            "-vf",
            &edge_str,
            "-r",
            "15",
            "-c:v",
            "libx264",
            "-crf",
            "28",
            "-preset",
            "veryfast",
        ])
        .arg(out);
    let output = cmd
        .output()
        .or_else(|_| {
            Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(movie)
                .args([
                    "-vf",
                    &edge_str,
                    "-r",
                    "15",
                    "-c:v",
                    "libx264",
                    "-crf",
                    "28",
                    "-preset",
                    "veryfast",
                ])
                .arg(out)
                .output()
        })
        .context("Failed to run ffmpeg shrink")?;
    ensure!(
        output.status.success() && out.exists(),
        "ffmpeg shrink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = out.metadata()?.len();
    let p = probe(out).unwrap_or(Probe {
        duration_ms: 0,
        width: 0,
        height: 0,
        audio_tracks: 0,
        fps: 0.0,
    });
    Ok(crate::retention::Shrunk {
        before,
        after,
        width: p.width,
        height: p.height,
        ms: start.elapsed().as_millis() as u64,
    })
}

#[cfg(target_os = "linux")]
pub fn export_audio(movie: &Path, wav: &Path) -> Result<()> {
    let output = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(movie)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "wav"])
        .arg(wav)
        .output()
        .context("Failed to run ffmpeg audio extraction")?;
    ensure!(
        output.status.success() && wav.exists(),
        "ffmpeg audio extraction failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn frames(movie: &Path, times: FrameTimes, max_edge: u32) -> Result<Vec<RgbaFrame>> {
    let list: Vec<u64> = match times {
        FrameTimes::Every(ms) => {
            let p = probe(movie)?;
            let step = ms.max(1);
            let mut t = 0;
            let mut res = Vec::new();
            while t < p.duration_ms {
                res.push(t);
                t += step;
            }
            res
        }
        FrameTimes::At(slice) => slice.to_vec(),
    };
    if list.is_empty() {
        return Ok(Vec::new());
    }
    let p = probe(movie)?;
    let (w, h) = if max_edge > 0 && p.width.max(p.height) > max_edge {
        let scale = max_edge as f32 / p.width.max(p.height) as f32;
        (
            (((p.width as f32 * scale) / 2.0).round() * 2.0) as u32,
            (((p.height as f32 * scale) / 2.0).round() * 2.0) as u32,
        )
    } else {
        (p.width, p.height)
    };
    let mut out_frames = Vec::with_capacity(list.len());
    for &t_ms in &list {
        let sec = format!("{:.3}", t_ms as f64 / 1000.0);
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-ss")
            .arg(&sec)
            .arg("-i")
            .arg(movie)
            .arg("-frames:v")
            .arg("1");
        if max_edge > 0 && (w != p.width || h != p.height) {
            cmd.arg("-vf").arg(format!("scale={w}:{h}"));
        }
        cmd.arg("-f").arg("rawvideo").arg("-pix_fmt").arg("rgba").arg("-");
        let output = cmd.output().context("Failed to run ffmpeg frame extraction")?;
        if output.status.success() && output.stdout.len() == (w as usize) * (h as usize) * 4 {
            out_frames.push(RgbaFrame::new(t_ms, w, h, output.stdout));
        }
    }
    Ok(out_frames)
}

#[cfg(target_os = "linux")]
pub fn qr(text: &str) -> Result<String> {
    Ok(format!("[QR: {text}]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffprobe_json_parses_dimensions_fps_duration_and_audio_tracks() {
        let sample = r#"{
            "streams": [
                {"codec_type": "video", "width": 1920, "height": 1080, "r_frame_rate": "30/1", "duration": "12.500"},
                {"codec_type": "audio", "duration": "12.500"}
            ],
            "format": {"duration": "12.500"}
        }"#;
        let probe = parse_ffprobe_json(sample).unwrap();
        assert_eq!((probe.width, probe.height), (1920, 1080));
        assert_eq!(probe.duration_ms, 12_500);
        assert_eq!(probe.audio_tracks, 1);
        assert!((probe.fps - 30.0).abs() < 1e-3);
    }
}

