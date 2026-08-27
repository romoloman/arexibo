// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Receive, decrypt and handle incoming XMR messages from CMS.

use std::{net::TcpStream, sync::Arc, thread, io::{Read, Write}};
use anyhow::{bail, Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use byteorder::{BE, ReadBytesExt};
use crossbeam_channel::{Receiver, Sender, unbounded};
use crate::util::fingerprint;
use rsa::RsaPrivateKey;
use serde::{Deserialize, Deserializer, de::Error};
use serde_json::{from_slice, from_str};
use time::{OffsetDateTime, Duration};
use tungstenite::{WebSocket, http::uri::Uri, stream::MaybeTlsStream};
use crate::config::{CmsSettings, PlayerSettings};

const READ_TMO: std::time::Duration = std::time::Duration::from_secs(40);
const RECONNECT: std::time::Duration = std::time::Duration::from_secs(10);
/// Give up reconnecting after this many attempts and let the mainloop
/// redo a fresh RegisterDisplay-based restart instead, rather than
/// retrying the same captured uri/channel/cms_key forever.
const RECONNECT_MAX_ATTEMPTS: u32 = 6;
/// A connection stable for at least this long before failing again is
/// treated as a fresh problem (resets the cycle counter), not a
/// continuation of a rapid reconnect loop -- needed because a relay
/// restart can make every reconnect *attempt* succeed but then
/// immediately close again, which a naive failure-count alone never
/// catches.
const RECONNECT_STABILITY_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// Tracks consecutive disconnect-reconnect cycles for the bounded-
/// give-up logic in both `run()` implementations below. Takes
/// explicit `Instant` values (not `Instant::now()` internally) so this
/// can be tested with programmatically-advanced time.
struct ReconnectBudget {
    consecutive_cycles: u32,
    connected_since: std::time::Instant,
}

impl ReconnectBudget {
    fn new(now: std::time::Instant) -> Self {
        Self { consecutive_cycles: 0, connected_since: now }
    }

    /// Call once, right when `process_msg()` first fails (before
    /// starting the reconnect-attempt loop) -- resets the cycle count
    /// if the connection had actually been stable for a while first,
    /// so this doesn't confuse "recovered fine, then failed again much
    /// later for an unrelated reason" with a genuine rapid-fail loop.
    fn on_disconnected(&mut self, now: std::time::Instant) {
        if now.duration_since(self.connected_since) >= RECONNECT_STABILITY_WINDOW {
            self.consecutive_cycles = 0;
        }
    }

    /// Call once per individual reconnect attempt, whether it goes on
    /// to succeed or fail. Returns whether the caller should give up
    /// now, instead of trying again.
    fn record_attempt_and_should_give_up(&mut self) -> bool {
        self.consecutive_cycles += 1;
        self.consecutive_cycles > RECONNECT_MAX_ATTEMPTS
    }

    fn record_reconnected(&mut self, now: std::time::Instant) {
        self.connected_since = now;
    }
}

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

    // Deliberately xmr_web_socket_address_in_use, not the raw
    // xmr_web_socket_address -- see PlayerSettings's own doc comment
    // for why these are two separate fields: this one reflects the
    // final, resolved address (after AREXIBO_FORCE_WS_ADDRESS, the
    // /xmr-derived default, or the sticky-address correction in
    // mainloop.rs), which is what a connection attempt actually needs
    // to use -- the raw field may be empty or incomplete even when
    // this one holds a perfectly good address to try.
    if !settings.xmr_web_socket_address_in_use.is_empty() {
        log::info!("Using WebSocket XMR at {} (channel {}, key fingerprint {})",
                    settings.xmr_web_socket_address_in_use, channel,
                    fingerprint(&settings.xmr_cms_key));
        let tls_config = cms.make_rustls_client_config(no_verify)?;
        match WsConnector::new(&channel, tls_config,
                               &settings.xmr_web_socket_address_in_use,
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
        // Attached as error context (not an unconditional log::warn --
        // only relevant if the connection actually fails, and would
        // otherwise fire on every startup for a CMS using the
        // derived-default convention). See missing_port_warning's own
        // doc comment for how a genuine misconfiguration is told apart
        // from our own intentional port-less default.
        let port_missing = uri.port_u16().is_none();
        // EINTR isn't auto-retried for "trivial" syscalls like connect()
        // -- see connect_retrying_eintr's own doc comment.
        let socket = connect_retrying_eintr((host, port)).with_context(|| {
            if port_missing {
                missing_port_warning(uri, port)
            } else {
                "connecting XMR WebSocket TCP stream".to_string()
            }
        })?;
        socket.set_read_timeout(Some(READ_TMO))?;
        let stream = match uri.scheme_str() {
            Some("ws") => MaybeTlsStream::Plain(socket),
            Some("wss") => {
                // Must be this connection's own real host, not a
                // hardcoded value -- a verified wss:// connection to
                // any real host with a genuine certificate would
                // otherwise always fail (masked previously only by
                // --no-verify, which skips this check entirely).
                let host = uri.host().context("XMR WebSocket URI missing host")?
                              .to_string();
                let connector = rustls::ClientConnection::new(
                    tls_config,
                    host.try_into()?
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
        retry_ws_on_eintr(|| socket.send(tungstenite::Message::text(init_msg.clone())))
              .context("sending XMR WebSocket init message")?;
        Ok(socket)
    }

    /// Gives up after a bounded number of disconnect-reconnect cycles
    /// (see ReconnectBudget), dropping self.sender to close the
    /// channel -- the mainloop's own handling of that (recv(self.xmr))
    /// restarts XMR via a fresh xmr::start() with current settings.
    fn run(mut self) {
        let mut budget = ReconnectBudget::new(std::time::Instant::now());
        loop {
            if let Err(e) = self.process_msg() {
                log::error!("handling XMR message: {:#}, reconnecting in 10s \
                            (channel {}, key fingerprint {})",
                            e, self.channel, fingerprint(&self.cms_key));
                budget.on_disconnected(std::time::Instant::now());
                thread::sleep(RECONNECT);
                loop {
                    if budget.record_attempt_and_should_give_up() {
                        log::error!("giving up on XMR WebSocket reconnection after \
                                    {RECONNECT_MAX_ATTEMPTS} disconnect/reconnect cycles \
                                    within a short time -- the mainloop will restart XMR \
                                    with fresh settings instead");
                        return;
                    }
                    match Self::connect(&self.uri, self.tls_config.clone(),
                                        &self.channel, &self.cms_key) {
                        Ok(socket) => {
                            self.socket = socket;
                            budget.record_reconnected(std::time::Instant::now());
                            break;
                        }
                        Err(e) => {
                            log::error!("failed to reconnect XMR socket: {e:#}");
                            thread::sleep(RECONNECT);
                        }
                    }
                }
            }
        }
    }

    fn process_msg(&mut self) -> Result<()> {
        // tungstenite's read() can also surface a raw EINTR -- see
        // retry_ws_on_eintr's own doc comment.
        let msg = retry_ws_on_eintr(|| self.socket.read())?;
        // CMS 4.5's XMR relay uses Ratchet's WsServer keepalive (pings
        // clients, closes non-responders) -- tungstenite auto-queues a
        // Pong but only flushes it on the next read/write call. Sending
        // it back explicitly and immediately avoids any timing
        // ambiguity against a possibly-short keepalive interval.
        if msg.is_ping() {
            retry_ws_on_eintr(|| self.socket.send(tungstenite::Message::Pong(msg.clone().into_data())))
                .context("sending XMR WebSocket pong reply")?;
            return Ok(());
        }
        // A Close message used to fall through silently, discarding the
        // relay's own close reason (e.g. "Invalid key") -- the next
        // read() then just fails with tungstenite's generic
        // "Connection closed normally". Log it directly instead.
        if let tungstenite::Message::Close(frame) = &msg {
            log::warn!("{}", describe_close(frame));
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
        let mut socket = ZmqSubSocket::connect(uri, READ_TMO).context("connecting XMR socket")?;
        socket.subscribe(channel.as_bytes())?;
        socket.subscribe(HEARTBEAT.as_bytes())?;
        Ok(socket)
    }

    /// See the WebSocket variant's own `run` for the full explanation
    /// of this fix -- same bounded-cycle-then-let-mainloop-restart
    /// approach, for the ZMQ fallback path.
    fn run(mut self) {
        let mut budget = ReconnectBudget::new(std::time::Instant::now());
        loop {
            if let Err(e) = self.process_msg() {
                log::error!("handling XMR message: {:#}, reconnecting in 10s \
                            (channel {})", e, self.channel);
                budget.on_disconnected(std::time::Instant::now());
                thread::sleep(RECONNECT);
                loop {
                    if budget.record_attempt_and_should_give_up() {
                        log::error!("giving up on XMR ZMQ reconnection after \
                                    {RECONNECT_MAX_ATTEMPTS} disconnect/reconnect cycles \
                                    within a short time -- the mainloop will restart XMR \
                                    with fresh settings instead");
                        return;
                    }
                    match Self::connect(&self.channel, &self.uri) {
                        Ok(socket) => {
                            self.socket = socket;
                            budget.record_reconnected(std::time::Instant::now());
                            break;
                        }
                        Err(e) => {
                            log::error!("failed to reconnect XMR socket: {e:#}");
                            thread::sleep(RECONNECT);
                        }
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

/// Retry an IO operation transparently if it fails specifically with
/// `ErrorKind::Interrupted` (EINTR -- interrupted by a signal), rather
/// than propagating that as a genuine failure. See
/// `connect_retrying_eintr`'s own doc comment for the full context of
/// why this is needed.
pub(crate) fn retry_on_eintr<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                log::debug!("IO operation interrupted by a signal (EINTR), retrying immediately");
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Connect a TCP stream, retrying transparently if the connect
/// syscall is interrupted by a signal (EINTR) -- std does not
/// auto-retry EINTR for "trivial" single-syscall wrappers like
/// TcpStream::connect (confirmed via Rust's own internals discussion).
pub(crate) fn connect_retrying_eintr<A: std::net::ToSocketAddrs>(addr: A) -> std::io::Result<TcpStream> {
    retry_on_eintr(|| TcpStream::connect(&addr))
}

/// Same as `retry_on_eintr`, but for tungstenite's own `Result<T,
/// tungstenite::Error>` -- its `Error::Io` variant wraps the same
/// `ErrorKind::Interrupted`.
fn retry_ws_on_eintr<T>(mut f: impl FnMut() -> Result<T, tungstenite::Error>)
    -> Result<T, tungstenite::Error> {
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                log::debug!("WebSocket IO interrupted by a signal (EINTR), retrying immediately");
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Builds the diagnostic warning for a port-less XMR WebSocket
/// address. Distinguishes our own derived default
/// (CmsSettings::default_xmr_websocket_address, deliberately
/// port-less) from a genuine CMS misconfiguration by path alone: the
/// derived default always ends in exactly "/xmr", which a manually-
/// configured address essentially never does by coincidence.
fn missing_port_warning(uri: &Uri, default_port: u16) -> String {
    if uri.path() == "/xmr" {
        format!("XMR WebSocket address {uri:?} has no explicit port -- \
                defaulting to {default_port} per URI scheme convention. This is \
                this player's own derived default (CMS sent an empty \
                \"XMR WebSocket Address\", see \
                account.xibosignage.com/docs/setup/xibo-for-docker for \
                the convention it follows) -- deliberately relying on \
                the scheme's own default port. If nothing is actually \
                listening for XMR on that port, this is likely a \
                deployment/reverse-proxy gap rather than a CMS \
                configuration mistake.")
    } else {
        format!("XMR WebSocket address {uri:?} has no explicit port -- \
                defaulting to {default_port} per URI scheme convention. This is \
                almost always a CMS misconfiguration (Administration -> \
                Settings -> Displays -> \"XMR WebSocket Address\") rather \
                than an intentional address -- if this address is wrong, \
                connections will fail with a confusing TCP-level error \
                rather than an obviously-XMR-related one.")
    }
}

/// Format a WebSocket Close message's own code/reason for logging --
/// this used to be discarded silently, with only tungstenite's own
/// generic "Connection closed normally" showing up on the next read().
fn describe_close(frame: &Option<tungstenite::protocol::CloseFrame>) -> String {
    match frame {
        Some(f) => format!("XMR WebSocket closed by the relay: {} (code {})", f.reason, f.code),
        None => "XMR WebSocket closed by the relay (no reason given)".to_string(),
    }
}

pub(crate) struct ZmqSubSocket(TcpStream);

/// Implementation of ZMTP as far as we need it for XMR. We don't want to pull in the
/// `zmq` crate since it is almost unmaintained.
impl ZmqSubSocket {
    pub(crate) fn connect(uri: &str, read_timeout: std::time::Duration) -> Result<Self> {
        let rx = regex::Regex::new("tcp://([^:]*):([0-9]+)").context("invalid validation Regex")?;
        let caps = rx.captures(uri).context("invalid XMR connect URI")?;
        let host = caps.get(1).expect("present").as_str();
        let port = caps[2].parse().expect("digits");

        let mut stream = connect_retrying_eintr((host, port))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;

        // greeting: signature, version (3.0), security (none) and server flag (no),
        // then pad to 64 bytes
        //
        // BUG fix: same EINTR gap as connect_retrying_eintr above, for
        // every direct write_all/read_exact call in this handshake.
        retry_on_eintr(|| stream.write_all(b"\xff\x00\x00\x00\x00\x00\x00\x00\x01\x7f\
                           \x03\x00\
                           NULL\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                           \x00\
                           \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
                           \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"))?;
        // read greeting from peer
        let mut buf = [0; 64];
        retry_on_eintr(|| stream.read_exact(&mut buf))?;
        if buf[0] != 0xff || buf[9] != 0x7f || buf[10] != 0x03 || &buf[12..16] != b"NULL" {
            bail!("ZMTP greeting not understood");
        }

        // send ready command
        retry_on_eintr(|| stream.write_all(b"\x04\x19\x05READY\x0bSocket-Type\x00\x00\x00\x03SUB"))?;
        // read ready command
        retry_on_eintr(|| stream.read_exact(&mut buf[..2]))?;
        if buf[0] != 0x04 {
            bail!("ZMTP command frame not understood");
        }
        let len = buf[1] as usize;
        if len >= 62 {
            bail!("ZMTP command frame too long");
        }
        retry_on_eintr(|| stream.read_exact(&mut buf[2..2+len]))?;
        if &buf[2..8] != b"\x05READY" {
            bail!("ZMTP READY command not understood");
        }

        // now we're ready to receive frames -- the caller picks a
        // timeout matching its own heartbeat cadence (XMR: 30s
        // heartbeats, uses 40; Sync Group: 5s Sync messages, uses a
        // much shorter one -- see syncgroup.rs's own call site) so a
        // dead peer gets detected promptly, and so any `stop`-flag
        // check tied to this read (like Sync Group's own Follower
        // loop) doesn't block for far longer than that cadence
        // actually requires.
        stream.set_read_timeout(Some(read_timeout))?;
        Ok(Self(stream))
    }

    pub(crate) fn subscribe(&mut self, topic: &[u8]) -> Result<()> {
        let mut msg = Vec::with_capacity(3 + topic.len());
        msg.push(0);  // single-frame message, short length
        msg.push(1 + topic.len() as u8);  // length of msg
        msg.push(1);  // subscribe command
        msg.extend_from_slice(topic);
        retry_on_eintr(|| self.0.write_all(&msg))?;
        Ok(())
    }

    pub(crate) fn recv_frame(&mut self) -> Result<(Vec<u8>, bool)> {
        // BUG fix: same EINTR gap as connect_retrying_eintr/process_msg's
        // WebSocket read above, just for this hand-rolled ZMTP frame
        // reader's own direct read_u8/read_u64/read_exact calls on the
        // raw TcpStream -- each wrapped individually since they're
        // separate reads, any one of which could individually get
        // interrupted by a signal.
        let flags = retry_on_eintr(|| self.0.read_u8())?;
        let more = flags & 1 != 0;
        let long_len = flags & 2 != 0;
        let len = if long_len {
            retry_on_eintr(|| self.0.read_u64::<BE>())? as usize
        } else {
            retry_on_eintr(|| self.0.read_u8())? as usize
        };
        let mut result = vec![0; len];
        retry_on_eintr(|| self.0.read_exact(&mut result))?;
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

#[cfg(test)]
mod retry_on_eintr_tests {
    use super::retry_on_eintr;
    use std::io::{Error, ErrorKind};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn succeeds_immediately_if_no_error() {
        let calls = AtomicU32::new(0);
        let result = retry_on_eintr(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, Error>(42)
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "must not retry when there's no error at all");
    }

    #[test]
    fn retries_transparently_on_interrupted_then_succeeds() {
        // The actual bug scenario: EINTR a few times (a signal arriving
        // at an inconvenient moment, matching the real report), then a
        // genuine success -- must retry silently and return the
        // eventual success, not the earlier interruptions.
        let calls = AtomicU32::new(0);
        let result = retry_on_eintr(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 3 {
                Err(Error::new(ErrorKind::Interrupted, "simulated EINTR"))
            } else {
                Ok(99)
            }
        });
        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls.load(Ordering::SeqCst), 4, "must retry exactly as many times as needed, no more");
    }

    #[test]
    fn a_genuine_non_interrupted_error_propagates_immediately_without_retrying() {
        // Must NOT retry on other error kinds (e.g. a real connection
        // refused) -- only EINTR specifically gets this transparent
        // retry treatment, anything else is a real failure that should
        // surface to the caller right away.
        let calls = AtomicU32::new(0);
        let result: std::io::Result<i32> = retry_on_eintr(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::new(ErrorKind::ConnectionRefused, "simulated real failure"))
        });
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::ConnectionRefused);
        assert_eq!(calls.load(Ordering::SeqCst), 1,
                   "must not retry at all for a non-Interrupted error");
    }
}

#[cfg(test)]
mod retry_ws_on_eintr_tests {
    use super::retry_ws_on_eintr;
    use std::io::{Error, ErrorKind};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn retries_transparently_on_interrupted_tungstenite_io_error_then_succeeds() {
        // Same scenario as retry_on_eintr's own test, but for
        // tungstenite's own Error type (used by both the WebSocket
        // init/pong sends and the message read, section 74) -- its
        // Error::Io variant must be unwrapped to find the same
        // underlying ErrorKind::Interrupted.
        let calls = AtomicU32::new(0);
        let result = retry_ws_on_eintr(|| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(tungstenite::Error::Io(Error::new(ErrorKind::Interrupted, "simulated EINTR")))
            } else {
                Ok(7)
            }
        });
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_genuine_non_io_tungstenite_error_propagates_immediately() {
        let calls = AtomicU32::new(0);
        let result: Result<i32, tungstenite::Error> = retry_ws_on_eintr(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(tungstenite::Error::ConnectionClosed)
        });
        assert!(matches!(result, Err(tungstenite::Error::ConnectionClosed)));
        assert_eq!(calls.load(Ordering::SeqCst), 1,
                   "must not retry for a non-Io tungstenite error");
    }
}

#[cfg(test)]
mod reconnect_budget_tests {
    use super::{ReconnectBudget, RECONNECT_MAX_ATTEMPTS, RECONNECT_STABILITY_WINDOW};
    use std::time::{Duration, Instant};

    #[test]
    fn gives_up_when_every_reconnect_succeeds_but_drops_again_immediately() {
        // Regression test for a real reproduction: restarting the XMR
        // relay caused every single reconnect *attempt* to succeed
        // cleanly, but the relay then closed each new connection again
        // immediately afterward (WebSocket close code 1000, no error) --
        // in an endless cycle. The original version of this fix only
        // counted consecutive connect() *failures*, which never
        // triggered here, since every individual attempt succeeded.
        let t0 = Instant::now();
        let mut budget = ReconnectBudget::new(t0);
        let mut now = t0;
        let mut gave_up = false;
        for _ in 0..(RECONNECT_MAX_ATTEMPTS + 2) {
            // process_msg() just failed (closed again almost
            // immediately after reconnecting) -- barely any time
            // passed since the last successful reconnect.
            now += Duration::from_millis(50);
            budget.on_disconnected(now);
            if budget.record_attempt_and_should_give_up() {
                gave_up = true;
                break;
            }
            // The reconnect itself succeeds right away every time --
            // this is the crux of the real report.
            now += Duration::from_millis(50);
            budget.record_reconnected(now);
        }
        assert!(gave_up, "must give up after too many rapid disconnect/reconnect \
                          cycles, even when every individual reconnect succeeds");
    }

    #[test]
    fn does_not_give_up_prematurely_on_an_isolated_failure() {
        let t0 = Instant::now();
        let mut budget = ReconnectBudget::new(t0);
        budget.on_disconnected(t0 + Duration::from_millis(50));
        assert!(!budget.record_attempt_and_should_give_up(),
                "a single disconnect/reconnect cycle must never trigger giving up");
    }

    #[test]
    fn a_genuinely_stable_connection_resets_the_counter() {
        // A connection that stayed up for a good while before failing
        // again must be treated as a fresh, unrelated problem -- not
        // silently accumulate toward the give-up threshold alongside
        // an earlier, unrelated blip.
        let t0 = Instant::now();
        let mut budget = ReconnectBudget::new(t0);

        // A few rapid cycles first (but not enough to give up).
        let mut now = t0;
        for _ in 0..(RECONNECT_MAX_ATTEMPTS - 1) {
            now += Duration::from_millis(50);
            budget.on_disconnected(now);
            assert!(!budget.record_attempt_and_should_give_up());
            now += Duration::from_millis(50);
            budget.record_reconnected(now);
        }

        // Then it stays up for a genuinely long, stable while.
        now += RECONNECT_STABILITY_WINDOW + Duration::from_secs(1);
        budget.on_disconnected(now);

        // The counter must have reset -- this single new cycle alone
        // must not be enough to give up.
        assert!(!budget.record_attempt_and_should_give_up(),
                "a long stable period must reset the counter, not carry over \
                 the earlier unrelated rapid-cycle count");
    }

    #[test]
    fn mixes_genuine_connect_failures_into_the_same_budget() {
        // A real connect() failure (not just "succeeded then dropped
        // again") must count toward the exact same budget -- the two
        // failure modes aren't tracked separately.
        let t0 = Instant::now();
        let mut budget = ReconnectBudget::new(t0);
        let mut now = t0;
        let mut gave_up = false;
        for i in 0..(RECONNECT_MAX_ATTEMPTS + 2) {
            now += Duration::from_millis(50);
            if i == 0 {
                budget.on_disconnected(now);
            }
            if budget.record_attempt_and_should_give_up() {
                gave_up = true;
                break;
            }
            // Every other attempt is a genuine connect() failure (no
            // record_reconnected call at all) -- alternating with a
            // successful-then-immediately-redropped one.
            if i % 2 == 0 {
                now += Duration::from_millis(50);
                budget.record_reconnected(now);
            }
        }
        assert!(gave_up, "genuine connect() failures must count toward the \
                          same give-up budget as successful-but-immediately-\
                          redropped cycles");
    }
}

#[cfg(test)]
mod wss_tls_hostname_tests {
    use super::*;

    /// Runs a minimal, real TLS server on 127.0.0.1 (random port),
    /// presenting a genuine self-signed certificate valid *only* for
    /// the IP address 127.0.0.1 (via rcgen's IP SAN support) -- not
    /// "localhost". Accepts exactly one connection, does the TLS
    /// handshake, then just keeps it open briefly (this test only
    /// needs the handshake itself to succeed or fail, not any actual
    /// WebSocket traffic afterward).
    fn start_tls_server_for_ip(ip: std::net::IpAddr)
        -> (u16, rustls::pki_types::CertificateDer<'static>) {
        // Must happen before the server thread below tries to build
        // its own ServerConfig -- install_default() can only succeed
        // once per process, and thread execution order isn't
        // guaranteed, so this could otherwise race against the test's
        // own later call to make_rustls_client_config (which installs
        // the same provider on the client side).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
        params.subject_alt_names = vec![rcgen::SanType::IpAddress(ip)];
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let cert_der = cert.der().clone();
        let cert_der_for_client = cert_der.clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());

        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        let server_config = Arc::new(server_config);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let conn = rustls::ServerConnection::new(server_config);
                if let Ok(conn) = conn {
                    let mut tls_stream = rustls::StreamOwned::new(conn, stream);
                    // Drive the handshake -- a real client's ClientHello
                    // needs a byte written back for the handshake to
                    // actually complete/fail visibly on the client side.
                    let mut buf = [0u8; 1];
                    let _ = tls_stream.read(&mut buf);
                    let _ = conn;
                }
            }
        });
        (port, cert_der_for_client)
    }

    #[test]
    fn wss_connect_validates_against_the_uris_own_host_not_a_hardcoded_localhost() {
        // Regression test for a real, live report against a genuine
        // CMS with a real TLS certificate: "invalid peer certificate:
        // certificate not valid for name 'localhost'; certificate is
        // only valid for DnsName('cms.example.com')" -- the SNI/
        // certificate-validation hostname was hardcoded to the literal
        // string "localhost" regardless of the actual host being
        // connected to, meaning a properly *verified* wss:// connection
        // to any real host could never have worked, ever, for any
        // deployment (only masked by --no-verify, which skips this
        // check entirely).
        //
        // This uses a real, live TLS handshake (not just a unit check
        // of some extracted string) against a certificate that is
        // genuinely valid *only* for IP 127.0.0.1 -- not "localhost" --
        // so the old hardcoded value would have caused this exact
        // validation failure. Uses an IP SAN specifically (rather than
        // a DNS name) so the connection can be made to a real local
        // listener (127.0.0.1) without needing DNS or /etc/hosts.
        let (port, test_cert_der) = start_tls_server_for_ip(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        let uri: Uri = format!("wss://127.0.0.1:{port}/xmr").parse().unwrap();

        // A custom client config that trusts *specifically* this
        // test's own self-signed certificate -- not the real, public
        // CA roots make_rustls_client_config would use (which would
        // otherwise correctly, but besides the point for this test,
        // reject the self-signed cert as UnknownIssuer regardless of
        // any hostname check at all).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add(test_cert_der.clone()).unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let tls_config = Arc::new(client_config);

        let result = WsConnector::connect(&uri, tls_config, "test-channel", "test-key");
        // Note: this test server only implements the raw TLS handshake,
        // not a full WebSocket-protocol response on top of it -- so a
        // *later*, WebSocket-level failure (e.g. the connection closing
        // once tungstenite's own handshake proceeds) is an expected
        // limitation of this minimal test server, not a sign of a real
        // problem. What actually matters for this specific fix is
        // narrower and precise: the error must NOT be a certificate/
        // hostname validation failure -- if the old hardcoded
        // "localhost" bug were still present, THIS is exactly the kind
        // of error that would appear instead of (or before) any
        // WebSocket-level one.
        if let Err(e) = &result {
            let msg = format!("{e:#}");
            assert!(!msg.to_lowercase().contains("certificate"),
                    "must not be a certificate/hostname validation failure -- the old bug \
                     hardcoded \"localhost\" regardless of the real host -- got: {msg}");
        }
    }
}

#[cfg(test)]
mod missing_port_warning_tests {
    use super::{missing_port_warning, WsConnector};
    use tungstenite::http::uri::Uri;

    #[test]
    fn recognizes_our_own_derived_default_by_its_xmr_path() {
        // The exact real report this distinction exists for: our own
        // default_xmr_websocket_address() fallback produces a
        // deliberately port-less address ending in "/xmr" -- must NOT
        // be described as "almost always a CMS misconfiguration",
        // which would be actively misleading for this specific case.
        let uri: Uri = "ws://192.168.1.11/xmr".parse().unwrap();
        let msg = missing_port_warning(&uri, 80);
        assert!(msg.contains("this player's own derived default"),
                "must recognize its own derived default -- got: {msg}");
        assert!(!msg.contains("almost always a CMS misconfiguration"),
                "must not blame a CMS misconfiguration for its own derived default");
    }

    #[test]
    fn treats_any_other_path_as_a_likely_cms_misconfiguration() {
        // A genuine CMS-configured address (no path, or a path that
        // isn't the exact /xmr convention) must keep the original,
        // more direct guidance -- this is the actual GitHub-reported
        // scenario this warning was originally added for.
        let uri: Uri = "ws://192.168.2.138".parse().unwrap();
        let msg = missing_port_warning(&uri, 80);
        assert!(msg.contains("almost always a CMS misconfiguration"),
                "must keep blaming a likely CMS misconfiguration -- got: {msg}");
        assert!(!msg.contains("this player's own derived default"));
    }

    #[test]
    fn a_path_that_merely_resembles_xmr_is_not_falsely_recognized() {
        // Guards against a too-loose match (e.g. substring matching
        // instead of an exact path comparison) -- only the *exact*
        // "/xmr" path should be recognized as our own convention.
        let uri: Uri = "ws://192.168.2.138/xmr-something-else".parse().unwrap();
        let msg = missing_port_warning(&uri, 80);
        assert!(msg.contains("almost always a CMS misconfiguration"),
                "a merely-similar path must not be falsely recognized -- got: {msg}");
    }

    #[test]
    fn the_diagnostic_only_surfaces_when_the_connection_actually_fails() {
        // Regression test for a real report, right after adding the
        // /xmr-path distinction above: this used to be logged
        // *unconditionally*, before even attempting the connection --
        // showing up on every single startup for a CMS relying on the
        // derived-default convention, even when the connection went on
        // to succeed just fine. Now it's attached as error *context*
        // instead, so it only ever surfaces as part of a genuine
        // connection failure's own message.
        //
        // Port 80 is unused in this sandbox (confirmed separately) --
        // connecting there is expected to reliably fail with
        // "connection refused", letting this test check the resulting
        // *error's own message* directly, without needing a real
        // listener (avoiding the same CI-fragility concern as
        // mainloop.rs's own sticky-address tests around binding to
        // port 80).
        let uri: Uri = "ws://127.0.0.1/xmr".parse().unwrap();
        let cms = crate::config::CmsSettings {
            address: "http://127.0.0.1".into(), key: "k".into(), display_id: "d".into(),
            display_name: None, proxy: None,
        };
        let tls_config = std::sync::Arc::new(cms.make_rustls_client_config(true).unwrap());
        let result = WsConnector::connect(&uri, tls_config, "test-channel", "test-key");
        let err = result.expect_err("connecting to an unused port must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("this player's own derived default"),
                "the diagnostic must appear in the error's own message when the \
                 connection genuinely fails -- got: {msg}");
    }
}

#[cfg(test)]
mod describe_close_tests {
    use super::describe_close;
    use tungstenite::protocol::CloseFrame;
    use tungstenite::protocol::frame::coding::CloseCode;

    #[test]
    fn includes_the_relays_own_reason_text_and_code() {
        // The exact scenario this whole fix is about: the relay closes
        // with a specific reason (e.g. "Invalid key") -- this must
        // appear verbatim in the formatted message, not be silently
        // discarded as it was before this fix.
        let frame = Some(CloseFrame {
            code: CloseCode::Protocol,
            reason: "Invalid key".into(),
        });
        let msg = describe_close(&frame);
        assert!(msg.contains("Invalid key"),
                "the relay's own close reason must appear in the log message -- got: {msg}");
        assert!(msg.contains("1002"), "the numeric close code must also appear -- got: {msg}");
    }

    #[test]
    fn handles_a_close_with_no_frame_at_all() {
        // A bare Close(None) -- no code/reason given at all -- must
        // still produce a sensible message, not panic or produce an
        // empty/confusing string.
        let msg = describe_close(&None);
        assert!(!msg.is_empty());
        assert!(msg.contains("no reason given"));
    }

    #[test]
    fn different_reasons_produce_distinguishable_messages() {
        // Sanity check: two different real-world reasons must not
        // collapse into the same generic message -- the whole point is
        // being able to tell them apart in the log.
        let invalid_key = describe_close(&Some(CloseFrame {
            code: CloseCode::Protocol,
            reason: "Invalid key".into(),
        }));
        let going_away = describe_close(&Some(CloseFrame {
            code: CloseCode::Away,
            reason: "Server restarting".into(),
        }));
        assert_ne!(invalid_key, going_away);
    }
}
