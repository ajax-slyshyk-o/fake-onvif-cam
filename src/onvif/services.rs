//! Small ONVIF services that only exist so an NVR finds what it expects on a real camera:
//! Imaging, DeviceIO, Analytics and Events. They report no analytics, no events and default
//! image settings; the Events service accepts pull-point subscriptions and always answers
//! with an empty batch.

use super::{action_not_supported_fault, names, request_value, soap_fault, Service};
use crate::config::Config;
use crate::util;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_SUBSCRIPTION: AtomicU64 = AtomicU64::new(1);

/// Lifetime reported for a pull-point subscription (and used by `Renew`).
const SUBSCRIPTION_SECONDS: i64 = 60;

/// The longest a `PullMessages` request is held open, whatever `Timeout` the client asks for.
const MAX_PULL_WAIT: Duration = Duration::from_secs(5);

pub fn soap_body(
    service: Service,
    operation: Option<&str>,
    config: &Config,
    request: &str,
) -> String {
    let Some(operation) = operation else {
        return soap_fault("Could not detect ONVIF operation");
    };

    match (service, operation) {
        (Service::Imaging, "GetServiceCapabilities") => {
            r#"<timg:GetServiceCapabilitiesResponse>
  <timg:Capabilities ImageStabilization="false" Presets="false" AdaptablePreset="false"/>
</timg:GetServiceCapabilitiesResponse>"#
                .to_string()
        }
        (Service::Imaging, "GetImagingSettings") => {
            if request_value(request, "VideoSourceToken").is_some_and(|token| token != names(config).video_source_token()) {
                return soap_fault("No such video source");
            }
            r#"<timg:GetImagingSettingsResponse>
  <timg:ImagingSettings>
    <tt:Brightness>50</tt:Brightness>
    <tt:ColorSaturation>50</tt:ColorSaturation>
    <tt:Contrast>50</tt:Contrast>
    <tt:Sharpness>50</tt:Sharpness>
  </timg:ImagingSettings>
</timg:GetImagingSettingsResponse>"#
                .to_string()
        }
        (Service::DeviceIo, "GetServiceCapabilities") => format!(
            r#"<tmd:GetServiceCapabilitiesResponse>
  <tmd:Capabilities VideoSources="1" VideoOutputs="0" AudioSources="{}" AudioOutputs="0" RelayOutputs="0" DigitalInputs="0" SerialPorts="0"/>
</tmd:GetServiceCapabilitiesResponse>"#,
            u8::from(config.audio.is_some())
        ),
        (Service::Analytics, "GetServiceCapabilities") => {
            r#"<tan:GetServiceCapabilitiesResponse>
  <tan:Capabilities RuleSupport="false" AnalyticsModuleSupport="false" CellBasedSceneDescriptionSupported="false"/>
</tan:GetServiceCapabilitiesResponse>"#
                .to_string()
        }
        (Service::Events, "GetServiceCapabilities") => {
            r#"<tev:GetServiceCapabilitiesResponse>
  <tev:Capabilities WSSubscriptionPolicySupport="false" WSPullPointSupport="true" WSPausableSubscriptionManagerInterfaceSupport="false" MaxNotificationProducers="20" MaxPullPoints="20" PersistentNotificationStorage="false"/>
</tev:GetServiceCapabilitiesResponse>"#
                .to_string()
        }
        (Service::Events, "GetEventProperties") => {
            r#"<tev:GetEventPropertiesResponse>
  <tev:TopicNamespaceLocation>http://www.onvif.org/onvif/ver10/topics/topicns.xml</tev:TopicNamespaceLocation>
  <wsnt:FixedTopicSet>true</wsnt:FixedTopicSet>
  <wstop:TopicSet/>
  <wsnt:TopicExpressionDialect>http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet</wsnt:TopicExpressionDialect>
  <wsnt:TopicExpressionDialect>http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete</wsnt:TopicExpressionDialect>
  <tev:MessageContentFilterDialect>http://www.onvif.org/ver10/tev/messageContentFilter/ItemFilter</tev:MessageContentFilterDialect>
  <tev:MessageContentSchemaLocation>http://www.onvif.org/onvif/ver10/schema/onvif.xsd</tev:MessageContentSchemaLocation>
</tev:GetEventPropertiesResponse>"#
                .to_string()
        }
        (Service::Events, "CreatePullPointSubscription") => create_pull_point(config),
        (Service::Events, "PullMessages") => format!(
            r#"<tev:PullMessagesResponse>
  <tev:CurrentTime>{}</tev:CurrentTime>
  <tev:TerminationTime>{}</tev:TerminationTime>
</tev:PullMessagesResponse>"#,
            util::utc_iso(0),
            util::utc_iso(SUBSCRIPTION_SECONDS)
        ),
        (Service::Events, "Renew") => format!(
            r#"<wsnt:RenewResponse>
  <wsnt:TerminationTime>{}</wsnt:TerminationTime>
  <wsnt:CurrentTime>{}</wsnt:CurrentTime>
</wsnt:RenewResponse>"#,
            util::utc_iso(SUBSCRIPTION_SECONDS),
            util::utc_iso(0)
        ),
        (Service::Events, "Unsubscribe") => "<wsnt:UnsubscribeResponse/>".to_string(),
        (Service::Events, "SetSynchronizationPoint") => {
            "<tev:SetSynchronizationPointResponse/>".to_string()
        }
        _ => action_not_supported_fault(),
    }
}

