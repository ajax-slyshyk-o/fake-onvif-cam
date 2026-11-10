//! Encode-once cache (`cache = true` / `--cache`).
//!
//! Decoding and encoding the video is what costs CPU. With the cache the whole `file` is
//! encoded once, at startup, with the camera's settings (resolution, codec, bitrate, overlay,
//! sub stream, ...) into one MP4 file per stream. A running camera then only stream-copies
//! those files to RTP (`ffmpeg::spawn_cached`): no decode, no encode.
//!
//! The file name contains a hash of the exact ffmpeg arguments and of the size and modification
//! time of the source videos, so changing any setting or the video creates a new cache file and
//! an unchanged setup reuses the old one. Old files are not deleted.

use crate::config::{Config, MediaInput};
use crate::ffmpeg;
use md5::{Digest as _, Md5};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Bumped when the cache file format changes, so old files are not reused.
const VERSION: &str = "1";

/// Where cache files go: `cache_dir`, or `fake-onvif-cam-cache` in the system temp directory.
pub fn cache_dir(config: &Config) -> PathBuf {
    config
        .cache_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("fake-onvif-cam-cache"))
}

/// Makes the cache files of `config` available (creating them when missing) and records them
/// in `config.cache_files`. On any problem the camera simply encodes live.
pub fn prepare(config: &mut Config) {
    config.cache_files.clear();
    if !config.cache || config.no_ffmpeg {
        return;
    }
    let MediaInput::File(main) = config.media_input.clone() else {
        println!(
            "cache: ignored for {}, it needs a video file",
            config.camera_name
        );
        return;
    };

    match prepare_files(config, &main) {
        Ok(files) => config.cache_files = files,
        Err(err) => eprintln!("warning: cache disabled for {}: {err}", config.camera_name),
    }
}

fn prepare_files(config: &Config, main: &Path) -> Result<Vec<PathBuf>, String> {
    let sources: Vec<&Path> = std::iter::once(main)
        .chain(config.sub.as_ref().and_then(|sub| sub.file.as_deref()))
        .collect();
    let identities = sources
        .iter()
        .map(|path| identity(path))
        .collect::<Result<Vec<_>, _>>()?;

    let names: Vec<&str> = config.streams().iter().map(|stream| stream.name).collect();
    let placeholders: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    let key = cache_key(&build_command(config, &placeholders)?, &identities);

    let stem = sanitize(&main.file_stem().unwrap_or_default().to_string_lossy());
    let dir = cache_dir(config);
    let files: Vec<PathBuf> = names
        .iter()
        .map(|name| dir.join(format!("{stem}-{key}-{name}.mp4")))
        .collect();

    if files.iter().all(|file| file.exists()) {
        println!("cache: using {}", describe(&files));
        return Ok(files);
    }

    println!(
        "cache: encoding {} once into {} (the first start takes a while)",
        main.display(),
        describe(&files)
    );
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;

    let parts: Vec<PathBuf> = files.iter().map(|file| part_path(file)).collect();
    let status = build_command(config, &parts)?
        .status()
        .map_err(|err| format!("ffmpeg failed to run: {err}"));
    let result = match status {
        Ok(status) if status.success() => parts.iter().zip(&files).try_for_each(|(part, file)| {
            std::fs::rename(part, file)
                .map_err(|err| format!("cannot rename {}: {err}", part.display()))
        }),
        Ok(status) => Err(format!("ffmpeg exited with {status}")),
        Err(err) => Err(err),
    };

    if result.is_err() {
        for part in &parts {
            let _ = std::fs::remove_file(part);
        }
    }
    result.map(|()| files)
}

/// The one-shot ffmpeg command that writes `outputs` (one MP4 file per stream). It runs as
/// fast as possible (no `-re`) and reads every input once. The main file also gets the audio
/// of the source, when the camera has an audio track taken from it.
fn build_command(config: &Config, outputs: &[PathBuf]) -> Result<Command, String> {
    let MediaInput::File(main) = &config.media_input else {
        return Err("no video file".to_string());
    };

    let mut command = Command::new(&config.ffmpeg_path);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(main);
    if let Some(sub_file) = config.sub.as_ref().and_then(|sub| sub.file.as_ref()) {
        command.arg("-i").arg(sub_file);
    }

    let with_audio = config.audio.is_some() && config.audio_from_file;
    ffmpeg::add_video_outputs(&mut command, config, |command, stream, index| {
        if index == 0 && with_audio {
            command.args(["-map", "0:a:0", "-c:a", "aac", "-b:a", "64k", "-ac", "1"]);
        }
        ffmpeg::add_video_encode(command, stream);
        command.args(["-f", "mp4"]).arg(&outputs[index]);
    })
    .map_err(|err| err.to_string())?;

    Ok(command)
}

