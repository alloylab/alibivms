//! Client for the LAPI HTTP API on Uniview-OEM NVRs (Alibi ALI-NR323P-8 and relatives).
//!
//! Only the calls verified against firmware `NVR-B3115.43.30.C22912.260706` are exposed; see
//! `docs/lapi-findings.md` for the captured request shapes.

pub mod cloud;
pub mod digest;

use std::{sync::Arc, time::Duration};

use reqwest::{Method, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("login failed: {0}")]
    Login(String),
    #[error("LAPI {url} returned status {code} ({text})")]
    Status { url: String, code: i64, text: String },
    #[error("unexpected response from {url}: {detail}")]
    Decode { url: String, detail: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// LAPI status codes that mean the session nonce is no longer accepted. A stale nonce comes
/// back as HTTP 599 with StatusCode 60031 ("Not Authorized").
const AUTH_FAILED: [i64; 3] = [401, 60009, 60031];
const HTTP_NOT_AUTHORIZED: u16 = 599;
const LOGIN_PATH: &str = "/LAPI/V1.0/System/Security/Login";
const CGI_PATH: &str = "/cgi-bin/main-cgi";

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "Response")]
    response: ResponseBody<T>,
}

#[derive(Deserialize)]
struct ResponseBody<T> {
    #[serde(rename = "StatusCode")]
    status_code: i64,
    #[serde(rename = "ResponseString", default)]
    response_string: String,
    #[serde(rename = "Data")]
    data: Option<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceInfo {
    pub device_name: String,
    pub device_model: String,
    pub serial_number: String,
    pub firmware_version: String,
    #[serde(default)]
    pub manufacturer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ChannelDeviceInfo {
    #[serde(rename = "ID")]
    pub id: u32,
    pub device_name: String,
    pub device_model: String,
    pub firmware_version: String,
    pub serial_number: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: u32,
    pub name: String,
    pub online: bool,
    pub stream_count: u8,
    pub camera_type: CameraType,
    pub ip: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraType {
    Fixed,
    Ptz,
    Fisheye,
    /// Not reported (cloud NVRs).
    Unknown,
}

impl From<u8> for CameraType {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Ptz,
            2 | 3 => Self::Fisheye,
            _ => Self::Fixed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordSegment {
    /// UTC epoch seconds.
    pub begin: i64,
    pub end: i64,
    pub record_type: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceTime {
    pub time_zone: String,
    pub device_time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub id: u32,
    pub name: String,
}

/// Stream index on the NVR: 0 is the main stream, 1 the sub stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Main,
    #[default]
    Sub,
}

impl Stream {
    pub fn index(self) -> u8 {
        match self {
            Stream::Main => 0,
            Stream::Sub => 1,
        }
    }
}

/// `PTZCmd` codes from the web UI's `Static.PtzCmd`. Each motion has a matching stop code
/// one below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtzAction {
    Up,
    Down,
    Left,
    Right,
    LeftUp,
    LeftDown,
    RightUp,
    RightDown,
    ZoomIn,
    ZoomOut,
    FocusNear,
    FocusFar,
    IrisOpen,
    IrisClose,
}

impl PtzAction {
    fn start_code(self) -> u32 {
        match self {
            PtzAction::Up => 1026,
            PtzAction::Down => 1028,
            PtzAction::Left => 1284,
            PtzAction::Right => 1282,
            PtzAction::LeftUp => 1794,
            PtzAction::LeftDown => 1796,
            PtzAction::RightUp => 2050,
            PtzAction::RightDown => 2052,
            PtzAction::ZoomIn => 770,
            PtzAction::ZoomOut => 772,
            PtzAction::FocusNear => 514,
            PtzAction::FocusFar => 516,
            PtzAction::IrisOpen => 260,
            PtzAction::IrisClose => 258,
        }
    }
}

const PTZ_ALL_STOP: u32 = 2305;

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

struct Session {
    nonce: String,
}

/// One authenticated connection to one NVR, direct or through Star4Live. Cheap to share behind
/// an `Arc`.
pub struct Client {
    http: reqwest::Client,
    base: url::Url,
    username: String,
    ha1: String,
    session: Mutex<Option<Session>>,
    /// Requests hold it shared; KeepAlive, which replaces the nonce, holds it alone so no request
    /// is in flight with the nonce it retires.
    gate: tokio::sync::RwLock<()>,
    cloud: Option<(Arc<cloud::Account>, String)>,
}

impl Client {
    pub fn new(base: &str, creds: &Credentials) -> Result<Self> {
        let base = url::Url::parse(base).map_err(|e| Error::Login(format!("bad base url {base}: {e}")))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http,
            base,
            username: creds.username.clone(),
            ha1: digest::ha1(&creds.username, &creds.password),
            session: Mutex::new(None),
            gate: tokio::sync::RwLock::new(()),
            cloud: None,
        })
    }

    /// An NVR reached through a Star4Live account, by serial number.
    pub fn cloud(account: Arc<cloud::Account>, serial: &str) -> Result<Self> {
        let mut c = Self::new("http://cloud.invalid", &Credentials { username: String::new(), password: String::new() })?;
        c.cloud = Some((account, serial.to_string()));
        Ok(c)
    }

    /// The Star4Live account and device serial, for a cloud NVR.
    pub fn cloud_link(&self) -> Option<(&Arc<cloud::Account>, &str)> {
        self.cloud.as_ref().map(|(a, s)| (a, s.as_str()))
    }

    pub fn host(&self) -> &str {
        match &self.cloud {
            Some((_, serial)) => serial,
            None => self.base.host_str().unwrap_or_default(),
        }
    }

    fn url(&self, path_and_query: &str) -> String {
        format!("{}{}", self.base.as_str().trim_end_matches('/'), path_and_query)
    }

    fn auth_header(&self, nonce: &str, method: &Method, uri: &str) -> String {
        digest::authorization(&self.username, &self.ha1, nonce, method.as_str(), uri)
    }

    /// Runs the two-step login and stores the session nonce.
    pub async fn login(&self) -> Result<()> {
        if let Some((account, _)) = &self.cloud {
            return account.login().await;
        }
        let mut guard = self.session.lock().await;
        *guard = None;
        let challenge = self
            .http
            .put(self.url(LOGIN_PATH))
            .header(header::COOKIE, "WebLoginHandle=10081124")
            .send()
            .await?;
        let www = challenge
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Login("no WWW-Authenticate on login challenge".into()))?;
        let nonce = digest::parse_nonce(www).ok_or_else(|| Error::Login(format!("no nonce in {www:?}")))?;

        let resp = self
            .http
            .put(self.url(LOGIN_PATH))
            .header(header::COOKIE, "WebLoginHandle=10081124")
            .header(header::AUTHORIZATION, self.auth_header(&nonce, &Method::PUT, LOGIN_PATH))
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        let parsed: Envelope<serde_json::Value> = serde_json::from_str(&body)
            .map_err(|e| Error::Login(format!("HTTP {status}, undecodable body: {e}")))?;
        if parsed.response.status_code != 0 {
            let data = parsed.response.data.unwrap_or_default();
            return Err(Error::Login(format!(
                "status {} ({}), remaining attempts {}, unlock in {}s",
                parsed.response.status_code,
                parsed.response.response_string,
                data["RemainLockTimes"],
                data["RemainUnlockTime"],
            )));
        }
        tracing::info!(host = self.host(), "LAPI login ok");
        *guard = Some(Session { nonce });
        Ok(())
    }

