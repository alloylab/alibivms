//! A one-connection RTSP proxy on loopback that sits between a player (ffmpeg, GStreamer) and an
//! NVR, reached directly over TCP or through the Star4Live relay (see `cloud_media`).
//!
//! It does what the players cannot:
//! - answers the NVR's auth challenge itself, so NVR credentials never appear in ffmpeg's
//!   command line (visible to every local user through `/proc`);
//! - adds `Scale: N` to `PLAY`, which the NVR uses for faster-than-real-time replay (the web UI's
//!   "high speed" download sends the same header);
//! - for cloud NVRs, carries RTSP over the relay's WebSocket and authenticates with the device's
//!   rotating one-time password.
//!
//! Interleaved RTP (`$` frames) is relayed untouched in both directions.

use std::sync::Arc;

use anyhow::{Context, bail};
use bytes::{Bytes, BytesMut};
use lapi::{Credentials, cloud::Account};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task::JoinHandle,
    time::{Duration, timeout},
};

pub(crate) enum Message {
    /// An interleaved binary frame, header included.
    Data(Bytes),
    /// An RTSP request or response: start line plus headers (without the blank line), and body.
    Text { head: String, body: Bytes },
}

const MAX_HEAD: usize = 64 * 1024;

pub(crate) async fn read_message<R: AsyncBufRead + Unpin>(r: &mut R) -> std::io::Result<Option<Message>> {
    let first = match r.fill_buf().await? {
        [] => return Ok(None),
        b => b[0],
    };
    if first == b'$' {
        let mut hdr = [0u8; 4];
        r.read_exact(&mut hdr).await?;
        let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        let mut frame = BytesMut::with_capacity(4 + len);
        frame.extend_from_slice(&hdr);
        frame.resize(4 + len, 0);
        r.read_exact(&mut frame[4..]).await?;
        return Ok(Some(Message::Data(frame.freeze())));
    }
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            if head.is_empty() {
                continue; // stray blank line between messages
            }
            break;
        }
        head.push_str(&line);
        if head.len() > MAX_HEAD {
            return Err(std::io::Error::other("RTSP header too large"));
        }
    }
    let len = header(&head, "Content-Length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(Message::Text { head: head.trim_end().to_string(), body: body.into() }))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim())
}

fn without_header(head: &str, name: &str) -> String {
    head.lines()
        .filter(|l| !l.split_once(':').is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(name)))
        .collect::<Vec<_>>()
        .join("\r\n")
}

