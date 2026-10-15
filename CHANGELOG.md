# Changelog

## 0.6.7 - Oct 2026

- The player's own HTTPS requests (CMS, media downloads) and the XMR
  WebSocket now also trust the Linux system CA store, besides the bundled
  Mozilla roots, so a CMS behind an internal CA no longer needs `--no-verify`.
- By default (offline mode), a display that loses its connection to the CMS
  mid-session now keeps playing its cached schedule instead of falling back
  to the splash screen after 3 failed collection cycles; once 3 cycles have
  failed it retries every 30 s, so a returning CMS is noticed promptly.
  `--disallow-offline` keeps the splash screen fallback.
- A display with a Sync Group role that starts from a valid cache (schedule
  and settings) now starts or joins its Sync Group right away, even with the
  CMS unreachable, instead of waiting for a first successful collection.
- Fix the startup splash screen being cut short when a cached schedule arrives
  within milliseconds of it: the first navigation could cancel the splash's own
  initialisation, leaving a blank letterboxed strip (no logo, black bars) for as
  long as the first layout took to load. Navigation now waits until the splash
  has initialised (at most 3 s) and has then been shown for 1 s, adjustable with
  the `AREXIBO_SPLASH_MS` environment variable (0 = no minimum).
- Fix a Navigate Layout button sending the display to a layout that no longer
  exists. The target id is fixed when the page is translated, so after the
  target layout was republished (new id, same code) and the old one purged, the
  already open page still pointed at the old id and the display got stuck
  retrying the missing page until a purge-all. A jump now goes to the layout
  that replaced it, or is ignored if there is none, and a jump target that
  cannot be loaded after several tries sends the view back to the scheduled
  layout (or the splash screen).
- Add an opt-in focus on load for webpage widgets in "Open Natively" mode: `[HasFocus]` in the
  URL (removed before loading) makes that page take keyboard focus once, when it first
  finishes loading.  Before, a native page only received focus on touch and keys typed
  earlier reached nobody.
- Add a watchdog for stuck layout loads: a layout page with no load progress for 20 s (`AREXIBO_LAYOUT_LOAD_TIMEOUT_S`,
  0 disables) is stopped and reloaded once, then accepted as it is. Regions only start on the page's `load` event, so one
  resource that never completes (a web font, an iframe) used to leave the screen blank until the next reload.
- Honor the Webpage widget's "Background transparent?" option in "Open Natively" mode: the native view is now transparent when
  it is checked (it was always opaque white outside the overlay), so the layout shows through the page's transparent areas.
- Add the `/info` endpoint (`xiboIC.info()`), with the same fields as the official Electron player: `version`, `displayName`,
  `hardwareKey`, `screenWidth`, `screenHeight`, `latitude`/`longitude` (always 0: no location source on Linux), `timeZone`,
  `currentLayoutId` and `displayStatus` (the CMS registration status). Served to local requests only.
- Add Data Connectors (real-time data), as in the official Electron player. A connector scheduled in the CMS runs in a
  hidden page of its own, with the `xiboDC` API (`setData`, `notifyHost`, `makeRequest`, `setCriteria`); its data is served at
  `/realtime`, the widgets are notified when it changes, and the criteria it sets drive the schedule. Its script is checked against the
  md5 announced by the CMS before every run (and downloaded again when edited in the CMS). A connector tied to a geographic area
  never runs, as there is no location source. See the README.
- Read the display's tags from the registration (kept with the settings, so they survive a start from the cache) and give them to the
  data connectors as `window.displayTags`, an object, before `onInit()`.
