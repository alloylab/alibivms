//! The NVR's digest scheme, as implemented by its own web UI (`Utils.HttpRequestHeaders`).
//!
//! It is RFC 2617 digest with a fixed realm, a session nonce issued once at login and reused
//! for every request, and `nc` pinned to `00000001`.

pub const REALM: &str = "NVRDVR";
const NC: &str = "00000001";
const QOP: &str = "auth";

fn hex_md5(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

/// `HA1 = md5(user:NVRDVR:password)`. Computed once so the plain password is not kept around.
pub fn ha1(user: &str, password: &str) -> String {
    hex_md5(&format!("{user}:{REALM}:{password}"))
}

/// Builds the `Authorization` header value for one request. `uri` is path plus query.
pub fn authorization(user: &str, ha1: &str, nonce: &str, method: &str, uri: &str) -> String {
    let cnonce = rand::random::<u32>().to_string();
    authorization_with_cnonce(user, ha1, nonce, method, uri, &cnonce)
}

fn authorization_with_cnonce(
    user: &str,
    ha1: &str,
    nonce: &str,
    method: &str,
    uri: &str,
    cnonce: &str,
) -> String {
    let ha2 = hex_md5(&format!("{method}:{uri}"));
    let response = hex_md5(&format!("{ha1}:{nonce}:{NC}:{cnonce}:{QOP}:{ha2}"));
    format!(
        r#"Digest username="{user}",realm="{REALM}",qop="{QOP}", nonce="{nonce}",algorithm="MD5",cnonce="{cnonce}",nc="{NC}",uri="{uri}",response="{response}""#
    )
}

/// Pulls `nonce` out of a `WWW-Authenticate` header. The web UI takes the fourth comma-separated
/// field; this looks the key up by name instead so field order does not matter.
pub fn parse_nonce(www_authenticate: &str) -> Option<String> {
    www_authenticate
        .trim_start_matches("Digest")
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("nonce"))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_matches_web_ui_formula() {
        // Values computed with the NVR's own hex_md5 formula.
        let ha1 = ha1("admin", "secret");
        assert_eq!(ha1, hex_md5("admin:NVRDVR:secret"));
        let h = authorization_with_cnonce("admin", &ha1, "123456", "GET", "/LAPI/V1.0/System/DeviceInfo", "42");
        let ha2 = hex_md5("GET:/LAPI/V1.0/System/DeviceInfo");
        let expect = hex_md5(&format!("{ha1}:123456:00000001:42:auth:{ha2}"));
        assert!(h.contains(&format!(r#"response="{expect}""#)), "{h}");
        assert!(h.starts_with(r#"Digest username="admin",realm="NVRDVR""#));
    }

    #[test]
    fn nonce_parsing() {
        let h = r#"Digest realm="NVRDVR",qop="auth",algorithm="MD5",nonce="1993823051",opaque="x""#;
        assert_eq!(parse_nonce(h).as_deref(), Some("1993823051"));
        assert_eq!(parse_nonce("Digest realm=NVRDVR").as_deref(), None);
    }
}
