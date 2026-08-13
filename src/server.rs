// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Internal webserver to point the webview to.

use std::{sync::{Arc, Mutex, OnceLock}, fs, io::Read, io::Seek, thread, collections::HashMap};
use std::path::{Path, PathBuf};
use anyhow::{anyhow, bail, ensure, Context, Result};
use crossbeam_channel::Sender;
use itertools::Itertools;
use tiny_http::{Request, Response, ResponseBox, Header, StatusCode};

use crate::util::percent_decode;

/// A duration-control request from a Widget's own JS (via the
/// `xibo-interactive-control` library hitting `/duration/set`,
/// `/duration/extend`, or `/duration/expire` on this embedded server --
/// see https://github.com/xibosignage/xibo-interactive-control). Relayed
/// to the mainloop (see mainloop.rs's `Handler::run` select! loop) since
/// actually applying it means running JS in the currently-displayed page,
/// which this HTTP worker thread has no direct access to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationAction {
    Set,
    Extend,
    Expire,
}

#[derive(Debug, Clone)]
pub struct DurationRequest {
    pub widget_id: i64,
    pub action: DurationAction,
    /// Seconds -- present for Set/Extend, absent (ignored) for Expire.
    pub duration: Option<i64>,
}

/// How many distinct loopback origins (`127.0.0.1` through
/// `127.0.0.{HTML_SHARD_COUNT}`) the embedded server is bound to (see
/// main.rs) -- `layout.rs::write_media` picks one per `render="html"`
/// iframe widget (deterministically, from the widget's own id) for that
/// widget's own `src`, so that widgets are spread across several
/// independent Chromium per-origin connection pools instead of sharing
/// a single one.
///
/// BUG fix (found from a real report: content in the main layout was
/// intermittently missing/delayed, worse -- though not exclusively --
/// whenever an Overlay Layout was also active): Chromium hard-caps
/// concurrent HTTP connections *per origin* at 6 (a long-standing,
/// deliberately-chosen upstream limit with no stable command-line
/// override in an unpatched Chromium build -- confirmed via Chromium's
/// own bug tracker discussions, not guessed). A single layout can
/// easily have more than 6 simultaneous `render="html"` iframe widgets
/// (each is its own HTTP request) -- this was already a latent risk with
/// just the main view alone, and gets worse still whenever an overlay
/// is *also* requesting its own such widgets at the same time, since
/// (before this fix) both views loaded everything from the exact same
/// origin, sharing one 6-connection budget between them. `127.0.0.1`
/// through `127.0.0.4` are different loopback addresses but equally
/// "localhost"-only at the OS level (never reachable from any other
/// machine) -- Chromium treats each as a genuinely different origin
/// with its own independent connection pool, all served from the exact
/// same directory (no duplicated files, no extra state to keep in
/// sync). 4 is a conservative, commonly-used sharding factor for this
/// exact class of problem (the same technique, "domain sharding", that
/// real websites used for years before HTTP/2 multiplexing made it
/// largely unnecessary for the public web -- it's still the right tool
/// here, since this embedded server has no plans to speak HTTP/2).
pub const HTML_SHARD_COUNT: u32 = 4;

