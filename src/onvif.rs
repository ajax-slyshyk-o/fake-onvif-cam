use crate::config::{Config, Stream};
use crate::util;

const VIDEO_SOURCE_TOKEN: &str = "video_source_1";
const VIDEO_SOURCE_CONFIG_TOKEN: &str = "video_source_config_1";

fn profile_token(index: usize) -> String {
    format!("profile_{}", index + 1)
}

fn video_encoder_config_token(index: usize) -> String {
    format!("video_encoder_config_{}", index + 1)
}

/// Text of the first `<...:name>value</...>` element in a SOAP request.
fn request_value<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("{name}>");
    let start = xml.find(&marker)? + marker.len();
    let value = &xml[start..xml[start..].find('<').map_or(xml.len(), |end| start + end)];
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

/// Resolves the stream a request refers to: by `ProfileToken`, else by encoder
/// `ConfigurationToken`. Without a token the main stream is used.
fn requested_stream(config: &Config, request: &str) -> Result<(usize, Stream), String> {
    let streams = config.streams();
    let found = if let Some(token) = request_value(request, "ProfileToken") {
        (0..streams.len()).find(|index| profile_token(*index) == token)
    } else if let Some(token) = request_value(request, "ConfigurationToken") {
        (0..streams.len()).find(|index| video_encoder_config_token(*index) == token)
    } else {
        Some(0)
    };

    found
        .map(|index| (index, streams[index].clone()))
        .ok_or_else(|| "No such profile or configuration token".to_string())
}

pub fn device_xaddr(config: &Config) -> String {
    format!(
        "http://{}:{}/onvif/device_service",
        config.advertise_host,
        config.http_addr.port()
    )
}

pub fn media_xaddr(config: &Config) -> String {
    format!(
        "http://{}:{}/onvif/media_service",
        config.advertise_host,
        config.http_addr.port()
    )
}

pub fn snapshot_uri(config: &Config) -> String {
    format!(
        "http://{}:{}/snapshot.jpg",
        config.advertise_host,
        config.http_addr.port()
    )
}

pub fn rtsp_uri(config: &Config, stream: &Stream) -> String {
    format!(
        "rtsp://{}:{}/{}",
        config.advertise_host, config.rtsp_port, stream.rtsp_path
    )
}

pub fn detect_operation(xml: &str) -> Option<String> {
    const OPS: &[&str] = &[
        "GetCapabilities",
        "GetDeviceInformation",
        "GetServices",
        "GetSystemDateAndTime",
        "GetScopes",
        "GetHostname",
        "GetNetworkInterfaces",
        "GetProfiles",
        "GetProfile",
        "GetStreamUri",
        "GetSnapshotUri",
        "GetVideoSources",
        "GetVideoSourceConfigurations",
        "GetVideoEncoderConfigurations",
        "GetVideoEncoderConfigurationOptions",
        "GetServiceCapabilities",
        "SetSynchronizationPoint",
    ];

    OPS.iter()
        .find(|op| {
            xml.contains(&format!(":{op}"))
                || xml.contains(&format!("<{op}"))
                || xml.contains(&format!("Action>{op}<"))
        })
        .map(|op| (*op).to_string())
}

pub fn discovery_probe_match(config: &Config, relates_to: &str) -> String {
    let name = util::xml_escape(&config.camera_name.replace(' ', "_"));
    let xaddr = util::xml_escape(&device_xaddr(config));
    let endpoint = util::xml_escape(&format!("urn:uuid:{}", config.uuid));
    let relates_to = util::xml_escape(relates_to);
    let message_id = util::message_id();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
            xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <e:Header>
    <w:MessageID>{message_id}</w:MessageID>
    <w:RelatesTo>{relates_to}</w:RelatesTo>
    <w:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</w:To>
    <w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</w:Action>
  </e:Header>
  <e:Body>
    <d:ProbeMatches>
      <d:ProbeMatch>
        <w:EndpointReference>
          <w:Address>{endpoint}</w:Address>
        </w:EndpointReference>
        <d:Types>dn:NetworkVideoTransmitter</d:Types>
        <d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/{name} onvif://www.onvif.org/location/TestLab</d:Scopes>
        <d:XAddrs>{xaddr}</d:XAddrs>
        <d:MetadataVersion>1</d:MetadataVersion>
      </d:ProbeMatch>
    </d:ProbeMatches>
  </e:Body>
</e:Envelope>"#
    )
}

