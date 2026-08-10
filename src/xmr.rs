// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Receive, decrypt and handle incoming XMR messages from CMS.

use std::{net::TcpStream, sync::Arc, thread, io::{Read, Write}};
use anyhow::{bail, Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use byteorder::{BE, ReadBytesExt};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Deserializer, de::Error};
use serde_json::{from_slice, from_str};
use time::{OffsetDateTime, Duration};
use tungstenite::{WebSocket, http::uri::Uri, stream::MaybeTlsStream};
use crate::config::{CmsSettings, PlayerSettings};

const READ_TMO: std::time::Duration = std::time::Duration::from_secs(40);
const RECONNECT: std::time::Duration = std::time::Duration::from_secs(10);

/// Possible messages to forward to the collect thread.
#[derive(Debug)]
pub enum Message {
    CollectNow,
    Screenshot,
    Purge,
    WebHook(String),
    Command(String),
    /// A specific widget's server-rendered content changed on the CMS
    /// (payload carries only its widgetId) -- see resource.rs's
    /// `Cache::refresh_resource` for how this gets applied.
    DataUpdate(i64),
    /// Force-show this one layout, bypassing the normal schedule, until
    /// a `RevertToSchedule` (or another `ChangeLayout`) arrives.
    ChangeLayout(i64),
    /// Show this layout as an overlay on top of whatever's currently
    /// playing, for the given duration in seconds -- doesn't interrupt
    /// the underlying schedule at all (unlike `ChangeLayout`).
    OverlayLayout(i64, u64),
    /// Cancel any active `ChangeLayout` override and resume the normal
    /// CMS-driven schedule.
    RevertToSchedule,
    /// One or more Schedule Criteria metrics changed:
    /// (metric, value, ttl in seconds) tuples -- see criteria.rs.
    CriteriaUpdate(Vec<(String, String, i64)>),
}

pub fn start(cms: &CmsSettings, settings: &PlayerSettings, privkey: RsaPrivateKey,
             no_verify: bool) -> Result<Receiver<Message>> {
    let channel = cms.xmr_channel();

    if !settings.xmr_web_socket_address.is_empty() {
        log::info!("Using WebSocket XMR at {}", settings.xmr_web_socket_address);
        let tls_config = cms.make_rustls_client_config(no_verify)?;
        match WsConnector::new(&channel, tls_config,
                               &settings.xmr_web_socket_address,
                               &settings.xmr_cms_key) {
            Ok((connector, receiver)) => {
                thread::spawn(move || connector.run());
                return Ok(receiver);
            }
            Err(e) => log::warn!("failed to connect to XMR WebSocket: {:#}, \
                                  falling back to ZMQ", e),
        }
    }

    log::info!("Using ZMQ XMR at {}", settings.xmr_network_address);
    let (connector, receiver) = ZmqConnector::new(&channel,
                                                  &settings.xmr_network_address,
                                                  privkey)
        .context("setting up XMR ZMQ connection")?;
    thread::spawn(move || connector.run());
    Ok(receiver)
}

const HEARTBEAT: &str = "H";

struct WsConnector {
    uri: Uri,
    tls_config: Arc<rustls::ClientConfig>,
    channel: String,
    cms_key: String,
    sender: Sender<Message>,
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WsConnector {
    fn new(channel: &str, tls_config: rustls::ClientConfig,
           uri: &str, cms_key: &str) -> Result<(Self, Receiver<Message>)> {
        let uri = Uri::try_from(uri).context("parsing XMR WebSocket URI")?;
        let tls_config = Arc::new(tls_config);
        let socket = Self::connect(&uri, tls_config.clone(), channel, cms_key)?;
        let (sender, receiver) = unbounded();
        Ok((Self {
            uri,
            tls_config,
            channel: channel.into(),
            cms_key: cms_key.into(),
            sender,
            socket,
        }, receiver))
    }

    fn connect(uri: &Uri, tls_config: Arc<rustls::ClientConfig>, channel: &str,
               cms_key: &str) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
        let host = uri.host()
                      .context("XMR WebSocket URI missing host")?;
        let port = uri.port_u16().unwrap_or(
            if uri.scheme_str() == Some("wss") { 443 } else { 80 }
        );
        let socket = TcpStream::connect((host, port))
            .context("connecting XMR WebSocket TCP stream")?;
        socket.set_read_timeout(Some(READ_TMO))?;
        let stream = match uri.scheme_str() {
            Some("ws") => MaybeTlsStream::Plain(socket),
            Some("wss") => {
                let connector = rustls::ClientConnection::new(
                    tls_config,
                    "localhost".try_into()?
                ).context("negotiating TLS connection for XMR WebSocket")?;
                let stream = rustls::StreamOwned::new(connector, socket);
                MaybeTlsStream::Rustls(stream)
            }
            _ => bail!("XMR WebSocket URI must start with ws:// or wss://"),
        };

        let (mut socket, _) = tungstenite::client::client(uri, stream)
            .context("handshaking XMR WebSocket")?;
        let init_msg = format!(
            "{{\"type\":\"init\",\"channel\":\"{}\",\"key\":\"{}\"}}",
            channel, cms_key);
        socket.send(tungstenite::Message::text(init_msg))
              .context("sending XMR WebSocket init message")?;
        Ok(socket)
    }