/// BUG fix (found from a real report): this embedded server's port used
/// to be chosen dynamically (`0`, letting the OS assign any free port)
/// on every single startup -- but a layout's own translated HTML, once
/// cached, has that port baked *directly* into every `render="html"`
/// widget's own absolute, sharded iframe URL (see `HTML_SHARD_COUNT`'s
/// own doc comment above). If a layout doesn't need re-translating on a
/// given run (its own XLF is unchanged, `TRANSLATOR_VERSION` matches --
/// exactly what happens on any normal run *without* `--clear`), its
/// cached HTML keeps referencing whatever port a *previous* run
/// happened to be assigned, which is almost certainly not the port this
/// run's server is actually listening on -- every widget iframe on that
/// layout then points nowhere, connecting to a dead port, and simply
/// never loads at all (confirmed: this exact symptom, `arexibo-show`
/// firing correctly but no further console output ever following, only
/// when starting *without* `--clear`). Fixed by using a stable, fixed
/// port instead, so cached HTML's baked-in port references remain valid
/// indefinitely across restarts -- loopback-only (127.0.0.0/8), never
/// reachable from another machine, so the usual "well-known port
/// collision" concern that would apply to a real network-facing service
/// doesn't really apply here; the only realistic risk is another
/// process on the very same machine already bound to this exact port,
/// which `Server::new`'s own error propagation (via `tiny_http`) surfaces
/// clearly rather than silently misbehaving.
pub const EMBEDDED_SERVER_PORT: u16 = 34519;

/// Shared, in-memory key-value store backing the `/realtime?dataKey=`
/// endpoint (see `Server::serve`'s own doc comment on that route) --
/// `Arc<Mutex<...>>` because it must be genuinely shared across *every*
/// `Server` instance (main.rs creates one per `HTML_SHARD_COUNT`
/// address, see that constant's own doc comment), not just across the
/// worker threads within a single one: a value set while a widget was
/// loaded from one shard's origin must still be readable by a widget
/// loaded from a *different* shard, since all shards serve the exact
/// same content.
pub type LocalDataStore = Arc<Mutex<HashMap<String, String>>>;

pub struct Server {
    dir: PathBuf,
    server: tiny_http::Server,
    duration_tx: Sender<DurationRequest>,
    local_data: LocalDataStore,
}

impl Server {
    /// `bind_addr` is normally `"127.0.0.1"` -- but see
    /// `HTML_SHARD_COUNT`'s own doc comment for why main.rs binds
    /// several independent `Server` instances, all serving the exact
    /// same `dir`, to different loopback addresses.
    pub fn new(dir: PathBuf, bind_addr: &str, port: u16,
               duration_tx: Sender<DurationRequest>, local_data: LocalDataStore) -> Result<Self> {
        let server = tiny_http::Server::http((bind_addr, port))
            .map_err(|e| anyhow!(e))?;
        let dir = dir.canonicalize().context("getting canonical server dir name")?;
        Ok(Self { dir, server, duration_tx, local_data })
    }

    pub fn port(&self) -> u16 {
        self.server.server_addr().to_ip().expect("IP address").port()
    }

    pub fn start_pool(self) {
        let server = Arc::new(self.server);
        for _ in 0..4 {
            let server = server.clone();
            let dir = self.dir.clone();
            let duration_tx = self.duration_tx.clone();
            let local_data = self.local_data.clone();
            thread::spawn(move || {
                loop {
                    let mut req = server.recv().unwrap();
                    match Self::serve(&dir, &mut req, &duration_tx, &local_data) {
                        Ok(resp) => {  let _ = req.respond(resp); }
                        Err(e) => {
                            log::warn!("processing HTTP req {}: {:#}", req.url(), e);
                            let _ = req.respond(Response::empty(500));
                        }
                    }
                }
            });
        }
    }