pub fn soap_response(operation: Option<&str>, config: &Config, request: &str) -> String {
    let body = match operation {
        Some("GetCapabilities") => get_capabilities(config),
        Some("GetDeviceInformation") => get_device_information(config),
        Some("GetServices") => get_services(config),
        Some("GetSystemDateAndTime") => get_system_date_and_time(),
        Some("GetScopes") => get_scopes(config),
        Some("GetHostname") => get_hostname(config),
        Some("GetNetworkInterfaces") => get_network_interfaces(config),
        Some("GetProfiles") => get_profiles(config),
        Some("GetProfile") => requested_stream(config, request)
            .map(|(index, stream)| get_profile(config, index, &stream))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetStreamUri") => requested_stream(config, request)
            .map(|(_, stream)| get_stream_uri(config, &stream))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetSnapshotUri") => get_snapshot_uri(config),
        Some("GetVideoSources") => get_video_sources(config),
        Some("GetVideoSourceConfigurations") => get_video_source_configurations(config),
        Some("GetVideoEncoderConfigurations") => get_video_encoder_configurations(config),
        Some("GetVideoEncoderConfigurationOptions") => requested_stream(config, request)
            .map(|(_, stream)| get_video_encoder_configuration_options(&stream))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetServiceCapabilities") => get_service_capabilities(),
        Some("SetSynchronizationPoint") => "<trt:SetSynchronizationPointResponse/>".to_string(),
        Some(other) => soap_fault(&format!("Unsupported ONVIF operation: {other}")),
        None => soap_fault("Could not detect ONVIF operation"),
    };

    envelope(&body)
}

fn envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl"
            xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
            xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    {body}
  </s:Body>
</s:Envelope>"#
    )
}

fn get_capabilities(config: &Config) -> String {
    let device = util::xml_escape(&device_xaddr(config));
    let media = util::xml_escape(&media_xaddr(config));

    format!(
        r#"<tds:GetCapabilitiesResponse>
  <tds:Capabilities>
    <tt:Device>
      <tt:XAddr>{device}</tt:XAddr>
      <tt:Network>
        <tt:IPFilter>false</tt:IPFilter>
        <tt:ZeroConfiguration>false</tt:ZeroConfiguration>
        <tt:IPVersion6>false</tt:IPVersion6>
        <tt:DynDNS>false</tt:DynDNS>
      </tt:Network>
      <tt:System>
        <tt:DiscoveryResolve>false</tt:DiscoveryResolve>
        <tt:DiscoveryBye>false</tt:DiscoveryBye>
        <tt:RemoteDiscovery>false</tt:RemoteDiscovery>
        <tt:SystemBackup>false</tt:SystemBackup>
        <tt:SystemLogging>false</tt:SystemLogging>
        <tt:FirmwareUpgrade>false</tt:FirmwareUpgrade>
      </tt:System>
    </tt:Device>
    <tt:Media>
      <tt:XAddr>{media}</tt:XAddr>
      <tt:StreamingCapabilities>
        <tt:RTPMulticast>false</tt:RTPMulticast>
        <tt:RTP_TCP>true</tt:RTP_TCP>
        <tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP>
      </tt:StreamingCapabilities>
    </tt:Media>
  </tds:Capabilities>
</tds:GetCapabilitiesResponse>"#
    )
}

fn get_device_information(config: &Config) -> String {
    let manufacturer = util::xml_escape(&config.manufacturer);
    let model = util::xml_escape(&config.model);
    let firmware = util::xml_escape(&config.firmware);
    let serial = util::xml_escape(&config.serial);

    format!(
        r#"<tds:GetDeviceInformationResponse>
  <tds:Manufacturer>{manufacturer}</tds:Manufacturer>
  <tds:Model>{model}</tds:Model>
  <tds:FirmwareVersion>{firmware}</tds:FirmwareVersion>
  <tds:SerialNumber>{serial}</tds:SerialNumber>
  <tds:HardwareId>fake-onvif-cam</tds:HardwareId>
</tds:GetDeviceInformationResponse>"#
    )
}

fn get_services(config: &Config) -> String {
    let device = util::xml_escape(&device_xaddr(config));
    let media = util::xml_escape(&media_xaddr(config));

    format!(
        r#"<tds:GetServicesResponse>
  <tds:Service>
    <tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace>
    <tds:XAddr>{device}</tds:XAddr>
    <tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version>
  </tds:Service>
  <tds:Service>
    <tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace>
    <tds:XAddr>{media}</tds:XAddr>
    <tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version>
  </tds:Service>
</tds:GetServicesResponse>"#
    )
}