    async fn nonce(&self) -> Result<String> {
        if let Some(s) = self.session.lock().await.as_ref() {
            return Ok(s.nonce.clone());
        }
        self.login().await?;
        Ok(self.session.lock().await.as_ref().map(|s| s.nonce.clone()).unwrap_or_default())
    }

    /// Sends one request with the session nonce; returns the nonce used alongside the answer.
    async fn send_raw(&self, method: &Method, uri: &str, body: Option<String>, form: bool) -> Result<(StatusCode, String, String)> {
        let _shared = self.gate.read().await;
        let nonce = self.nonce().await?;
        let mut req = self
            .http
            .request(method.clone(), self.url(uri))
            .header(header::COOKIE, "WebLoginHandle=10081124")
            .header(header::AUTHORIZATION, self.auth_header(&nonce, method, uri));
        if let Some(b) = body {
            let ct = if form { "application/x-www-form-urlencoded; charset=UTF-8" } else { "application/json" };
            req = req.header(header::CONTENT_TYPE, ct).body(b);
        }
        let resp = req.send().await?;
        Ok((resp.status(), resp.text().await?, nonce))
    }

    /// Logs in again after `used` was refused, unless another request already did.
    async fn relogin(&self, used: &str, uri: &str) -> Result<()> {
        let current = self.session.lock().await.as_ref().map(|s| s.nonce.clone());
        if current.as_deref() != Some(used) {
            return Ok(());
        }
        tracing::warn!(
            host = self.host(),
            uri,
            "LAPI session rejected, logging in again (if this repeats, something else is signed in to this NVR as the same user: its web page, or the NVR added twice)"
        );
        self.login().await
    }

