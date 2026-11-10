//! Optional authentication for the ONVIF SOAP endpoints and RTSP.
//!
//! Enabled per camera when both `username` and `password` are configured (and `--no-auth` /
//! `auth = false` is not set). Supported: WS-Security UsernameToken (digest and text) and HTTP
//! Digest (RFC 2617, MD5) on SOAP, and Digest on RTSP. Nonces are stateless: any nonce the client
//! echoes is accepted, so this is a login check for NVR testing, not a security boundary.

use crate::config::Config;
use crate::util;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use md5::{Digest as _, Md5};
use sha1::Sha1;
use std::collections::HashMap;

pub const REALM: &str = "fake-onvif-cam";

/// Operations a client may call before it has logged in (ONVIF Core spec, plus the ones real
/// cameras such as logent leave open for discovery and clock sync).
const OPEN_OPERATIONS: &[&str] = &[
    "GetServices",
    "GetServiceCapabilities",
    "GetCapabilities",
    "GetDeviceInformation",
    "GetSystemDateAndTime",
    "GetScopes",
    "GetWsdlUrl",
];

pub fn is_open_operation(operation: Option<&str>) -> bool {
    operation.is_some_and(|op| OPEN_OPERATIONS.contains(&op))
}

/// The `WWW-Authenticate` value that asks the client to log in with Digest.
pub fn digest_challenge() -> String {
    format!(
        r#"Digest realm="{REALM}", qop="auth", nonce="{}", algorithm=MD5"#,
        md5_hex(&util::make_uuid())
    )
}

/// Checks a SOAP request: a WS-Security UsernameToken in the body, or an HTTP Digest header.
pub fn check_soap(config: &Config, body: &str, authorization: Option<&str>, method: &str) -> bool {
    let Some((username, password)) = config.auth_credentials() else {
        return true;
    };

    check_username_token(body, username, password)
        || authorization.is_some_and(|header| check_digest(header, method, username, password))
}

/// Checks the `Authorization` header of an RTSP request.
pub fn check_rtsp(config: &Config, authorization: Option<&str>, method: &str) -> bool {
    let Some((username, password)) = config.auth_credentials() else {
        return true;
    };

    authorization.is_some_and(|header| check_digest(header, method, username, password))
}

fn check_digest(header: &str, method: &str, username: &str, password: &str) -> bool {
    let Some(rest) = header.trim().strip_prefix_ignore_case("digest") else {
        return false;
    };
    let params = parse_digest_params(rest);
    let get = |key: &str| params.get(key).map(String::as_str);

    let (Some(user), Some(nonce), Some(uri), Some(response)) =
        (get("username"), get("nonce"), get("uri"), get("response"))
    else {
        return false;
    };
    if user != username {
        return false;
    }

    let ha1 = md5_hex(&format!("{username}:{REALM}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let expected = match (get("qop"), get("nc"), get("cnonce")) {
        (Some(qop), Some(nc), Some(cnonce)) => {
            md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))
        }
        (None, _, _) => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
        _ => return false,
    };

    expected.eq_ignore_ascii_case(response)
}

/// Parses `key="value", key=value, ...` (values may contain commas inside quotes).
fn parse_digest_params(text: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    let mut chars = text.trim().chars().peekable();

    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && *c != ',') {
            key.push(c);
        }
        if chars.next_if_eq(&'=').is_none() {
            if chars.peek().is_none() {
                break;
            }
            continue;
        }

        let mut value = String::new();
        if chars.next_if_eq(&'"').is_some() {
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                value.push(c);
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != ',') {
                value.push(c);
            }
        }
        params.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    params
}

fn check_username_token(xml: &str, username: &str, password: &str) -> bool {
    let Some((_, user)) = element(xml, "Username") else {
        return false;
    };
    if user.trim() != username {
        return false;
    }
    let Some((attributes, sent)) = element(xml, "Password") else {
        return false;
    };
    let sent = sent.trim();

    if attributes.contains("PasswordDigest") {
        let (Some((_, nonce)), Some((_, created))) =
            (element(xml, "Nonce"), element(xml, "Created"))
        else {
            return false;
        };
        let Ok(nonce) = STANDARD.decode(nonce.trim()) else {
            return false;
        };

        let mut hasher = Sha1::new();
        hasher.update(&nonce);
        hasher.update(created.trim().as_bytes());
        hasher.update(password.as_bytes());
        STANDARD.encode(hasher.finalize()) == sent
    } else {
        sent == password
    }
}

/// Finds `<[prefix:]local ...>text<` and returns `(attributes, text)`.
fn element<'a>(xml: &'a str, local: &str) -> Option<(&'a str, &'a str)> {
    let mut from = 0;
    while let Some(offset) = xml[from..].find(local) {
        let start = from + offset;
        let end = start + local.len();
        from = end;

        let next = xml[end..].chars().next()?;
        if !matches!(next, ' ' | '>' | '/' | '\t' | '\r' | '\n') {
            continue;
        }

        let before = &xml[..start];
        let opened = match before.strip_suffix(':') {
            Some(prefixed) => prefixed
                .trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                .ends_with('<'),
            None => before.ends_with('<'),
        };
        if !opened {
            continue;
        }

        let close = end + xml[end..].find('>')?;
        let text_end = xml[close + 1..]
            .find('<')
            .map_or(xml.len(), |index| close + 1 + index);
        return Some((&xml[end..close], &xml[close + 1..text_end]));
    }

    None
}