    fn run(mut self) {
        loop {
            if let Err(e) = self.process_msg() {
                log::error!("handling XMR message: {:#}, reconnecting in 10s", e);
                thread::sleep(RECONNECT);
                loop {
                    match Self::connect(&self.uri, self.tls_config.clone(),
                                        &self.channel, &self.cms_key) {
                        Ok(socket) => {
                            self.socket = socket;
                            break;
                        }
                        Err(e) => log::error!("failed to reconnect XMR socket: {:#}", e),
                    }
                }
            }
        }
    }

    fn process_msg(&mut self) -> Result<()> {
        let msg = self.socket.read()?;
        // BUG fix (found from a real report: XMR WebSocket connection
        // closing almost immediately after connecting, newly appearing
        // after a CMS 4.5 upgrade -- CONFIRMED via xibo-xmr's own real
        // source code, index.php: `$wsServer->enableKeepAlive(...)`,
        // using Ratchet's WsServer keepalive, which pings connected
        // clients and closes any that don't Pong back in time.
        // tungstenite *does* auto-queue a Pong reply on receiving a
        // Ping, per its own docs, but only *flushes* it out on the
        // *next* call to read/write/write_pending -- relying on that
        // implicit queue-and-flush-next-time behavior, combined with
        // our blocking read() with a 40s timeout, felt like an
        // avoidable source of a race/delay against a keepalive that
        // (per the earlier "linux WebSocket XMR" feature being brand
        // new, merged March 2026, apparently only tested against "the
        // new Linux player in beta") could plausibly have a short
        // interval. Sending the Pong back explicitly and immediately
        // removes any ambiguity about timing, regardless of whether
        // this exact mechanism turns out to be the full explanation.
        if msg.is_ping() {
            self.socket.send(tungstenite::Message::Pong(msg.into_data()))
                .context("sending XMR WebSocket pong reply")?;
            return Ok(());
        }
        if msg.is_text() {
            if msg.to_text().ok() == Some(HEARTBEAT) {
                return Ok(());
            }
            log::debug!("got XMR WebSocket message: {:?}", msg);
            if let Ok(json_msg) = from_str::<JsonMessage>(msg.to_text()?) {
                if let Some(msg) = json_msg.into_msg() {
                    self.sender.send(msg).unwrap();
                }
            }
        }
        Ok(())
    }
}

struct ZmqConnector {
    uri: String,
    channel: String,
    private_key: RsaPrivateKey,
    sender: Sender<Message>,
    socket: ZmqSubSocket,
}

impl ZmqConnector {
    fn new(channel: &str, uri: &str, private_key: RsaPrivateKey)
           -> Result<(Self, Receiver<Message>)> {
        let socket = Self::connect(channel, uri)?;
        let (sender, receiver) = unbounded();

        Ok((Self {
            uri: uri.into(),
            channel: channel.into(),
            private_key,
            sender,
            socket,
        }, receiver))
    }

