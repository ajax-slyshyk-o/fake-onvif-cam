mod media2;

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

/// Media2 is only offered when a stream needs it (H.265); H.264-only cameras stay Media1.
pub fn media2_xaddr(config: &Config) -> Option<String> {
    config.has_hevc().then(|| {
        format!(
            "http://{}:{}/onvif/media2_service",
            config.advertise_host,
            config.http_addr.port()
        )
    })
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

/// ONVIF service that received a SOAP request. Both share one handler, but a few
/// operations (e.g. `GetServiceCapabilities`) exist in each service with a different answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Device,
    Media,
}

/// Response of the Media service (`/onvif/media_service`).
#[cfg(test)]
pub fn soap_response(operation: Option<&str>, config: &Config, request: &str) -> String {
    soap_response_for_service(Service::Media, operation, config, request)
}

/// Response of the Device (`/onvif/device_service`) or Media service.
pub fn soap_response_for_service(
    service: Service,
    operation: Option<&str>,
    config: &Config,
    request: &str,
) -> String {
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
        Some("GetServiceCapabilities") => match service {
            Service::Device => get_device_service_capabilities(),
            Service::Media => get_media_service_capabilities(),
        },
        Some("SetSynchronizationPoint") => "<trt:SetSynchronizationPointResponse/>".to_string(),
        Some(other) => soap_fault(&format!("Unsupported ONVIF operation: {other}")),
        None => soap_fault("Could not detect ONVIF operation"),
    };

    envelope(&body)
}

/// Response of the Media2 service (`/onvif/media2_service`).
pub fn soap_response_media2(operation: Option<&str>, config: &Config, request: &str) -> String {
    envelope(&media2::soap_body(operation, config, request))
}

fn envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"
            xmlns:tds="http://www.onvif.org/ver10/device/wsdl"
            xmlns:trt="http://www.onvif.org/ver10/media/wsdl"
            xmlns:tr2="http://www.onvif.org/ver20/media/wsdl"
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
    let media2 = media2_xaddr(config)
        .map(|xaddr| {
            format!(
                r#"
  <tds:Service>
    <tds:Namespace>http://www.onvif.org/ver20/media/wsdl</tds:Namespace>
    <tds:XAddr>{}</tds:XAddr>
    <tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version>
  </tds:Service>"#,
                util::xml_escape(&xaddr)
            )
        })
        .unwrap_or_default();

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
  </tds:Service>{media2}
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

fn profile_name(config: &Config, index: usize, stream: &Stream) -> String {
    if index == 0 {
        config.camera_name.clone()
    } else {
        format!("{} {}", config.camera_name, stream.name)
    }
}

