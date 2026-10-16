//! Runtime of the Data Connectors (the rules are in dataconnector.rs): the
//! state shared by the server (the bridge endpoints the connector pages call),
//! a timer thread for coalesced writes, and the mainloop.
//!
//! Each connector runs in its own hidden browser page, loaded from
//! `/dc/host/<dataSetId>.html` (see `host_page`). The page's `xiboDC` object
//! has the API of the official players and talks to `/dc/<action>` over HTTP.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tiny_http::{Header, Request, Response, ResponseBox};

use crate::dataconnector::{decode_data_params, ConnectorPayload, HostCommand, Hosts,
                           NotifyAction, RealtimeWriter, SetOutcome, Write};
use crate::mainloop::ToGui;
use crate::server::LocalDataStore;

/// As in the Electron player.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CRITERIA_TTL: i64 = 300;

/// What the connectors ask of the mainloop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DcEvent {
    Criteria { metric: String, value: String, ttl: i64 },
}

/// The answer to a bridge call: `done` callbacks on success, `error` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub success: bool,
    pub status: u16,
    pub data: String,
}

impl Reply {
    fn ok() -> Self { Self { success: true, status: 200, data: String::new() } }

    fn failed(status: u16, data: impl Into<String>) -> Self {
        Self { success: false, status, data: data.into() }
    }

    fn to_json(&self) -> Value {
        json!({ "success": self.success, "status": self.status, "data": self.data })
    }
}

struct Inner {
    hosts: Hosts,
    writer: RealtimeWriter,
    pages: HashMap<i64, String>,
    to_gui: Option<Sender<ToGui>>,
    display_tags: BTreeMap<String, String>,
}

pub struct DcRuntime {
    inner: Mutex<Inner>,
    wake: Condvar,
    local_data: LocalDataStore,
    events_tx: Sender<DcEvent>,
    events_rx: Receiver<DcEvent>,
}

static RUNTIME: Lazy<DcRuntime> = Lazy::new(DcRuntime::new);

/// The player-wide runtime; starts its timer thread on first use.
pub fn runtime() -> &'static DcRuntime {
    static TIMER: Once = Once::new();
    TIMER.call_once(|| {
        std::thread::Builder::new().name("data-connectors".into())
            .spawn(|| RUNTIME.timer_loop()).expect("spawning the data connector timer");
    });
    &RUNTIME
}

impl Default for DcRuntime {
    fn default() -> Self { Self::new() }
}

