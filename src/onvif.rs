mod media2;
mod services;

pub use services::pull_wait;

use crate::config::{Config, Naming, Stream};
use crate::util;

/// Tokens and display names of the ONVIF objects of one camera, per [`Naming`] scheme. The default
/// scheme numbers things (`profile_1`); the logent scheme copies a logent camera (`cam-0-main`).
#[derive(Clone, Copy)]
struct Names(Naming);

fn names(config: &Config) -> Names {
    Names(config.naming)
}

impl Names {
    fn logent(self) -> bool {
        self.0 == Naming::Logent
    }

    /// `main` or `sub`, as logent spells the stream in tokens.
    fn kind(index: usize) -> &'static str {
        if index == 0 {
            "main"
        } else {
            "sub"
        }
    }

    fn profile_token(self, index: usize) -> String {
        if self.logent() {
            format!("cam-0-{}", Self::kind(index))
        } else {
            format!("profile_{}", index + 1)
        }
    }

    fn profile_name(self, config: &Config, index: usize, stream: &Stream) -> String {
        if self.logent() {
            format!("Cam_{}", Self::kind(index))
        } else if index == 0 {
            config.camera_name.clone()
        } else {
            format!("{} {}", config.camera_name, stream.name)
        }
    }

    fn video_source_token(self) -> &'static str {
        if self.logent() {
            "cam-0"
        } else {
            "video_source_1"
        }
    }

    fn video_source_config_token(self) -> &'static str {
        if self.logent() {
            "cam-0-vcfg"
        } else {
            "video_source_config_1"
        }
    }

    fn video_source_config_name(self) -> &'static str {
        if self.logent() {
            "Cam_vsrc"
        } else {
            "VideoSourceConfig"
        }
    }

    fn video_encoder_token(self, index: usize) -> String {
        if self.logent() {
            format!("cam-0-venc-{}", Self::kind(index))
        } else {
            format!("video_encoder_config_{}", index + 1)
        }
    }

    /// `family` is the encoder family (`H264` or `H265`) of the default names.
    fn video_encoder_name(self, index: usize, family: &str) -> String {
        match (self.logent(), index == 0) {
            (true, true) => "Cam_vmain".to_string(),
            (true, false) => "Cam_vsub".to_string(),
            (false, true) => format!("{family}Encoder"),
            (false, false) => format!("{family}EncoderSub"),
        }
    }

    fn audio_source_token(self) -> &'static str {
        if self.logent() {
            "cam-0-a"
        } else {
            "audio_source_1"
        }
    }

    fn audio_source_config_token(self) -> &'static str {
        if self.logent() {
            "cam-0-acfg"
        } else {
            "audio_source_config_1"
        }
    }

    fn audio_source_config_name(self) -> &'static str {
        if self.logent() {
            "Cam_asrc"
        } else {
            "AudioSourceConfig"
        }
    }

    fn audio_encoder_token(self) -> &'static str {
        if self.logent() {
            "cam-0-aenc"
        } else {
            "audio_encoder_config_1"
        }
    }

    fn audio_encoder_name(self) -> &'static str {
        if self.logent() {
            "Cam_a"
        } else {
            "AudioEncoder"
        }
    }
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
        (0..streams.len()).find(|index| names(config).profile_token(*index) == token)
    } else if let Some(token) = request_value(request, "ConfigurationToken") {
        (0..streams.len()).find(|index| names(config).video_encoder_token(*index) == token)
    } else {
        Some(0)
    };

    found
        .map(|index| (index, streams[index].clone()))
        .ok_or_else(|| "No such profile or configuration token".to_string())
}

pub fn device_xaddr(config: &Config) -> String {
    Service::Device.xaddr(config)
}

