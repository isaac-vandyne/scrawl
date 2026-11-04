//! Optional, disposable video for a remote composer. Original capture media is never rewritten.

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "recording-web.mp4";
const CACHE_NAME: &str = ".recording-web-cache.json";
// Bump whenever encoding, timing, validation, or cache semantics change.
const RECIPE: u32 = 1;
const AUDIO_BPS: u64 = 48_000;
const MAX_PROBE_BYTES: usize = 64 * 1024;
static PREPARE: Mutex<()> = Mutex::new(());
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    bytes: u64,
    modified_ns: u128,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

fn stamp(path: &Path) -> Result<Stamp> {
    let metadata = fs::metadata(path).map_err(|_| anyhow!("Recording media is unavailable."))?;
    ensure!(metadata.is_file(), "Recording media is unavailable.");
    Ok(Stamp {
        bytes: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .ok_or_else(|| anyhow!("Recording media metadata is unavailable."))?
            .as_nanos(),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        },
    })
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Key {
    recipe: u32,
    video: String,
    video_stamp: Stamp,
    audio: Option<(String, Stamp)>,
    offset_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    key: Key,
    output: Stamp,
}

struct Temporary {
    video: PathBuf,
    cache: PathBuf,
}

impl Temporary {
    fn new(dir: &Path) -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let prefix = format!(".recording-web-{}-{id}", std::process::id());
        Self {
            video: dir.join(format!("{prefix}.partial.mp4")),
            cache: dir.join(format!("{prefix}.cache.json")),
        }
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.video);
        let _ = fs::remove_file(&self.cache);
    }
}

/// Produces and verifies a full-length MP4 no larger than one fifth of the original video.
/// This is blocking work: callers should run it off the UI thread and keep their image/text
/// delivery usable when it fails. Errors deliberately omit paths, child output, and arguments.
pub fn prepare(bundle_dir: &Path, bundle: &crate::model::ScrawlBundle) -> Result<PathBuf> {
    // Two copies of one capture must not replace the MP4 between validation and cache commit.
    let _guard = PREPARE
        .lock()
        .map_err(|_| anyhow!("Recording compression is unavailable."))?;
    let video_name = bundle
        .video
        .as_deref()
        .ok_or_else(|| anyhow!("This capture has no original recording."))?;
    ensure!(
        video_name != FILE_NAME,
        "This capture has no original recording."
    );
    let video = media_path(bundle_dir, video_name)?;
    let audio = match bundle.audio.as_deref() {
        Some(name) => {
            let path = media_path(bundle_dir, name)?;
            // Retention may already have removed a separate WAV. Keep original movie audio.
            path.is_file().then_some((name.to_owned(), path))
        }
        None => None,
    };
    let key = Key {
        recipe: RECIPE,
        video: video_name.to_owned(),
        video_stamp: stamp(&video)?,
        audio: audio
            .as_ref()
            .map(|(name, path)| -> Result<_> { Ok((name.clone(), stamp(path)?)) })
            .transpose()?,
        offset_ms: bundle.video_offset_ms,
    };
    let budget = byte_budget(key.video_stamp.bytes);
    ensure!(
        budget >= 4096,
        "This recording is too small for an 80% smaller video."
    );
    let output = bundle_dir.join(FILE_NAME);
    if let Ok(existing_output) = output.canonicalize() {
        ensure!(
            video.canonicalize().ok().as_ref() != Some(&existing_output)
                && audio.as_ref().is_none_or(|(_, path)| {
                    path.canonicalize().ok().as_ref() != Some(&existing_output)
                }),
            "The compressed recording must be separate from the original media."
        );
    }
    let cache_path = bundle_dir.join(CACHE_NAME);
    if cached(&cache_path, &output, &key, budget) {
        return Ok(output);
    }

    let tools = Tools::find()?;
    let source = probe(&tools, &video)?;
    let wav = audio.as_ref().map(|(_, path)| path.as_path());
    if let Some(wav) = wav {
        ensure!(
            probe_audio(&tools, wav)?,
            "The separate narration could not be read."
        );
    }
    let has_audio = wav.is_some() || source.has_audio;
    let temp = Temporary::new(bundle_dir);
    let first = Encoding {
        edge: 1600,
        fps: 24,
        crf: 24,
        max_bps: None,
    };
    encode(
        &tools,
        &video,
        wav,
        bundle.video_offset_ms,
        &source,
        &first,
        &temp.video,
    )?;
    if stamp(&temp.video)?.bytes > budget {
        fs::remove_file(&temp.video)
            .map_err(|_| anyhow!("Could not replace the temporary compressed recording."))?;
        // Reserve 25% for MP4 tables, rate-control variation, and short-capture overhead.
        let bps = retry_video_bps(budget, source.duration, has_audio).ok_or_else(|| {
            anyhow!("A full-length recording cannot fit the 80% reduction target.")
        })?;
        let retry = Encoding {
            edge: 960,
            fps: 20,
            crf: 30,
            max_bps: Some(bps),
        };
        encode(
            &tools,
            &video,
            wav,
            bundle.video_offset_ms,
            &source,
            &retry,
            &temp.video,
        )?;
    }
    ensure!(
        stamp(&temp.video)?.bytes <= budget,
        "The full-length recording could not reach the required 80% size reduction."
    );
    validate(&tools, &temp.video, &source, has_audio)?;
    ensure!(
        stamp(&video)? == key.video_stamp
            && match (&audio, &key.audio) {
                (Some((_, path)), Some((_, expected))) => stamp(path)? == *expected,
                (None, None) => true,
                _ => false,
            },
        "Recording media changed during compression; try copying again."
    );

    replace(&temp.video, &output)?;
    let cache = Cache {
        key,
        output: stamp(&output)?,
    };
    // A derivative remains useful if only the optional cache sidecar cannot be saved.
    if let Ok(json) = serde_json::to_vec(&cache)
        && fs::write(&temp.cache, json).is_ok()
    {
        let _ = replace(&temp.cache, &cache_path);
    }
    Ok(output)
}

