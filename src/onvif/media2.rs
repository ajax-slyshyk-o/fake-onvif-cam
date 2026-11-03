//! ONVIF Media2 service (`ver20/media/wsdl`), the only media service that can describe
//! H.265. Media1 reports an HEVC stream as H.264 Baseline (see `video_encoder_configuration_xml`).

use super::{
    bitrate_limit, profile_name, profile_token, request_value, requested_stream, rtsp_uri,
    snapshot_uri, soap_fault, video_encoder_config_token, VIDEO_SOURCE_CONFIG_TOKEN,
    VIDEO_SOURCE_TOKEN,
};
use crate::config::{Config, Stream, VideoFormat};
use crate::util;

pub fn soap_body(operation: Option<&str>, config: &Config, request: &str) -> String {
    match operation {
        Some("GetProfiles") => get_profiles(config, request),
        Some("GetStreamUri") => requested_stream(config, request)
            .map(|(_, stream)| {
                format!(
                    "<tr2:GetStreamUriResponse>\n  <tr2:Uri>{}</tr2:Uri>\n</tr2:GetStreamUriResponse>",
                    util::xml_escape(&rtsp_uri(config, &stream))
                )
            })
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetSnapshotUri") => format!(
            "<tr2:GetSnapshotUriResponse>\n  <tr2:Uri>{}</tr2:Uri>\n</tr2:GetSnapshotUriResponse>",
            util::xml_escape(&snapshot_uri(config))
        ),
        Some("GetVideoSourceConfigurations") => format!(
            "<tr2:GetVideoSourceConfigurationsResponse>\n  {}\n</tr2:GetVideoSourceConfigurationsResponse>",
            video_source_xml(config, "tr2:Configurations")
        ),
        Some("GetVideoEncoderConfigurations") => get_video_encoder_configurations(config, request),
        Some("GetVideoEncoderConfigurationOptions") => {
            get_video_encoder_configuration_options(config, request)
        }
        Some("GetServiceCapabilities") => service_capabilities(),
        Some("SetSynchronizationPoint") => "<tr2:SetSynchronizationPointResponse/>".to_string(),
        Some(other) => soap_fault(&format!("Unsupported Media2 operation: {other}")),
        None => soap_fault("Could not detect ONVIF operation"),
    }
}

fn format_name(format: VideoFormat) -> &'static str {
    match format {
        VideoFormat::H264 => "H264",
        VideoFormat::H265 => "H265",
    }
}

fn profile_name_of(format: VideoFormat) -> &'static str {
    match format {
        VideoFormat::H264 => "Baseline",
        VideoFormat::H265 => "Main",
    }
}

/// Streams selected by the optional token filters of a request, in profile order. A token
/// that matches nothing is an error; no token selects every stream.
fn selected_streams(
    config: &Config,
    request: &str,
    profile_key: &str,
) -> Result<Vec<(usize, Stream)>, String> {
    let streams: Vec<(usize, Stream)> = config.streams().into_iter().enumerate().collect();
    let profile = request_value(request, profile_key);
    let configuration = request_value(request, "ConfigurationToken");

    if profile.is_none() && configuration.is_none() {
        return Ok(streams);
    }

    let selected: Vec<(usize, Stream)> = streams
        .into_iter()
        .filter(|(index, _)| {
            profile.is_none_or(|token| profile_token(*index) == token)
                && configuration.is_none_or(|token| video_encoder_config_token(*index) == token)
        })
        .collect();

    if selected.is_empty() {
        Err("No such profile or configuration token".to_string())
    } else {
        Ok(selected)
    }
}

fn get_profiles(config: &Config, request: &str) -> String {
    // `<tr2:Token>` filters by profile; `ProfileToken` is not part of this request.
    let profiles = match selected_streams(config, request, ":Token") {
        Ok(streams) => streams
            .iter()
            .map(|(index, stream)| profile_xml(config, *index, stream))
            .collect::<Vec<_>>()
            .join("\n  "),
        Err(reason) => return soap_fault(&reason),
    };

    format!("<tr2:GetProfilesResponse>\n  {profiles}\n</tr2:GetProfilesResponse>")
}

fn profile_xml(config: &Config, index: usize, stream: &Stream) -> String {
    let name = util::xml_escape(&profile_name(config, index, stream));
    let token = profile_token(index);

    format!(
        r#"<tr2:Profiles token="{token}" fixed="true">
    <tr2:Name>{name}</tr2:Name>
    <tr2:Configurations>
      {}
      {}
    </tr2:Configurations>
  </tr2:Profiles>"#,
        video_source_xml(config, "tr2:VideoSource"),
        video_encoder_xml(index, stream, "tr2:VideoEncoder")
    )
}

fn video_source_xml(config: &Config, element: &str) -> String {
    format!(
        r#"<{element} token="{VIDEO_SOURCE_CONFIG_TOKEN}">
      <tt:Name>VideoSourceConfig</tt:Name>
      <tt:UseCount>{}</tt:UseCount>
      <tt:SourceToken>{VIDEO_SOURCE_TOKEN}</tt:SourceToken>
      <tt:Bounds x="0" y="0" width="{}" height="{}"/>
    </{element}>"#,
        config.streams().len(),
        config.width,
        config.height
    )
}