pub fn media_xaddr(config: &Config) -> String {
    Service::Media.xaddr(config)
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
        "GetNetworkProtocols",
        "GetNetworkDefaultGateway",
        "GetDiscoveryMode",
        "GetUsers",
        "GetDNS",
        "GetNTP",
        "GetZeroConfiguration",
        "GetRelayOutputs",
        "GetWsdlUrl",
        "GetDigitalInputs",
        "GetIPAddressFilter",
        "GetSystemUris",
        "GetProfiles",
        "GetProfile",
        "GetStreamUri",
        "GetSnapshotUri",
        "GetVideoSources",
        "GetVideoSourceConfigurations",
        "GetVideoEncoderConfigurations",
        "GetVideoEncoderConfigurationOptions",
        "GetCompatibleVideoEncoderConfigurations",
        "GetEventProperties",
        "CreatePullPointSubscription",
        "PullMessages",
        "Renew",
        "Unsubscribe",
        "GetImagingSettings",
        "GetMetadataConfigurations",
        "GetAudioSources",
        "GetAudioEncoderConfigurations",
        "GetAudioSourceConfigurations",
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
    let scopes = util::xml_escape(
        &scope_list(config)
            .iter()
            .map(|(_, item)| item.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
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
        <d:Scopes>{scopes}</d:Scopes>
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
    Imaging,
    DeviceIo,
    Analytics,
    Events,
}

impl Service {
    /// The services served by [`services::soap_body`] (everything but Device and Media).
    const EXTRA: [Service; 4] = [
        Service::Imaging,
        Service::DeviceIo,
        Service::Analytics,
        Service::Events,
    ];

    pub fn from_path(path: &str) -> Option<Self> {
        [Self::Device, Self::Media]
            .into_iter()
            .chain(Self::EXTRA)
            .find(|service| service.path() == path)
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::Device => "/onvif/device_service",
            Self::Media => "/onvif/media_service",
            Self::Imaging => "/onvif/imaging_service",
            Self::DeviceIo => "/onvif/deviceIO_service",
            Self::Analytics => "/onvif/analytics_service",
            Self::Events => "/onvif/event_service",
        }
    }

    fn namespace(self) -> &'static str {
        match self {
            Self::Device => "http://www.onvif.org/ver10/device/wsdl",
            Self::Media => "http://www.onvif.org/ver10/media/wsdl",
            Self::Imaging => "http://www.onvif.org/ver20/imaging/wsdl",
            Self::DeviceIo => "http://www.onvif.org/ver10/deviceIO/wsdl",
            Self::Analytics => "http://www.onvif.org/ver20/analytics/wsdl",
            Self::Events => "http://www.onvif.org/ver10/events/wsdl",
        }
    }

    fn xaddr(self, config: &Config) -> String {
        format!(
            "http://{}:{}{}",
            config.advertise_host,
            config.http_addr.port(),
            self.path()
        )
    }
}