fn media_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    ensure!(
        !name.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_))),
        "The capture has an invalid recording reference."
    );
    let path = dir.join(path);
    // Missing media is handled by the caller. Existing symlinks may only resolve in this bundle.
    if path.exists() {
        let root = dir
            .canonicalize()
            .map_err(|_| anyhow!("The capture folder is unavailable."))?;
        ensure!(
            path.canonicalize()
                .map_err(|_| anyhow!("Recording media is unavailable."))?
                .starts_with(root),
            "The capture has an invalid recording reference."
        );
    }
    Ok(path)
}

fn cached(cache: &Path, output: &Path, key: &Key, budget: u64) -> bool {
    let Ok(file) = File::open(cache) else {
        return false;
    };
    let Ok(cache) = serde_json::from_reader::<_, Cache>(file.take(MAX_PROBE_BYTES as u64)) else {
        return false;
    };
    cache.key == *key
        && cache.output.bytes > 0
        && cache.output.bytes <= budget
        && stamp(output).is_ok_and(|actual| actual == cache.output)
}

fn replace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    if to.exists() {
        fs::remove_file(to).map_err(|_| anyhow!("Could not save the compressed recording."))?;
    }
    fs::rename(from, to).map_err(|_| anyhow!("Could not save the compressed recording."))
}

fn byte_budget(original: u64) -> u64 {
    original / 5
}

fn retry_video_bps(budget: u64, seconds: f64, has_audio: bool) -> Option<u64> {
    let available = (budget as f64 * 0.75 * 8.0 / seconds).floor() as u64;
    let video = available.saturating_sub(if has_audio { AUDIO_BPS } else { 0 });
    (video >= 24_000).then_some(video)
}

struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl Tools {
    fn find() -> Result<Self> {
        Ok(Self {
            ffmpeg: find_program("ffmpeg")
                .ok_or_else(|| anyhow!("Video compression needs ffmpeg installed."))?,
            ffprobe: find_program("ffprobe")
                .ok_or_else(|| anyhow!("Video compression needs ffprobe installed."))?,
        })
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        dirs.push(dir.to_path_buf());
        dirs.push(dir.join("../Resources"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    // Finder-launched apps usually have no shell/Homebrew PATH.
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    #[cfg(windows)]
    let name = format!("{name}.exe");
    dirs.into_iter().map(|dir| dir.join(&name)).find(|path| {
        let Ok(metadata) = path.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// Child diagnostics never escape: they can contain file paths and embedded metadata.
fn run(command: &mut Command, timeout: Duration, capture: bool) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .map_err(|_| anyhow!("Could not start the video processor."))?;
    let reader = child.stdout.take().map(|stdout| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take((MAX_PROBE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(anyhow!("Video processing timed out or could not finish."));
            }
        }
    };
    let bytes = match reader {
        Some(reader) => reader
            .join()
            .ok()
            .and_then(std::result::Result::ok)
            .ok_or_else(|| anyhow!("Could not read video processor results."))?,
        None => Vec::new(),
    };
    ensure!(
        status?.success(),
        "The video processor could not complete this recording."
    );
    ensure!(
        bytes.len() <= MAX_PROBE_BYTES,
        "Video processor results were too large."
    );
    Ok(bytes)
}

#[derive(Default, Deserialize)]
struct ProbeData {
    #[serde(default)]
    streams: Vec<Stream>,
    #[serde(default)]
    format: Format,
}

#[derive(Default, Deserialize)]
struct Stream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    pix_fmt: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    duration: Option<String>,
}

#[derive(Default, Deserialize)]
struct Format {
    duration: Option<String>,
    #[serde(default)]
    format_name: String,
}

struct Video {
    duration: f64,
    has_audio: bool,
    h264: bool,
    yuv420p: bool,
    mp4: bool,
}

fn probe_data(tools: &Tools, path: &Path) -> Result<ProbeData> {
    let bytes = run(Command::new(&tools.ffprobe).args([
        "-v", "error", "-show_entries",
        "stream=codec_type,codec_name,pix_fmt,width,height,duration:format=duration,format_name",
        "-of", "json",
    ]).arg(path), Duration::from_secs(15), true)?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("The recording metadata could not be read."))
}

fn video_info(data: ProbeData) -> Result<Video> {
    let stream = data
        .streams
        .iter()
        .find(|s| s.codec_type == "video")
        .ok_or_else(|| anyhow!("The recording contains no readable video."))?;
    let duration = stream
        .duration
        .as_deref()
        .and_then(seconds)
        .or_else(|| data.format.duration.as_deref().and_then(seconds))
        .ok_or_else(|| anyhow!("The recording duration could not be read."))?;
    ensure!(
        stream.width >= 2 && stream.height >= 2,
        "The recording dimensions are invalid."
    );
    Ok(Video {
        duration,
        has_audio: data.streams.iter().any(|s| s.codec_type == "audio"),
        h264: stream.codec_name == "h264",
        yuv420p: stream.pix_fmt == "yuv420p",
        mp4: data.format.format_name.split(',').any(|f| f == "mp4"),
    })
}

fn probe(tools: &Tools, path: &Path) -> Result<Video> {
    video_info(probe_data(tools, path)?)
}

fn probe_audio(tools: &Tools, path: &Path) -> Result<bool> {
    Ok(probe_data(tools, path)?
        .streams
        .iter()
        .any(|s| s.codec_type == "audio"))
}

fn seconds(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)
}

struct Encoding {
    edge: u32,
    fps: u32,
    crf: u32,
    max_bps: Option<u64>,
}

fn encode(
    tools: &Tools,
    video: &Path,
    wav: Option<&Path>,
    offset_ms: u64,
    source: &Video,
    encoding: &Encoding,
    output: &Path,
) -> Result<()> {
    let mut command = Command::new(&tools.ffmpeg);
    command
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(video);
    if let Some(wav) = wav {
        command.arg("-i").arg(wav);
    }
    command.args(["-map", "0:v:0"]);
    if wav.is_some() {
        // WAV time is bundle time; the video begins offset_ms later. Pad its shorter tail,
        // never use -shortest, and never let an unusually long WAV extend the recording.
        command
            .args(["-map", "1:a:0", "-af"])
            .arg(audio_filter(offset_ms, source.duration));
    } else if source.has_audio {
        command
            .args(["-map", "0:a:0", "-af"])
            .arg(audio_filter(0, source.duration));
    } else {
        command.arg("-an");
    }
    // Use the filter's actual dimensions after ffmpeg applies display rotation (phone videos),
    // rather than stretching a rotated frame to the source's unrotated container dimensions.
    let edge = encoding.edge;
    command.args(["-vf", &format!(
            "setpts=PTS-STARTPTS,scale=w='min({edge},iw)':h='min({edge},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,fps={}:round=up:eof_action=pass,format=yuv420p",
        encoding.fps), "-c:v", "libx264", "-preset", "veryfast", "-crf", &encoding.crf.to_string(),
        "-pix_fmt", "yuv420p", "-threads", "4", "-filter_threads", "2"]);
    if let Some(bps) = encoding.max_bps {
        command.args(["-maxrate", &bps.to_string(), "-bufsize", &bps.to_string()]);
    }
    command
        .args([
            "-c:a",
            "aac",
            "-b:a",
            "48k",
            "-ac",
            "1",
            "-ar",
            "24000",
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ])
        .arg(output);
    run(&mut command, processing_timeout(source.duration), false)?;
    Ok(())
}

fn audio_filter(offset_ms: u64, duration: f64) -> String {
    format!(
        "atrim=start={:.6},asetpts=PTS-STARTPTS,apad,atrim=duration={duration:.6}",
        offset_ms as f64 / 1000.0
    )
}

fn processing_timeout(seconds: f64) -> Duration {
    Duration::from_secs_f64((15.0 + seconds * 3.0).clamp(20.0, 180.0))
}

fn validate(tools: &Tools, path: &Path, source: &Video, has_audio: bool) -> Result<()> {
    let output = probe(tools, path)?;
    // A CFR conversion can round the last frame up, but cannot lose meaningful recording time.
    ensure!(
        output.duration + 0.002 >= source.duration && output.duration <= source.duration + 0.12,
        "The compressed recording did not preserve the full video duration."
    );
    ensure!(
        output.h264 && output.yuv420p && output.mp4,
        "The compressed recording has an unsupported video format."
    );
    ensure!(
        !has_audio || output.has_audio,
        "The compressed recording lost its narration."
    );
    ensure!(
        faststart(path)?,
        "The compressed recording is not ready for web playback."
    );
    run(
        Command::new(&tools.ffmpeg)
            .args(["-nostdin", "-v", "error", "-xerror", "-i"])
            .arg(path)
            .args(["-map", "0:v:0", "-map", "0:a?", "-f", "null", "-"]),
        processing_timeout(source.duration),
        false,
    )?;
    Ok(())
}

fn faststart(path: &Path) -> Result<bool> {
    let check = || -> std::io::Result<bool> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let mut offset = 0u64;
        while offset.saturating_add(8) <= length {
            let mut header = [0u8; 8];
            file.read_exact(&mut header)?;
            let mut size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
            let mut header_size = 8;
            if size == 1 {
                let mut extended = [0; 8];
                file.read_exact(&mut extended)?;
                size = u64::from_be_bytes(extended);
                header_size = 16;
            }
            if size == 0 {
                size = length - offset;
            }
            if size < header_size || size > length - offset {
                return Ok(false);
            }
            if &header[4..] == b"moov" {
                return Ok(true);
            }
            if &header[4..] == b"mdat" {
                return Ok(false);
            }
            offset += size;
            file.seek(SeekFrom::Start(offset))?;
        }
        Ok(false)
    };
    check().map_err(|_| anyhow!("The compressed recording could not be validated."))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "scrawl-web-recording-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn bundle(video: &str, audio: Option<&str>, offset_ms: u64) -> crate::model::ScrawlBundle {
        serde_json::from_value(serde_json::json!({
            "version": 1, "id": "test", "createdAtMs": 0, "durationMs": 2500,
            "target": {"mode": "display", "displayId": 0,
                "frame": {"x": 0, "y": 0, "width": 640, "height": 480, "scale": 1}},
            "transcript": {"source": "test", "text": "", "segments": []},
            "strokes": [], "marks": [], "events": [], "keyframes": [], "contactSheet": "c.png",
            "video": video, "audio": audio, "videoOffsetMs": offset_ms
        }))
        .unwrap()
    }

    #[test]
    fn budget_never_rounds_above_twenty_percent() {
        assert_eq!(byte_budget(104), 20);
        assert_eq!(byte_budget(u64::MAX), u64::MAX / 5);
        assert!(retry_video_bps(10_000, 60.0, true).is_none());
        assert!(retry_video_bps(1_000_000, 10.0, true).unwrap() < 800_000);
    }

    #[test]
    fn duration_uses_video_not_a_longer_audio_track_and_rejects_nonfinite_values() {
        let data = serde_json::from_str(r#"{"streams":[{"codec_type":"video","width":4,"height":4,"duration":"2"}],"format":{"duration":"99"}}"#).unwrap();
        assert_eq!(video_info(data).unwrap().duration, 2.0);
        for value in ["NaN", "inf", "-1", "0", "N/A"] {
            assert!(seconds(value).is_none());
        }
    }

    #[test]
    fn cache_tracks_both_media_files_offset_recipe_and_output() {
        let dir = TestDir::new();
        let video = dir.0.join("recording.mp4");
        let audio = dir.0.join("audio.wav");
        let out = dir.0.join(FILE_NAME);
        let cache_path = dir.0.join(CACHE_NAME);
        fs::write(&video, vec![0; 20_000]).unwrap();
        fs::write(&audio, [0; 10]).unwrap();
        fs::write(&out, [0; 100]).unwrap();
        let mut key = Key {
            recipe: RECIPE,
            video: "recording.mp4".into(),
            video_stamp: stamp(&video).unwrap(),
            audio: Some(("audio.wav".into(), stamp(&audio).unwrap())),
            offset_ms: 5,
        };
        fs::write(
            &cache_path,
            serde_json::to_vec(&Cache {
                key: Key {
                    recipe: key.recipe,
                    video: key.video.clone(),
                    video_stamp: key.video_stamp.clone(),
                    audio: key.audio.clone(),
                    offset_ms: key.offset_ms,
                },
                output: stamp(&out).unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(cached(&cache_path, &out, &key, 4000));
        key.offset_ms += 1;
        assert!(!cached(&cache_path, &out, &key, 4000));
        key.offset_ms -= 1;
        key.recipe += 1;
        assert!(!cached(&cache_path, &out, &key, 4000));
        key.recipe -= 1;
        key.audio.as_mut().unwrap().1.bytes += 1;
        assert!(!cached(&cache_path, &out, &key, 4000));
        key.audio.as_mut().unwrap().1.bytes -= 1;
        key.video_stamp.bytes += 1;
        assert!(!cached(&cache_path, &out, &key, 4000));
        key.video_stamp.bytes -= 1;
        fs::write(&out, [0; 101]).unwrap();
        assert!(!cached(&cache_path, &out, &key, 4000));
    }

    #[test]
    fn unsafe_references_and_private_paths_do_not_escape_in_errors() {
        let dir = TestDir::new();
        for name in ["../private/recording.mp4", "/private/recording.mp4", ""] {
            let error = prepare(&dir.0, &bundle(name, None, 0))
                .unwrap_err()
                .to_string();
            assert!(!error.contains(name) || name.is_empty());
            assert!(!error.contains(dir.0.to_str().unwrap()));
        }
    }

    #[test]
    fn mp4_must_have_a_complete_moov_before_media_data() {
        let dir = TestDir::new();
        let path = dir.0.join("atoms.mp4");
        fs::write(&path, b"\0\0\0\x08ftyp\0\0\0\x08moov\0\0\0\x08mdat").unwrap();
        assert!(faststart(&path).unwrap());
        fs::write(&path, b"\0\0\0\x08ftyp\0\0\0\x08mdat\0\0\0\x08moov").unwrap();
        assert!(!faststart(&path).unwrap());
        fs::write(&path, b"\0\0\0\xffmoov").unwrap();
        assert!(!faststart(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn child_timeout_terminates_and_reaps_the_processor() {
        let started = Instant::now();
        assert!(
            run(
                Command::new("/bin/sleep").arg("5"),
                Duration::from_millis(30),
                false
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn real_encoding_keeps_narration_alignment_duration_and_originals() {
        let Ok(tools) = Tools::find() else { return };
        let dir = TestDir::new();
        let video = dir.0.join("recording.mp4");
        run(
            Command::new(&tools.ffmpeg)
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=640x480:rate=60",
                    "-t",
                    "2",
                    "-c:v",
                    "libx264",
                    "-crf",
                    "0",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(&video),
            Duration::from_secs(20),
            false,
        )
        .unwrap();
        let audio = dir.0.join("audio.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut wav = hound::WavWriter::create(&audio, spec).unwrap();
        for sample in 0..20_000 {
            let value = if sample < 4000 {
                0
            } else {
                ((sample as f64 * 440.0 * std::f64::consts::TAU / 8000.0).sin() * 16000.0) as i16
            };
            wav.write_sample(value).unwrap();
        }
        wav.finalize().unwrap();
        let original = stamp(&video).unwrap();
        let original_audio = stamp(&audio).unwrap();
        let output = prepare(&dir.0, &bundle("recording.mp4", Some("audio.wav"), 500)).unwrap();
        let encoded = probe(&tools, &output).unwrap();
        assert!(encoded.has_audio && encoded.h264 && encoded.yuv420p && encoded.mp4);
        assert!(encoded.duration >= 2.0 && encoded.duration <= 2.12);
        assert!(stamp(&output).unwrap().bytes <= byte_budget(original.bytes));
        assert_eq!(stamp(&video).unwrap(), original);
        assert_eq!(stamp(&audio).unwrap(), original_audio);
        // The first half-second was silence in the WAV: trimming its offset makes the video's
        // first 100 ms audible. Delaying the WAV instead would incorrectly produce silence.
        let pcm = run(
            Command::new(&tools.ffmpeg)
                .args(["-nostdin", "-v", "error", "-i"])
                .arg(&output)
                .args([
                    "-map",
                    "0:a:0",
                    "-t",
                    "0.1",
                    "-f",
                    "f32le",
                    "-acodec",
                    "pcm_f32le",
                    "-",
                ]),
            Duration::from_secs(10),
            true,
        )
        .unwrap();
        let power = pcm
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()).powi(2))
            .sum::<f32>()
            / (pcm.len() / 4) as f32;
        assert!(
            power > 0.01,
            "Narration was not aligned to the video's start"
        );
        let encoded_stamp = stamp(&output).unwrap();
        assert_eq!(
            prepare(&dir.0, &bundle("recording.mp4", Some("audio.wav"), 500)).unwrap(),
            output
        );
        assert_eq!(
            stamp(&output).unwrap(),
            encoded_stamp,
            "A cache hit must not recompress"
        );

        // A phone-style movie already owns its audio and may store display rotation separately.
        // This also proves changing the source invalidates the previous cached derivative.
        let phone = dir.0.join("phone.mp4");
        run(
            Command::new(&tools.ffmpeg)
                .args(["-nostdin", "-v", "error", "-display_rotation", "90", "-i"])
                .arg(&video)
                .args(["-ss", "0.5", "-i"])
                .arg(&audio)
                .args([
                    "-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy", "-c:a", "aac", "-t", "2",
                ])
                .arg(&phone),
            Duration::from_secs(20),
            false,
        )
        .unwrap();
        let phone_stamp = stamp(&phone).unwrap();
        let output = prepare(&dir.0, &bundle("phone.mp4", None, 0)).unwrap();
        let data = probe_data(&tools, &output).unwrap();
        let picture = data
            .streams
            .iter()
            .find(|s| s.codec_type == "video")
            .unwrap();
        assert_eq!((picture.width, picture.height), (480, 640));
        assert!(data.streams.iter().any(|s| s.codec_type == "audio"));
        assert_eq!(stamp(&phone).unwrap(), phone_stamp);
        assert!(stamp(&output).unwrap().bytes <= byte_budget(phone_stamp.bytes));
    }

    /// Explicit, local-only measurement. Copies media into a disposable directory; no feedback,
    /// transcript, recording content, or source path is printed.
    #[test]
    #[ignore = "requires SCRAWL_RECORDING_TEST_BUNDLE pointing to a real local capture"]
    fn measure_local_capture() {
        let path = PathBuf::from(
            std::env::var_os("SCRAWL_RECORDING_TEST_BUNDLE")
                .expect("Set the local capture fixture"),
        );
        let mut bundle: crate::model::ScrawlBundle = serde_json::from_reader(
            File::open(path.join("scrawl.json")).expect("Read the local capture fixture"),
        )
        .expect("Read capture metadata");
        let dir = TestDir::new();
        fs::copy(
            path.join(bundle.video.as_ref().unwrap()),
            dir.0.join("source.mp4"),
        )
        .unwrap();
        bundle.video = Some("source.mp4".into());
        if let Some(name) = &bundle.audio {
            fs::copy(path.join(name), dir.0.join("source.wav")).unwrap();
            bundle.audio = Some("source.wav".into());
        }
        let source = stamp(&dir.0.join("source.mp4")).unwrap();
        let started = Instant::now();
        let output = prepare(&dir.0, &bundle).unwrap();
        let elapsed = started.elapsed();
        let compressed = stamp(&output).unwrap();
        let tools = Tools::find().unwrap();
        let info = probe(&tools, &output).unwrap();
        let cached_at = Instant::now();
        prepare(&dir.0, &bundle).unwrap();
        eprintln!(
            "recording measurement: original={} bytes; compressed={} bytes; reduction={:.2}%; duration={:.6}s; audio={}; prepare={:.3}s; cache={:.3}s",
            source.bytes,
            compressed.bytes,
            100.0 * (1.0 - compressed.bytes as f64 / source.bytes as f64),
            info.duration,
            info.has_audio,
            elapsed.as_secs_f64(),
            cached_at.elapsed().as_secs_f64()
        );
        assert_eq!(stamp(&output).unwrap(), compressed);
        assert_eq!(stamp(&dir.0.join("source.mp4")).unwrap(), source);
    }
}