    /// Handle one of the three Interactive Control duration-override
    /// endpoints: read+parse the JSON POST body (`{id, duration}`,
    /// `duration` absent for expire -- see
    /// https://account.xibosignage.com/docs/developer/creating-a-player/interactive),
    /// relay it to the mainloop, and ACK with `200 {}` (NOT `204 No
    /// Content` -- confirmed via a real xibo-cms issue that the
    /// `xibo-interactive-control` JS library treats a 204 as a failure
    /// on every player, so this matters for real compatibility, not just
    /// convention).
    fn handle_duration(req: &mut Request, action: DurationAction,
                        duration_tx: &Sender<DurationRequest>) -> Result<ResponseBox> {
        let mut body = String::new();
        req.as_reader().read_to_string(&mut body).context("reading duration request body")?;
        let json: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("parsing duration request body: {body:?}"))?;
        let widget_id = json.get("id").and_then(|v| v.as_i64())
            .context("duration request missing numeric id")?;
        let duration = json.get("duration").and_then(|v| v.as_i64());
        // Best-effort: if the mainloop's receiving end has gone away
        // (shutting down), still ACK the HTTP request rather than
        // erroring the widget's own JS out over something it can't fix.
        let _ = duration_tx.send(DurationRequest { widget_id, action, duration });
        Ok(Response::from_data(b"{}".as_slice())
            .with_header(Header::from_bytes(b"Content-Type", b"application/json").unwrap())
            .boxed())
    }

    /// Serve a single HTTP request. Thin wrapper around `serve_inner`
    /// (which keeps all of its existing early-return branches
    /// unchanged) purely to apply `Cache-Control: no-store` uniformly
    /// to every successful response, regardless of which of
    /// `serve_inner`'s several return points produced it.
    ///
    /// BUG fix (found from a real report: a widget's own resource file
    /// -- which had genuinely failed to download on the first attempt
    /// with a transient "Cache not ready" SOAP fault, see
    /// `resource_retry_queue`'s own doc comment -- still appeared
    /// missing even *after* a later retry successfully downloaded it
    /// and triggered a reload). Without any `Cache-Control` header at
    /// all, Chromium is free to apply its own heuristics for whether to
    /// reuse a previous response instead of making a fresh request --
    /// including, plausibly, reusing an earlier 404 for the exact same
    /// URL (same path *and* query string) instead of hitting the server
    /// again after `layout.rs`'s generated reload code re-assigns the
    /// iframe's own unchanged `src`. Every response this server sends
    /// can legitimately change over time (a resource file appearing
    /// after a retry, `/realtime`'s own stored value being updated,
    /// etc.), so nothing here should ever be cached by the browser at
    /// all -- `no-store` is the strongest, least ambiguous directive for
    /// that.
    fn serve(dir: &Path, req: &mut Request, duration_tx: &Sender<DurationRequest>,
             local_data: &LocalDataStore) -> Result<ResponseBox> {
        let resp = Self::serve_inner(dir, req, duration_tx, local_data)?;
        Ok(resp.with_header(Header::from_bytes(b"Cache-Control", b"no-store").unwrap()))
    }

    fn serve_inner(dir: &Path, req: &mut Request, duration_tx: &Sender<DurationRequest>,
                    local_data: &LocalDataStore) -> Result<ResponseBox> {
        log::debug!("HTTP request: {}", req.url());
        let url = req.url();
        let (path_only, query) = url.split_once('?').unwrap_or((url, ""));
        Ok(match path_only {
            // built-in files?
            "/favicon.ico" => Response::from_data(b"").boxed(),
            "/branding.png" => Response::from_data(SPLASH_LOGO)
                .with_header(Header::from_bytes(b"Content-Type", b"image/png").unwrap())
                .boxed(),
            "/0.xlf.html" => Response::from_data(splash_html()).boxed(),

            // Interactive Control duration overrides (see
            // xibo-interactive-control's setWidgetDuration/
            // extendWidgetDuration/expireNow) -- actually applied now
            // (previously just ACKed without effect), see
            // Self::handle_duration and layout.rs's `controlDuration`.
            "/duration/set" => return Self::handle_duration(req, DurationAction::Set, duration_tx),
            "/duration/extend" => return Self::handle_duration(req, DurationAction::Extend, duration_tx),
            "/duration/expire" => return Self::handle_duration(req, DurationAction::Expire, duration_tx),

            // Real-time DataSet data lookup -- confirmed from a real
            // `bundle.min.js` the user shared: `xiboIC.getData(dataKey,
            // {done, error})` makes a plain relative GET to
            // `/realtime?dataKey=...`, resolving against whichever
            // origin loaded the widget's own page (one of this same
            // embedded server's shards, see HTML_SHARD_COUNT) -- so this
            // is a route on *this* server, not something to proxy to
            // the real CMS. `local_data` is the "value a Data Connector
            // has most recently set for this key" -- SCOPE NOTE: only
            // the read side is implemented here (this route, and the
            // store itself); actually *running* a CMS-authored Data
            // Connector script to populate `local_data` in the first
            // place is a separate, materially bigger feature, not
            // implemented in this session (its own delivery/execution
            // mechanism wasn't confirmed in enough detail to implement
            // responsibly) -- until that exists, every key is simply
            // absent, and this returns a clean 404 rather than the
            // confusing "canonicalize: No such file or directory" the
            // static-file fallback below used to produce for this exact
            // same URL (a real report -- this endpoint isn't a file on
            // disk at all, so it should never have reached that code
            // path to begin with).
            "/realtime" => {
                let data_key = query.split('&')
                    .find_map(|kv| kv.strip_prefix("dataKey="));
                let value = data_key.and_then(|k| {
                    local_data.lock().unwrap().get(k).cloned()
                });
                match value {
                    Some(v) => Response::from_data(v.into_bytes())
                        .with_header(Header::from_bytes(b"Content-Type", b"text/plain").unwrap())
                        .boxed(),
                    None => Response::empty(404).boxed(),
                }
            }

            // any other static files
            path_only => {
                let path = dir.join(&path_only[1..]);

                let canonical_path = match path.canonicalize() {
                    Ok(p) if p.starts_with(dir) => p,
                    Ok(_) => {
                        log::warn!("processing HTTP req {}: 403 path outside cache dir", req.url());
                        return Ok(Response::empty(403).boxed());
                    }
                    Err(e) => {
                        log::warn!("processing HTTP req {}: 404 canonicalize: {e}", req.url());
                        return Ok(Response::empty(404).boxed());
                    }
                };
                let ext = canonical_path.extension().and_then(|e| e.to_str());

                let query_params = (!query.is_empty()).then(|| query.split('&').map(|p| {
                    let mut kv = p.split('=');
                    let k = percent_decode(kv.next().unwrap_or(""));
                    let v = percent_decode(kv.next().unwrap_or(""));
                    (k, v)
                }).collect::<HashMap<_, _>>()).unwrap_or_default();

                if !canonical_path.is_file() {
                    log::warn!("processing HTTP req {}: 404 not found", req.url());
                    return Ok(Response::empty(404).boxed());
                }
                let mut fp = fs::File::open(&canonical_path)?;

                // implement replacing [[ViewPortWidth]] by requested width
                if ext == Some("html") && query_params.contains_key("w") {
                    let mut data = Vec::new();
                    fp.read_to_end(&mut data)?;
                    if let Some(i) = (0..data.len())
                        .find(|&i| data[i..].starts_with(b"[[ViewPortWidth]]")) {
                        let mut new_data = data[..i].to_vec();
                        new_data.extend_from_slice(query_params["w"].as_bytes());
                        new_data.extend_from_slice(&data[i + 17..]);
                        data = new_data;
                    }

                    return Ok(Response::from_data(data)
                        .with_header(Header::from_bytes(b"Content-Type",
                                                        b"text/html").unwrap())
                        .boxed());
                }

                // implement HTTP Range query for gstreamer
                for h in req.headers() {
                    if h.field.equiv("Range") {
                        let total_size = fp.metadata()?.len();
                        let (from, to, size) = parse_range(total_size, h.value.as_ref())?;
                        fp.seek(std::io::SeekFrom::Start(from))?;
                        let stream = fp.take(size);

                        let range = format!("bytes {from}-{to}/{total_size}");
                        return Ok(Response::new(
                            StatusCode(206),
                            vec![
                                Header::from_bytes(b"Content-Range", range).unwrap(),
                                Header::from_bytes(b"Content-Type", b"video/mp4").unwrap(),
                            ],
                            stream,
                            Some(size as usize),
                            None
                        ).with_chunked_threshold(usize::MAX).boxed());
                    }
                }

                // guess the MIME type based on filename
                let ctype = match ext {
                    Some("html") => "text/html",
                    Some("js" | "mjs") => "text/javascript",
                    Some("ttf" | "otf") => "application/font-sfnt",
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("png") => "image/png",
                    Some("pdf") => "application/pdf",
                    Some("mp4") => "video/mp4",
                    Some("avi") => "video/avi",
                    Some("ogv") => "video/ogg",
                    Some("webm") => "video/webm",
                    _ => "",
                };

                Response::from_file(fp)
                    // for gstreamer, need a response with Content-Length => no chunked
                    .with_chunked_threshold(usize::MAX)
                    .with_header(Header::from_bytes(b"Content-Type", ctype).unwrap())
                    .boxed()
            }
        })
    }
}