/// `tt:VideoEncoder2Configuration`; `element` is the wrapper name (`tr2:VideoEncoder` inside a
/// profile, `tr2:Configurations` in `GetVideoEncoderConfigurations`).
fn video_encoder_xml(index: usize, stream: &Stream, element: &str) -> String {
    let format = stream.format();
    let token = video_encoder_config_token(index);
    let name = match (format, index == 0) {
        (VideoFormat::H264, true) => "H264Encoder",
        (VideoFormat::H264, false) => "H264EncoderSub",
        (VideoFormat::H265, true) => "H265Encoder",
        (VideoFormat::H265, false) => "H265EncoderSub",
    };

    format!(
        r#"<{element} token="{token}" GovLength="{}" Profile="{}">
      <tt:Name>{name}</tt:Name>
      <tt:UseCount>1</tt:UseCount>
      <tt:Encoding>{}</tt:Encoding>
      <tt:Resolution>
        <tt:Width>{}</tt:Width>
        <tt:Height>{}</tt:Height>
      </tt:Resolution>
      <tt:RateControl ConstantBitRate="true">
        <tt:FrameRateLimit>{}</tt:FrameRateLimit>
        <tt:BitrateLimit>{}</tt:BitrateLimit>
      </tt:RateControl>
      <tt:Multicast>
        <tt:Address>
          <tt:Type>IPv4</tt:Type>
          <tt:IPv4Address>0.0.0.0</tt:IPv4Address>
        </tt:Address>
        <tt:Port>0</tt:Port>
        <tt:TTL>1</tt:TTL>
        <tt:AutoStart>false</tt:AutoStart>
      </tt:Multicast>
      <tt:Quality>5</tt:Quality>
    </{element}>"#,
        stream.fps * 2,
        profile_name_of(format),
        format_name(format),
        stream.width,
        stream.height,
        stream.fps,
        bitrate_limit(stream)
    )
}

fn get_video_encoder_configurations(config: &Config, request: &str) -> String {
    match selected_streams(config, request, "ProfileToken") {
        Ok(streams) => {
            let configurations = streams
                .iter()
                .map(|(index, stream)| video_encoder_xml(*index, stream, "tr2:Configurations"))
                .collect::<Vec<_>>()
                .join("\n  ");
            format!(
                "<tr2:GetVideoEncoderConfigurationsResponse>\n  {configurations}\n</tr2:GetVideoEncoderConfigurationsResponse>"
            )
        }
        Err(reason) => soap_fault(&reason),
    }
}

fn get_video_encoder_configuration_options(config: &Config, request: &str) -> String {
    let streams = match selected_streams(config, request, "ProfileToken") {
        Ok(streams) => streams,
        Err(reason) => return soap_fault(&reason),
    };

    // Without a token the options of every encoding the camera offers are listed once.
    let mut options: Vec<(VideoFormat, &Stream)> = Vec::new();
    for (_, stream) in &streams {
        if !options.iter().any(|(format, _)| *format == stream.format()) {
            options.push((stream.format(), stream));
        }
    }

    let options = options
        .iter()
        .map(|(format, stream)| options_xml(*format, stream))
        .collect::<Vec<_>>()
        .join("\n  ");

    format!(
        "<tr2:GetVideoEncoderConfigurationOptionsResponse>\n  {options}\n</tr2:GetVideoEncoderConfigurationOptionsResponse>"
    )
}

fn options_xml(format: VideoFormat, stream: &Stream) -> String {
    let frame_rates = (1..=stream.fps)
        .rev()
        .map(|fps| fps.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        r#"<tr2:Options GovLengthRange="1 {}" FrameRatesSupported="{frame_rates}" ProfilesSupported="{}" ConstantBitRateSupported="true" GuaranteedFrameRateSupported="false">
    <tt:Encoding>{}</tt:Encoding>
    <tt:QualityRange>
      <tt:Min>1</tt:Min>
      <tt:Max>10</tt:Max>
    </tt:QualityRange>
    <tt:ResolutionsAvailable>
      <tt:Width>{}</tt:Width>
      <tt:Height>{}</tt:Height>
    </tt:ResolutionsAvailable>
    <tt:BitrateRange>
      <tt:Min>32</tt:Min>
      <tt:Max>{}</tt:Max>
    </tt:BitrateRange>
  </tr2:Options>"#,
        stream.fps * 4,
        profile_name_of(format),
        format_name(format),
        stream.width,
        stream.height,
        bitrate_limit(stream)
    )
}

fn service_capabilities() -> String {
    r#"<tr2:GetServiceCapabilitiesResponse>
  <tr2:Capabilities SnapshotUri="true" Rotation="false" VideoSourceMode="false" OSD="false">
    <tr2:ProfileCapabilities MaximumNumberOfProfiles="2" ConfigurationsSupported="VideoSource VideoEncoder"/>
    <tr2:StreamingCapabilities RTSPStreaming="true" RTPMulticast="false" RTP_RTSP_TCP="true" NonAggregateControl="false"/>
  </tr2:Capabilities>
</tr2:GetServiceCapabilitiesResponse>"#
        .to_string()
}