fn event_xaddr(config: &Config) -> String {
    Service::Events.xaddr(config)
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
    if Service::EXTRA.contains(&service) {
        return envelope(&services::soap_body(service, operation, config, request));
    }

    let body = match operation {
        Some("GetCapabilities") => get_capabilities(config),
        Some("GetDeviceInformation") => get_device_information(config),
        Some("GetServices") => get_services(config),
        Some("GetSystemDateAndTime") => get_system_date_and_time(),
        Some("GetScopes") => get_scopes(config),
        Some("GetHostname") => get_hostname(config),
        Some("GetNetworkInterfaces") => get_network_interfaces(config),
        Some("GetNetworkProtocols") => get_network_protocols(config),
        Some("GetNetworkDefaultGateway") => get_network_default_gateway(config),
        Some("GetDiscoveryMode") => {
            "<tds:GetDiscoveryModeResponse><tds:DiscoveryMode>Discoverable</tds:DiscoveryMode></tds:GetDiscoveryModeResponse>"
                .to_string()
        }
        Some("GetUsers") => get_users(config),
        Some("GetDNS") => {
            "<tds:GetDNSResponse><tds:DNSInformation><tt:FromDHCP>true</tt:FromDHCP></tds:DNSInformation></tds:GetDNSResponse>"
                .to_string()
        }
        Some("GetNTP") => {
            "<tds:GetNTPResponse><tds:NTPInformation><tt:FromDHCP>false</tt:FromDHCP></tds:NTPInformation></tds:GetNTPResponse>"
                .to_string()
        }
        Some("GetZeroConfiguration") => {
            "<tds:GetZeroConfigurationResponse><tds:ZeroConfiguration/></tds:GetZeroConfigurationResponse>"
                .to_string()
        }
        Some("GetRelayOutputs") => "<tds:GetRelayOutputsResponse/>".to_string(),
        Some("GetWsdlUrl") => {
            "<tds:GetWsdlUrlResponse><tds:WsdlUrl>https://www.onvif.org/ver10/device/wsdl/devicemgmt.wsdl</tds:WsdlUrl></tds:GetWsdlUrlResponse>"
                .to_string()
        }
        Some("GetDigitalInputs") | Some("GetIPAddressFilter") | Some("GetSystemUris") => {
            action_not_supported_fault()
        }
        Some("GetProfiles") => get_profiles(config),
        Some("GetProfile") => requested_stream(config, request)
            .map(|(index, stream)| get_profile(config, index, &stream))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetStreamUri") => requested_stream(config, request)
            .map(|(_, stream)| get_stream_uri(config, &stream))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetSnapshotUri") if config.no_snapshot => action_not_supported_fault(),
        Some("GetSnapshotUri") => get_snapshot_uri(config),
        // The camera has no audio and no metadata streams: empty lists, like logent's metadata.
        Some("GetMetadataConfigurations") => "<trt:GetMetadataConfigurationsResponse/>".to_string(),
        Some("GetAudioSources") => get_audio_sources(config),
        Some("GetAudioEncoderConfigurations") => list_response(
            "trt:GetAudioEncoderConfigurationsResponse",
            &audio_encoder_configuration_xml(config, "trt:Configurations"),
        ),
        Some("GetAudioSourceConfigurations") => list_response(
            "trt:GetAudioSourceConfigurationsResponse",
            &audio_source_configuration_xml(config, "trt:Configurations"),
        ),
        Some("GetCompatibleVideoEncoderConfigurations") => requested_stream(config, request)
            .map(|_| get_compatible_video_encoder_configurations(config))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetVideoSources") => get_video_sources(config),
        Some("GetVideoSourceConfigurations") => get_video_source_configurations(config),
        Some("GetVideoEncoderConfigurations") => get_video_encoder_configurations(config),
        Some("GetVideoEncoderConfigurationOptions") => requested_stream(config, request)
            .map(|(_, stream)| get_video_encoder_configuration_options(&stream))
            .unwrap_or_else(|reason| soap_fault(&reason)),
        Some("GetServiceCapabilities") => match service {
            Service::Device => get_device_service_capabilities(config),
            _ => get_media_service_capabilities(config),
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
            xmlns:tt="http://www.onvif.org/ver10/schema"
            xmlns:timg="http://www.onvif.org/ver20/imaging/wsdl"
            xmlns:tmd="http://www.onvif.org/ver10/deviceIO/wsdl"
            xmlns:tan="http://www.onvif.org/ver20/analytics/wsdl"
            xmlns:tev="http://www.onvif.org/ver10/events/wsdl"
            xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2"
            xmlns:wstop="http://docs.oasis-open.org/wsn/t-1"
            xmlns:wsa5="http://www.w3.org/2005/08/addressing"
            xmlns:ter="http://www.onvif.org/ver10/error">
  <s:Body>
    {body}
  </s:Body>
</s:Envelope>"#
    )
}

fn get_capabilities(config: &Config) -> String {
    let device = util::xml_escape(&device_xaddr(config));
    let media = util::xml_escape(&media_xaddr(config));
    let analytics = util::xml_escape(&Service::Analytics.xaddr(config));
    let events = util::xml_escape(&Service::Events.xaddr(config));
    let imaging = util::xml_escape(&Service::Imaging.xaddr(config));
    let device_io = util::xml_escape(&Service::DeviceIo.xaddr(config));
    let audio_sources = u8::from(config.audio.is_some());

    format!(
        r#"<tds:GetCapabilitiesResponse>
  <tds:Capabilities>
    <tt:Analytics>
      <tt:XAddr>{analytics}</tt:XAddr>
      <tt:RuleSupport>false</tt:RuleSupport>
      <tt:AnalyticsModuleSupport>false</tt:AnalyticsModuleSupport>
    </tt:Analytics>
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
    <tt:Events>
      <tt:XAddr>{events}</tt:XAddr>
      <tt:WSSubscriptionPolicySupport>false</tt:WSSubscriptionPolicySupport>
      <tt:WSPullPointSupport>true</tt:WSPullPointSupport>
      <tt:WSPausableSubscriptionManagerInterfaceSupport>false</tt:WSPausableSubscriptionManagerInterfaceSupport>
    </tt:Events>
    <tt:Imaging>
      <tt:XAddr>{imaging}</tt:XAddr>
    </tt:Imaging>
    <tt:Media>
      <tt:XAddr>{media}</tt:XAddr>
      <tt:StreamingCapabilities>
        <tt:RTPMulticast>false</tt:RTPMulticast>
        <tt:RTP_TCP>true</tt:RTP_TCP>
        <tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP>
      </tt:StreamingCapabilities>
    </tt:Media>
    <tt:Extension>
      <tt:DeviceIO>
        <tt:XAddr>{device_io}</tt:XAddr>
        <tt:VideoSources>1</tt:VideoSources>
        <tt:VideoOutputs>0</tt:VideoOutputs>
        <tt:AudioSources>{audio_sources}</tt:AudioSources>
        <tt:AudioOutputs>0</tt:AudioOutputs>
        <tt:RelayOutputs>0</tt:RelayOutputs>
      </tt:DeviceIO>
    </tt:Extension>
  </tds:Capabilities>
</tds:GetCapabilitiesResponse>"#
    )
}

fn get_device_information(config: &Config) -> String {
    let manufacturer = util::xml_escape(&config.manufacturer);
    let model = util::xml_escape(&config.model);
    let firmware = util::xml_escape(&config.firmware);
    let serial = util::xml_escape(&config.serial);
    let uuid = util::xml_escape(&config.uuid);

    format!(
        r#"<tds:GetDeviceInformationResponse>
  <tds:Manufacturer>{manufacturer}</tds:Manufacturer>
  <tds:Model>{model}</tds:Model>
  <tds:FirmwareVersion>{firmware}</tds:FirmwareVersion>
  <tds:SerialNumber>{serial}</tds:SerialNumber>
  <tds:HardwareId>urn:uuid:{uuid}</tds:HardwareId>
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

    let extra: String = Service::EXTRA
        .iter()
        .map(|service| {
            format!(
                r#"
  <tds:Service>
    <tds:Namespace>{}</tds:Namespace>
    <tds:XAddr>{}</tds:XAddr>
    <tds:Version><tt:Major>2</tt:Major><tt:Minor>0</tt:Minor></tds:Version>
  </tds:Service>"#,
                service.namespace(),
                util::xml_escape(&service.xaddr(config))
            )
        })
        .collect();

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
  </tds:Service>{media2}{extra}
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

/// ONVIF scopes as `(definition, uri)`, shared by `GetScopes` and the WS-Discovery ProbeMatch.
/// `Profile/S` and `Profile/Streaming` are what NVRs usually filter discovery results on.
fn scope_list(config: &Config) -> Vec<(&'static str, String)> {
    const BASE: &str = "onvif://www.onvif.org";
    let mut scopes = vec![
        ("Fixed", format!("{BASE}/Profile/Streaming")),
        ("Fixed", format!("{BASE}/Profile/S")),
        ("Fixed", format!("{BASE}/type/video_encoder")),
    ];
    if config.audio.is_some() {
        scopes.push(("Fixed", format!("{BASE}/type/audio_encoder")));
    }
    scopes.extend([
        (
            "Fixed",
            format!("{BASE}/hardware/{}", util::percent_encode(&config.model)),
        ),
        (
            "Configurable",
            format!("{BASE}/name/{}", config.camera_name.replace(' ', "_")),
        ),
        ("Configurable", format!("{BASE}/location/TestLab")),
    ]);
    scopes
}

fn get_scopes(config: &Config) -> String {
    let scopes: String = scope_list(config)
        .iter()
        .map(|(definition, item)| {
            format!(
                "\n  <tds:Scopes>\n    <tt:ScopeDef>{definition}</tt:ScopeDef>\n    <tt:ScopeItem>{}</tt:ScopeItem>\n  </tds:Scopes>",
                util::xml_escape(item)
            )
        })
        .collect();

    format!("<tds:GetScopesResponse>{scopes}\n</tds:GetScopesResponse>")
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

fn get_network_protocols(config: &Config) -> String {
    format!(
        r#"<tds:GetNetworkProtocolsResponse>
  <tds:NetworkProtocols>
    <tt:Name>HTTP</tt:Name>
    <tt:Enabled>true</tt:Enabled>
    <tt:Port>{}</tt:Port>
  </tds:NetworkProtocols>
  <tds:NetworkProtocols>
    <tt:Name>RTSP</tt:Name>
    <tt:Enabled>true</tt:Enabled>
    <tt:Port>{}</tt:Port>
  </tds:NetworkProtocols>
</tds:GetNetworkProtocolsResponse>"#,
        config.http_addr.port(),
        config.rtsp_port
    )
}

/// The `.1` host of the advertised IPv4 subnet, or `0.0.0.0` for anything else.
fn get_network_default_gateway(config: &Config) -> String {
    let gateway = config
        .advertise_host
        .parse::<std::net::Ipv4Addr>()
        .map(|ip| {
            let [a, b, c, _] = ip.octets();
            std::net::Ipv4Addr::new(a, b, c, 1)
        })
        .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);

    format!(
        "<tds:GetNetworkDefaultGatewayResponse><tds:NetworkGateway><tt:IPv4Address>{gateway}</tt:IPv4Address></tds:NetworkGateway></tds:GetNetworkDefaultGatewayResponse>"
    )
}

/// Lists the configured camera user (the credentials are not checked).
fn get_users(config: &Config) -> String {
    let user = config
        .username
        .as_deref()
        .map(|username| {
            format!(
                "<tds:User><tt:Username>{}</tt:Username><tt:UserLevel>Administrator</tt:UserLevel></tds:User>",
                util::xml_escape(username)
            )
        })
        .unwrap_or_default();

    format!("<tds:GetUsersResponse>{user}</tds:GetUsersResponse>")
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
    names(config).profile_name(config, index, stream)
}

fn profile_xml(config: &Config, index: usize, stream: &Stream) -> String {
    let name = util::xml_escape(&profile_name(config, index, stream));
    let token = names(config).profile_token(index);

    let audio_source = audio_source_configuration_xml(config, "tt:AudioSourceConfiguration");
    let audio_encoder = audio_encoder_configuration_xml(config, "tt:AudioEncoderConfiguration");

    format!(
        r#"<trt:Profiles token="{token}" fixed="true">
    <tt:Name>{name}</tt:Name>
    {}{audio_source}
    {}{audio_encoder}
  </trt:Profiles>"#,
        video_source_configuration_xml(config),
        video_encoder_configuration_xml(config, index, stream)
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
  <trt:VideoSources token="{}">
    <tt:Framerate>{}</tt:Framerate>
    <tt:Resolution>
      <tt:Width>{}</tt:Width>
      <tt:Height>{}</tt:Height>
    </tt:Resolution>
  </trt:VideoSources>
</trt:GetVideoSourcesResponse>"#,
        names(config).video_source_token(),
        config.fps,
        config.width,
        config.height
    )
}

/// Every video encoder configuration can be added to any (video-only) profile of this camera.
fn get_compatible_video_encoder_configurations(config: &Config) -> String {
    let configurations: Vec<String> = config
        .streams()
        .iter()
        .enumerate()
        .map(|(index, stream)| {
            // In a list response the element is `trt:Configurations`, not `tt:VideoEncoderConfiguration`.
            video_encoder_configuration_xml(config, index, stream)
                .replace("tt:VideoEncoderConfiguration", "trt:Configurations")
        })
        .collect();

    format!(
        "<trt:GetCompatibleVideoEncoderConfigurationsResponse>\n  {}\n</trt:GetCompatibleVideoEncoderConfigurationsResponse>",
        configurations.join("\n  ")
    )
}

/// A list response: an empty element when there is nothing to list.
fn list_response(element: &str, items: &str) -> String {
    if items.is_empty() {
        format!("<{element}/>")
    } else {
        format!("<{element}>\n  {items}\n</{element}>")
    }
}

fn get_audio_sources(config: &Config) -> String {
    if config.audio.is_none() {
        return "<trt:GetAudioSourcesResponse/>".to_string();
    }

    format!(
        r#"<trt:GetAudioSourcesResponse>
  <trt:AudioSources token="{}">
    <tt:Channels>1</tt:Channels>
  </trt:AudioSources>
</trt:GetAudioSourcesResponse>"#,
        names(config).audio_source_token()
    )
}

/// Empty when the camera has no audio. All streams share one audio source configuration.
fn audio_source_configuration_xml(config: &Config, element: &str) -> String {
    if config.audio.is_none() {
        return String::new();
    }

    let names = names(config);
    format!(
        r#"<{element} token="{}">
      <tt:Name>{}</tt:Name>
      <tt:UseCount>{}</tt:UseCount>
      <tt:SourceToken>{}</tt:SourceToken>
    </{element}>"#,
        names.audio_source_config_token(),
        names.audio_source_config_name(),
        config.streams().len(),
        names.audio_source_token()
    )
}

/// Empty when the camera has no audio. Media1 can only name G711 (see `onvif_encoding`).
fn audio_encoder_configuration_xml(config: &Config, element: &str) -> String {
    let Some(codec) = config.audio else {
        return String::new();
    };

    format!(
        r#"<{element} token="{}">
      <tt:Name>{}</tt:Name>
      <tt:UseCount>{}</tt:UseCount>
      <tt:Encoding>{}</tt:Encoding>
      <tt:Bitrate>{}</tt:Bitrate>
      <tt:SampleRate>{}</tt:SampleRate>
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
    </{element}>"#,
        names(config).audio_encoder_token(),
        names(config).audio_encoder_name(),
        config.streams().len(),
        codec.onvif_encoding(),
        codec.bitrate_kbps(),
        codec.sample_rate() / 1000
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
        r#"<tt:VideoSourceConfiguration token="{}">
      <tt:Name>{}</tt:Name>
      <tt:UseCount>{}</tt:UseCount>
      <tt:SourceToken>{}</tt:SourceToken>
      <tt:Bounds x="0" y="0" width="{}" height="{}"/>
    </tt:VideoSourceConfiguration>"#,
        names(config).video_source_config_token(),
        names(config).video_source_config_name(),
        config.streams().len(),
        names(config).video_source_token(),
        config.width,
        config.height
    )
}

fn get_video_encoder_configurations(config: &Config) -> String {
    let configurations: Vec<String> = config
        .streams()
        .iter()
        .enumerate()
        .map(|(index, stream)| video_encoder_configuration_xml(config, index, stream))
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
fn video_encoder_configuration_xml(config: &Config, index: usize, stream: &Stream) -> String {
    let token = names(config).video_encoder_token(index);
    let name = names(config).video_encoder_name(index, "H264");

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
    <tt:Extension>
      <tt:H264>
        <tt:ResolutionsAvailable>
          <tt:Width>{0}</tt:Width>
          <tt:Height>{1}</tt:Height>
        </tt:ResolutionsAvailable>
        <tt:GovLengthRange>
          <tt:Min>{2}</tt:Min>
          <tt:Max>{3}</tt:Max>
        </tt:GovLengthRange>
        <tt:FrameRateRange>
          <tt:Min>1</tt:Min>
          <tt:Max>{4}</tt:Max>
        </tt:FrameRateRange>
        <tt:EncodingIntervalRange>
          <tt:Min>1</tt:Min>
          <tt:Max>1</tt:Max>
        </tt:EncodingIntervalRange>
        <tt:H264ProfilesSupported>Baseline</tt:H264ProfilesSupported>
        <tt:BitrateRange>
          <tt:Min>1</tt:Min>
          <tt:Max>{5}</tt:Max>
        </tt:BitrateRange>
      </tt:H264>
    </tt:Extension>
  </trt:Options>
</trt:GetVideoEncoderConfigurationOptionsResponse>"#,
        stream.width,
        stream.height,
        stream.fps,
        stream.fps * 4,
        stream.fps,
        bitrate_limit(stream)
    )
}

/// Device service capabilities (tds). TLS is not implemented.
fn get_device_service_capabilities(config: &Config) -> String {
    let auth = config.auth_credentials().is_some();

    format!(
        r#"<tds:GetServiceCapabilitiesResponse>
  <tds:Capabilities>
    <tds:Network IPFilter="false" ZeroConfiguration="false" IPVersion6="false" DynDNS="false"/>
    <tds:Security TLS1.0="false" TLS1.1="false" TLS1.2="false" UsernameToken="{auth}" HttpDigest="{auth}"/>
    <tds:System DiscoveryResolve="false" DiscoveryBye="false" RemoteDiscovery="false" SystemLogging="false" FirmwareUpgrade="false"/>
  </tds:Capabilities>
</tds:GetServiceCapabilitiesResponse>"#
    )
}

/// Media service capabilities (trt).
fn get_media_service_capabilities(config: &Config) -> String {
    format!(
        r#"<trt:GetServiceCapabilitiesResponse>
  <trt:Capabilities SnapshotUri="{}" Rotation="false" VideoSourceMode="false" OSD="false">
    <trt:ProfileCapabilities MaximumNumberOfProfiles="128"/>
    <trt:StreamingCapabilities RTPMulticast="false" RTP_TCP="true" RTP_RTSP_TCP="true" NonAggregateControl="false"/>
  </trt:Capabilities>
</trt:GetServiceCapabilitiesResponse>"#,
        !config.no_snapshot
    )
}

/// Fault real cameras return for operations they do not implement.
fn action_not_supported_fault() -> String {
    r#"<s:Fault>
  <s:Code>
    <s:Value>s:Receiver</s:Value>
    <s:Subcode><s:Value>ter:ActionNotSupported</s:Value></s:Subcode>
  </s:Code>
  <s:Reason><s:Text xml:lang="en">Not supported</s:Text></s:Reason>
</s:Fault>"#
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
    use crate::config::{AudioCodec, SubStream};

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
                file: None,
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
    fn scopes_include_profile_s_and_match_discovery() {
        let mut config = two_stream_config();
        config.camera_name = "Cam 1".to_string();
        config.model = "Fake Cam".to_string();

        let scopes = get_scopes(&config);
        for item in [
            "onvif://www.onvif.org/Profile/Streaming",
            "onvif://www.onvif.org/Profile/S",
            "onvif://www.onvif.org/type/video_encoder",
            "onvif://www.onvif.org/hardware/Fake%20Cam",
            "onvif://www.onvif.org/name/Cam_1",
            "onvif://www.onvif.org/location/TestLab",
        ] {
            assert!(
                scopes.contains(&format!("<tt:ScopeItem>{item}</tt:ScopeItem>")),
                "{item}"
            );
            assert!(
                discovery_probe_match(&config, "uuid:x").contains(item),
                "{item}"
            );
        }
        assert!(scopes.contains("<tt:ScopeDef>Fixed</tt:ScopeDef>"));
    }

    #[test]
    fn hardware_id_is_the_uuid_urn() {
        let mut config = two_stream_config();
        config.uuid = "4919811a-5e84-4f3e-b5dd-00000000fc01".to_string();
        assert!(get_device_information(&config).contains(
            "<tds:HardwareId>urn:uuid:4919811a-5e84-4f3e-b5dd-00000000fc01</tds:HardwareId>"
        ));
    }

    fn device(op: &str, config: &Config) -> String {
        let request = format!("<tds:{op}/>");
        let operation = detect_operation(&request);
        assert_eq!(operation.as_deref(), Some(op));
        soap_response_for_service(Service::Device, operation.as_deref(), config, &request)
    }

    #[test]
    fn device_network_operations() {
        let mut config = two_stream_config();
        config.advertise_host = "192.168.7.42".to_string();
        config.http_addr = "0.0.0.0:8123".parse().unwrap();
        config.rtsp_port = 8654;

        let protocols = device("GetNetworkProtocols", &config);
        assert!(protocols.contains("<tt:Port>8123</tt:Port>"));
        assert!(protocols.contains("<tt:Port>8654</tt:Port>"));
        assert!(device("GetNetworkDefaultGateway", &config).contains("192.168.7.1<"));
        assert!(device("GetDiscoveryMode", &config).contains("Discoverable"));
        assert!(device("GetDNS", &config).contains("<tt:FromDHCP>true"));
        assert!(device("GetNTP", &config).contains("<tt:FromDHCP>false"));
        assert!(device("GetZeroConfiguration", &config).contains("ZeroConfiguration"));
        assert!(device("GetRelayOutputs", &config).contains("GetRelayOutputsResponse"));
        assert!(device("GetWsdlUrl", &config).contains("devicemgmt.wsdl"));
    }

    #[test]
    fn users_list_the_configured_user() {
        let mut config = two_stream_config();
        assert!(!device("GetUsers", &config).contains("<tds:User>"));
        config.username = Some("ad<min".to_string());
        let xml = device("GetUsers", &config);
        assert!(xml.contains("<tt:Username>ad&lt;min</tt:Username>"));
        assert!(xml.contains("Administrator"));
    }

    #[test]
    fn unimplemented_device_operations_are_action_not_supported() {
        let config = two_stream_config();
        for op in ["GetDigitalInputs", "GetIPAddressFilter", "GetSystemUris"] {
            assert!(
                device(op, &config).contains("ter:ActionNotSupported"),
                "{op}"
            );
        }
    }

    #[test]
    fn device_capabilities_announce_authentication() {
        let mut config = two_stream_config();
        let request = "<tds:GetServiceCapabilities/>";
        let capabilities = |config: &Config| {
            soap_response_for_service(
                Service::Device,
                Some("GetServiceCapabilities"),
                config,
                request,
            )
        };
        assert!(capabilities(&config).contains(r#"UsernameToken="false" HttpDigest="false""#));

        config.username = Some("admin".to_string());
        config.password = Some("admin123".to_string());
        assert!(capabilities(&config).contains(r#"UsernameToken="true" HttpDigest="true""#));

        config.no_auth = true;
        assert!(capabilities(&config).contains(r#"UsernameToken="false""#));
    }

    fn media(op: &str, config: &Config, body: &str) -> String {
        let request = format!("<trt:{op}>{body}</trt:{op}>");
        let operation = detect_operation(&request);
        assert_eq!(operation.as_deref(), Some(op));
        soap_response_for_service(Service::Media, operation.as_deref(), config, &request)
    }

    #[test]
    fn empty_audio_and_metadata_lists() {
        let config = two_stream_config();
        for (op, response) in [
            (
                "GetMetadataConfigurations",
                "GetMetadataConfigurationsResponse",
            ),
            ("GetAudioSources", "GetAudioSourcesResponse"),
            (
                "GetAudioEncoderConfigurations",
                "GetAudioEncoderConfigurationsResponse",
            ),
            (
                "GetAudioSourceConfigurations",
                "GetAudioSourceConfigurationsResponse",
            ),
        ] {
            let xml = media(op, &config, "");
            assert!(xml.contains(&format!("<trt:{response}/>")), "{op}");
        }
    }

    fn audio_config(codec: AudioCodec) -> Config {
        Config {
            audio: Some(codec),
            ..two_stream_config()
        }
    }

    #[test]
    fn audio_shows_up_in_profiles_sources_and_scopes() {
        let config = audio_config(AudioCodec::Pcma);

        let profiles = get_profiles(&config);
        assert_eq!(profiles.matches("<tt:AudioSourceConfiguration").count(), 2);
        assert_eq!(profiles.matches("<tt:AudioEncoderConfiguration").count(), 2);
        assert!(profiles.contains("<tt:Encoding>G711</tt:Encoding>"));
        assert!(profiles.contains("<tt:SampleRate>8</tt:SampleRate>"));
        assert!(!get_profiles(&two_stream_config()).contains("Audio"));

        let sources = media("GetAudioSources", &config, "");
        assert!(sources.contains(r#"<trt:AudioSources token="audio_source_1">"#));
        let encoders = media("GetAudioEncoderConfigurations", &config, "");
        assert!(encoders.contains(r#"<trt:Configurations token="audio_encoder_config_1">"#));
        let source_configs = media("GetAudioSourceConfigurations", &config, "");
        assert!(source_configs.contains("<tt:SourceToken>audio_source_1</tt:SourceToken>"));

        assert!(get_scopes(&config).contains("type/audio_encoder"));
        assert!(!get_scopes(&two_stream_config()).contains("audio_encoder"));
        assert!(get_capabilities(&config).contains("<tt:AudioSources>1</tt:AudioSources>"));
    }

    fn logent_config() -> Config {
        Config {
            naming: Naming::Logent,
            audio: Some(AudioCodec::Pcmu),
            rtsp_path: "cam-0_m".to_string(),
            ..two_stream_config()
        }
    }

    #[test]
    fn logent_naming_renames_tokens_and_names() {
        let mut config = logent_config();
        config.sub.as_mut().unwrap().rtsp_path = "cam-0_s".to_string();
        let profiles = get_profiles(&config);

        for expected in [
            r#"<trt:Profiles token="cam-0-main""#,
            r#"<trt:Profiles token="cam-0-sub""#,
            "<tt:Name>Cam_main</tt:Name>",
            "<tt:Name>Cam_sub</tt:Name>",
            r#"<tt:VideoSourceConfiguration token="cam-0-vcfg">"#,
            "<tt:Name>Cam_vsrc</tt:Name>",
            "<tt:SourceToken>cam-0</tt:SourceToken>",
            r#"<tt:VideoEncoderConfiguration token="cam-0-venc-main">"#,
            r#"<tt:VideoEncoderConfiguration token="cam-0-venc-sub">"#,
            "<tt:Name>Cam_vmain</tt:Name>",
            "<tt:Name>Cam_vsub</tt:Name>",
            r#"<tt:AudioSourceConfiguration token="cam-0-acfg">"#,
            "<tt:SourceToken>cam-0-a</tt:SourceToken>",
            r#"<tt:AudioEncoderConfiguration token="cam-0-aenc">"#,
            "<tt:Name>Cam_asrc</tt:Name>",
            "<tt:Name>Cam_a</tt:Name>",
        ] {
            assert!(profiles.contains(expected), "{expected}");
        }
        assert!(!profiles.contains("profile_1") && !profiles.contains("video_source_1"));
        assert!(get_video_sources(&config).contains(r#"VideoSources token="cam-0""#));
        assert!(media("GetAudioSources", &config, "").contains(r#"AudioSources token="cam-0-a""#));
    }

    #[test]
    fn logent_tokens_are_what_requests_are_matched_against() {
        let config = logent_config();
        let uri = |token: &str| {
            soap_response(
                Some("GetStreamUri"),
                &config,
                &request("GetStreamUri", token),
            )
        };
        assert!(uri("cam-0-main").contains("/cam-0_m</tt:Uri>"));
        assert!(uri("cam-0-sub").contains("/live_sub</tt:Uri>"));
        assert!(
            uri("profile_1").contains("<s:Fault>"),
            "default tokens no longer match"
        );
        // Media2 uses the same names.
        let media2 = soap_response_media2(
            Some("GetProfiles"),
            &config,
            "<tr2:GetProfiles><tr2:Token>cam-0-sub</tr2:Token></tr2:GetProfiles>",
        );
        assert!(media2.contains(r#"<tr2:Profiles token="cam-0-sub""#));
        assert!(!media2.contains("cam-0-main"));
    }

    #[test]
    fn default_naming_is_unchanged_by_the_logent_option() {
        let profiles = get_profiles(&two_stream_config());
        assert!(profiles.contains(r#"token="profile_1""#));
        assert!(profiles.contains(r#"token="video_encoder_config_2""#));
        assert!(!profiles.contains("cam-0"));
    }

    #[test]
    fn g722_is_g711_in_media1_but_g722_in_media2() {
        let config = audio_config(AudioCodec::G722);
        let media1 = get_profiles(&config);
        assert!(media1.contains("<tt:Encoding>G711</tt:Encoding>"));
        assert!(media1.contains("<tt:SampleRate>16</tt:SampleRate>"));

        let media2 = soap_response_media2(Some("GetProfiles"), &config, "<tr2:GetProfiles/>");
        assert!(media2.contains("<tt:Encoding>G722</tt:Encoding>"));
    }

    #[test]
    fn compatible_video_encoder_configurations_list_all_and_check_the_token() {
        let config = two_stream_config();
        let ok = media(
            "GetCompatibleVideoEncoderConfigurations",
            &config,
            "<trt:ProfileToken>profile_2</trt:ProfileToken>",
        );
        assert!(ok.contains(r#"token="video_encoder_config_1""#));
        assert!(ok.contains(r#"token="video_encoder_config_2""#));
        assert!(ok.contains("<trt:Configurations token="));
        assert!(!ok.contains("tt:VideoEncoderConfiguration"));

        let bad = media(
            "GetCompatibleVideoEncoderConfigurations",
            &config,
            "<trt:ProfileToken>nope</trt:ProfileToken>",
        );
        assert!(bad.contains("<s:Fault>"));
    }

    #[test]
    fn encoder_options_carry_a_bitrate_range() {
        let options = get_video_encoder_configuration_options(&two_stream_config().streams()[0]);
        assert!(options.contains("<tt:Extension>"));
        assert!(options.contains("<tt:BitrateRange>"));
    }

    #[test]
    fn snapshots_can_be_switched_off() {
        let mut config = two_stream_config();
        let request = "<trt:GetSnapshotUri><trt:ProfileToken>profile_1</trt:ProfileToken></trt:GetSnapshotUri>";
        let snapshot = |config: &Config| {
            soap_response_for_service(Service::Media, Some("GetSnapshotUri"), config, request)
        };
        assert!(snapshot(&config).contains("/snapshot.jpg"));
        assert!(get_media_service_capabilities(&config).contains(r#"SnapshotUri="true""#));

        config.no_snapshot = true;
        assert!(snapshot(&config).contains("ter:ActionNotSupported"));
        assert!(get_media_service_capabilities(&config).contains(r#"SnapshotUri="false""#));
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