fn md5_hex(value: &str) -> String {
    Md5::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

trait StripPrefixIgnoreCase {
    fn strip_prefix_ignore_case(&self, prefix: &str) -> Option<&str>;
}

impl StripPrefixIgnoreCase for str {
    fn strip_prefix_ignore_case(&self, prefix: &str) -> Option<&str> {
        let head = self.get(..prefix.len())?;
        head.eq_ignore_ascii_case(prefix)
            .then(|| &self[prefix.len()..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(auth: bool) -> Config {
        Config {
            username: Some("admin".to_string()),
            password: Some("admin123".to_string()),
            no_auth: !auth,
            ..Config::default()
        }
    }

    fn digest_header(user: &str, password: &str, method: &str, uri: &str, qop: bool) -> String {
        let nonce = "abc123";
        let ha1 = md5_hex(&format!("{user}:{REALM}:{password}"));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        if qop {
            let response = md5_hex(&format!("{ha1}:{nonce}:00000001:xyz:auth:{ha2}"));
            format!(
                r#"Digest username="{user}", realm="{REALM}", nonce="{nonce}", uri="{uri}", qop=auth, nc=00000001, cnonce="xyz", response="{response}""#
            )
        } else {
            let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
            format!(
                r#"Digest username="{user}", realm="{REALM}", nonce="{nonce}", uri="{uri}", response="{response}""#
            )
        }
    }

    fn token(digest: bool, user: &str, password: &str) -> String {
        let nonce = b"0123456789abcdef";
        let created = "2026-11-10T10:00:00Z";
        let (kind, sent) = if digest {
            let mut hasher = Sha1::new();
            hasher.update(nonce);
            hasher.update(created.as_bytes());
            hasher.update(password.as_bytes());
            ("PasswordDigest", STANDARD.encode(hasher.finalize()))
        } else {
            ("PasswordText", password.to_string())
        };
        format!(
            r#"<s:Header><wsse:Security xmlns:wsse="x" xmlns:wsu="y"><wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#{kind}">{sent}</wsse:Password><wsse:Nonce EncodingType="b">{}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header>"#,
            STANDARD.encode(nonce)
        )
    }

    #[test]
    fn md5_matches_known_vector() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn username_token_digest_and_text() {
        let config = config(true);
        for digest in [true, false] {
            assert!(check_soap(
                &config,
                &token(digest, "admin", "admin123"),
                None,
                "POST"
            ));
            assert!(!check_soap(
                &config,
                &token(digest, "admin", "wrong"),
                None,
                "POST"
            ));
            assert!(!check_soap(
                &config,
                &token(digest, "root", "admin123"),
                None,
                "POST"
            ));
        }
        assert!(!check_soap(&config, "<s:Body/>", None, "POST"));
    }

    #[test]
    fn http_digest_with_and_without_qop() {
        let config = config(true);
        for qop in [true, false] {
            let good = digest_header("admin", "admin123", "POST", "/onvif/media_service", qop);
            assert!(check_soap(&config, "", Some(&good), "POST"));
            assert!(check_rtsp(&config, Some(&good), "POST"));
            // the method is part of the hash
            assert!(!check_rtsp(&config, Some(&good), "DESCRIBE"));

            let bad = digest_header("admin", "wrong", "POST", "/onvif/media_service", qop);
            assert!(!check_soap(&config, "", Some(&bad), "POST"));
        }
        assert!(!check_soap(
            &config,
            "",
            Some("Basic YWRtaW46YWRtaW4xMjM="),
            "POST"
        ));
        assert!(!check_soap(&config, "", None, "POST"));
    }

    #[test]
    fn rtsp_digest_over_the_wire_uri() {
        let config = config(true);
        let header = digest_header("admin", "admin123", "DESCRIBE", "rtsp://h:8554/live", true);
        assert!(check_rtsp(&config, Some(&header), "DESCRIBE"));
    }

    #[test]
    fn disabled_when_credentials_missing_or_no_auth() {
        assert!(check_soap(&config(false), "", None, "POST"));
        assert!(check_rtsp(&config(false), None, "DESCRIBE"));
        assert!(check_rtsp(&Config::default(), None, "DESCRIBE"));
    }

    #[test]
    fn open_operations() {
        assert!(is_open_operation(Some("GetSystemDateAndTime")));
        assert!(is_open_operation(Some("GetServices")));
        assert!(!is_open_operation(Some("GetProfiles")));
        assert!(!is_open_operation(None));
    }

    #[test]
    fn element_handles_prefixes_and_ignores_lookalikes() {
        let xml = r#"<a:UsernameToken><a:Username>bob</a:Username></a:UsernameToken>"#;
        assert_eq!(element(xml, "Username"), Some(("", "bob")));
        assert_eq!(
            element("<Password Type=\"t\">pw</Password>", "Password"),
            Some((" Type=\"t\"", "pw"))
        );
        assert_eq!(element("<x:Usernames>a</x:Usernames>", "Username"), None);
        assert_eq!(element("</a:Username>", "Username"), None);
    }

    #[test]
    fn challenge_names_the_realm() {
        let challenge = digest_challenge();
        assert!(challenge.starts_with(&format!("Digest realm=\"{REALM}\"")));
        assert_ne!(challenge, digest_challenge());
    }
}