    /// Sends one LAPI request, logging in again once if the session has lapsed.
    async fn call<T: DeserializeOwned>(&self, method: Method, uri: &str, body: Option<&serde_json::Value>) -> Result<T> {
        if let Some((account, serial)) = &self.cloud {
            let v = account.lapi(serial, method.as_str(), uri, body).await?;
            let data = cloud_lapi_data(&v).map_err(|(code, text)| Error::Status { url: uri.into(), code, text })?;
            return serde_json::from_value(data.clone()).map_err(|e| {
                tracing::warn!(uri, response = %truncate(&v.to_string(), 600), "unexpected LAPI response through Star4Live");
                Error::Decode { url: uri.into(), detail: e.to_string() }
            });
        }
        let body = body.map(|b| format!("{b}\r\n"));
        let mut retried = false;
        loop {
            let (status, text, used) = self.send_raw(&method, uri, body.clone(), false).await?;
            // Check the status before decoding Data: error responses carry an empty Data object.
            let env: std::result::Result<Envelope<serde_json::Value>, _> = serde_json::from_str(&text);
            let auth_failed = status == StatusCode::UNAUTHORIZED
                || status.as_u16() == HTTP_NOT_AUTHORIZED
                || env.as_ref().is_ok_and(|e| AUTH_FAILED.contains(&e.response.status_code));
            if auth_failed && !retried {
                retried = true;
                self.relogin(&used, uri).await?;
                continue;
            }
            let env = env.map_err(|e| Error::Decode { url: uri.into(), detail: format!("HTTP {status}: {e}") })?;
            if env.response.status_code != 0 {
                return Err(Error::Status {
                    url: uri.into(),
                    code: env.response.status_code,
                    text: env.response.response_string,
                });
            }
            let data = env.response.data.unwrap_or_default();
            return serde_json::from_value(data).map_err(|e| Error::Decode { url: uri.into(), detail: e.to_string() });
        }
    }