fn profile_xml(config: &Config, index: usize, stream: &Stream) -> String {
    let name = util::xml_escape(&profile_name(config, index, stream));
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

/// Media1 profile encoder configuration. Media1 only knows JPEG/MPEG4/H264, so like the real
/// cameras this emulates, an HEVC stream is reported here as H.264 Baseline; clients that
/// understand H.265 read the real encoding from Media2.
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

/// Device service capabilities (tds). No authentication or TLS is implemented.
fn get_device_service_capabilities() -> String {
    r#"<tds:GetServiceCapabilitiesResponse>
  <tds:Capabilities>
    <tds:Network IPFilter="false" ZeroConfiguration="false" IPVersion6="false" DynDNS="false"/>
    <tds:Security TLS1.0="false" TLS1.1="false" TLS1.2="false" UsernameToken="false" HttpDigest="false"/>
    <tds:System DiscoveryResolve="false" DiscoveryBye="false" RemoteDiscovery="false" SystemLogging="false" FirmwareUpgrade="false"/>
  </tds:Capabilities>
</tds:GetServiceCapabilitiesResponse>"#
        .to_string()
}

/// Media service capabilities (trt).
fn get_media_service_capabilities() -> String {
    r#"<trt:GetServiceCapabilitiesResponse>
  <trt:Capabilities SnapshotUri="true" Rotation="false" VideoSourceMode="false" OSD="false">
    <trt:ProfileCapabilities MaximumNumberOfProfiles="128"/>
    <trt:StreamingCapabilities RTPMulticast="false" RTP_TCP="true" RTP_RTSP_TCP="true" NonAggregateControl="false"/>
  </trt:Capabilities>
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

    fn hevc_config() -> Config {
        let mut config = two_stream_config();
        config.sub.as_mut().unwrap().codec = "libx265".to_string();
        config
    }

    fn media2(operation: &str, config: &Config, body: &str) -> String {
        soap_response_media2(Some(operation), config, body)
    }

    #[test]
    fn media1_reports_hevc_sub_as_h264_baseline() {
        let config = hevc_config();
        let profiles = get_profiles(&config);
        assert!(profiles.contains("<tt:Encoding>H264</tt:Encoding>"));
        assert!(!profiles.contains("H265"));

        let options = get_video_encoder_configuration_options(&config.streams()[1]);
        assert!(options.contains("<tt:H264ProfilesSupported>Baseline</tt:H264ProfilesSupported>"));
        assert!(!options.contains("H265"));
    }

    #[test]
    fn media2_is_advertised_only_for_hevc_cameras() {
        assert!(get_services(&two_stream_config())
            .find("ver20/media")
            .is_none());
        assert!(media2_xaddr(&two_stream_config()).is_none());

        let config = hevc_config();
        let services = get_services(&config);
        assert!(services.contains("http://www.onvif.org/ver20/media/wsdl"));
        assert!(services.contains("/onvif/media2_service"));
        assert!(media2_xaddr(&config)
            .unwrap()
            .ends_with("/onvif/media2_service"));
    }

    #[test]
    fn media2_profiles_report_real_encodings() {
        let xml = media2(
            "GetProfiles",
            &hevc_config(),
            "<tr2:GetProfiles><tr2:Type>All</tr2:Type></tr2:GetProfiles>",
        );
        assert!(xml.contains(r#"<tr2:Profiles token="profile_1" fixed="true">"#));
        assert!(xml.contains(r#"<tr2:Profiles token="profile_2" fixed="true">"#));
        assert!(xml.contains("<tt:Encoding>H264</tt:Encoding>"));
        assert!(xml.contains("<tt:Encoding>H265</tt:Encoding>"));
        assert!(xml.contains(r#"Profile="Main""#));
        assert!(xml.contains("<tt:Name>H265EncoderSub</tt:Name>"));
        assert!(xml.contains(r#"xmlns:tr2="http://www.onvif.org/ver20/media/wsdl""#));
    }

    #[test]
    fn media2_profiles_can_be_filtered_by_token() {
        let config = hevc_config();
        let xml = media2(
            "GetProfiles",
            &config,
            "<tr2:GetProfiles><tr2:Token>profile_2</tr2:Token></tr2:GetProfiles>",
        );
        assert!(xml.contains(r#"token="profile_2""#));
        assert!(!xml.contains(r#"token="profile_1""#));

        let unknown = media2(
            "GetProfiles",
            &config,
            "<tr2:GetProfiles><tr2:Token>nope</tr2:Token></tr2:GetProfiles>",
        );
        assert!(unknown.contains("<s:Fault>"));
    }

    #[test]
    fn media2_encoder_configurations_and_options() {
        let config = hevc_config();
        let all = media2(
            "GetVideoEncoderConfigurations",
            &config,
            "<tr2:GetVideoEncoderConfigurations/>",
        );
        assert!(all.contains(r#"<tr2:Configurations token="video_encoder_config_1""#));
        assert!(all.contains(r#"<tr2:Configurations token="video_encoder_config_2""#));

        let sub = media2(
            "GetVideoEncoderConfigurations",
            &config,
            "<tr2:GetVideoEncoderConfigurations><tr2:ConfigurationToken>video_encoder_config_2</tr2:ConfigurationToken></tr2:GetVideoEncoderConfigurations>",
        );
        assert!(sub.contains("<tt:Encoding>H265</tt:Encoding>"));
        assert!(!sub.contains("<tt:Encoding>H264</tt:Encoding>"));

        let sub_options = media2(
            "GetVideoEncoderConfigurationOptions",
            &config,
            "<tr2:GetVideoEncoderConfigurationOptions><tr2:ProfileToken>profile_2</tr2:ProfileToken></tr2:GetVideoEncoderConfigurationOptions>",
        );
        assert!(sub_options.contains("<tt:Encoding>H265</tt:Encoding>"));
        assert!(sub_options.contains(r#"ProfilesSupported="Main""#));
        assert!(!sub_options.contains("H264"));

        let all_options = media2(
            "GetVideoEncoderConfigurationOptions",
            &config,
            "<tr2:GetVideoEncoderConfigurationOptions/>",
        );
        assert!(all_options.contains("<tt:Encoding>H264</tt:Encoding>"));
        assert!(all_options.contains("<tt:Encoding>H265</tt:Encoding>"));
    }

    #[test]
    fn media2_stream_uri_follows_profile_token() {
        let config = hevc_config();
        let body = |token: &str| {
            format!(
                "<tr2:GetStreamUri><tr2:Protocol>RTSP</tr2:Protocol><tr2:ProfileToken>{token}</tr2:ProfileToken></tr2:GetStreamUri>"
            )
        };
        let main = media2("GetStreamUri", &config, &body("profile_1"));
        let sub = media2("GetStreamUri", &config, &body("profile_2"));
        assert!(main.contains("/live</tr2:Uri>"));
        assert!(sub.contains("/live_sub</tr2:Uri>"));
        assert!(media2("GetStreamUri", &config, &body("x")).contains("<s:Fault>"));
    }

    #[test]
    fn device_and_media_service_capabilities_differ() {
        let config = two_stream_config();
        let request = "<tds:GetServiceCapabilities/>";
        let device = soap_response_for_service(
            Service::Device,
            Some("GetServiceCapabilities"),
            &config,
            request,
        );
        assert!(device.contains("<tds:GetServiceCapabilitiesResponse>"));
        assert!(device.contains("<tds:Security "));
        assert!(!device.contains("SnapshotUri"));

        let media = soap_response_for_service(
            Service::Media,
            Some("GetServiceCapabilities"),
            &config,
            request,
        );
        assert!(media.contains("<trt:GetServiceCapabilitiesResponse>"));
        assert!(media.contains(r#"SnapshotUri="true""#));
        assert!(media.contains("MaximumNumberOfProfiles"));
        assert!(media.contains("<trt:StreamingCapabilities "));
    }

    #[test]
    fn media2_service_capabilities() {
        let xml = media2(
            "GetServiceCapabilities",
            &hevc_config(),
            "<tr2:GetServiceCapabilities/>",
        );
        assert!(xml.contains(r#"RTSPStreaming="true""#));
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