    fn connect(channel: &str, uri: &str) -> Result<ZmqSubSocket> {
        let mut socket = ZmqSubSocket::connect(uri).context("connecting XMR socket")?;
        socket.subscribe(channel.as_bytes())?;
        socket.subscribe(HEARTBEAT.as_bytes())?;
        Ok(socket)
    }

    fn run(mut self) {
        loop {
            if let Err(e) = self.process_msg() {
                log::error!("handling XMR message: {:#}, reconnecting in 10s", e);
                thread::sleep(RECONNECT);
                loop {
                    match Self::connect(&self.channel, &self.uri) {
                        Ok(socket) => {
                            self.socket = socket;
                            break;
                        }
                        Err(e) => log::error!("failed to reconnect XMR socket: {:#}", e),
                    }
                }
            }
        }
    }

    fn process_msg(&mut self) -> Result<()> {
        let (channel, more) = self.socket.recv_frame()?;
        if !more {
            bail!("malformed XMR frame: expected multi-part, channel is terminal");
        }
        let (key, more) = self.socket.recv_frame()?;
        if !more {
            bail!("malformed XMR frame: expected multi-part, key is terminal");
        }
        let (content, more) = self.socket.recv_frame()?;
        if more {
            bail!("malformed XMR frame: expected 3 parts, content has more");
        }
        if channel != HEARTBEAT.as_bytes() {
            let json_msg = JsonMessage::decrypt(&self.private_key, &key, &content)?;
            log::debug!("got XMR message: {:?}", json_msg);
            if let Some(msg) = json_msg.into_msg() {
                self.sender.send(msg).unwrap();
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct JsonMessage {
    action: String,
    #[serde(rename = "createdDt")]
    #[serde(deserialize_with = "deserialize_datetime")]
    created: OffsetDateTime,
    #[serde(default)]
    ttl: i64,
    #[serde(rename = "triggerCode")]
    #[serde(default)]
    trigger_code: Option<String>,  // for webhooks
    #[serde(rename = "commandCode")]
    #[serde(default)]
    command_code: Option<String>,  // for commands
    #[serde(rename = "widgetId")]
    #[serde(default)]
    widget_id: Option<i64>,  // for dataUpdate
    // FLAGGED AS UNVERIFIED: field names inferred from the C# client's
    // LayoutChangePlayerAction (layoutId, changeMode: "replace"/other) --
    // could not fetch that exact POCO file's source directly, only
    // confirmed its existence and rough shape via ScheduleManager.cs and
    // XmrSubscriber.cs call sites. Verify against a real changeLayout
    // XMR payload from the CMS before relying on this.
    #[serde(rename = "layoutId")]
    #[serde(default)]
    layout_id: Option<i64>,  // for changeLayout
    #[serde(rename = "changeMode")]
    #[serde(default)]
    change_mode: Option<String>,  // for changeLayout ("replace" vs. queue/add)
    // FLAGGED AS UNVERIFIED: same caveat as layoutId/changeMode above --
    // field name assumed for how long (in seconds) an overlayLayout
    // action should stay visible before automatically hiding again.
    #[serde(default)]
    duration: Option<i64>,  // for overlayLayout
    // Confirmed via official docs (account.xibosignage.com/docs/
    // developer/player-control/schedule-criteria): payload is
    // `{"criteriaUpdates": [{"metric": "...", "value": "...", "ttl": N}]}`
    // -- action name is singular ("criteriaUpdate") but this field is
    // plural, an array of updates in one message.
    #[serde(rename = "criteriaUpdates")]
    #[serde(default)]
    criteria_updates: Option<Vec<CriteriaUpdateItem>>,
}

#[derive(Debug, Deserialize)]
struct CriteriaUpdateItem {
    metric: String,
    value: String,
    #[serde(default)]
    ttl: i64,
}

impl JsonMessage {
    fn decrypt(private_key: &RsaPrivateKey, key: &[u8], content: &[u8]) -> Result<Self> {
        let enc_key = BASE64.decode(key)?;
        let mut msg = BASE64.decode(content)?;
        let msg_key = decrypt_private_key(&enc_key, private_key)?;
        arc4::Arc4::with_key(&msg_key).encrypt(&mut msg);
        Ok(from_slice(&msg)?)
    }

    fn is_expired(&self) -> bool {
        self.created + Duration::seconds(self.ttl) < OffsetDateTime::now_utc()
    }

    fn into_msg(self) -> Option<Message> {
        if self.is_expired() {
            return None;
        }
        match &*self.action {
            "collectNow" => Some(Message::CollectNow),
            // we treat this the same as a collect, which will re-send the pubkey
            "rekeyAction" => Some(Message::CollectNow),
            "screenShot" => Some(Message::Screenshot),
            "purgeAll" => Some(Message::Purge),
            "triggerWebhook" => self.trigger_code.map(Message::WebHook),
            "commandAction" => self.command_code.map(Message::Command),
            "dataUpdate" => self.widget_id.map(Message::DataUpdate),
            "changeLayout" => {
                if let Some(mode) = &self.change_mode {
                    if mode != "replace" {
                        log::warn!("changeLayout with changeMode {mode:?} received -- only \
                                    \"replace\" semantics are implemented (a single override \
                                    slot), not queuing/cycling multiple override layouts");
                    }
                }
                self.layout_id.map(Message::ChangeLayout)
            }
            "revertToSchedule" => Some(Message::RevertToSchedule),
            "overlayLayout" => self.layout_id.map(|id| {
                // Default duration if the CMS message omits it entirely
                // -- arbitrary but conservative choice so a malformed/
                // missing duration doesn't leave the overlay stuck up
                // forever; a real duration from the CMS always overrides
                // this.
                let secs = self.duration.filter(|&d| d > 0).unwrap_or(60) as u64;
                Message::OverlayLayout(id, secs)
            }),
            "criteriaUpdate" => self.criteria_updates.map(|items| {
                Message::CriteriaUpdate(
                    items.into_iter().map(|i| (i.metric, i.value, i.ttl)).collect()
                )
            }),
            _ => {
                log::info!("got unsupported XMR action {:?}", self.action);
                None
            }
        }
    }
}

fn deserialize_datetime<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<OffsetDateTime, D::Error> {
    let s = <String as Deserialize>::deserialize(d)?;
    OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
        .map_err(|_| D::Error::custom("invalid datetime string"))
}

fn decrypt_private_key(enc_key: &[u8], private_key: &RsaPrivateKey) -> Result<Vec<u8>> {
    let dec_data = private_key.decrypt(rsa::Pkcs1v15Encrypt, enc_key).context("failed to decrypt PK")?;
    Ok(dec_data)
}

struct ZmqSubSocket(TcpStream);

/// Implementation of ZMTP as far as we need it for XMR. We don't want to pull in the
/// `zmq` crate since it is almost unmaintained.
impl ZmqSubSocket {
    fn connect(uri: &str) -> Result<Self> {
        let rx = regex::Regex::new("tcp://([^:]*):([0-9]+)").context("invalid validation Regex")?;
        let caps = rx.captures(uri).context("invalid XMR connect URI")?;
        let host = caps.get(1).expect("present").as_str();
        let port = caps[2].parse().expect("digits");

        let mut stream = TcpStream::connect((host, port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;

        // greeting: signature, version (3.0), security (none) and server flag (no),
        // then pad to 64 bytes
        stream.write_all(b"\xff\x00\x00\x00\x00\x00\x00\x00\x01\x7f\
                           \x03\x00\
                           NULL\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                           \x00\
                           \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                           \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")?;
        // read greeting from peer
        let mut buf = [0; 64];
        stream.read_exact(&mut buf)?;
        if buf[0] != 0xff || buf[9] != 0x7f || buf[10] != 0x03 || &buf[12..16] != b"NULL" {
            bail!("ZMTP greeting not understood");
        }

        // send ready command
        stream.write_all(b"\x04\x19\x05READY\x0bSocket-Type\x00\x00\x00\x03SUB")?;
        // read ready command
        stream.read_exact(&mut buf[..2])?;
        if buf[0] != 0x04 {
            bail!("ZMTP command frame not understood");
        }
        let len = buf[1] as usize;
        if len >= 62 {
            bail!("ZMTP command frame too long");
        }
        stream.read_exact(&mut buf[2..2+len])?;
        if &buf[2..8] != b"\x05READY" {
            bail!("ZMTP READY command not understood");
        }

        // now we're ready to receive frames, heartbeats come in 30s interval so use 40
        // to detect dead connection
        stream.set_read_timeout(Some(READ_TMO))?;
        Ok(Self(stream))
    }

    fn subscribe(&mut self, topic: &[u8]) -> Result<()> {
        let mut msg = Vec::with_capacity(3 + topic.len());
        msg.push(0);  // single-frame message, short length
        msg.push(1 + topic.len() as u8);  // length of msg
        msg.push(1);  // subscribe command
        msg.extend_from_slice(topic);
        self.0.write_all(&msg)?;
        Ok(())
    }

    fn recv_frame(&mut self) -> Result<(Vec<u8>, bool)> {
        let flags = self.0.read_u8()?;
        let more = flags & 1 != 0;
        let long_len = flags & 2 != 0;
        let len = if long_len {
            self.0.read_u64::<BE>()? as usize
        } else {
            self.0.read_u8()? as usize
        };
        let mut result = vec![0; len];
        self.0.read_exact(&mut result)?;
        Ok((result, more))
    }
}

#[test]
fn test_data_update_action() {
    let now = OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    let msg = JsonMessage {
        action: "dataUpdate".into(),
        created: OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339).unwrap(),
        ttl: 120,
        trigger_code: None,
        command_code: None,
        widget_id: Some(20349),
        layout_id: None,
        change_mode: None,
        duration: None, criteria_updates: None,
    };
    assert!(matches!(msg.into_msg(), Some(Message::DataUpdate(20349))));

    // malformed/older-CMS message missing the widgetId entirely -> ignored,
    // not a panic or a DataUpdate(0)
    let msg = JsonMessage {
        action: "dataUpdate".into(),
        created: OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339).unwrap(),
        ttl: 120,
        trigger_code: None,
        command_code: None,
        widget_id: None,
        layout_id: None,
        change_mode: None,
        duration: None, criteria_updates: None,
    };
    assert!(msg.into_msg().is_none());
}

#[test]
fn test_change_layout_and_revert_actions() {
    let now = OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    let created = OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339).unwrap();

    let msg = JsonMessage {
        action: "changeLayout".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: Some(627), change_mode: Some("replace".into()), duration: None, criteria_updates: None,
    };
    assert!(matches!(msg.into_msg(), Some(Message::ChangeLayout(627))));

    // missing layoutId entirely -> ignored, not ChangeLayout(0)
    let msg = JsonMessage {
        action: "changeLayout".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: None, change_mode: None, duration: None, criteria_updates: None,
    };
    assert!(msg.into_msg().is_none());

    let msg = JsonMessage {
        action: "revertToSchedule".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: None, change_mode: None, duration: None, criteria_updates: None,
    };
    assert!(matches!(msg.into_msg(), Some(Message::RevertToSchedule)));
}

#[test]
fn test_overlay_layout_action() {
    let now = OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    let created = OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339).unwrap();

    let msg = JsonMessage {
        action: "overlayLayout".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: Some(42), change_mode: None, duration: Some(30), criteria_updates: None,
    };
    assert!(matches!(msg.into_msg(), Some(Message::OverlayLayout(42, 30))));

    // missing/zero duration -> falls back to the default instead of a
    // zero or negative duration that would hide the overlay instantly
    let msg = JsonMessage {
        action: "overlayLayout".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: Some(42), change_mode: None, duration: None, criteria_updates: None,
    };
    assert!(matches!(msg.into_msg(), Some(Message::OverlayLayout(42, 60))));

    let msg = JsonMessage {
        action: "overlayLayout".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: Some(42), change_mode: None, duration: Some(0), criteria_updates: None,
    };
    assert!(matches!(msg.into_msg(), Some(Message::OverlayLayout(42, 60))));

    // missing layoutId entirely -> ignored
    let msg = JsonMessage {
        action: "overlayLayout".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: None, change_mode: None, duration: Some(30), criteria_updates: None,
    };
    assert!(msg.into_msg().is_none());
}

#[test]
fn test_criteria_update_action() {
    let now = OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap();
    let created = OffsetDateTime::parse(&now, &time::format_description::well_known::Rfc3339).unwrap();

    let msg = JsonMessage {
        action: "criteriaUpdate".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: None, change_mode: None, duration: None,
        criteria_updates: Some(vec![
            CriteriaUpdateItem { metric: "temperature".into(), value: "25".into(), ttl: 3600 },
            CriteriaUpdateItem { metric: "weather_condition".into(), value: "clear".into(), ttl: 60 },
        ]),
    };
    match msg.into_msg() {
        Some(Message::CriteriaUpdate(items)) => {
            assert_eq!(items, vec![
                ("temperature".to_string(), "25".to_string(), 3600),
                ("weather_condition".to_string(), "clear".to_string(), 60),
            ]);
        }
        other => panic!("expected CriteriaUpdate, got {other:?}"),
    }

    // missing criteriaUpdates entirely -> ignored
    let msg = JsonMessage {
        action: "criteriaUpdate".into(), created, ttl: 120,
        trigger_code: None, command_code: None, widget_id: None,
        layout_id: None, change_mode: None, duration: None, criteria_updates: None,
    };
    assert!(msg.into_msg().is_none());
}

#[test]
fn test_decrypt() {
    let pem = "-----BEGIN RSA PRIVATE KEY-----
MIICXAIBAAKBgQDJg84myV3VE+v53gQKVbX+6pQrveSfZTcs/a3mikxhXO32peqh
OP2namgoixfBBwK6wzRjRzOHdsB4yQPTMRTZIsipTYHyIqYl5/6AxoRGAsjZtmaB
MNsxrBxMCGlWEKLPwSCecT8EbCrfl3GArf56SEglxDRyx7pDRRnAihPgMQIBEQKB
gAQ7xwUeC6blhxvWaX8kOIeBs4QlVXmrABVh1Wa5wzfTs0BXYoJPt+IsL11bH7E7
TpQO23QaPD4Ba03U5TCJotumgDf0zIfVx5p7GrpK4oqI4o+PX7gWCzurXaqmQiYq
CfZCCeHF+Z2KV2OmhXq3tvlx8Ne4gOiZ65K2vNhNiAEZAkEA1wAyT/hFPUoDnqYD
UfRJEQM1XyRxa0MTkUJh4UO+WCp+d2OtEuydMUdfSu9oGPUNPsMaXr3SzsE8rhp8
1iXB1QJBAO/xQqxO0YvYnDJgQFTXB34Lv66pCHkbBddvYnByfxqeIQJM9o61grUK
LCLjrZ9qPqa87xcYLPP4i8/iPuMKtu0CQQCXw+dHghLB2eRv/LcMrG/PxgeOdBPT
PmgqTPnML9GnpYZyZHoredhfBTQ05Tpr+EWVtuVwDYW/Hv2oErJ5C5fhAkEAm0HB
usmWpchlEYmTCbhQJGH0gBMFe4n0uJNd0EoWAioVW9dyXFdUk0LRQ8B/ZyahAnpA
WjzRywo8WVYosQbu1QJBAIK8lUC6fBRr2ElLltNV/cmR2To5rUYSQJJB9rDw9Inv
cwFD2YnuxuF9szIeWPTmHUl6aXRIByuKNexbHqTeNhY=
-----END RSA PRIVATE KEY-----
";
    use rsa::pkcs1::DecodeRsaPrivateKey;
    let privkey = rsa::RsaPrivateKey::from_pkcs1_pem(pem).unwrap();
    let msg = JsonMessage::decrypt(
        &privkey,
        b"uKgfpneak5Qx5vppLlJZEEcFQ5Y/xrk45ysmnsIVQGvndFR0R86pPRRDPxvqSBgCDb\
          4xInqC8fQLApEzEjULL4QwERycgfHWMY+KSAEDjaS2/3IvSUPa+XYZVZssC/jddIar\
          ZvqHdfylHqm1IiL6Tgaps05BYeyDYynRmngW8NM=",
        b"TOwhZC5mz2N0GoQvUDXsXVDfC3A6Ov5I+raxOsBvvhOLgPFlpz2VxWTsvq5TX8JJ/b\
          gCSdfpe5DTA0bEvwXzDst1KtGjK1Nvdg==").unwrap();
    assert_eq!(msg.action, "screenShot");
}