    /// Keeps the session open. The NVR answers with `NextNonce`, which replaces the session nonce:
    /// the old one stops working immediately.
    pub async fn keep_alive(&self) -> Result<()> {
        if self.cloud.is_some() {
            return Ok(());
        }
        const URI: &str = "/LAPI/V1.0/System/Security/KeepAlive";
        let _alone = self.gate.write().await;
        let Some(nonce) = self.session.lock().await.as_ref().map(|s| s.nonce.clone()) else {
            drop(_alone);
            return self.login().await;
        };
        let resp = self
            .http
            .put(self.url(URI))
            .header(header::COOKIE, "WebLoginHandle=10081124")
            .header(header::AUTHORIZATION, self.auth_header(&nonce, &Method::PUT, URI))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let env: Option<Envelope<serde_json::Value>> = serde_json::from_str(&text).ok();
        let ok = status.is_success() && env.as_ref().is_some_and(|e| e.response.status_code == 0);
        if !ok {
            drop(_alone);
            tracing::warn!(host = self.host(), %status, "LAPI keep-alive refused, logging in again (another sign-in to this NVR as the same user ends this one)");
            return self.login().await;
        }
        let d = env.and_then(|e| e.response.data).unwrap_or_default();
        let next = match &d["NextNonce"] {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        if let Some(n) = next.filter(|n| !n.is_empty()) {
            if let Some(s) = self.session.lock().await.as_mut().filter(|s| s.nonce == nonce) {
                s.nonce = n;
            }
        }
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        if self.cloud.is_some() {
            return Ok(());
        }
        let r = self.call::<serde_json::Value>(Method::PUT, "/LAPI/V1.0/System/Security/Logout", None).await;
        *self.session.lock().await = None;
        r.map(|_| ())
    }

    /// Any LAPI request, returning its `Data` as JSON. For diagnostics.
    pub async fn raw(&self, method: &str, uri: &str) -> Result<serde_json::Value> {
        let m = Method::from_bytes(method.as_bytes()).map_err(|e| Error::Login(e.to_string()))?;
        self.call(m, uri, None).await
    }

    pub async fn device_info(&self) -> Result<DeviceInfo> {
        self.call(Method::GET, "/LAPI/V1.0/System/DeviceInfo", None).await
    }

    pub async fn device_time(&self) -> Result<DeviceTime> {
        self.call(Method::GET, "/LAPI/V1.0/System/Time", None).await
    }

    pub async fn channel_device_infos(&self) -> Result<Vec<ChannelDeviceInfo>> {
        #[derive(Deserialize)]
        struct D {
            #[serde(rename = "DeviceInfos", default)]
            infos: Vec<ChannelDeviceInfo>,
        }
        Ok(self.call::<D>(Method::GET, "/LAPI/V1.0/Channels/System/DeviceInfos", None).await?.infos)
    }

    /// Channel list with online state, via the legacy CGI (cmd 26) the web UI uses.
    pub async fn channels(&self) -> Result<Vec<Channel>> {
        if let Some((account, serial)) = &self.cloud {
            return Ok(account
                .channels(serial)
                .await?
                .into_iter()
                .map(|c| Channel { id: c.number, name: c.name, online: c.online, stream_count: 2, camera_type: CameraType::Unknown, ip: String::new() })
                .collect());
        }
        #[derive(Deserialize)]
        struct Ch {
            #[serde(rename = "stResourceCode")]
            code: String,
            #[serde(rename = "szResourceName", default)]
            name: String,
            #[serde(rename = "u8bIsOnline", default)]
            online: u8,
            #[serde(rename = "u8StreamNum", default)]
            streams: u8,
            #[serde(rename = "u8CameraType", default)]
            camera_type: u8,
            #[serde(rename = "stIPAddress", default)]
            ip: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(rename = "DetailChList", default)]
            list: Vec<Ch>,
        }

        let mut retried = false;
        loop {
            let nonce = self.nonce().await?;
            let user: String = url::form_urlencoded::byte_serialize(self.username.as_bytes()).collect();
            let json = serde_json::json!({ "cmd": 26, "szUserName": user, "u32UserLoginHandle": nonce.parse::<u64>().unwrap_or(0) });
            let form: String = url::form_urlencoded::Serializer::new(String::new()).append_pair("json", &json.to_string()).finish();
            let (status, text, used) = self.send_raw(&Method::POST, CGI_PATH, Some(form), true).await?;
            if status == StatusCode::UNAUTHORIZED && !retried {
                retried = true;
                self.relogin(&used, CGI_PATH).await?;
                continue;
            }
            let resp: Resp = serde_json::from_str(&text)
                .map_err(|e| Error::Decode { url: CGI_PATH.into(), detail: format!("HTTP {status}: {e}") })?;
            let mut out: Vec<Channel> = resp
                .list
                .into_iter()
                .map(|c| {
                    let id = c.code.get(c.code.len().saturating_sub(4)..).and_then(|h| u32::from_str_radix(h, 16).ok()).unwrap_or(0);
                    Channel {
                        id,
                        name: if c.name.is_empty() { format!("Channel {id}") } else { c.name },
                        online: c.online == 1,
                        stream_count: c.streams,
                        camera_type: c.camera_type.into(),
                        ip: c.ip,
                    }
                })
                .filter(|c| c.id > 0)
                .collect();
            out.sort_by_key(|c| c.id);
            return Ok(out);
        }
    }