/// Hash of everything the cached content depends on.
fn cache_key(command: &Command, identities: &[(u64, u64)]) -> String {
    let mut hasher = Md5::new();
    hasher.update(VERSION.as_bytes());
    hasher.update(command.get_program().as_encoded_bytes());
    for arg in command.get_args() {
        hasher.update(arg.as_encoded_bytes());
        hasher.update([0]);
    }
    for (len, modified) in identities {
        hasher.update(len.to_le_bytes());
        hasher.update(modified.to_le_bytes());
    }

    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Size and modification time (seconds) of a source video.
fn identity(path: &Path) -> Result<(u64, u64), String> {
    let metadata =
        std::fs::metadata(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs());
    Ok((metadata.len(), modified))
}

/// Deletes the cache files of the cameras that have `cache_clean` when it is dropped, so keep
/// it alive for the whole run and declare it before the ffmpeg guards: they stop ffmpeg first
/// (Windows cannot delete a file that is still open).
pub struct Cleanup {
    files: Vec<PathBuf>,
}

impl Cleanup {
    /// `leaders` are the cameras that run ffmpeg, after [`prepare`].
    pub fn new<'a>(leaders: impl IntoIterator<Item = &'a Config>) -> Self {
        let mut files: Vec<PathBuf> = leaders
            .into_iter()
            .filter(|config| config.cache_clean)
            .flat_map(|config| config.cache_files.iter().cloned())
            .collect();
        files.sort();
        files.dedup();
        Self { files }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = std::fs::remove_file(file);
            let _ = std::fs::remove_file(part_path(file));
        }
        if !self.files.is_empty() {
            println!("cache: removed {} file(s)", self.files.len());
        }
    }
}

fn part_path(file: &Path) -> PathBuf {
    let mut name: OsString = file.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    file.with_file_name(name)
}

fn sanitize(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "video".to_string()
    } else {
        clean
    }
}

fn describe(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SubStream;

    fn config() -> Config {
        Config {
            media_input: MediaInput::File(PathBuf::from("/videos/My Clip.mp4")),
            cache: true,
            overlay_font: None,
            ..Config::default()
        }
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn outputs(config: &Config) -> Vec<PathBuf> {
        config
            .streams()
            .iter()
            .map(|stream| PathBuf::from(format!("out-{}.mp4", stream.name)))
            .collect()
    }

    #[test]
    fn command_encodes_once_to_mp4_without_realtime_or_loop() {
        let config = config();
        let args = args(&build_command(&config, &outputs(&config)).unwrap());

        assert!(args.contains(&"mp4".to_string()));
        assert!(args.contains(&"libx264".to_string()));
        assert!(!args.contains(&"-re".to_string()));
        assert!(!args.contains(&"-stream_loop".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("out-main.mp4"));
    }

    #[test]
    fn sub_stream_gets_its_own_file_and_audio_goes_to_main_only() {
        let mut config = config();
        config.sub = Some(SubStream {
            width: None,
            height: None,
            fps: None,
            bitrate_kbps: 512,
            rtsp_path: "live_sub".to_string(),
            rtp_port: None,
            codec: "libx264".to_string(),
            preset: None,
            file: None,
        });
        config.audio = Some(crate::config::AudioCodec::Pcmu);
        config.audio_from_file = true;

        let args = args(&build_command(&config, &outputs(&config)).unwrap());
        let main_end = args.iter().position(|arg| arg == "out-main.mp4").unwrap();
        let sub_end = args.iter().position(|arg| arg == "out-sub.mp4").unwrap();
        let audio_map = args.iter().position(|arg| arg == "0:a:0").unwrap();

        assert!(audio_map < main_end, "audio belongs to the main output");
        assert!(main_end < sub_end);
        assert_eq!(args.iter().filter(|arg| *arg == "0:a:0").count(), 1);
    }

    #[test]
    fn key_is_stable_and_changes_with_settings_and_source() {
        let config = config();
        let identity = [(100, 5)];
        let key = |config: &Config, outputs: &[PathBuf]| {
            cache_key(&build_command(config, outputs).unwrap(), &identity)
        };

        let base = key(&config, &outputs(&config));
        assert_eq!(base.len(), 16);
        assert_eq!(base, key(&config, &outputs(&config)), "stable");

        let mut other = config.clone();
        other.bitrate_kbps += 1;
        assert_ne!(base, key(&other, &outputs(&other)));

        other = config.clone();
        other.overlay_text = Some("another".to_string());
        assert_ne!(base, key(&other, &outputs(&other)));

        let changed_file = cache_key(
            &build_command(&config, &outputs(&config)).unwrap(),
            &[(101, 5)],
        );
        assert_ne!(base, changed_file);
    }

    #[test]
    fn names_and_directory() {
        assert_eq!(sanitize("My Clip.v2"), "My_Clip_v2");
        assert_eq!(sanitize(""), "video");
        assert_eq!(
            part_path(Path::new("/c/clip-1-main.mp4")),
            PathBuf::from("/c/clip-1-main.mp4.part")
        );

        let mut config = config();
        assert!(cache_dir(&config).ends_with("fake-onvif-cam-cache"));
        config.cache_dir = Some(PathBuf::from("/data/cache"));
        assert_eq!(cache_dir(&config), PathBuf::from("/data/cache"));
    }

    #[test]
    fn cleanup_removes_files_of_cameras_with_cache_clean_only() {
        let dir = std::env::temp_dir().join(format!("fake-onvif-cam-clean-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let touch = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        };
        let (a, b) = (touch("a.mp4"), touch("b.mp4"));
        let a_part = touch("a.mp4.part");

        let clean = Config {
            cache_files: vec![a.clone(), a.clone()],
            ..Config::default()
        };
        let keep = Config {
            cache_clean: false,
            cache_files: vec![b.clone()],
            ..Config::default()
        };

        drop(Cleanup::new([&clean, &keep]));
        assert!(!a.exists() && !a_part.exists());
        assert!(b.exists(), "cache_clean = false keeps the files");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prepare_is_a_no_op_when_disabled_or_without_a_file() {
        let mut off = config();
        off.cache = false;
        prepare(&mut off);
        assert!(off.cache_files.is_empty());

        let mut pattern = Config {
            cache: true,
            ..Config::default()
        };
        prepare(&mut pattern);
        assert!(pattern.cache_files.is_empty());
    }
}