fn get_system_date_and_time() -> String {
    let (year, month, day, hour, minute, second) = util::utc_parts();

    format!(
        r#"<tds:GetSystemDateAndTimeResponse>
  <tds:SystemDateAndTime>
    <tt:DateTimeType>NTP</tt:DateTimeType>
    <tt:DaylightSavings>false</tt:DaylightSavings>
    <tt:TimeZone><tt:TZ>UTC</tt:TZ></tt:TimeZone>
    <tt:UTCDateTime>
      <tt:Time>
        <tt:Hour>{hour}</tt:Hour>
        <tt:Minute>{minute}</tt:Minute>
        <tt:Second>{second}</tt:Second>
      </tt:Time>
      <tt:Date>
        <tt:Year>{year}</tt:Year>
        <tt:Month>{month}</tt:Month>
        <tt:Day>{day}</tt:Day>
      </tt:Date>
    </tt:UTCDateTime>
  </tds:SystemDateAndTime>
</tds:GetSystemDateAndTimeResponse>"#
    )
}

fn get_scopes(config: &Config) -> String {
    let name = util::xml_escape(&config.camera_name.replace(' ', "_"));

    format!(
        r#"<tds:GetScopesResponse>
  <tds:Scopes>
    <tt:ScopeDef>Fixed</tt:ScopeDef>
    <tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem>
  </tds:Scopes>
  <tds:Scopes>
    <tt:ScopeDef>Configurable</tt:ScopeDef>
    <tt:ScopeItem>onvif://www.onvif.org/name/{name}</tt:ScopeItem>
  </tds:Scopes>
  <tds:Scopes>
    <tt:ScopeDef>Configurable</tt:ScopeDef>
    <tt:ScopeItem>onvif://www.onvif.org/location/TestLab</tt:ScopeItem>
  </tds:Scopes>
</tds:GetScopesResponse>"#
    )
}

fn get_hostname(config: &Config) -> String {
    let name = util::xml_escape(&config.camera_name.replace(' ', "-").to_lowercase());

    format!(
        r#"<tds:GetHostnameResponse>
  <tds:HostnameInformation>
    <tt:FromDHCP>false</tt:FromDHCP>
    <tt:Name>{name}</tt:Name>
  </tds:HostnameInformation>
</tds:GetHostnameResponse>"#
    )
}

fn get_network_interfaces(config: &Config) -> String {
    let host = util::xml_escape(&config.advertise_host);

    format!(
        r#"<tds:GetNetworkInterfacesResponse>
  <tds:NetworkInterfaces token="eth0" enabled="true">
    <tt:Info>
      <tt:Name>eth0</tt:Name>
      <tt:HwAddress>02:00:00:00:00:01</tt:HwAddress>
      <tt:MTU>1500</tt:MTU>
    </tt:Info>
    <tt:IPv4>
      <tt:Enabled>true</tt:Enabled>
      <tt:Config>
        <tt:Manual>
          <tt:Address>{host}</tt:Address>
          <tt:PrefixLength>24</tt:PrefixLength>
        </tt:Manual>
        <tt:DHCP>false</tt:DHCP>
      </tt:Config>
    </tt:IPv4>
  </tds:NetworkInterfaces>
</tds:GetNetworkInterfacesResponse>"#
    )
}

fn get_profiles(config: &Config) -> String {
    let profiles: Vec<String> = config
        .streams()
        .iter()
        .enumerate()
        .map(|(index, stream)| profile_xml(config, index, stream))
        .collect();

    format!(
        r#"<trt:GetProfilesResponse>
  {}
</trt:GetProfilesResponse>"#,
        profiles.join("\n  ")
    )
}

fn get_profile(config: &Config, index: usize, stream: &Stream) -> String {
    format!(
        r#"<trt:GetProfileResponse>
  {}
</trt:GetProfileResponse>"#,
        profile_xml(config, index, stream)
    )
}

fn profile_xml(config: &Config, index: usize, stream: &Stream) -> String {
    let name = if index == 0 {
        config.camera_name.clone()
    } else {
        format!("{} {}", config.camera_name, stream.name)
    };
    let name = util::xml_escape(&name);
    let token = profile_token(index);

    format!(
        r#"<trt:Profiles token="{token}" fixed="true">
    <tt:Name>{name}</tt:Name>
    {}
    {}
  </trt:Profiles>"#,
        video_source_configuration_xml(config),
        video_encoder_configuration_xml(index, stream)
    )
}