    pub async fn live_stream_url(&self, channel: u32, stream: Stream) -> Result<String> {
        #[derive(Deserialize)]
        struct D {
            #[serde(rename = "URL")]
            url: String,
        }
        let uri = format!(
            "/LAPI/V1.0/Channels/{channel}/Media/Video/Streams/{}/LiveStreamURL?TransProtocol=tcp",
            stream.index()
        );
        let url = self.call::<D>(Method::GET, &uri, None).await?.url;
        let fixed = with_stream(&url, stream);
        tracing::info!(host = self.host(), channel, ?stream, nvr_url = %url, url = %fixed, "live stream");
        Ok(fixed)
    }

    /// RTSP URL that replays `begin..end` (UTC seconds). The NVR always returns the main
    /// stream (`/s0/`); the sub stream is selected by rewriting it to `/s1/`, as the web UI does.
    pub async fn record_url(&self, channel: u32, begin: i64, end: i64, stream: Stream) -> Result<String> {
        #[derive(Deserialize)]
        struct D {
            #[serde(rename = "URL")]
            url: String,
        }
        let uri = format!("/LAPI/V1.0/Channels/{channel}/Media/Video/Streams/RecordURL?Begin={begin}&End={end}");
        let url = self.call::<D>(Method::GET, &uri, None).await?.url;
        Ok(match stream {
            Stream::Main => url,
            Stream::Sub => url.replace("/replay/s0", "/replay/s1"),
        })
    }

