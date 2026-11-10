//! Grouping of cameras that share one ffmpeg process (`share_source`).
//!
//! Cameras whose video settings are identical produce the same stream, so one ffmpeg process
//! (one decode, one encode or one cache copy) can feed all of them. The first camera of a group
//! is its leader: its ports are the ones ffmpeg sends to and its config builds the ffmpeg command.
//! The other cameras only add their own ONVIF and RTSP front ends on top of the leader's relay.

use crate::config::Config;
use std::collections::HashMap;

/// Indexes of the cameras served by one ffmpeg process. The first one is the leader.
pub type Group = Vec<usize>;

/// Splits the cameras into groups. A camera without `share_source` (or without ffmpeg) is its
/// own group; the others are grouped by [`share_key`]. Groups come in the order of their leader.
pub fn group_cameras(configs: &[Config]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();

    for (index, config) in configs.iter().enumerate() {
        match share_key(config) {
            Some(key) => match by_key.get(&key) {
                Some(&group) => groups[group].push(index),
                None => {
                    by_key.insert(key, groups.len());
                    groups.push(vec![index]);
                }
            },
            None => groups.push(vec![index]),
        }
    }

    groups
}

/// Everything that decides what ffmpeg produces, except what is per camera: names, ports,
/// RTSP paths, credentials and the overlay text (the leader's text is used for the whole group).
/// `None` for cameras that do not share.
fn share_key(config: &Config) -> Option<String> {
    if !config.share_source || config.no_ffmpeg {
        return None;
    }

    let streams: Vec<_> = config
        .streams()
        .iter()
        .map(|stream| {
            format!(
                "{}:{}x{}@{}:{}k:{}:{:?}:{}",
                stream.name,
                stream.width,
                stream.height,
                stream.fps,
                stream.bitrate_kbps,
                stream.codec,
                stream.preset,
                stream.audio.is_some()
            )
        })
        .collect();

    let sub_file = config.sub.as_ref().and_then(|sub| sub.file.clone());
    let cache_dir = config.cache.then(|| config.cache_dir.clone());

    Some(format!(
        "{:?}|{:?}|{}|{}|{:?}|{}|{}|{:?}|{:?}|{:?}|{streams:?}",
        config.ffmpeg_path,
        config.media_input,
        config.scale,
        config.overlay_text.is_some(),
        config.overlay_font,
        config.overlay_font_size,
        config.audio_from_file,
        config.audio,
        sub_file,
        cache_dir,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MediaInput;
    use std::path::PathBuf;

    fn camera(shared: bool, name: &str) -> Config {
        Config {
            camera_name: name.to_string(),
            media_input: MediaInput::File(PathBuf::from("clip.mp4")),
            share_source: shared,
            overlay_text: Some(name.to_string()),
            ..Config::default()
        }
    }

    #[test]
    fn cameras_with_the_same_video_settings_share_even_with_different_overlays() {
        let mut cameras = vec![camera(true, "a"), camera(true, "b"), camera(true, "c")];
        cameras[1].rtsp_port = 9000;
        cameras[1].rtp_port = 7000;
        cameras[2].serial = "other".to_string();

        assert_eq!(group_cameras(&cameras), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn different_video_settings_or_files_split_groups() {
        let mut cameras = vec![
            camera(true, "a"),
            camera(true, "b"),
            camera(true, "c"),
            camera(true, "d"),
        ];
        cameras[1].bitrate_kbps = 1000;
        cameras[2].media_input = MediaInput::File(PathBuf::from("other.mp4"));
        cameras[3].codec = "h264_nvenc".to_string();

        assert_eq!(
            group_cameras(&cameras),
            vec![vec![0], vec![1], vec![2], vec![3]]
        );
    }

    #[test]
    fn cameras_that_do_not_share_stay_alone_and_groups_follow_leader_order() {
        let mut cameras = vec![
            camera(true, "a"),
            camera(false, "b"),
            camera(true, "c"),
            camera(false, "d"),
        ];
        assert_eq!(group_cameras(&cameras), vec![vec![0, 2], vec![1], vec![3]]);

        cameras[2].no_ffmpeg = true;
        assert_eq!(
            group_cameras(&cameras),
            vec![vec![0], vec![1], vec![2], vec![3]]
        );
    }
}