pub(crate) fn encode(head: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(head.len() + 4 + body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(body);
    out
}

#[derive(Clone)]
enum Challenge {
    Basic,
    Digest { realm: String, nonce: String, qop_auth: bool, opaque: Option<String> },
}

fn parse_challenge(values: &[&str]) -> Option<Challenge> {
    let digest = values.iter().find(|v| v.trim_start().to_ascii_lowercase().starts_with("digest"));
    if let Some(d) = digest {
        let params = d.trim_start()[6..].to_string();
        let get = |key: &str| {
            params
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
                .map(|(_, v)| v.trim().trim_matches('"').to_string())
        };
        return Some(Challenge::Digest {
            realm: get("realm")?,
            nonce: get("nonce")?,
            qop_auth: get("qop").is_some_and(|q| q.split(',').any(|q| q.trim() == "auth")),
            opaque: get("opaque"),
        });
    }
    values.iter().any(|v| v.trim_start().to_ascii_lowercase().starts_with("basic")).then_some(Challenge::Basic)
}

fn md5hex(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

fn authorization(ch: &Challenge, creds: &Credentials, method: &str, uri: &str, nc: u32) -> String {
    match ch {
        Challenge::Basic => {
            use base64_lite::encode;
            format!("Basic {}", encode(format!("{}:{}", creds.username, creds.password).as_bytes()))
        }
        Challenge::Digest { realm, nonce, qop_auth, opaque } => {
            let ha1 = md5hex(&format!("{}:{realm}:{}", creds.username, creds.password));
            let ha2 = md5hex(&format!("{method}:{uri}"));
            let mut h = format!(r#"Digest username="{}", realm="{realm}", nonce="{nonce}", uri="{uri}""#, creds.username);
            if *qop_auth {
                let cnonce = format!("{:016x}", rand::random::<u64>());
                let nc = format!("{nc:08x}");
                let resp = md5hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
                h.push_str(&format!(r#", response="{resp}", qop=auth, nc={nc}, cnonce="{cnonce}""#));
            } else {
                h.push_str(&format!(r#", response="{}""#, md5hex(&format!("{ha1}:{nonce}:{ha2}"))));
            }
            if let Some(o) = opaque {
                h.push_str(&format!(r#", opaque="{o}""#));
            }
            h
        }
    }
}

/// Minimal base64 for the Basic scheme, to avoid a dependency for one header.
mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(input: &[u8]) -> String {
        let mut out = String::new();
        for c in input.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
}

struct Shared {
    creds: Credentials,
    /// `rtsp://127.0.0.1:<port>` as the player sees it, and the NVR's own origin it stands for.
    local_origin: String,
    upstream_origin: String,
    scale: Option<u32>,
    challenge: Option<Challenge>,
    nc: u32,
    /// The last request ffmpeg sent, kept so it can be replayed with credentials after a 401.
    last_request: Option<(String, Bytes)>,
    retried_cseq: Option<String>,
}

impl Shared {
    /// Rewrites a request from the player: points it at the NVR's own URL, drops any auth it sent,
    /// adds ours, and adds `Scale` to PLAY.
    fn prepare(&mut self, head: &str) -> String {
        let head = match head.split_once("\r\n") {
            Some((line, rest)) => format!("{}\r\n{rest}", line.replacen(&self.local_origin, &self.upstream_origin, 1)),
            None => head.replacen(&self.local_origin, &self.upstream_origin, 1),
        };
        let mut head = without_header(&head, "Authorization");
        let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
        let (method, uri) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
        if method == "PLAY" {
            if let Some(scale) = self.scale.filter(|s| *s > 1) {
                head = without_header(&head, "Scale");
                head.push_str(&format!("\r\nScale: {scale}.000"));
            }
        }
        if let Some(ch) = &self.challenge {
            self.nc += 1;
            head.push_str(&format!("\r\nAuthorization: {}", authorization(ch, &self.creds, &method, &uri, self.nc)));
        }
        head
    }
}

/// Where the proxy's one connection goes.
pub enum Upstream {
    /// An `rtsp://host:port/path` URL from the NVR, with the NVR login.
    Direct { url: String, creds: Credentials },
    /// An NVR-local RTSP URL (`rtsp://127.0.0.1:554/...`) on a device behind Star4Live.
    Cloud { url: String, account: Arc<Account>, serial: String },
}

impl Upstream {
    fn url(&self) -> &str {
        match self {
            Upstream::Direct { url, .. } | Upstream::Cloud { url, .. } => url,
        }
    }
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

impl Upstream {
    async fn connect(self) -> anyhow::Result<(Reader, Writer, Credentials)> {
        match self {
            Upstream::Direct { url, creds } => {
                let u = url_parts(&url).context("not an rtsp url")?;
                let s = timeout(Duration::from_secs(10), TcpStream::connect(format!("{}:{}", u.host, u.port))).await.context("connect timeout")??;
                let _ = s.set_nodelay(true);
                let (r, w) = s.into_split();
                Ok((Box::new(r), Box::new(w), creds))
            }
            Upstream::Cloud { account, serial, .. } => {
                let (stream, creds) = crate::cloud_media::connect(&account, &serial).await?;
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w), creds))
            }
        }
    }
}

pub struct Proxy {
    /// `rtsp://127.0.0.1:<port>/<path>` for the player; carries no credentials.
    pub local_url: String,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Connects to `upstream`, then starts a proxy for it and returns the URL the player should
/// open. The proxy serves exactly one connection. Connecting first keeps a slow upstream (the
/// cloud relay takes several seconds to set up) out of the player's own response timeout, and
/// reports its failures as they are rather than as an RTSP timeout.
pub async fn start(upstream: Upstream, scale: Option<u32>) -> anyhow::Result<Proxy> {
    let target = upstream.url().to_string();
    let url = url_parts(&target).with_context(|| format!("not an rtsp url: {target}"))?;
    let conn = upstream.connect().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_origin = format!("rtsp://{}", listener.local_addr()?);
    let local_url = format!("{local_origin}{}", url.path);
    let upstream_origin = target.strip_suffix(url.path.as_str()).unwrap_or(&target).to_string();
    let task = tokio::spawn(async move {
        let client = match timeout(Duration::from_secs(20), listener.accept()).await {
            Ok(Ok((s, _))) => s,
            _ => return,
        };
        drop(listener);
        if let Err(e) = relay(client, conn, scale, local_origin, upstream_origin).await {
            tracing::debug!(%target, error = %format!("{e:#}"), "rtsp proxy closed");
        }
    });
    Ok(Proxy { local_url, task })
}

struct UrlParts {
    host: String,
    port: u16,
    path: String,
}

fn url_parts(u: &str) -> Option<UrlParts> {
    let rest = u.strip_prefix("rtsp://")?;
    let (authority, path) = rest.find('/').map(|i| (&rest[..i], &rest[i..])).unwrap_or((rest, "/"));
    let authority = authority.rsplit('@').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 554),
    };
    Some(UrlParts { host, port, path: path.to_string() })
}

async fn relay(client: TcpStream, (ur, uw, creds): (Reader, Writer, Credentials), scale: Option<u32>, local_origin: String, upstream_origin: String) -> anyhow::Result<()> {
    let _ = client.set_nodelay(true);
    let (cr, cw) = client.into_split();
    let uw = Arc::new(Mutex::new(uw));
    let cw = Arc::new(Mutex::new(cw));
    let shared = Arc::new(Mutex::new(Shared {
        creds,
        local_origin: local_origin.clone(),
        upstream_origin: upstream_origin.clone(),
        scale,
        challenge: None,
        nc: 0,
        last_request: None,
        retried_cseq: None,
    }));

    let up = {
        let (uw, shared) = (uw.clone(), shared.clone());
        async move {
            let mut cr = BufReader::new(cr);
            while let Some(msg) = read_message(&mut cr).await? {
                match msg {
                    Message::Data(d) => uw.lock().await.write_all(&d).await?,
                    Message::Text { head, body } => {
                        let out = {
                            let mut s = shared.lock().await;
                            s.last_request = Some((head.clone(), body.clone()));
                            s.prepare(&head)
                        };
                        uw.lock().await.write_all(&encode(&out, &body)).await?;
                    }
                }
            }
            anyhow::Ok(())
        }
    };

    let down = {
        let (uw, cw, shared) = (uw.clone(), cw.clone(), shared.clone());
        async move {
            let mut ur = BufReader::with_capacity(256 * 1024, ur);
            while let Some(msg) = read_message(&mut ur).await? {
                match msg {
                    Message::Data(d) => cw.lock().await.write_all(&d).await?,
                    Message::Text { head, body } => {
                        if let Some(resend) = handle_401(&head, &shared).await? {
                            uw.lock().await.write_all(&resend).await?;
                            continue;
                        }
                        let head = localize(&head, &upstream_origin, &local_origin);
                        cw.lock().await.write_all(&encode(&head, &body)).await?;
                    }
                }
            }
            anyhow::Ok(())
        }
    };

    let r = tokio::select! { r = up => r, r = down => r };
    let _ = uw.lock().await.shutdown().await;
    let _ = cw.lock().await.shutdown().await;
    r
}

/// Points URLs in a response's headers (`Content-Base`, `Content-Location`) back at the proxy.
fn localize(head: &str, upstream_origin: &str, local_origin: &str) -> String {
    head.lines()
        .map(|l| match l.split_once(':') {
            Some((k, _)) if ["content-base", "content-location"].contains(&k.trim().to_ascii_lowercase().as_str()) => l.replace(upstream_origin, local_origin),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// On the first 401 for a request, learns the challenge and returns the request to resend.
async fn handle_401(head: &str, shared: &Mutex<Shared>) -> anyhow::Result<Option<Vec<u8>>> {
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or_default();
    if status != "401" {
        return Ok(None);
    }
    let cseq = header(head, "CSeq").map(str::to_string);
    let mut s = shared.lock().await;
    if s.retried_cseq.is_some() && s.retried_cseq == cseq {
        bail!("NVR rejected RTSP credentials");
    }
    let values: Vec<&str> = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case("WWW-Authenticate"))
        .map(|(_, v)| v.trim())
        .collect();
    let Some(ch) = parse_challenge(&values) else { bail!("401 without a usable challenge") };
    s.challenge = Some(ch);
    s.retried_cseq = cseq;
    let Some((req, body)) = s.last_request.clone() else { return Ok(None) };
    let out = s.prepare(&req);
    Ok(Some(encode(&out, &body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = url_parts("rtsp://10.9.0.2:554/c1/b1791942030/e1791942630/replay/s0/").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("10.9.0.2", 554, "/c1/b1791942030/e1791942630/replay/s0/"));
        assert_eq!(url_parts("rtsp://cam/live").unwrap().port, 554);
    }

    #[test]
    fn play_gets_scale_and_auth() {
        let mut s = Shared {
            creds: Credentials { username: "admin".into(), password: "pw".into() },
            local_origin: "rtsp://127.0.0.1:1".into(),
            upstream_origin: "rtsp://10.9.0.2:554".into(),
            scale: Some(8),
            challenge: Some(Challenge::Digest { realm: "r".into(), nonce: "n".into(), qop_auth: false, opaque: None }),
            nc: 0,
            last_request: None,
            retried_cseq: None,
        };
        let out = s.prepare("PLAY rtsp://127.0.0.1:1/c1/ RTSP/1.0\r\nCSeq: 5\r\nRange: npt=0.000-");
        assert!(out.starts_with("PLAY rtsp://10.9.0.2:554/c1/ RTSP/1.0\r\n"), "{out}");
        assert!(out.contains("\r\nScale: 8.000"));
        let ha1 = md5hex("admin:r:pw");
        let ha2 = md5hex("PLAY:rtsp://10.9.0.2:554/c1/");
        assert!(out.contains(&format!(r#"response="{}""#, md5hex(&format!("{ha1}:n:{ha2}")))), "{out}");
        let describe = s.prepare("DESCRIBE rtsp://x/ RTSP/1.0\r\nCSeq: 2");
        assert!(!describe.contains("Scale"));
    }

    #[test]
    fn content_base_points_back_at_proxy() {
        let h = localize("RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Base: rtsp://127.0.0.1:554/c2/s1/live/", "rtsp://127.0.0.1:554", "rtsp://127.0.0.1:4000");
        assert_eq!(h, "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Base: rtsp://127.0.0.1:4000/c2/s1/live/");
    }

    #[test]
    fn basic_encoding() {
        assert_eq!(base64_lite::encode(b"admin:pw"), "YWRtaW46cHc=");
    }

    #[tokio::test]
    async fn reads_interleaved_and_text() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nabc$\x00\x00\x02hiRTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        assert!(matches!(read_message(&mut r).await.unwrap(), Some(Message::Text { ref body, .. }) if &body[..] == b"abc"));
        assert!(matches!(read_message(&mut r).await.unwrap(), Some(Message::Data(ref d)) if d.len() == 6));
        assert!(matches!(read_message(&mut r).await.unwrap(), Some(Message::Text { .. })));
        assert!(read_message(&mut r).await.unwrap().is_none());
    }
}