- `/realtime` now answers as the Electron player does: the key is decoded like a query parameter (`%20` and `+` were not), a missing key is
  a 400, a miss a 404 (the widget's error callback), both with a JSON error, and a hit is served as `application/json`.
- Commands accept `[[tag]]` placeholders, filled with the display's tags when the command runs, as in the Electron player. See the README.

## 0.6.6 - Oct 2026

- Fix image and video alignment being ignored: the CMS writes it as
  `<alignId>` / `<valignId>` child options, but only an older attribute
  spelling was read, so everything ended up centered. Left/right and
  top/bottom now work (visible with the `center` scale type, where the
  media doesn't fill its region).
- Native webpage widgets ("Open Natively"): a touch or click now restarts
  the widget's duration countdown, so a layout no longer expires or reloads
  while someone is using the page. Main layout only (not overlay layouts);
  keyboard, wheel and dragging do not count.
- Serve a real `/favicon.ico` (the arexibo logo) instead of an empty reply
  without a `Content-Type`, which made Chromium sniff the content on every
  page load.
- Fix `--web-debug` not enabling part of the Qt-side logging: the flag was a
  separate copy per source file, so some lines (e.g. `run JavaScript:`) were
  never printed. They now appear with `--web-debug` (noisy by design).
- The translator version is bumped, so cached layouts are re-downloaded and
  retranslated once at the first start after upgrading and pick up the
  fixes above without needing `--clear`.
- Fix widgets that the CMS scales up with a CSS transform (e.g. the flip
  clock) being cut off at their unscaled height: the shrink-to-fit script
  forced `overflow: hidden` on `<html>`, which made `<body>`'s own overflow
  clip at its untransformed box instead of the viewport.
- Embedded widgets are shown at their authored size, as in the Xibo player:
  content larger than the region is clipped instead of being shrunk to fit.
  The shrink-to-fit fallback now skips modules declaring `scaleContent`.

## 0.6.5 - Oct 2026

- Webpage widget in "Open Natively" mode: `[DisplayName]` and
  `[HardwareKey]` in the URL are replaced with the display's name (as
  assigned by the CMS) and hardware key, url-encoded. Cached layouts are
  retranslated when the display name changes (at runtime or since the
  last run), since the value is baked into the translated page. Manual
  Position / Best Fit modes are not covered (their HTML is generated by
  the CMS).
- Fix a blank page when navigating to a layout that had been purged: the
  already-translated `N.xlf.html` was left on disk (still referencing
  widget resources that had been removed), because only the raw `N.xlf`
  was purged.
- With `expire_modified_layouts` off, a modified version of the layout
  currently playing was only picked up on the next collection cycle,
  which never helps a layout that loops itself (the default layout, or a
  single-layout schedule). It is now fetched right before the reload that
  happens whenever such a layout completes its cycle.
- Fix Navigate Layout actions staying pointed at an old layout after the
  target was republished: actions reference their target by a stable
  `layoutCode`, resolved to the numeric id at translation time, but a
  layout was only retranslated when its own XML changed. Cached layouts
  are now retranslated (no download) when a code starts resolving to a
  different id. Several layouts sharing the same code within one
  response no longer count as a change on every cycle.

## 0.6.4 - Oct 2026

- Fix DataSet/"Elements" widgets never receiving their data (GetData)
  when their own layout is reached only via a Navigate Layout action
  and genuinely no longer served by the CMS at all: a layout can be
  replaced/removed without the CMS ever sending an explicit purge
  directive for it, leaving the display polling GetData forever for
  widgets that will never succeed. The CMS's own RequiredFiles
  response always lists every currently-needed layout regardless of
  whether it needs downloading, so a cached layout missing from that
  list (along with its own widget resources) is now removed just as
  authoritatively as an explicit purge, without ever guessing from
  repeated GetData failures (the CMS can take several minutes to
  materialize a dataset's own cache after a bulk collectNow, during
  which a genuinely valid widget could otherwise be pruned by mistake).
- Fix the CMS explicitly purging a layout not also stopping GetData
  polling for its own widgets (only purging the widget's own resource
  directly did).
- Log every file actually purged (and how many of its own data widgets
  stopped being tracked as a result) -- previously silent on success.

## 0.6.3 - Oct 2026

- Fix `--allow-offline` exiting the process anyway when XMR setup fails
  on a genuinely unreachable network; retries XMR automatically once
  the network returns.
- Apply the CMS's own `logLevel` Display Profile setting (previously
  parsed but never actually used); `--debug` still always takes
  precedence when passed.
- Implement `DownloadStartWindow`/`DownloadEndWindow` (previously
  parsed but never enforced).
- Implement Overlay Layout as a CMS-scheduled event type (the
  `<overlays>` schedule section), in addition to the existing XMR
  `overlayLayout` push action; don't render the overlay's own
  background, matching documented Xibo behaviour.
- Fix several DataSet View / "Elements" data-table widget issues that
  could prevent the widget from ever rendering: a resource missing
  from the CMS's own `RequiredFiles` response, widgets nested inside
  another widget's combined resource HTML, the embedded server's port
  changing on every restart, a cross-origin widget-reload failure, and
  missing `Cache-Control` headers.
- Work around Chromium's per-origin HTTP connection limit for layouts
  with many `render="html"` widgets by sharding requests across
  several loopback origins.
- Apply Interactive Control duration changes (`/duration/set|extend|
  expire`), previously acknowledged but never actually applied.
- Add a dedicated, longer timeout for `GetResource` specifically, so a
  legitimately slow (not dead) CMS response isn't cut off prematurely.
- Add optional `--web-debug`-gated diagnostics: JS console capture
  from every frame, plus a couple of internal state logs useful for
  troubleshooting a "content renders but isn't visible" widget.
- Customizable startup splash screen (logo + text).
- Add a loading watchdog for native ("Open Natively") webpage widgets:
  force-stops a page stuck with no load progress for 15s, so one
  unreachable/slow external site can't hang the whole layout.