fn get_stream_uri(config: &Config, stream: &Stream) -> String {
    let uri = util::xml_escape(&rtsp_uri(config, stream));

    format!(
        r#"<trt:GetStreamUriResponse>
  <trt:MediaUri>
    <tt:Uri>{uri}</tt:Uri>
    <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
    <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
    <tt:Timeout>PT60S</tt:Timeout>
  </trt:MediaUri>
</trt:GetStreamUriResponse>"#
    )
}

fn get_snapshot_uri(config: &Config) -> String {
    let uri = util::xml_escape(&snapshot_uri(config));

    format!(
        r#"<trt:GetSnapshotUriResponse>
  <trt:MediaUri>
    <tt:Uri>{uri}</tt:Uri>
    <tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>
    <tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>
    <tt:Timeout>PT60S</tt:Timeout>
  </trt:MediaUri>
</trt:GetSnapshotUriResponse>"#
    )
}

fn get_video_sources(config: &Config) -> String {
    format!(
        r#"<trt:GetVideoSourcesResponse>
  <trt:VideoSources token="{VIDEO_SOURCE_TOKEN}">
    <tt:Framerate>{}</tt:Framerate>
    <tt:Resolution>
      <tt:Width>{}</tt:Width>
      <tt:Height>{}</tt:Height>
    </tt:Resolution>
  </trt:VideoSources>
</trt:GetVideoSourcesResponse>"#,
        config.fps, config.width, config.height
    )
}

fn get_video_source_configurations(config: &Config) -> String {
    format!(
        r#"<trt:GetVideoSourceConfigurationsResponse>
  {}
</trt:GetVideoSourceConfigurationsResponse>"#,
        video_source_configuration_xml(config)
    )
}

fn video_source_configuration_xml(config: &Config) -> String {
    format!(
        r#"<tt:VideoSourceConfiguration token="{VIDEO_SOURCE_CONFIG_TOKEN}">
      <tt:Name>VideoSourceConfig</tt:Name>
      <tt:UseCount>{}</tt:UseCount>
      <tt:SourceToken>{VIDEO_SOURCE_TOKEN}</tt:SourceToken>
      <tt:Bounds x="0" y="0" width="{}" height="{}"/>
    </tt:VideoSourceConfiguration>"#,
        config.streams().len(),
        config.width,
        config.height
    )
}

fn get_video_encoder_configurations(config: &Config) -> String {
    let configurations: Vec<String> = config
        .streams()
        .iter()
        .enumerate()
        .map(|(index, stream)| video_encoder_configuration_xml(index, stream))
        .collect();

    format!(
        r#"<trt:GetVideoEncoderConfigurationsResponse>
  {}
</trt:GetVideoEncoderConfigurationsResponse>"#,
        configurations.join("\n  ")
    )
}

fn video_encoder_configuration_xml(index: usize, stream: &Stream) -> String {
    let token = video_encoder_config_token(index);
    let name = if index == 0 {
        "H264Encoder"
    } else {
        "H264EncoderSub"
    };

    format!(
        r#"<tt:VideoEncoderConfiguration token="{token}">
      <tt:Name>{name}</tt:Name>
      <tt:UseCount>1</tt:UseCount>
      <tt:Encoding>H264</tt:Encoding>
      <tt:Resolution>
        <tt:Width>{}</tt:Width>
        <tt:Height>{}</tt:Height>
      </tt:Resolution>
      <tt:Quality>5</tt:Quality>
      <tt:RateControl>
        <tt:FrameRateLimit>{}</tt:FrameRateLimit>
        <tt:EncodingInterval>1</tt:EncodingInterval>
        <tt:BitrateLimit>{}</tt:BitrateLimit>
      </tt:RateControl>
      <tt:H264>
        <tt:GovLength>{}</tt:GovLength>
        <tt:H264Profile>Baseline</tt:H264Profile>
      </tt:H264>
      <tt:Multicast>
        <tt:Address>
          <tt:Type>IPv4</tt:Type>
          <tt:IPv4Address>0.0.0.0</tt:IPv4Address>
        </tt:Address>
        <tt:Port>0</tt:Port>
        <tt:TTL>1</tt:TTL>
        <tt:AutoStart>false</tt:AutoStart>
      </tt:Multicast>
      <tt:SessionTimeout>PT60S</tt:SessionTimeout>
    </tt:VideoEncoderConfiguration>"#,
        stream.width,
        stream.height,
        stream.fps,
        bitrate_limit(stream),
        stream.fps * 2
    )
}

