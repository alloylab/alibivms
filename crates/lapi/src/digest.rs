//! The NVR's digest scheme, as implemented by its own web UI (`Utils.HttpRequestHeaders`).
//!
//! It is RFC 2617 digest with a session nonce issued once at login and reused for every
//! request, and `nc` pinned to `00000001`. The realm comes from the login challenge; every NVR
//! seen so far announces [`REALM`].

/// The realm Uniview NVRs announce, used when a challenge does not name one.
pub const REALM: &str = "NVRDVR";
const NC: &str = "00000001";
const QOP: &str = "auth";

fn hex_md5(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

/// `HA1 = md5(user:NVRDVR:password)`, for the default realm.
pub fn ha1(user: &str, password: &str) -> String {
    ha1_in_realm(user, REALM, password)
}

/// `HA1 = md5(user:realm:password)`.
pub fn ha1_in_realm(user: &str, realm: &str, password: &str) -> String {
    hex_md5(&format!("{user}:{realm}:{password}"))
}

/// Builds the `Authorization` header value for one request. `uri` is path plus query; `realm`
/// must be the one `ha1` was computed for.
pub fn authorization(user: &str, ha1: &str, realm: &str, nonce: &str, method: &str, uri: &str) -> String {
    let cnonce = rand::random::<u32>().to_string();
    authorization_with_cnonce(user, ha1, realm, nonce, method, uri, &cnonce)
}

fn authorization_with_cnonce(user: &str, ha1: &str, realm: &str, nonce: &str, method: &str, uri: &str, cnonce: &str) -> String {
    let ha2 = hex_md5(&format!("{method}:{uri}"));
    let response = hex_md5(&format!("{ha1}:{nonce}:{NC}:{cnonce}:{QOP}:{ha2}"));
    format!(
        r#"Digest username="{user}",realm="{realm}",qop="{QOP}", nonce="{nonce}",algorithm="MD5",cnonce="{cnonce}",nc="{NC}",uri="{uri}",response="{response}""#
    )
}

/// One `key=value` field of a `WWW-Authenticate` header, by name, unquoted. The web UI takes
/// the fourth comma-separated field for the nonce; looking the key up means field order does
/// not matter.
fn parse_field(www_authenticate: &str, key: &str) -> Option<String> {
    www_authenticate
        .trim_start()
        .strip_prefix("Digest")
        .unwrap_or(www_authenticate)
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

/// Pulls `nonce` out of a `WWW-Authenticate` header.
pub fn parse_nonce(www_authenticate: &str) -> Option<String> {
    parse_field(www_authenticate, "nonce")
}

/// Pulls `realm` out of a `WWW-Authenticate` header, if it names one.
pub fn parse_realm(www_authenticate: &str) -> Option<String> {
    parse_field(www_authenticate, "realm").filter(|r| !r.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_matches_web_ui_formula() {
        // Values computed with the NVR's own hex_md5 formula.
        let ha1 = ha1("admin", "secret");
        assert_eq!(ha1, hex_md5("admin:NVRDVR:secret"));
        let h = authorization_with_cnonce("admin", &ha1, REALM, "123456", "GET", "/LAPI/V1.0/System/DeviceInfo", "42");
        let ha2 = hex_md5("GET:/LAPI/V1.0/System/DeviceInfo");
        let expect = hex_md5(&format!("{ha1}:123456:00000001:42:auth:{ha2}"));
        assert!(h.contains(&format!(r#"response="{expect}""#)), "{h}");
        assert!(h.starts_with(r#"Digest username="admin",realm="NVRDVR""#));
    }

    #[test]
    fn realm_from_challenge_is_used() {
        let ha1 = ha1_in_realm("admin", "Other", "secret");
        assert_eq!(ha1, hex_md5("admin:Other:secret"));
        let h = authorization_with_cnonce("admin", &ha1, "Other", "1", "GET", "/x", "2");
        assert!(h.contains(r#"realm="Other""#), "{h}");
    }

    #[test]
    fn nonce_parsing() {
        let h = r#"Digest realm="NVRDVR",qop="auth",algorithm="MD5",nonce="1993823051",opaque="x""#;
        assert_eq!(parse_nonce(h).as_deref(), Some("1993823051"));
        assert_eq!(parse_nonce("Digest realm=NVRDVR").as_deref(), None);
    }

    #[test]
    fn realm_parsing() {
        let h = r#"Digest realm="NVRDVR",qop="auth",algorithm="MD5",nonce="1993823051",opaque="x""#;
        assert_eq!(parse_realm(h).as_deref(), Some("NVRDVR"));
        assert_eq!(parse_realm(r#"Digest nonce="1", realm=Custom"#).as_deref(), Some("Custom"));
        assert_eq!(parse_realm(r#"Digest nonce="1", realm="""#), None);
        assert_eq!(parse_realm(r#"Digest nonce="1""#), None);
    }
}