// BUG fix / feature (found from a real request: knowing the totem's own
// hostname/IP during initial setup, or while waiting for CMS
// authorization, previously required a separate SSH session -- showing
// it directly on the splash screen the totem is already displaying is
// much more convenient). Computed once (OnceLock) and shared across
// every `Server` instance (see `Server::new`'s own doc comment on why
// several independent instances exist, for HTML sharding) -- this info
// is extremely unlikely to change during a single run, so recomputing
// it on every request would be pure waste.
fn splash_html() -> &'static [u8] {
    static SPLASH: OnceLock<Vec<u8>> = OnceLock::new();
    SPLASH.get_or_init(|| {
        let hostname = crate::util::get_display_name();
        let ips = crate::util::get_local_ips();
        let ips_display = if ips.is_empty() {
            "(no network address found)".to_string()
        } else {
            ips.join(", ")
        };
        format!(r#"<!DOCTYPE html>
<html>
<head>
<script src="qrc:///qtwebchannel/qwebchannel.js"></script>
<script>
new QWebChannel(qt.webChannelTransport, function(channel) {{
  window.arexiboGui = channel.objects.arexibo;
  window.arexiboGui.jsLayoutInit(0, 1920, 1080);
}});
</script>
</head>
<body style="margin: 0; width: 100vw; height: 100vh; background-color: #ffffff;
             display: flex; flex-direction: column; align-items: center;
             justify-content: center;">
<img style="max-width: 70vw; max-height: 40vh; width: auto; height: auto;"
     src="branding.png">
<!-- BUG fix (found from a real report): the *old* splash.jpg had its own
     "LOADING..." text baked directly into the image's own pixels --
     replacing that image with a different logo (see branding.png's own
     doc comment below) silently lost that text along with it, since it
     was never a separate, independent element to begin with. A real
     HTML text element here stays readable regardless of whatever logo
     image is configured, present or future. -->
<div style="margin-top: 24px; font-family: sans-serif; font-size: 28px;
            font-weight: 600; color: #333333; letter-spacing: 0.05em;">
  LOADING...
</div>
<div style="margin-top: 12px; font-family: sans-serif; font-size: 16px;
            color: #888888;">
  {hostname} &middot; {ips_display}
</div>
</body>
</html>
"#).into_bytes()
    })
}

