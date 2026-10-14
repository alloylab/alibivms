//! Video from an NVR behind Star4Live: RTSP carried over the cloud relay's WebSocket.
//!
//! The relay socket is opened with subprotocol `rtsp`, answers with a digest challenge in a text
//! frame, and after that carries RTSP requests and responses as text frames and interleaved media
//! (`$` packets) as binary frames. See `docs/star4live-cloud.md`.

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use lapi::{Credentials, cloud::Account};
use tokio::{
    io::{AsyncWriteExt, BufReader, DuplexStream},
    time::{Duration, timeout},
};
use tokio_tungstenite::tungstenite::{Message as Ws, client::IntoClientRequest, http::HeaderValue};

use crate::rtsp_proxy::{Message, encode, read_message};

const DEVICE_USER: &str = "admin";

/// Opens a relay connection to `serial` and returns a byte stream that speaks plain RTSP over TCP
/// framing, plus the credentials the NVR's RTSP server expects on it.
pub async fn connect(account: &Account, serial: &str) -> anyhow::Result<(DuplexStream, Credentials)> {
    let (path, relay) = tokio::join!(device_ws_path(account, serial), account.relay_url(serial));
    let relay = url::Url::parse(&relay.context("Star4Live relay")?).context("relay url")?;
    let socket = socket_url(&relay, &path)?;
    // Rotates every few minutes: fetch it right before use.
    let password = account.dynamic_password(serial).await.context("device one-time password")?;
    let creds = Credentials { username: DEVICE_USER.into(), password };

    let mut req = socket.as_str().into_client_request()?;
    req.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("rtsp"));
    let (mut ws, _) = timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req)).await.context("relay connect timeout")?.context("relay connect")?;

    let challenge = next_text(&mut ws).await?;
    let v: serde_json::Value = serde_json::from_str(&challenge).with_context(|| format!("relay said {challenge:?}"))?;
    if v["errorCode"].as_i64() != Some(401) {
        bail!("relay did not ask for credentials: {challenge}");
    }
    let detail = v["detail"].as_str().unwrap_or_default();
    ws.send(Ws::text(authorization(detail, &creds, &socket, &format!("{:016x}", rand::random::<u64>()))?)).await?;
    let answer = next_text(&mut ws).await?;
    let v: serde_json::Value = serde_json::from_str(&answer).with_context(|| format!("relay said {answer:?}"))?;
    if v["errorCode"].as_i64() != Some(0) {
        bail!("relay refused the device password: {answer}");
    }

    let (ours, theirs) = tokio::io::duplex(512 * 1024);
    tokio::spawn(async move {
        let (tr, mut tw) = tokio::io::split(theirs);
        let (mut sink, mut stream) = ws.split();
        let up = async move {
            let mut tr = BufReader::new(tr);
            while let Some(msg) = read_message(&mut tr).await? {
                let frame = match msg {
                    Message::Text { head, body } => Ws::text(String::from_utf8_lossy(&encode(&head, &body)).into_owned()),
                    Message::Data(d) => Ws::binary(d),
                };
                sink.send(frame).await.map_err(std::io::Error::other)?;
            }
            let _ = sink.close().await;
            std::io::Result::Ok(())
        };
        let down = async move {
            while let Some(frame) = stream.next().await {
                match frame.map_err(std::io::Error::other)? {
                    Ws::Text(t) => tw.write_all(t.as_bytes()).await?,
                    Ws::Binary(b) => tw.write_all(&b).await?,
                    Ws::Close(_) => break,
                    _ => {}
                }
            }
            let _ = tw.shutdown().await;
            std::io::Result::Ok(())
        };
        let r = tokio::select! { r = up => r, r = down => r };
        if let Err(e) = r {
            tracing::debug!(error = %e, "cloud relay closed");
        }
    });
    Ok((ours, creds))
}

async fn next_text<S>(ws: &mut S) -> anyhow::Result<String>
where
    S: futures_util::Stream<Item = Result<Ws, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match timeout(Duration::from_secs(15), ws.next()).await.context("relay timed out")? {
            Some(Ok(Ws::Text(t))) => return Ok(t.as_str().trim().to_string()),
            Some(Ok(Ws::Close(c))) => bail!("relay closed: {c:?}"),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => bail!("relay closed"),
        }
    }
}