- Treat a webpage widget's own `duration` of `0` or unset as "as long
  as the layout", instead of collapsing to 1 second.
- Splash screen now shows one of three distinct statuses instead of a
  single generic "LOADING...": "REGISTERING TO CMS..." while
  registered but not yet authorized, and "WAITING FOR CMS
  CONNECTION..." while genuinely unreachable (with no cache, or with
  the new `--disallow-offline`).
- `--allow-offline`'s own behaviour (fall back to a cached schedule
  instead of exiting) is now the default; the flag is still accepted
  but now does nothing, only so existing scripts don't break. Add
  `--disallow-offline` for the old, strict behaviour.
- A display that loses its connection mid-session (not just at
  startup) now falls back to the splash screen after 3 consecutive
  failed collection cycles, instead of silently cycling stale content
  forever with no indication anything's wrong.
- Fix a Sync Group reload being force-triggered (and misleadingly
  logged as "Sync Group") for a display that merely has a Sync Group
  role configured, even with no Sync Group Command actually active.
- Fix a rare crash right after completing registration
  (`QThreadStorage: entry ... destroyed before end of thread`): the
  main window (and every native web view) is now torn down explicitly
  before quitting, instead of being abandoned mid-event-loop.
- A renderer process crash now force-exits immediately, instead of
  risking a hang in Chromium's own exit-time cleanup trying to reach
  the very process that just crashed.
- Fix a shrink-to-fit sizing bug: a widget whose own width was already
  correct could still get squeezed narrower than intended to
  compensate for unrelated vertical overflow (e.g. a template's own
  static header not accounted for in its declared height); only width
  overflow is corrected now, height overflow is left to clip as
  before, matching the other official Xibo players' own behaviour.
- Fix DataSet/"Elements" widgets never receiving their data (GetData)
  when their own layout is only reachable via a Navigate Layout action
  (not part of the resolved schedule itself) -- their own tracking was
  being dropped again right after being discovered.

## 0.5.1 - upcoming

- Implement `--no-verify` for WebSocket XMR.
- Reconnect to WebSocket XMR after errors or timeout.

## 0.5.0 - Apr 2026

- Implement WebSocket XMR protocol.
- Reimplement ZMTP for ZMQ XMR to avoid relying on non-updated zeromq
  crate.
- Fix a few bugs found by the "xibo-players/arexibo" fork, thanks to
  Pau Aliagas.

## 0.4.0 - Apr 2026

- Prevent path traversal in embedded webserver.
- Fix runtime segfaults with zeromq on some newer distributions.
- Allow giving "--screen" on command line.
- Properly handle high-dpi screens in Qt part.
- Allow giving "--debug" on command line, ignore CMS given loglevel.

## 0.3.3 - Mar 2026

- Allow "pdf" content type.
- Add desktop entry file and icon to repository.
- Fix problems with PDF display by using an embedded copy of
  the pdf.js library.

## 0.3.2 - May 2025

- Implement shell commands triggered by layouts.
- Allow "localvideo" content type.
- Set a minimum media duration of 1 second.
- Implement zindex property for layout regions.

## 0.3.1 - Apr 2025

- Implement player commands.
- Implement trigger actions/webhooks.
- Implement resource file purging as directed from CMS.
- Better debugging of SOAP responses.
- Parse embedded duration and numitems from resources.
- Properly translate references to view port size in content.
- Store raw XML responses from CMS for debugging.

## 0.3.0 - Mar 2025

- Switch to Qt6 for displaying web content. Arexibo now requires
  a C++ compiler, CMake and Qt including QtWebEngine to build.
- Add connection timeout for CMS connection.
- Automatically retranslate layouts on translator code update.
- Use a random port number for the internal web server.

## 0.2.8 - Feb 2025

- Add command line option to allow starting without a
  connection to the CMS, showing the last cached schedule.
- Fix display of background images.

## 0.2.7 - Jan 2025

- Update dependencies, require Rust 1.75.

## 0.2.6 - Jan 2025

- Add command line option to disabled HTTPS certificate
  verification when connecting to the CMS.
- Fix "stretch" scaling for images in layouts.

## 0.2.5 - Jul 2024

- Allow giving an initial display name on the command line,
  defaults to the hostname.
- Remove dependencies on jQuery in generated JavaScript.

## 0.2.4 - Jul 2024

- Update dependencies, require Rust 1.66.

## 0.2.3 - Jun 2022

- Fix broken unit test.

## 0.2.2 - May 2022

- Ensure media playback works from the internal web server
  by avoiding chunked responses.

## 0.2.1 - May 2022

- Fix some layout bugs.
- Remove official Xibo branding.

## 0.2.0 - Feb 2022

- Initial public release.