fn get_video_encoder_configuration_options(stream: &Stream) -> String {
    format!(
        r#"<trt:GetVideoEncoderConfigurationOptionsResponse>
  <trt:Options>
    <tt:QualityRange>
      <tt:Min>1</tt:Min>
      <tt:Max>10</tt:Max>
    </tt:QualityRange>
    <tt:H264>
      <tt:ResolutionsAvailable>
        <tt:Width>{}</tt:Width>
        <tt:Height>{}</tt:Height>
      </tt:ResolutionsAvailable>
      <tt:GovLengthRange>
        <tt:Min>{}</tt:Min>
        <tt:Max>{}</tt:Max>
      </tt:GovLengthRange>
      <tt:FrameRateRange>
        <tt:Min>1</tt:Min>
        <tt:Max>{}</tt:Max>
      </tt:FrameRateRange>
      <tt:EncodingIntervalRange>
        <tt:Min>1</tt:Min>
        <tt:Max>1</tt:Max>
      </tt:EncodingIntervalRange>
      <tt:H264ProfilesSupported>Baseline</tt:H264ProfilesSupported>
    </tt:H264>
  </trt:Options>
</trt:GetVideoEncoderConfigurationOptionsResponse>"#,
        stream.width,
        stream.height,
        stream.fps,
        stream.fps * 4,
        stream.fps
    )
}

fn get_service_capabilities() -> String {
    r#"<trt:GetServiceCapabilitiesResponse>
  <trt:Capabilities SnapshotUri="true" Rotation="false" VideoSourceMode="false" OSD="false"/>
</trt:GetServiceCapabilitiesResponse>"#
        .to_string()
}

fn soap_fault(reason: &str) -> String {
    let reason = util::xml_escape(reason);

    format!(
        r#"<s:Fault>
  <s:Code><s:Value>s:Sender</s:Value></s:Code>
  <s:Reason><s:Text xml:lang="en">{reason}</s:Text></s:Reason>
</s:Fault>"#
    )
}

fn bitrate_limit(stream: &Stream) -> u32 {
    let pixels = stream.width.saturating_mul(stream.height);
    let bits_per_frame = pixels.saturating_mul(2);
    let bits_per_second = bits_per_frame.saturating_mul(stream.fps);
    (bits_per_second / 1_000).max(512)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SubStream;

    fn two_stream_config() -> Config {
        Config {
            width: 1920,
            height: 1080,
            sub: Some(SubStream {
                width: None,
                height: None,
                fps: None,
                bitrate_kbps: 512,
                rtsp_path: "live_sub".to_string(),
                rtp_port: None,
                codec: "libx264".to_string(),
                preset: None,
            }),
            ..Config::default()
        }
    }

    fn request(op: &str, token: &str) -> String {
        format!("<trt:{op}><trt:ProfileToken>{token}</trt:ProfileToken></trt:{op}>")
    }

    #[test]
    fn profiles_list_both_streams() {
        let xml = get_profiles(&two_stream_config());
        assert!(xml.contains(r#"token="profile_1""#));
        assert!(xml.contains(r#"token="profile_2""#));
        assert!(xml.contains("<tt:Width>1920</tt:Width>"));
        assert!(xml.contains("<tt:Width>426</tt:Width>"));
    }

    #[test]
    fn stream_uri_follows_profile_token() {
        let config = two_stream_config();
        let main = soap_response(
            Some("GetStreamUri"),
            &config,
            &request("GetStreamUri", "profile_1"),
        );
        let sub = soap_response(
            Some("GetStreamUri"),
            &config,
            &request("GetStreamUri", "profile_2"),
        );
        assert!(main.contains("/live</tt:Uri>"));
        assert!(sub.contains("/live_sub</tt:Uri>"));
    }

    #[test]
    fn unknown_profile_token_is_a_fault() {
        let config = two_stream_config();
        let xml = soap_response(
            Some("GetStreamUri"),
            &config,
            &request("GetStreamUri", "nope"),
        );
        assert!(xml.contains("<s:Fault>"));
    }

    #[test]
    fn missing_token_defaults_to_main_stream() {
        let config = two_stream_config();
        let xml = soap_response(Some("GetStreamUri"), &config, "<trt:GetStreamUri/>");
        assert!(xml.contains("/live</tt:Uri>"));
    }

    #[test]
    fn single_stream_camera_has_one_profile() {
        let xml = get_profiles(&Config::default());
        assert!(xml.contains(r#"token="profile_1""#));
        assert!(!xml.contains("profile_2"));
    }
}