    pub async fn records(&self, channel: u32, begin: i64, end: i64) -> Result<Vec<RecordSegment>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct R {
            begin: i64,
            end: i64,
            #[serde(default)]
            record_type: u32,
        }
        #[derive(Deserialize)]
        struct D {
            #[serde(rename = "RecordInfos", default)]
            infos: Vec<R>,
        }
        let uri = format!("/LAPI/V1.0/Channels/{channel}/Media/Video/Streams/0/Records?Begin={begin}&End={end}");
        let d = self.call::<D>(Method::GET, &uri, None).await?;
        Ok(d.infos.into_iter().map(|r| RecordSegment { begin: r.begin, end: r.end, record_type: r.record_type }).collect())
    }

    /// Which days of the month (device-local) hold video. Index 0 is day 1.
    pub async fn daily_distribution(&self, channel: u32, year: i32, month: u32) -> Result<Vec<bool>> {
        #[derive(Deserialize)]
        struct D {
            #[serde(rename = "DailyStatus", default)]
            status: Vec<u8>,
        }
        let uri = format!("/LAPI/V1.0/Channels/{channel}/Media/Video/Streams/0/Records/DailyDistribution?Year={year}&Month={month}");
        Ok(self.call::<D>(Method::GET, &uri, None).await?.status.into_iter().map(|s| s != 0).collect())
    }

    /// Earliest and latest recorded instants across `channels` (UTC seconds; 0 if none).
    pub async fn record_bounds(&self, channels: &[u32]) -> Result<(i64, i64)> {
        let body = serde_json::json!({ "Num": channels.len(), "ChannelList": channels });
        let e: serde_json::Value = self.call(Method::POST, "/LAPI/V1.0/Channels/Media/Record/Earliest", Some(&body)).await?;
        let l: serde_json::Value = self.call(Method::POST, "/LAPI/V1.0/Channels/Media/Record/Latest", Some(&body)).await?;
        Ok((e["Earliest"].as_i64().unwrap_or(0), l["Latest"].as_i64().unwrap_or(0)))
    }

    pub async fn ptz_start(&self, channel: u32, action: PtzAction, speed: u8) -> Result<()> {
        let speed = speed.clamp(1, 9);
        let body = serde_json::json!({ "PTZCmd": action.start_code(), "Para1": speed, "Para2": speed });
        self.call::<serde_json::Value>(Method::PUT, &format!("/LAPI/V1.0/Channels/{channel}/PTZ/PTZCtrl"), Some(&body))
            .await
            .map(|_| ())
    }

    pub async fn ptz_stop(&self, channel: u32, action: Option<PtzAction>) -> Result<()> {
        let code = action.map(|a| a.start_code() - 1).unwrap_or(PTZ_ALL_STOP);
        let body = serde_json::json!({ "PTZCmd": code, "Para1": 0, "Para2": 0 });
        self.call::<serde_json::Value>(Method::PUT, &format!("/LAPI/V1.0/Channels/{channel}/PTZ/PTZCtrl"), Some(&body))
            .await
            .map(|_| ())
    }

    pub async fn ptz_presets(&self, channel: u32) -> Result<Vec<Preset>> {
        let v: serde_json::Value = self.call(Method::GET, &format!("/LAPI/V1.0/Channels/{channel}/PTZ/Presets"), None).await?;
        let list = v
            .as_object()
            .and_then(|o| o.values().find(|x| x.is_array()))
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|p| {
                Some(Preset {
                    id: p["ID"].as_u64()? as u32,
                    name: p["Name"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    pub async fn ptz_goto_preset(&self, channel: u32, preset_id: u32) -> Result<()> {
        self.call::<serde_json::Value>(Method::PUT, &format!("/LAPI/V1.0/Channels/{channel}/PTZ/Presets/{preset_id}/Goto"), None)
            .await
            .map(|_| ())
    }
}

/// The `Data` of a LAPI response relayed by Star4Live, or its error code and text. The relay's
/// copy may carry only `ResponseCode`, use other key casing, or be the bare data.
fn cloud_lapi_data(v: &serde_json::Value) -> std::result::Result<&serde_json::Value, (i64, String)> {
    let field = |names: &[&str]| names.iter().find_map(|n| v.get(*n));
    let code = field(&["StatusCode", "statusCode", "ResponseCode", "responseCode"]).and_then(|c| c.as_i64().or_else(|| c.as_str()?.parse().ok()));
    if let Some(c) = code.filter(|c| *c != 0) {
        let text = field(&["ResponseString", "responseString", "StatusString"]).and_then(|t| t.as_str()).unwrap_or_default();
        return Err((c, text.to_string()));
    }
    Ok(field(&["Data", "data"]).unwrap_or(v))
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Makes the `/s0/` (main) or `/s1/` (sub) path segment of an NVR RTSP URL match `stream`; some
/// NVRs hand out the main stream whichever one is asked for.
pub fn with_stream(url: &str, stream: Stream) -> String {
    let want = format!("/s{}/", stream.index());
    let other = format!("/s{}/", 1 - stream.index());
    let path_start = url.find("://").map(|i| i + 3).and_then(|i| url[i..].find('/').map(|j| i + j)).unwrap_or(0);
    let (head, path) = url.split_at(path_start);
    // Paths end in `/live` or `/` with no trailing slash after the stream on some firmware.
    let path = format!("{path}/");
    let fixed = path.replacen(&other, &want, 1);
    format!("{head}{}", &fixed[..fixed.len() - 1])
}

/// Inserts credentials into an RTSP URL returned by the NVR.
pub fn rtsp_with_credentials(rtsp: &str, creds: &Credentials) -> String {
    match url::Url::parse(rtsp) {
        Ok(mut u) => {
            let _ = u.set_username(&creds.username);
            let _ = u.set_password(Some(&creds.password));
            u.to_string()
        }
        Err(_) => rtsp.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_decodes_captured_response() {
        let body = r#"{"Response":{"ResponseURL":"/LAPI/V1.0/Channels/1/Media/Video/Streams/0/Records","ResponseCode":0,"ResponseString":"Succeed","StatusCode":0,"Data":{"RecordInfos":[{"FileName":"","RecordType":132,"RecordTypes2":0,"Begin":1791924059,"End":1791924221}]}}}"#;
        let e: Envelope<serde_json::Value> = serde_json::from_str(body).unwrap();
        assert_eq!(e.response.status_code, 0);
        assert_eq!(e.response.data.unwrap()["RecordInfos"][0]["Begin"], 1791924059);
    }

    #[test]
    fn stale_nonce_response_is_an_auth_failure() {
        // Captured from 10.9.0.2 with an expired nonce (HTTP 599).
        let body = r#"{ "Response": { "ResponseURL": "/LAPI/V1.0/System/DeviceInfo", "ResponseCode": 3, "ResponseString": "Not Authorized", "StatusCode": 60031, "Data": { } } }"#;
        let e: Envelope<serde_json::Value> = serde_json::from_str(body).unwrap();
        assert!(AUTH_FAILED.contains(&e.response.status_code));
    }

    #[test]
    fn cloud_lapi_shapes() {
        let v = serde_json::json!({"ResponseCode": 0, "Data": {"A": 1}});
        assert_eq!(cloud_lapi_data(&v).unwrap()["A"], 1);
        let v = serde_json::json!({"ResponseCode": 0, "StatusCode": 60006, "ResponseString": "x"});
        assert_eq!(cloud_lapi_data(&v).unwrap_err().0, 60006);
        let v = serde_json::json!({"DeviceModel": "m"});
        assert_eq!(cloud_lapi_data(&v).unwrap()["DeviceModel"], "m");
    }

    #[test]
    fn stream_segment_is_forced() {
        assert_eq!(with_stream("rtsp://10.9.0.2:554/unicast/c1/s0/live", Stream::Sub), "rtsp://10.9.0.2:554/unicast/c1/s1/live");
        assert_eq!(with_stream("rtsp://10.9.0.2:554/unicast/c1/s1/live", Stream::Main), "rtsp://10.9.0.2:554/unicast/c1/s0/live");
        assert_eq!(with_stream("rtsp://10.9.0.2:554/unicast/c1/s1/live", Stream::Sub), "rtsp://10.9.0.2:554/unicast/c1/s1/live");
        assert_eq!(with_stream("rtsp://h/c1/b1/e2/replay/s0", Stream::Sub), "rtsp://h/c1/b1/e2/replay/s1");
    }

    #[test]
    fn rtsp_credentials_are_escaped() {
        let c = Credentials { username: "admin".into(), password: "p@ss:w/rd".into() };
        let u = rtsp_with_credentials("rtsp://10.9.0.2:554/c1/b1/e2/replay/s0/", &c);
        assert_eq!(u, "rtsp://admin:p%40ss%3Aw%2Frd@10.9.0.2:554/c1/b1/e2/replay/s0/");
    }

    #[test]
    fn ptz_stop_codes_are_one_below_start() {
        assert_eq!(PtzAction::Up.start_code() - 1, 1025);
        assert_eq!(PtzAction::ZoomIn.start_code() - 1, 769);
    }
}