fn create_pull_point(config: &Config) -> String {
    let id = NEXT_SUBSCRIPTION.fetch_add(1, Ordering::Relaxed);
    let address = util::xml_escape(&format!("{}?sub={id}", super::event_xaddr(config)));

    format!(
        r#"<tev:CreatePullPointSubscriptionResponse>
  <tev:SubscriptionReference>
    <wsa5:Address>{address}</wsa5:Address>
  </tev:SubscriptionReference>
  <wsnt:CurrentTime>{}</wsnt:CurrentTime>
  <wsnt:TerminationTime>{}</wsnt:TerminationTime>
</tev:CreatePullPointSubscriptionResponse>"#,
        util::utc_iso(0),
        util::utc_iso(SUBSCRIPTION_SECONDS)
    )
}

/// How long to hold a `PullMessages` request open before answering: the requested `Timeout`,
/// at most [`MAX_PULL_WAIT`]. There are never events to deliver, so this only paces the
/// client's polling like a real camera would.
pub fn pull_wait(request: &str, operation: Option<&str>) -> Duration {
    if operation != Some("PullMessages") {
        return Duration::ZERO;
    }
    request_value(request, "Timeout")
        .and_then(util::parse_xs_duration)
        .map_or(Duration::ZERO, |wait| wait.min(MAX_PULL_WAIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(service: Service, op: &str, request: &str) -> String {
        soap_body(service, Some(op), &Config::default(), request)
    }

    #[test]
    fn capabilities_of_each_service() {
        assert!(call(Service::Imaging, "GetServiceCapabilities", "").contains("timg:Capabilities"));
        assert!(
            call(Service::DeviceIo, "GetServiceCapabilities", "").contains(r#"VideoSources="1""#)
        );
        assert!(call(Service::Analytics, "GetServiceCapabilities", "")
            .contains(r#"RuleSupport="false""#));
        assert!(call(Service::Events, "GetServiceCapabilities", "")
            .contains(r#"WSPullPointSupport="true""#));
    }

    #[test]
    fn pull_point_subscription_flow() {
        let first = call(Service::Events, "CreatePullPointSubscription", "");
        let second = call(Service::Events, "CreatePullPointSubscription", "");
        assert!(first.contains("/onvif/event_service?sub="));
        assert_ne!(first, second, "every subscription gets its own address");

        let pulled = call(Service::Events, "PullMessages", "");
        assert!(pulled.contains("<tev:PullMessagesResponse>"));
        assert!(!pulled.contains("NotificationMessage"));
        assert!(call(Service::Events, "Renew", "").contains("RenewResponse"));
        assert!(call(Service::Events, "Unsubscribe", "").contains("UnsubscribeResponse"));
        assert!(call(Service::Events, "GetEventProperties", "").contains("FixedTopicSet"));
    }

    #[test]
    fn imaging_checks_the_video_source() {
        let ok = call(
            Service::Imaging,
            "GetImagingSettings",
            &format!(
                "<timg:VideoSourceToken>{}</timg:VideoSourceToken>",
                names(&Config::default()).video_source_token()
            ),
        );
        assert!(ok.contains("<tt:Brightness>"));
        let bad = call(
            Service::Imaging,
            "GetImagingSettings",
            "<timg:VideoSourceToken>nope</timg:VideoSourceToken>",
        );
        assert!(bad.contains("<s:Fault>"));
    }

    #[test]
    fn unknown_operations_are_not_supported() {
        assert!(
            call(Service::Analytics, "GetSupportedRules", "").contains("ter:ActionNotSupported")
        );
        assert!(soap_body(Service::Events, None, &Config::default(), "").contains("<s:Fault>"));
    }

    #[test]
    fn pull_wait_follows_the_timeout_up_to_the_cap() {
        let wait = |value: &str| {
            pull_wait(
                &format!("<tev:Timeout>{value}</tev:Timeout>"),
                Some("PullMessages"),
            )
        };
        assert_eq!(wait("PT1S"), Duration::from_secs(1));
        assert_eq!(wait("PT0.5S"), Duration::from_millis(500));
        assert_eq!(wait("PT1M"), MAX_PULL_WAIT);
        assert_eq!(wait("garbage"), Duration::ZERO);
        assert_eq!(pull_wait("<x/>", Some("PullMessages")), Duration::ZERO);
        assert_eq!(
            pull_wait("<tev:Timeout>PT3S</tev:Timeout>", Some("Renew")),
            Duration::ZERO
        );
    }
}