impl DcRuntime {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded();
        Self {
            inner: Mutex::new(Inner { hosts: Hosts::default(), writer: RealtimeWriter::default(),
                                      pages: HashMap::new(), to_gui: None,
                                      display_tags: BTreeMap::new() }),
            wake: Condvar::new(),
            local_data: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            events_rx,
        }
    }

    /// The store served at `/realtime`.
    pub fn local_data(&self) -> LocalDataStore {
        self.local_data.clone()
    }

    pub fn events(&self) -> Receiver<DcEvent> {
        self.events_rx.clone()
    }

    pub fn attach(&self, to_gui: Sender<ToGui>) {
        self.lock().to_gui = Some(to_gui);
    }

    /// The display's tags, given to the connectors that start from now on.
    pub fn set_display_tags(&self, tags: BTreeMap<String, String>) {
        self.lock().display_tags = tags;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn gui(inner: &Inner, message: ToGui) {
        if let Some(tx) = &inner.to_gui {
            let _ = tx.send(message);
        }
    }

    fn store(&self, write: Write) {
        self.local_data.lock().unwrap_or_else(|e| e.into_inner()).insert(write.key, write.data);
    }

    /// Starts, restarts and stops the connector pages so they match `payloads`.
    /// `load_script` gives a connector's verified script, `host_url` the address
    /// its page is loaded from.
    pub fn sync(&self, payloads: Vec<ConnectorPayload>,
                load_script: impl Fn(&ConnectorPayload) -> Option<String>,
                host_url: impl Fn(i64) -> String) {
        let mut inner = self.lock();
        for command in inner.hosts.sync(payloads) {
            match command {
                HostCommand::Stop(id) => self.stop_locked(&mut inner, id),
                HostCommand::Start(p) => match load_script(&p) {
                    Some(script) => {
                        let page = host_page(p.data_set_id, &decode_data_params(&p.data_params), &script,
                                             &inner.display_tags);
                        inner.pages.insert(p.data_set_id, page);
                        log::info!("data connector {} started (schedule {})", p.data_set_id, p.schedule_id);
                        Self::gui(&inner, ToGui::DataConnectorStart(p.data_set_id, host_url(p.data_set_id)));
                    }
                    None => {
                        log::debug!("data connector {} script not available, will retry", p.data_set_id);
                        inner.hosts.forget(p.data_set_id);
                    }
                },
            }
        }
    }

    /// A connector stops: its page goes, and so do its values; the widgets are
    /// told so they read again and degrade on the resulting 404.
    fn stop_locked(&self, inner: &mut Inner, data_set_id: i64) {
        inner.pages.remove(&data_set_id);
        Self::gui(inner, ToGui::DataConnectorStop(data_set_id));
        for key in inner.writer.stop_dataset(data_set_id) {
            self.local_data.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            Self::gui(inner, ToGui::NotifyData(key));
        }
        log::info!("data connector {data_set_id} stopped");
    }

    /// Stops one connector now (without waiting for the missed assessments).
    pub fn stop(&self, data_set_id: i64) {
        let mut inner = self.lock();
        inner.hosts.forget(data_set_id);
        self.stop_locked(&mut inner, data_set_id);
    }

    /// Everything stops and every value goes (the cache was purged).
    pub fn stop_all(&self) {
        let mut inner = self.lock();
        for id in inner.hosts.active_ids() {
            inner.hosts.forget(id);
            self.stop_locked(&mut inner, id);
        }
        self.local_data.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn active_ids(&self) -> Vec<i64> {
        self.lock().hosts.active_ids()
    }

    pub fn host_page(&self, data_set_id: i64) -> Option<String> {
        self.lock().pages.get(&data_set_id).cloned()
    }

    pub fn set_data(&self, now: Instant, data_set_id: i64, key: &str, data: String) -> Reply {
        let mut inner = self.lock();
        match inner.writer.set(now, data_set_id, key, data) {
            SetOutcome::Write(write) => {
                self.store(write);
                Reply::ok()
            }
            // Accepted now, stored when the interval has passed (the Electron
            // player answers when it is stored).
            SetOutcome::Queued => {
                self.wake.notify_all();
                Reply::ok()
            }
            SetOutcome::Rejected { status, message } => Reply::failed(status, message),
        }
    }

    pub fn notify(&self, key: &str) {
        let mut inner = self.lock();
        if inner.writer.notify(key) == NotifyAction::Now {
            Self::gui(&inner, ToGui::NotifyData(key.to_string()));
        }
    }

    pub fn criteria(&self, metric: String, value: String, ttl: i64) {
        let _ = self.events_tx.send(DcEvent::Criteria { metric, value, ttl });
    }

    /// Stores the queued writes that are due.
    pub fn tick(&self, now: Instant) {
        let mut inner = self.lock();
        self.tick_locked(&mut inner, now);
    }

    fn tick_locked(&self, inner: &mut Inner, now: Instant) {
        for flush in inner.writer.due(now) {
            let key = flush.write.key.clone();
            self.store(flush.write);
            if flush.notify {
                Self::gui(inner, ToGui::NotifyData(key));
            }
        }
    }

    fn timer_loop(&self) {
        let mut inner = self.lock();
        loop {
            let now = Instant::now();
            self.tick_locked(&mut inner, now);
            let wait = inner.writer.next_deadline()
                .map_or(Duration::from_secs(3600), |d| d.saturating_duration_since(now));
            inner = self.wake.wait_timeout(inner, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Keeps a script from closing its own `<script>` element.
fn escape_script_content(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    let lower = js.to_ascii_lowercase();
    let mut i = 0;
    while i < js.len() {
        if lower[i..].starts_with("</script") {
            out.push_str("<\\/");
            i += 2;
        } else if lower[i..].starts_with("<!--") {
            out.push_str("<\\!--");
            i += 4;
        } else {
            let c = js[i..].chars().next().expect("char");
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// The page that hosts a connector: the `xiboDC` API of the official players
/// (see electron-player's `hostPage.ts`), then the connector's script, then
/// `initialise` once loaded (which calls the script's `onInit`).
const BRIDGE: &str = r#"
window.xiboDC = (function() {
  'use strict';
  var DATASET_ID = __ID__;

  function post(action, body, callback) {
    body.dataSetId = DATASET_ID;
    var request;
    try {
      request = fetch('/dc/' + action, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify(body),
      });
    } catch (e) {
      if (callback) { callback({success: false, status: 0, data: String(e)}); }
      return;
    }
    request.then(function(r) { return r.json(); }).then(function(reply) {
      if (callback) { callback(reply); }
    }, function(e) {
      if (callback) { callback({success: false, status: 0, data: String(e)}); }
    });
  }

  function key(value) {
    return (value === undefined || value === null) ? '' : String(value);
  }

  function respond(callbacks, reply) {
    if (reply.success) {
      if (typeof (callbacks.done) == 'function') { callbacks.done(reply.status, reply.data); }
    } else {
      if (typeof (callbacks.error) == 'function') { callbacks.error(reply.status, reply.data); }
    }
  }

  var mainLib = {
    initialise: function(dataSetId, dataSetParameters) {
      window.dataSetId = dataSetId;
      new URLSearchParams(dataSetParameters).forEach(function(value, key) {
        window[key] = value;
      });
      if (typeof (window.onInit) == 'function') {
        window.onInit();
      }
    },

    setDisplayTags: function(displayTags) {
      window.displayTags = displayTags;
    },

    setData: function(dataKey, data, {done, error} = {}) {
      post('set', {dataKey: key(dataKey), data: String(data === undefined || data === null ? '' : data)},
        function(reply) {
          respond({done: function() { if (typeof (done) == 'function') { done(true); } }, error: error}, reply);
        });
    },

    notifyHost: function(dataKey) {
      post('notify', {dataKey: key(dataKey)});
    },

    makeRequest: function(path, {type, headers, data, done, error} = {}) {
      post('request', {path: path, options: {type: type, headers: headers, data: data}},
        function(reply) { respond({done: done, error: error}, reply); });
    },

    setCriteria: function(metric, value, ttl) {
      post('criteria', {dataKey: key(metric), data: {metric: key(metric), value: value || null, ttl: ttl}});
    },
  };

  // Capture console logs and report out.
  var log = console.log;
  console.log = function() {
    log.apply(this, Array.prototype.slice.call(arguments));
    try {
      post('log', {data: Array.prototype.slice.call(arguments)});
    } catch (e) {
      // Arguments not serialisable, ignore.
    }
  };

  // Surface connector exceptions to the player.
  window.onerror = function(message, source, line) {
    post('error', {message: String(message), line: line});
  };

  // Say when we're loaded: the parameters go in, the connector starts.
  window.addEventListener('load', function() {
    window.xiboDC.setDisplayTags(__TAGS__);
    window.xiboDC.initialise(DATASET_ID, __PARAMS__);
  });

  return mainLib;
})();
"#;

pub fn host_page(data_set_id: i64, params: &str, script: &str,
                 tags: &BTreeMap<String, String>) -> String {
    let bridge = BRIDGE
        .replace("__ID__", &data_set_id.to_string())
        .replace("__TAGS__", &serde_json::to_string(tags).expect("tags as json"))
        .replace("__PARAMS__", &serde_json::to_string(params).expect("params as json"));
    format!("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Xibo Data Connector</title></head>\
             <body><script type=\"text/javascript\">{}</script>\
             <script type=\"text/javascript\">{}</script></body></html>",
            escape_script_content(&bridge), escape_script_content(script))
}

fn json_response(value: Value) -> ResponseBox {
    Response::from_data(value.to_string().into_bytes())
        .with_header(Header::from_bytes(b"Content-Type", b"application/json").unwrap())
        .boxed()
}

/// `GET /dc/host/<dataSetId>.html`
pub fn host_page_response(rt: &DcRuntime, path: &str) -> ResponseBox {
    let page = path.strip_prefix("/dc/host/").and_then(|p| p.strip_suffix(".html"))
        .and_then(|id| id.parse::<i64>().ok())
        .and_then(|id| rt.host_page(id));
    match page {
        Some(html) => Response::from_data(html.into_bytes())
            .with_header(Header::from_bytes(b"Content-Type", b"text/html; charset=utf-8").unwrap())
            .with_header(Header::from_bytes(b"Cache-Control", b"no-store").unwrap())
            .boxed(),
        None => Response::empty(404).boxed(),
    }
}

fn read_json(req: &mut Request) -> Result<Value> {
    let mut body = String::new();
    req.as_reader().read_to_string(&mut body).context("reading data connector request body")?;
    serde_json::from_str(&body).with_context(|| format!("parsing data connector request body: {body:?}"))
}

/// A data key: text, or a number (a connector often uses its own `dataSetId`).
fn key_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `POST /dc/<set|notify|criteria|log|error>`
pub fn handle_bridge(rt: &DcRuntime, action: &str, req: &mut Request) -> Result<ResponseBox> {
    let body = read_json(req)?;
    let id = body.get("dataSetId").and_then(Value::as_i64).context("missing numeric dataSetId")?;
    let key = key_of(body.get("dataKey"));
    match action {
        "set" => {
            let data = body.get("data").and_then(Value::as_str).unwrap_or("").to_string();
            Ok(json_response(rt.set_data(Instant::now(), id, &key, data).to_json()))
        }
        "notify" => {
            if !key.is_empty() {
                rt.notify(&key);
            }
            Ok(json_response(json!({})))
        }
        "criteria" => {
            let data = body.get("data").unwrap_or(&Value::Null);
            let metric = data.get("metric").and_then(Value::as_str).unwrap_or("");
            let value = data.get("value").filter(|v| !v.is_null());
            if let (false, Some(value)) = (metric.is_empty(), value) {
                let ttl = data.get("ttl").and_then(|t| t.as_i64().or_else(|| t.as_str()?.parse().ok()))
                    .unwrap_or(DEFAULT_CRITERIA_TTL);
                rt.criteria(metric.to_string(), text_of(value), ttl);
            }
            Ok(json_response(json!({})))
        }
        "log" => {
            let parts: Vec<String> = body.get("data").and_then(Value::as_array)
                .map(|a| a.iter().map(text_of).collect()).unwrap_or_default();
            log::debug!("[data connector {id}] {}", parts.join(" "));
            Ok(json_response(json!({})))
        }
        "error" => {
            let message = body.get("message").map(text_of).unwrap_or_default();
            log::error!("[data connector {id}] {message} (line {})",
                        body.get("line").map(text_of).unwrap_or_default());
            Ok(json_response(json!({})))
        }
        other => bail!("unknown data connector action {other:?}"),
    }
}

/// An HTTP request made for a connector, from here rather than from the
/// browser: not subject to CORS.
pub fn proxy_request(path: &str, method: &str, headers: &[(String, String)],
                     data: Option<String>) -> Reply {
    let result = (|| -> Result<Reply> {
        let mut builder = ureq::http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder.body(data.unwrap_or_default()).context("invalid HTTP request")?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .tls_config(crate::tls::http_config(false))
            .build().into();
        let mut response = agent.run(request).context("making HTTP request")?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().context("reading the response")?;
        Ok(Reply { success: (200..300).contains(&status), status, data: body })
    })();
    result.unwrap_or_else(|e| Reply::failed(0, format!("{e:#}")))
}

/// `POST /dc/request`, with `{path, options: {type, headers: [{key, value}], data}}`.
/// Can take as long as the request does: the server runs it on its own thread.
pub fn handle_request(req: &mut Request) -> ResponseBox {
    let reply = read_json(req).map(|body| {
        let path = body.get("path").and_then(Value::as_str).unwrap_or("");
        let options = body.get("options").unwrap_or(&Value::Null);
        let method = options.get("type").and_then(Value::as_str).filter(|s| !s.is_empty())
            .unwrap_or("GET").to_uppercase();
        let mut headers: Vec<(String, String)> = options.get("headers").and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|h| Some((
                h.get("key")?.as_str()?.to_string(), h.get("value").map(text_of)?))).collect())
            .unwrap_or_default();
        let mut data = None;
        if let Some(d) = options.get("data").filter(|d| !d.is_null()) {
            if method != "GET" && method != "HEAD" {
                data = Some(text_of(d));
                if !headers.iter().any(|(k, _)| k == "Content-Type") {
                    headers.push(("Content-Type".into(), "application/json;charset=UTF-8".into()));
                }
            }
        }
        proxy_request(path, &method, &headers, data)
    }).unwrap_or_else(|e| Reply::failed(0, format!("{e:#}")));
    json_response(reply.to_json())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn payload(id: i64, md5: &str) -> ConnectorPayload {
        ConnectorPayload { data_set_id: id, schedule_id: 1, data_params: "city%3DMassa+Carrara".into(),
                           js: format!("dataSet_{id}.js"), md5: md5.into() }
    }

    fn attached() -> (DcRuntime, Receiver<ToGui>) {
        let rt = DcRuntime::new();
        let (tx, rx) = unbounded();
        rt.attach(tx);
        (rt, rx)
    }

    fn url(id: i64) -> String { format!("http://127.0.0.1:1/dc/host/{id}.html") }

    fn drain(rx: &Receiver<ToGui>) -> Vec<String> {
        rx.try_iter().map(|m| match m {
            ToGui::DataConnectorStart(id, u) => format!("start {id} {u}"),
            ToGui::DataConnectorStop(id) => format!("stop {id}"),
            ToGui::NotifyData(k) => format!("notify {k}"),
            _ => "other".to_string(),
        }).collect()
    }

    fn start(rt: &DcRuntime, id: i64) {
        rt.sync(vec![payload(id, "m")], |_| Some("var x = 1;".into()), url);
    }

    /// Writes the page of a sample connector, to try it in a real browser:
    /// `AREXIBO_DC_PAGE_OUT=/tmp/page.html cargo test write_sample_host_page -- --ignored`.
    #[test]
    #[ignore]
    fn write_sample_host_page() {
        let script = std::env::var("AREXIBO_DC_SCRIPT").ok()
            .and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let out = std::env::var("AREXIBO_DC_PAGE_OUT").expect("AREXIBO_DC_PAGE_OUT");
        // AREXIBO_DC_TAGS="lat=44.0355,city=Massa Carrara"
        let tags = std::env::var("AREXIBO_DC_TAGS").unwrap_or_default().split(',')
            .filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        // AREXIBO_DC_PARAMS=<the string as the CMS sends it, urlencoded>
        let params = std::env::var("AREXIBO_DC_PARAMS").map(|p| decode_data_params(&p))
            .unwrap_or_else(|_| "city=Massa Carrara&n=2".into());
        std::fs::write(out, host_page(1, &params, &script, &tags)).unwrap();
    }

    #[test]
    fn a_connector_is_started_once_with_its_page() {
        let (rt, rx) = attached();
        start(&rt, 16);
        assert_eq!(drain(&rx), ["start 16 http://127.0.0.1:1/dc/host/16.html"]);
        let page = rt.host_page(16).expect("page registered");
        assert!(page.contains("var x = 1;") && page.contains("window.xiboDC"));
        start(&rt, 16);
        assert!(drain(&rx).is_empty(), "unchanged: nothing to do");
        assert_eq!(rt.active_ids(), [16]);
    }

    #[test]
    fn a_connector_whose_script_is_not_available_is_tried_again() {
        let (rt, rx) = attached();
        rt.sync(vec![payload(16, "m")], |_| None, url);
        assert!(drain(&rx).is_empty());
        assert!(rt.active_ids().is_empty() && rt.host_page(16).is_none());
        start(&rt, 16);
        assert_eq!(drain(&rx).len(), 1);
    }

    #[test]
    fn a_stopped_connector_takes_its_values_and_page_with_it() {
        let (rt, rx) = attached();
        start(&rt, 16);
        assert_eq!(rt.set_data(Instant::now(), 16, "b", "1".into()), Reply::ok());
        rt.set_data(Instant::now(), 16, "a", "1".into());
        drain(&rx);
        for _ in 0..3 {
            rt.sync(vec![], |_| None, url);
        }
        assert_eq!(drain(&rx), ["stop 16", "notify a", "notify b"]);
        assert!(rt.local_data().lock().unwrap().is_empty());
        assert!(rt.host_page(16).is_none());
    }

    #[test]
    fn a_stopped_connector_keeps_its_page_until_the_third_miss() {
        let (rt, rx) = attached();
        start(&rt, 16);
        drain(&rx);
        rt.sync(vec![], |_| None, url);
        rt.sync(vec![], |_| None, url);
        assert!(drain(&rx).is_empty() && rt.host_page(16).is_some());
    }

    #[test]
    fn stop_all_clears_everything() {
        let (rt, rx) = attached();
        start(&rt, 16);
        start(&rt, 17);
        rt.set_data(Instant::now(), 16, "k", "v".into());
        drain(&rx);
        rt.stop_all();
        assert!(rt.active_ids().is_empty() && rt.local_data().lock().unwrap().is_empty());
        assert!(drain(&rx).contains(&"stop 16".to_string()));
    }

    #[test]
    fn a_value_is_stored_at_once_and_rejections_carry_their_status() {
        let (rt, _rx) = attached();
        let t0 = Instant::now();
        assert_eq!(rt.set_data(t0, 16, "k", "v".into()), Reply::ok());
        assert_eq!(rt.local_data().lock().unwrap().get("k").map(String::as_str), Some("v"));
        assert_eq!(rt.set_data(t0, 16, "", "v".into()), Reply::failed(400, "A dataKey is required"));
    }

    #[test]
    fn writes_within_the_interval_store_the_latest_when_due() {
        let (rt, _rx) = attached();
        let t0 = Instant::now();
        rt.set_data(t0, 16, "k", "1".into());
        assert_eq!(rt.set_data(t0 + Duration::from_millis(100), 16, "k", "2".into()), Reply::ok());
        assert_eq!(rt.set_data(t0 + Duration::from_millis(200), 16, "k", "3".into()), Reply::ok());
        let value = || rt.local_data().lock().unwrap().get("k").cloned();
        assert_eq!(value().as_deref(), Some("1"), "not stored yet");
        rt.tick(t0 + Duration::from_millis(499));
        assert_eq!(value().as_deref(), Some("1"));
        rt.tick(t0 + Duration::from_millis(500));
        assert_eq!(value().as_deref(), Some("3"), "only the latest value");
    }

    #[test]
    fn a_notification_waits_for_a_pending_write() {
        let (rt, rx) = attached();
        let t0 = Instant::now();
        rt.set_data(t0, 16, "k", "1".into());
        rt.notify("k");
        assert_eq!(drain(&rx), ["notify k"], "nothing pending: at once");
        rt.set_data(t0 + Duration::from_millis(100), 16, "k", "2".into());
        rt.notify("k");
        assert!(drain(&rx).is_empty(), "a write is pending: wait");
        rt.tick(t0 + Duration::from_millis(500));
        assert_eq!(drain(&rx), ["notify k"], "told once the value is there");
    }

    #[test]
    fn a_data_key_may_be_text_or_a_number() {
        assert_eq!(key_of(Some(&json!("weather"))), "weather");
        assert_eq!(key_of(Some(&json!(16))), "16");
        assert_eq!(key_of(Some(&json!(-3))), "-3");
        for empty in [None, Some(&Value::Null), Some(&json!(true)), Some(&json!([1]))] {
            assert_eq!(key_of(empty), "");
        }
    }

    #[test]
    fn the_page_makes_every_key_text_before_sending_it() {
        let page = host_page(16, "", "", &BTreeMap::new());
        for call in ["post('set', {dataKey: key(dataKey)", "post('notify', {dataKey: key(dataKey)",
                     "post('criteria', {dataKey: key(metric), data: {metric: key(metric)"] {
            assert!(page.contains(call), "{call}");
        }
    }

    #[test]
    fn criteria_go_to_the_mainloop() {
        let (rt, _rx) = attached();
        rt.criteria("weather".into(), "rain".into(), 60);
        assert_eq!(rt.events().try_recv().unwrap(),
                   DcEvent::Criteria { metric: "weather".into(), value: "rain".into(), ttl: 60 });
    }

    #[test]
    fn the_page_carries_the_id_the_decoded_parameters_and_the_script() {
        let page = host_page(16, "city=Massa Carrara&n=2", "function onInit() {}", &BTreeMap::new());
        assert!(page.contains("var DATASET_ID = 16;"));
        assert!(page.contains(r#"initialise(DATASET_ID, "city=Massa Carrara&n=2")"#));
        assert!(page.contains("function onInit() {}"));
        assert!(!page.contains("__ID__") && !page.contains("__PARAMS__"));
    }

    fn tags(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_page_gives_the_display_tags_to_the_connector_before_it_starts() {
        let page = host_page(16, "", "", &tags(&[("lat", "44.0355"), ("city", "Massa \"Carrara\"")]));
        let set = page.find(r#"setDisplayTags({"city":"Massa \"Carrara\"","lat":"44.0355"})"#).expect("tags set");
        assert!(set < page.find("xiboDC.initialise(DATASET_ID").expect("initialise"), "tags first");
        assert!(host_page(16, "", "", &BTreeMap::new()).contains("setDisplayTags({})"));
    }

    #[test]
    fn a_tag_cannot_break_out_of_the_page() {
        let page = host_page(1, "", "", &tags(&[("x", "</script><script>alert(1)</script>")]));
        assert_eq!(page.matches("</script>").count(), 2);
    }

    #[test]
    fn a_started_connector_gets_the_tags_known_at_that_moment() {
        let (rt, _rx) = attached();
        rt.set_display_tags(tags(&[("lat", "44.0355")]));
        start(&rt, 16);
        assert!(rt.host_page(16).unwrap().contains(r#"setDisplayTags({"lat":"44.0355"})"#));
    }

    #[test]
    fn a_script_cannot_close_its_own_element() {
        let page = host_page(1, "", "var s = '</script><b>'; // <!-- x\nvar t = '</SCRIPT>';", &BTreeMap::new());
        assert_eq!(page.matches("</script>").count(), 2, "only the two the page itself closes");
        assert!(page.contains(r"<\/script><b>") && page.contains(r"<\/SCRIPT>") && page.contains(r"<\!-- x"));
    }

    #[test]
    fn parameters_cannot_break_out_of_the_page() {
        let page = host_page(1, "x=</script><script>alert(1)</script>", "", &BTreeMap::new());
        assert_eq!(page.matches("</script>").count(), 2);
    }

    #[test]
    fn escaping_leaves_ordinary_text_alone() {
        assert_eq!(escape_script_content("caff\u{e8} \u{1F600} </div> a<b"), "caff\u{e8} \u{1F600} </div> a<b");
    }

    /// What the mock server saw: method, url, headers, body.
    type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>, String)>>>;

    /// A tiny HTTP server: records each request, answers by path.
    fn mock_server() -> (u16, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tiny_http::Server::from_listener(listener, None).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let headers = req.headers().iter()
                    .map(|h| (h.field.to_string().to_lowercase(), h.value.to_string())).collect();
                log.lock().unwrap().push((req.method().to_string(), req.url().to_string(), headers, body.clone()));
                let (status, text) = match req.url() {
                    "/ok" => (200, "hello".to_string()),
                    "/created" => (201, format!("got {body}")),
                    _ => (404, "nope".to_string()),
                };
                let _ = req.respond(Response::from_string(text).with_status_code(status));
            }
        });
        (port, seen)
    }

    #[test]
    fn a_request_succeeds_with_its_status_and_body() {
        let (port, _seen) = mock_server();
        let reply = proxy_request(&format!("http://127.0.0.1:{port}/ok"), "GET", &[], None);
        assert_eq!(reply, Reply { success: true, status: 200, data: "hello".into() });
    }

    #[test]
    fn a_non_2xx_answer_is_a_failure_with_its_status_and_body() {
        let (port, _seen) = mock_server();
        let reply = proxy_request(&format!("http://127.0.0.1:{port}/missing"), "GET", &[], None);
        assert_eq!(reply, Reply { success: false, status: 404, data: "nope".into() });
    }

    #[test]
    fn a_connection_failure_is_a_failure_with_status_zero() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let reply = proxy_request(&format!("http://127.0.0.1:{port}/"), "GET", &[], None);
        assert!(!reply.success && reply.status == 0 && !reply.data.is_empty(), "{reply:?}");
    }

    #[test]
    fn a_post_sends_its_headers_and_body() {
        let (port, seen) = mock_server();
        let reply = proxy_request(&format!("http://127.0.0.1:{port}/created"), "POST",
                                  &[("X-Token".into(), "abc".into())], Some("payload".into()));
        assert_eq!(reply, Reply { success: true, status: 201, data: "got payload".into() });
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "POST");
        assert!(seen[0].2.contains(&("x-token".to_string(), "abc".to_string())));
        assert_eq!(seen[0].3, "payload");
    }
}