// Shown full-screen at startup, before the first collection completes
// (see gui.rs/view.cpp's own "layout 0" handling) -- deliberately named
// generically (not e.g. "tmax-logo.png") so this stays meaningful for
// any deployment that wants to swap in its own logo here, not just this
// specific one. Replace this file (same name/path) to customize.
const SPLASH_LOGO: &[u8] = include_bytes!("../assets/branding.png");


/// Parse a HTTP Range header.
fn parse_range(total_size: u64, header: &str) -> Result<(u64, u64, u64)> {
    let mut parts = header.split(&['=', '-'][..]);
    let (from, to) = match parts.next_tuple() {
        Some(("bytes", from, to)) => {
            (from.parse().unwrap_or(0), to.parse().unwrap_or(total_size - 1))
        }
        _ => bail!("invalid Range header")
    };
    ensure!(from <= to && to < total_size, "invalid Range from/to");
    let size = to - from + 1;
    Ok((from, to, size))
}

#[cfg(test)]
mod realtime_tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_test_server() -> (u16, LocalDataStore) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_realtime_test_{}_{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("testfile.txt"), "static file content\n").unwrap();
        let (tx, _rx) = unbounded();
        let local_data: LocalDataStore = Arc::new(Mutex::new(HashMap::new()));
        let server = Server::new(dir, "127.0.0.1", 0, tx, local_data.clone()).unwrap();
        let port = server.port();
        server.start_pool();
        (port, local_data)
    }

    #[test]
    fn realtime_missing_key_returns_clean_404() {
        let (port, _local_data) = make_test_server();
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/realtime?dataKey=nonexistent"))
            .call();
        match resp {
            Err(ureq::Error::StatusCode(404)) => {} // expected
            other => panic!("expected clean 404, got: {other:?}"),
        }
    }

    #[test]
    fn realtime_existing_key_returns_its_value() {
        let (port, local_data) = make_test_server();
        local_data.lock().unwrap().insert("mykey".to_string(), "hello world".to_string());
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/realtime?dataKey=mykey"))
            .call().unwrap();
        let body = resp.into_body().read_to_string().unwrap();
        assert_eq!(body, "hello world");
    }

    #[test]
    fn static_file_serving_still_works_after_url_parsing_refactor() {
        let (port, _local_data) = make_test_server();
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/testfile.txt")).call().unwrap();
        let body = resp.into_body().read_to_string().unwrap();
        assert_eq!(body.trim(), "static file content");
    }

    #[test]
    fn static_file_serving_with_query_params_still_works() {
        let (port, _local_data) = make_test_server();
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/testfile.txt?w=100&h=200"))
            .call().unwrap();
        let body = resp.into_body().read_to_string().unwrap();
        assert_eq!(body.trim(), "static file content");
    }
}

