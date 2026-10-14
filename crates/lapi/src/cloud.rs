//! Star4Live (Uniview EZCloud) account API: sign-in, the account's devices and channels, a tunnel
//! for LAPI calls, and what is needed to open a video relay to a device.
//!
//! Request shapes are in `docs/star4live-cloud.md`.

use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{Error, Result};

pub const DEFAULT_SERVER: &str = "https://global.star4live.com";
const OK: i64 = 200;

/// `md5(m + m[0..8])` with `m = md5(password)`: what the portal sends as the password. Kept
/// instead of the password itself.
pub fn password_hash(password: &str) -> String {
    let m = format!("{:x}", md5::compute(password.as_bytes()));
    format!("{:x}", md5::compute(format!("{m}{}", &m[..8]).as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub name: String,
    pub model: String,
    pub firmware: String,
    /// 1 is an NVR.
    pub kind: i64,
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudChannel {
    pub number: u32,
    pub name: String,
    pub online: bool,
}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Value,
}

struct Session {
    base: String,
    token: String,
}

/// One signed-in Star4Live account. Signs in on first use and again whenever the token is refused.
pub struct Account {
    http: reqwest::Client,
    login_name: String,
    password_hash: String,
    hard_id: String,
    session: Mutex<Option<Session>>,
}

impl Account {
    /// `password_hash` is [`password_hash`] of the password; `hard_id` is any id that stays the same
    /// for this installation.
    pub fn new(login_name: &str, password_hash: &str, hard_id: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).connect_timeout(Duration::from_secs(8)).build()?;
        Ok(Self {
            http,
            login_name: login_name.to_string(),
            password_hash: password_hash.to_string(),
            hard_id: hard_id.to_string(),
            session: Mutex::new(None),
        })
    }

    pub fn login_name(&self) -> &str {
        &self.login_name
    }

    async fn raw(&self, base: &str, path: &str, token: Option<&str>, body: &Value) -> Result<Envelope> {
        let url = format!("{base}{path}");
        let mut req = self.http.post(&url).json(body);
        if let Some(t) = token {
            req = req.header(reqwest::header::AUTHORIZATION, t);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|e| Error::Decode { url, detail: format!("HTTP {status}: {e}") })
    }

    /// Finds the account's server and signs in.
    pub async fn login(&self) -> Result<()> {
        let mut guard = self.session.lock().await;
        *guard = None;
        let found = self.raw(DEFAULT_SERVER, "/openapi/user/account/server/getbyloginname", None, &json!({ "loginName": self.login_name })).await?;
        let base = match found.data["serverAddress"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(a) if a.contains("://") => a.trim_end_matches('/').to_string(),
            Some(a) => format!("https://{}", a.trim_end_matches('/')),
            None if found.code == OK => DEFAULT_SERVER.to_string(),
            None => return Err(Error::Login(cloud_message(found.code, &found.message))),
        };
        let body = json!({
            "username": self.login_name,
            "password": self.password_hash,
            "clientId": "",
            "passwordVersion": 1,
            "clientName": "ucs-bs",
            "hardId": self.hard_id,
            "platform": "X11; Linux x86_64",
        });
        let r = self.raw(&base, "/openapi/user/account/token/get", None, &body).await?;
        if r.code != OK {
            return Err(Error::Login(cloud_message(r.code, &r.message)));
        }
        let Some(token) = r.data["accessToken"].as_str().filter(|t| !t.is_empty()) else {
            // Two-step verification and captcha challenges come back without a token.
            let keys: Vec<&str> = r.data.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
            return Err(Error::Login(format!("Star4Live asked for an extra sign-in step this app cannot do yet ({})", keys.join(", "))));
        };
        tracing::info!(account = %self.login_name, %base, "Star4Live sign-in ok");
        *guard = Some(Session { base, token: token.to_string() });
        Ok(())
    }

    async fn session(&self) -> Result<(String, String)> {
        if let Some(s) = self.session.lock().await.as_ref() {
            return Ok((s.base.clone(), s.token.clone()));
        }
        self.login().await?;
        let g = self.session.lock().await;
        let s = g.as_ref().ok_or_else(|| Error::Login("not signed in".into()))?;
        Ok((s.base.clone(), s.token.clone()))
    }

    /// One API call, signing in again once if the token has lapsed.
    async fn call(&self, path: &str, body: &Value) -> Result<Value> {
        let mut retried = false;
        loop {
            let (base, token) = self.session().await?;
            let r = self.raw(&base, path, Some(&token), body).await?;
            if r.code == OK {
                return Ok(r.data);
            }
            if !retried && token_refused(r.code, &r.message) {
                retried = true;
                tracing::warn!(account = %self.login_name, code = r.code, "Star4Live token refused, signing in again");
                self.login().await?;
                continue;
            }
            return Err(Error::Status { url: path.into(), code: r.code, text: r.message });
        }
    }

    /// Any API call, returning its `data`. For diagnostics.
    pub async fn call_raw(&self, path: &str, body: &Value) -> Result<Value> {
        self.call(path, body).await
    }

    async fn call_as<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let v = self.call(path, body).await?;
        serde_json::from_value(v).map_err(|e| Error::Decode { url: path.into(), detail: e.to_string() })
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct D {
            device_serial: String,
            #[serde(default)]
            device_name: String,
            #[serde(default)]
            device_model: String,
            #[serde(default)]
            device_version: String,
            #[serde(default)]
            device_type: i64,
            #[serde(default)]
            status: i64,
        }
        #[derive(Deserialize)]
        struct Page {
            #[serde(rename = "deviceList", default)]
            list: Vec<D>,
        }
        const PAGE: usize = 50;
        let mut out = Vec::new();
        for page in 1.. {
            let p: Page = self.call_as("/openapi/device/list", &json!({ "pageNo": page, "pageSize": PAGE })).await?;
            let n = p.list.len();
            out.extend(p.list.into_iter().map(|d| Device {
                name: if d.device_name.is_empty() { d.device_serial.clone() } else { d.device_name },
                serial: d.device_serial,
                model: d.device_model,
                firmware: d.device_version,
                kind: d.device_type,
                online: d.status == 1,
            }));
            if n < PAGE || page >= 40 {
                break;
            }
        }
        Ok(out)
    }

    pub async fn channels(&self, serial: &str) -> Result<Vec<CloudChannel>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct C {
            channel_no: u32,
            #[serde(default)]
            channel_name: String,
            #[serde(default)]
            status: i64,
            #[serde(default)]
            channel_type: Value,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            #[serde(default)]
            total: usize,
            #[serde(default)]
            channel_list: Vec<C>,
        }
        const PAGE: usize = 100;
        let mut out: Vec<CloudChannel> = Vec::new();
        let mut start = 0;
        loop {
            let body = json!({ "deviceSerial": serial, "queryType": 0, "pageStart": start, "pageSize": PAGE });
            let p: Page = self.call_as("/openapi/inner/device/channel/list", &body).await?;
            let n = p.channel_list.len();
            for c in &p.channel_list {
                tracing::debug!(serial, channel = c.channel_no, channel_type = %c.channel_type, "Star4Live channel");
            }
            out.extend(p.channel_list.into_iter().map(|c| CloudChannel {
                name: if c.channel_name.is_empty() { format!("Channel {}", c.channel_no) } else { c.channel_name },
                number: c.channel_no,
                online: c.status == 1,
            }));
            start += n;
            if n == 0 || start >= p.total {
                break;
            }
        }
        out.sort_by_key(|c| c.number);
        out.dedup_by_key(|c| c.number);
        Ok(out)
    }

    /// Sends one LAPI request to the device and returns its `Response` object.
    pub async fn lapi(&self, serial: &str, method: &str, uri: &str, body: Option<&Value>) -> Result<Value> {
        let mut request = json!({ "RequestURL": uri, "Method": method });
        if let Some(b) = body {
            // Not yet verified: the portal's own calls seen so far were all GETs.
            request["Data"] = b.clone();
        }
        let body = json!({ "deviceSerial": serial, "request": request });
        let d = match self.call("/openapi/device/lapi/invoke", &body).await {
            Ok(d) => d,
            // The portal falls back to the P2P tunnel with the same body.
            Err(Error::Status { .. }) => self.call("/openapi/prm/device/p2p/invoke", &body).await?,
            Err(e) => return Err(e),
        };
        let resp = &d["response"];
        // Some firmware returns the LAPI response as a JSON string.
        let resp = match resp {
            Value::String(s) => serde_json::from_str(s).map_err(|e| Error::Decode { url: uri.into(), detail: e.to_string() })?,
            v => v.clone(),
        };
        tracing::debug!(serial, uri, response = %resp, "Star4Live LAPI response");
        Ok(match resp.get("Response").or(resp.get("response")) {
            Some(r) => r.clone(),
            None => resp,
        })
    }

    /// A fresh `https://<relay>/<session>?<query>` URL for one video connection.
    pub async fn relay_url(&self, serial: &str) -> Result<String> {
        let d = self.call("/openapi/device/p2p/url/get", &json!({ "deviceSerial": serial, "connectionProtocol": 2 })).await?;
        d["url"].as_str().map(str::to_string).ok_or_else(|| Error::Decode { url: "p2p/url/get".into(), detail: "no url".into() })
    }

    /// The device's current one-time password for user `admin`. It rotates every few minutes.
    pub async fn dynamic_password(&self, serial: &str) -> Result<String> {
        let d = self.call("/openapi/device/network/nat/list", &json!({ "deviceSerialList": [serial] })).await?;
        d["deviceList"][0]["dynamicPasswordNew"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .ok_or_else(|| Error::Decode { url: "network/nat/list".into(), detail: "no dynamic password".into() })
    }
}

fn token_refused(code: i64, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    matches!(code, 401 | 403) || m.contains("token") || m.contains("login") || m.contains("expired")
}

fn cloud_message(code: i64, message: &str) -> String {
    match code {
        2004 => "wrong username or password".into(),
        _ if message.is_empty() => format!("Star4Live error {code}"),
        _ => format!("{message} (code {code})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_matches_portal() {
        let m = format!("{:x}", md5::compute("secret"));
        assert_eq!(m, "5ebe2294ecd0e0f08eab7690d2a6ee69");
        assert_eq!(password_hash("secret"), format!("{:x}", md5::compute(format!("{m}5ebe2294"))));
    }
}