/// The device's WebSocket path (`/webSocketServer` on the NVRs seen so far). Asked once per
/// device: it does not change, and every stream would otherwise spend a cloud LAPI call on it.
async fn device_ws_path(account: &Account, serial: &str) -> String {
    static PATHS: std::sync::Mutex<Option<std::collections::HashMap<String, String>>> = std::sync::Mutex::new(None);
    if let Some(p) = PATHS.lock().unwrap().as_ref().and_then(|m| m.get(serial)) {
        return p.clone();
    }
    let r = account.lapi(serial, "GET", "/LAPI/V1.0/Network/WebSocketServer/URL", None).await;
    let url = r.ok().and_then(|v| v["Data"]["WSSURL"].as_str().or(v["Data"]["WSURL"].as_str()).map(str::to_string));
    let Some(path) = url.and_then(|u| url::Url::parse(&u).ok()).map(|u| u.path().to_string()).filter(|p| p.len() > 1) else {
        return "/webSocketServer".into();
    };
    PATHS.lock().unwrap().get_or_insert_default().insert(serial.to_string(), path.clone());
    path
}

/// `wss://<relay host>:<relay port>` + device path + relay path + relay query.
fn socket_url(relay: &url::Url, device_path: &str) -> anyhow::Result<String> {
    let host = relay.host_str().context("relay url has no host")?;
    let port = relay.port_or_known_default().unwrap_or(443);
    let query = relay.query().map(|q| format!("?{q}")).unwrap_or_default();
    Ok(format!("wss://{host}:{port}{}{}{query}", device_path.trim_end_matches('/'), relay.path()))
}

/// The text frame answering the relay's challenge, e.g.
/// `Digest qop=auth,algorithm=MD5,realm=...,nonce=...,opaque=...`.
fn authorization(challenge: &str, creds: &Credentials, uri: &str, cnonce: &str) -> anyhow::Result<String> {
    let params = challenge.trim().strip_prefix("Digest").unwrap_or(challenge);
    let get = |key: &str| {
        params
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
    };
    let (Some(realm), Some(nonce)) = (get("realm"), get("nonce")) else { bail!("relay challenge without realm or nonce: {challenge}") };
    let qop = get("qop").unwrap_or_else(|| "auth".into());
    let algorithm = get("algorithm").unwrap_or_else(|| "MD5".into());
    let opaque = get("opaque").unwrap_or_default();
    let nc = "00000000";
    let md5 = |s: String| format!("{:x}", md5::compute(s.as_bytes()));
    let ha1 = md5(format!("{}:{realm}:{}", creds.username, creds.password));
    let ha2 = md5(format!("GET:{uri}"));
    let response = md5(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"));
    Ok(format!(
        r#"Authorization=Digest username="{}", realm="{realm}", nonce="{nonce}", algorithm="{algorithm}", uri="{uri}", response="{response}", qop="{qop}", nc="{nc}", cnonce="{cnonce}", opaque="{opaque}""#,
        creds.username
    ) + "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_url_joins_relay_and_device_paths() {
        let relay = url::Url::parse("https://turn-noplugin-os.ezforview.com/0b5c-11?a=1&b=2").unwrap();
        assert_eq!(socket_url(&relay, "/webSocketServer").unwrap(), "wss://turn-noplugin-os.ezforview.com:443/webSocketServer/0b5c-11?a=1&b=2");
    }

    #[test]
    fn digest_answer() {
        let creds = Credentials { username: "admin".into(), password: "pw".into() };
        let a = authorization("Digest qop=auth,algorithm=MD5,realm=R,nonce=N,opaque=O", &creds, "wss://h:443/p", "c").unwrap();
        let md5 = |s: &str| format!("{:x}", md5::compute(s.as_bytes()));
        let want = md5(&format!("{}:N:00000000:c:auth:{}", md5("admin:R:pw"), md5("GET:wss://h:443/p")));
        assert!(a.starts_with("Authorization=Digest username=\"admin\", realm=\"R\", nonce=\"N\""), "{a}");
        assert!(a.contains(&format!("response=\"{want}\"")), "{a}");
        assert!(a.ends_with("opaque=\"O\"\r\n"));
    }
}