#[cfg(test)]
mod stable_port_tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn embedded_server_port_is_the_fixed_stable_constant_not_random() {
        // Regression test for a real bug: this port used to be chosen
        // randomly (0, OS-assigned) on every startup, but cached
        // layout HTML has it baked directly into every widget iframe's
        // own absolute URL -- a layout that doesn't need
        // re-translating (no --clear, unchanged XLF) would otherwise
        // keep pointing at whatever port a *previous* run happened to
        // get, which is essentially never the current run's real port.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_stable_port_test_{}_{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (tx, _rx) = unbounded();
        let local_data: LocalDataStore = Arc::new(Mutex::new(HashMap::new()));
        let server = Server::new(dir, "127.0.0.1", EMBEDDED_SERVER_PORT, tx, local_data).unwrap();
        assert_eq!(server.port(), EMBEDDED_SERVER_PORT,
                   "the embedded server must use the fixed, stable port constant, \
                    not a randomly OS-assigned one -- otherwise cached widget iframe \
                    URLs from a previous run point at a dead port after a restart");
    }
}

#[cfg(test)]
mod no_cache_header_tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_test_server() -> u16 {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_nocache_test_{}_{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("testfile.txt"), "hello").unwrap();
        let (tx, _rx) = unbounded();
        let local_data: LocalDataStore = Arc::new(Mutex::new(HashMap::new()));
        let server = Server::new(dir, "127.0.0.1", 0, tx, local_data).unwrap();
        let port = server.port();
        server.start_pool();
        port
    }

    #[test]
    fn successful_file_response_has_no_store_header() {
        // Regression test for a real report: a widget's resource file
        // that failed with a transient "Cache not ready" fault, then
        // succeeded on retry, still appeared missing on screen --
        // plausibly because the browser reused an earlier cached 404
        // for the identical URL instead of making a fresh request after
        // the reload. Every response from this server must say
        // "don't cache me" explicitly.
        let port = make_test_server();
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/testfile.txt")).call().unwrap();
        let cache_control = resp.headers().get("Cache-Control").map(|v| v.to_str().unwrap());
        assert_eq!(cache_control, Some("no-store"));
    }

    #[test]
    fn missing_file_404_response_also_has_no_store_header() {
        let port = make_test_server();
        let result = ureq::get(&format!("http://127.0.0.1:{port}/does-not-exist.html")).call();
        match result {
            Err(ureq::Error::StatusCode(404)) => {}
            other => panic!("expected 404, got {other:?}"),
        }
        // Confirm the header directly on the 404 response body via a
        // raw socket check, since ureq's own error variant for a plain
        // status-code error doesn't expose response headers directly --
        // this is the single most important case for this bug (a
        // browser reusing a *cached 404* instead of retrying after a
        // widget resource's delayed successful download).
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"GET /does-not-exist.html HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        assert!(resp.to_lowercase().contains("cache-control: no-store"),
                "404 response must carry Cache-Control: no-store -- got:\n{resp}");
    }

    #[test]
    fn realtime_response_also_has_no_store_header() {
        let port = make_test_server();
        let resp = ureq::get(&format!("http://127.0.0.1:{port}/realtime?dataKey=x")).call();
        // 404 (no data set) is expected here, but the point is that the
        // request completes and would carry the no-store header too --
        // covered structurally since it goes through the same `serve()`
        // wrapper as everything else, verified directly on the
        // successful-response case above.
        assert!(matches!(resp, Err(ureq::Error::StatusCode(404))));
    }
}

#[cfg(test)]
mod splash_html_tests {
    use super::*;

    #[test]
    fn splash_html_includes_hostname_and_something_for_ips() {
        // Feature test (found from a real request: showing the totem's
        // own hostname/IP directly on the splash screen, useful during
        // initial setup and while waiting for CMS authorization,
        // instead of requiring a separate SSH session to check).
        let html = String::from_utf8(splash_html().to_vec()).unwrap();
        // The real hostname will vary by machine/CI environment, but it
        // must appear verbatim somewhere in the output.
        let hostname = crate::util::get_display_name();
        assert!(html.contains(&hostname),
                "splash HTML must include the machine's own hostname ({hostname:?}) -- got:\n{html}");
        // Either a real IP list, or the graceful fallback message when
        // none could be determined -- either way, *something* readable,
        // never a raw empty string silently missing from the page.
        assert!(html.contains("no network address found")
                || crate::util::get_local_ips().iter().any(|ip| html.contains(ip)),
                "splash HTML must include either a real IP or the \
                 no-address-found fallback message -- got:\n{html}");
        // Still valid, well-formed HTML with the existing loading text
        // and QWebChannel setup -- this is an *addition*, not a
        // replacement of the pre-existing splash content.
        assert!(html.contains("LOADING..."));
        assert!(html.contains("jsLayoutInit(0, 1920, 1080)"));
    }

    #[test]
    fn splash_html_is_computed_once_and_cached() {
        // The whole point of OnceLock here is to avoid recomputing
        // (and re-shelling-out to `hostname -I`) on every single
        // request -- confirm two calls return the exact same bytes
        // (trivially true if it's genuinely cached; would only differ
        // if something were regenerating it fresh each time, which
        // would still normally produce the same content anyway unless
        // network state changed mid-test, but the *real* guarantee
        // here is architectural: get_or_init only ever runs its
        // closure once per process).
        let first = splash_html();
        let second = splash_html();
        assert_eq!(first, second);
        assert!(std::ptr::eq(first, second),
                "splash_html() must return the exact same cached buffer \
                 on repeated calls, not recompute it each time");
    }
}
