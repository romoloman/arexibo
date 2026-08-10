// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! XLF layout parser and translator.

use std::{fs, io::{Write, BufWriter}, collections::HashMap};
use std::path::Path;
use anyhow::{Context, Result};
use elementtree::Element;
use crate::resource::LayoutId;
use crate::util::{ElementExt, percent_decode};

// TODO:
// - fly transition (fadeIn/fadeOut implemented, see write_region)
// - reloading resources in iframes
// - overriding duration from resources
// - fromDt/toDt

pub const TRANSLATOR_VERSION: u32 = 29;

const LAYOUT_CSS: &str = r##"
body { margin: 0; background-repeat: no-repeat; overflow: hidden; }
iframe { border: 0 }
.media { position: absolute; visibility: hidden; }
.pdf-canvas { display: block; }
p { margin-top: 0; }
"##;

/// Inline pdf.js rendering engine — loaded once per layout that contains a PDF widget.
/// Renders pages to a canvas with timed cycling (duration / numPages per page).
const PDF_SCRIPT: &str = r##"
window.arexiboPdf = {
  _instances: {},

  start: function(canvasId, url, width, height, duration) {
    var self = this;
    var canvas = document.getElementById(canvasId);
    var ctx = canvas.getContext('2d');
    canvas.width = width;
    canvas.height = height;

    var state = { pdf: null, currentPage: 1, totalPages: 0, timer: null, destroyed: false };
    self._instances[canvasId] = state;

    pdfjsLib.getDocument({ url: url, isEvalSupported: false }).promise.then(function(pdf) {
      if (state.destroyed) { pdf.destroy(); return; }
      state.pdf = pdf;
      state.totalPages = pdf.numPages;

      function renderPage(num) {
        if (state.destroyed) return;
        pdf.getPage(num).then(function(page) {
          if (state.destroyed) { page.cleanup(); return; }
          var viewport = page.getViewport({ scale: 1 });
          var scale = Math.min(width / viewport.width, height / viewport.height);
          var scaledViewport = page.getViewport({ scale: scale });

          // Center on canvas
          ctx.clearRect(0, 0, width, height);
          ctx.save();
          ctx.translate(
            (width - scaledViewport.width) / 2,
            (height - scaledViewport.height) / 2
          );

          page.render({ canvasContext: ctx, viewport: scaledViewport }).promise.then(function() {
            ctx.restore();
            page.cleanup();
          });
        });
      }

      renderPage(1);

      if (state.totalPages > 1) {
        var interval = (duration / state.totalPages) * 1000;
        state.timer = setInterval(function() {
          if (state.destroyed) return;
          state.currentPage = (state.currentPage % state.totalPages) + 1;
          renderPage(state.currentPage);
        }, interval);
      }
    });
  },

  stop: function(canvasId) {
    var state = this._instances[canvasId];
    if (!state) return;
    state.destroyed = true;
    if (state.timer) { clearInterval(state.timer); state.timer = null; }
    if (state.pdf) { state.pdf.destroy(); state.pdf = null; }
    delete this._instances[canvasId];
  }
};
"##;

const SCRIPT: &str = r##"
new QWebChannel(qt.webChannelTransport, function(channel) {
  window.arexiboGui = channel.objects.arexibo;
  window.arexiboGui.jsLayoutInit(window.arexibo.id,
                                 window.arexibo.width, window.arexibo.height);
});

window.arexibo = {
  id: 0,
  width: 0,
  height: 0,
  done: false,
  regions_total: 0,
  triggers: {},
  regions: {},

  region_switch: function(rid, next, first) {
    let {cur, total, timeoutid, media, loop} = this.regions[rid];
    // stop a timeout, if it still exists
    window.clearTimeout(timeoutid);

    // determine next media
    if (next == -1)
      next = (cur + 1) % total;
    else if (next == -2)
      next = (cur + total - 1) % total;

    // BUG fix (found from a real report: "Playlist with 3 pictures
    // slideshow timed for 3 seconds each freezes on last picture after
    // 1st run"). CONFIRMED via official Xibo documentation (two
    // independent sources, xibosignage.com and the xibo.org.uk manual,
    // consistent wording): "The Loop option is only applicable when
    // there is only 1 media item in the region." -- for a region with
    // MORE than one item, `loop` simply doesn't apply at all; it must
    // always keep cycling through its items regardless of that
    // setting. This freeze-on-wrap logic was previously applied
    // unconditionally (added to fix a *different*, real report of
    // "playlists kept cycling forever" -- that one, on reflection, was
    // very likely about a single-item region, or about layout/campaign-
    // level cycle counts, not about this multi-item case at all) --
    // now scoped to `total <= 1` specifically, matching the documented
    // semantics precisely instead of applying it to every region
    // regardless of how many items it actually has.
    if (next == 0 && !first && !loop && total <= 1) {
      this.region_done(rid);
      return;
    }

    // stop showing the current media
    if (cur !== null)
      media[cur][1]();

    this.regions[rid].cur = next;
    // when the first media is called for the second time, the region is "done"
    if (next == 0 && !first) {
      this.region_done(rid);
    }

    // start showing the next media
    media[next][0]();

    // set timeout to switch to the next media
    let duration = media[next][2]() || 1;
    this.regions[rid].timeoutDuration = duration * 1000;
    this.regions[rid].timeoutStart = Date.now();
    this.regions[rid].timeoutid = window.setTimeout(() => {
      this.region_switch(rid, -1, false);
    }, duration * 1000);
  },

  // Interactive Control duration overrides -- see
  // https://github.com/xibosignage/xibo-interactive-control and
  // /duration/set|extend|expire in server.rs, which relay here via
  // run_js after a Widget's own JS (running inside its own iframe) posts
  // to the player's embedded webserver. Only has an effect on the widget
  // that is CURRENTLY showing in its region: a set/extend/expire
  // targeting a widget that isn't presently the active one in its
  // region is a no-op, since there's no live timer to adjust for it (a
  // widget waiting its turn doesn't have a running countdown yet).
  controlDuration: function(widgetId, action, durationSecs) {
    for (const ridStr in this.regions) {
      const rid = Number(ridStr);
      const region = this.regions[rid];
      const idx = region.media.findIndex(m => m[3] === widgetId);
      if (idx === -1 || region.cur !== idx) continue;

      if (action === 'expire') {
        this.region_switch(rid, -1, false);
      } else if (action === 'set' || action === 'extend') {
        window.clearTimeout(region.timeoutid);
        let newDurationMs;
        if (action === 'set') {
          newDurationMs = durationSecs * 1000;
        } else {
          const elapsed = Date.now() - region.timeoutStart;
          const remaining = Math.max(0, region.timeoutDuration - elapsed);
          newDurationMs = remaining + durationSecs * 1000;
        }
        region.timeoutDuration = newDurationMs;
        region.timeoutStart = Date.now();
        region.timeoutid = window.setTimeout(() => {
          this.region_switch(rid, -1, false);
        }, newDurationMs);
      }
      return;
    }
    console.warn('controlDuration: widget ' + widgetId +
                 ' not found or not currently active in its region');
  },

  region_done: function(rid) {
    if (this.done) return;

    this.regions[rid].done = true;
    // check if all regions are done
    for (let region of Object.values(this.regions)) {
      if (!region.done) return;
    }
    window.arexiboGui.jsLayoutDone(window.arexibo.id);
    this.done = true;
  },

  trigger: function(code) {
    if (this.triggers[code] !== undefined) {
      this.triggers[code]();
    }
  },

  // Runs a `next`/`previous`/`navLayout` action directly -- shared by
  // both the webhook path (trigger(), above, looked up by code) and
  // touch-triggered click handlers (called inline via a closure baked
  // in at HTML-generation time -- see Translator::write_action).
  performAction: function(action, target, targetid, layoutid) {
    if (action == 'navLayout') {
      window.arexiboGui.jsLayoutJump(window.arexibo.id, layoutid);
    } else if (action == 'previous' || action == 'next') {
      if (target == 'layout') {
        if (action == 'next')
          window.arexiboGui.jsLayoutDone(window.arexibo.id);
        else
          window.arexiboGui.jsLayoutPrev(window.arexibo.id);
      } else {
        if (action == 'next')
          this.region_switch(targetid, -1);
        else
          this.region_switch(targetid, -2);
      }
    }
  },

  // navWidget: jump directly to a specific widget's index within its
  // region -- the (region id, index) pair is resolved once, at
  // HTML-generation time in Rust (see Translator::widget_regions),
  // not looked up at runtime here.
  navWidget: function(rid, index) {
    this.region_switch(rid, index, false);
  },
};
"##;


/// (mid, duration expr, add_start, add_stop, fade_in, fade_out, transition_ms)
/// -- see write_media's transition-resolution logic for fade_in/fade_out/
/// transition_ms.
type MediaInfo = (i32, String, String, String, bool, bool, u32);

pub struct Translator<'a> {
    id: LayoutId,
    tree: Option<Element>,
    out: BufWriter<fs::File>,
    regions: Vec<i32>,
    size: (i32, i32),
    code_map: &'a HashMap<String, LayoutId>,
    has_pdf: bool,
    /// Maps a widget's `id` (as used in the XLF <media> element) to the
    /// (region id, index-within-region) pair it ends up at in the
    /// generated HTML/JS -- needed to resolve `navWidget` touch/webhook
    /// actions, which reference a target widget id directly rather
    /// than a region + relative-index like `next`/`previous` do.
    widget_regions: HashMap<i32, (i32, usize)>,
    /// Maps a region's `id` to its (x, y, w, h) geometry -- needed for
    /// touch actions, which get an invisible click-catching overlay
    /// `<div>` positioned over the *region* (not attached to the
    /// widget's own DOM element directly: for `render="html"` widgets
    /// like interactive-button, that element is an `<iframe>`, and a
    /// click on content *inside* an iframe does not bubble out to a
    /// 'click' listener on the iframe element itself in the parent
    /// document).
    region_geom: HashMap<i32, [i32; 4]>,
    /// The layout's own `enableStat` XLF attribute (defaults to enabled
    /// if absent, per Xibo's documented convention) -- needed by Proof
    /// of Play (see stats.rs/mainloop.rs) to decide whether a "layout"
    /// stat record should be recorded at all for this layout.
    enable_stat: bool,
    /// `Some` only if Adspace Exchange is enabled (see
    /// `resource::Cache::adspace_enabled`) -- used to resolve `ssp`
    /// widgets synchronously during translation (see `write_media`'s
    /// `Some("ssp")` branch and adspace.rs's module-level doc comment
    /// for the significant caveats on this whole feature). `None` means
    /// any `ssp` widget encountered is simply skipped (same as any
    /// other genuinely unsupported type), matching today's behavior
    /// when Adspace isn't configured at all.
    adspace: Option<crate::adspace::AdspaceConfig>,
    /// Port the embedded HTTP server (server.rs) is listening on, shared
    /// identically across all `HTML_SHARD_COUNT` loopback addresses (see
    /// that constant's own doc comment) -- used to build each
    /// `render="html"` widget's own absolute, sharded iframe `src` in
    /// `write_media`.
    html_port: u16,
}

impl<'a> Translator<'a> {
    pub fn new(id: LayoutId, xlf: &Path, html: &Path,
               code_map: &'a HashMap<String, LayoutId>,
               adspace: Option<crate::adspace::AdspaceConfig>,
               html_port: u16) -> Result<Self> {
        let file = fs::File::open(xlf)?;
        let tree = Some(Element::from_reader(file).context("parsing XLF")?);

        let out = fs::File::create(html)?;
        let out = BufWriter::new(out);

        Ok(Self { id, tree, out, regions: Vec::new(), size: (0, 0), code_map, has_pdf: false,
                  widget_regions: HashMap::new(), region_geom: HashMap::new(),
                  enable_stat: true, adspace, html_port })
    }

    pub fn translate(mut self) -> Result<(i32, i32, bool)> {
        let tree = self.tree.take().unwrap();
        // Pre-scan for PDF widgets to know if we need pdf.js
        for region in tree.find_all("region") {
            for media in region.find_all("media") {
                if media.get_attr("type") == Some("pdf") {
                    self.has_pdf = true;
                    break;
                }
            }
            if self.has_pdf { break; }
        }
        self.write_header(&tree)?;

        // Actions can appear at any nesting level: directly under
        // <layout> (layout-scoped), under <region> (region-scoped), or
        // under <media> (widget-scoped -- confirmed as the real-world
        // case for touch-triggered navLayout buttons: `find_all` only
        // searches *direct* children, so a plain `tree.find_all("action")`
        // alone misses anything nested inside a <media> element).
        // Collected here, before writing regions below, so navWidget's
        // target-widget lookup (built progressively while writing
        // regions) sees the complete action list regardless of where
        // in the document each action -- or its target widget -- happens
        // to be positioned.
        let mut actions: Vec<&Element> = tree.find_all("action").collect();
        for region in tree.find_all("region") {
            actions.extend(region.find_all("action"));
            for media in region.find_all("media") {
                actions.extend(media.find_all("action"));
            }
        }

        for region in tree.find_all("region") {
            if let Err(e) = self.write_region(region) {
                log::error!("layout: could not translate region: {:#}", e);
            }
        }
        writeln!(self.out, "<script type='text/javascript'>")?;
        for action in actions {
            if let Err(e) = self.write_action(action) {
                log::error!("layout: could not translate action: {:#}", e);
            }
        }
        writeln!(self.out, "</script>")?;
        self.write_footer()?;
        Ok((self.size.0, self.size.1, self.enable_stat))
    }

    fn write_action(&mut self, el: &Element) -> Result<()> {
        let typ = el.req_attr("triggerType")?;
        let action = el.req_attr("actionType")?;
        let code = el.def_attr("triggerCode", "<not set>");
        let layoutcode = el.def_attr("layoutCode", "<not set>");

        // Resolve the actual JS call to run when this action fires.
        // navWidget is special: it targets a specific widget id
        // (resolved to a region+index pair at translation time, via
        // Translator::widget_regions -- NOT the same target/targetId
        // pair next/previous/navLayout use), everything else goes
        // through the shared performAction() helper.
        // UNVERIFIED against a real CMS-exported XLF sample: exact
        // attribute names below (`source`/`sourceId`/`widgetId`) are
        // reconstructed from the Xibo action schema as documented
        // elsewhere, not confirmed against this project's own XLF
        // files -- if a real layout's touch/navWidget actions don't
        // fire, check these names first.
        let call = if action == "navWidget" {
            let widget_id: i32 = el.parse_attr("widgetId")?;
            let (rid, index) = self.widget_regions.get(&widget_id).copied()
                .with_context(|| format!("navWidget: unknown target widget id {widget_id}"))?;
            format!("window.arexibo.navWidget({rid}, {index})")
        } else {
            let target = el.def_attr("target", "screen");
            let targetid = el.def_attr("targetId", "0").parse::<i64>()
                .context("bad targetId")?;
            let mut layoutid = 0;
            if action == "navLayout" {
                // BUG fix (found from a real report, confirmed with a
                // real XLF sample from the CMS): `targetId` already
                // carries the target layout's own numeric id directly
                // (confirmed in the real sample: `targetId="780"`
                // correctly pointing at that same, valid layout) --
                // resolving via `layoutCode`/`code_map` instead
                // unconditionally meant a stale/unassigned layoutCode
                // value (e.g. "test1", not present in code_map for any
                // currently-required layout) failed the whole action,
                // even though targetId alone was already sufficient and
                // correct. Now: use targetId directly when it's a real,
                // positive id; only fall back to the layoutCode lookup
                // if targetId is absent/zero.
                layoutid = if targetid > 0 {
                    targetid
                } else {
                    self.code_map.get(layoutcode).copied()
                        .context("unknown layout code")?
                };
            }
            format!("window.arexibo.performAction({action:?}, {target:?}, {targetid}, {layoutid})")
        };

        if typ == "webhook" {
            writeln!(self.out, "window.arexibo.triggers[{code:?}] = function() {{ {call}; }};")?;
        } else if typ == "touch" {
            // `source`/`sourceId` say *where* to place the invisible,
            // click-catching overlay: over a specific widget's owning
            // region, over a named region directly, or (fallback) the
            // whole layout body. An overlay `<div>` is used instead of
            // attaching a listener to the widget's own existing DOM
            // element, because for `render="html"` widgets (e.g.
            // interactive-button) that element is an `<iframe>` -- a
            // click on content *inside* an iframe does not bubble out
            // to a 'click' listener on the iframe element itself in
            // the parent document.
            let source = el.def_attr("source", "layout");
            let geom = match source {
                "widget" => {
                    let widget_id: i32 = el.parse_attr("sourceId")?;
                    let (rid, _) = self.widget_regions.get(&widget_id).copied()
                        .with_context(|| format!("touch action: unknown source widget id {widget_id}"))?;
                    self.region_geom.get(&rid).copied()
                }
                "region" => {
                    let region_id: i32 = el.parse_attr("sourceId")?;
                    self.region_geom.get(&region_id).copied()
                }
                _ => None,
            };
            match geom {
                Some([x, y, w, h]) => {
                    writeln!(self.out, "{{ const overlay = document.createElement('div'); \
                                        overlay.style.cssText = 'position:absolute; left:{x}px; \
                                        top:{y}px; width:{w}px; height:{h}px; z-index:9999; \
                                        cursor:pointer;'; \
                                        overlay.addEventListener('click', function() {{ {call}; }}); \
                                        document.body.appendChild(overlay); }}")?;
                }
                None => {
                    // Whole-layout touch zone (source == "layout", or a
                    // widget/region source we couldn't resolve a
                    // geometry for) -- cover the entire screen instead
                    // of a specific rect.
                    writeln!(self.out, "document.body.addEventListener('click', function() {{ {call}; }});")?;
                }
            }
            // BUG fix (found from a real report: "Interactive layout
            // button... did not recognize keyboard space key press...
            // but touch is recognized"). CONFIRMED via a real XLF
            // sample from the CMS: Xibo's "Key Press" interactive
            // trigger (CMS 4.4+, docs: "trigger interactive content
            // with your keyboard, without the need for a touchscreen
            // display") does NOT use a separate triggerType -- it's the
            // *same* triggerType="touch" action, with `triggerCode`
            // additionally carrying a keyboard key name (e.g. "Space",
            // matching the KeyboardEvent.code convention) as an
            // *alternative* way to fire the identical action, alongside
            // the touch/click zone above, not instead of it. This was
            // previously never implemented at all -- triggerCode was
            // read only for the (unrelated) webhook action type,
            // completely ignored here. Always adding this listener is
            // safe/inert for a touch-only action that doesn't use this
            // feature: a triggerCode that isn't a real KeyboardEvent.code
            // value (e.g. "<not set>") will simply never match any real
            // key event.
            writeln!(self.out, "document.addEventListener('keydown', function(e) {{ \
                                if (e.code === {code:?}) {{ {call}; }} }});")?;
        } else {
            log::warn!("unsupported action type: {typ}");
        }
        Ok(())
    }

    fn write_header(&mut self, el: &Element) -> Result<()> {
        self.size = (el.parse_attr("width")?, el.parse_attr("height")?);
        // Defaults to enabled if absent -- per Xibo's documented Proof
        // of Play convention ("stats ... 0 or 1 ... default 1 if not
        // present").
        self.enable_stat = el.get_attr("enableStat")
            .map(|s| s != "0").unwrap_or(true);

        writeln!(self.out, "<!DOCTYPE html>\n<!-- VERSION={TRANSLATOR_VERSION} -->")?;
        writeln!(self.out, "<html><head>")?;
        writeln!(self.out, "<meta charset='utf-8'>")?;
        writeln!(self.out, "<script src='qrc:///qtwebchannel/qwebchannel.js'></script>")?;
        writeln!(self.out, "<script type='text/javascript'>{SCRIPT}\
                            window.arexibo.id = {};\n\
                            window.arexibo.width = {};\n\
                            window.arexibo.height = {};\n\
                            </script>", self.id, self.size.0, self.size.1)?;
        writeln!(self.out, "<style type='text/css'>{LAYOUT_CSS}")?;

        if let Some(file) = el.get_attr("background") {
            writeln!(self.out, "body {{ background-image: url('{file}'); \
                                background-size: 100vw 100vh; }}")?;
        }
        if let Some(color) = el.get_attr("bgcolor") {
            writeln!(self.out, "body {{ background-color: {color}; }}")?;
        }

        writeln!(self.out, "</style>")?;
        if self.has_pdf {
            writeln!(self.out, "<script src='pdfjs/pdf.min.mjs' type='module'></script>")?;
            writeln!(self.out, "<script type='module'>")?;
            writeln!(self.out, "import * as pdfjsLib from './pdfjs/pdf.min.mjs';")?;
            writeln!(self.out, "pdfjsLib.GlobalWorkerOptions.workerSrc = './pdfjs/pdf.worker.min.mjs';")?;
            writeln!(self.out, "window.pdfjsLib = pdfjsLib;")?;
            writeln!(self.out, "</script>")?;
            writeln!(self.out, "<script type='text/javascript'>{PDF_SCRIPT}</script>")?;
        }
        writeln!(self.out, "</head><body>")?;
        Ok(())
    }

    fn write_footer(&mut self) -> Result<()> {
        // start all regions' first item
        writeln!(self.out, "<script type='text/javascript'>\n\
                            window.addEventListener('load', function() {{")?;
        for rid in &self.regions {
            writeln!(self.out, "  window.arexibo.region_switch({rid}, 0, true);")?;
        }
        writeln!(self.out, "}});\n</script>")?;
        writeln!(self.out, "</body></html>")?;
        Ok(())
    }

    fn write_region(&mut self, region: &Element) -> Result<()> {
        let rid = region.parse_attr("id")?;
        let x = region.parse_attr("left")?;
        let y = region.parse_attr("top")?;
        let w = region.parse_attr("width")?;
        let h = region.parse_attr("height")?;
        let geom = [x, y, w, h];
        self.region_geom.insert(rid, geom);
        writeln!(self.out, "<!-- region {rid} -->")?;

        if let Some(zindex) = region.get_attr("zindex") {
            writeln!(self.out, "<style type='text/css'> \
                                .r{rid} {{ z-index: {zindex}; }} \
                                </style>")?;
        }

        // Confirmed real schema (account.xibosignage.com/docs/developer/
        // creating-a-player/xlf, fetched and read during development):
        // a region's own <options> can carry <transitionType>
        // (fadeIn/fadeOut/fly), <transitionDuration> (milliseconds), and
        // <transitionDirection> (compass point, "fly" only). HOWEVER,
        // a real XLF from a real CMS (shared by the user after
        // reporting that transitions weren't being honored) showed this
        // region-level trio present but almost always *empty*, while
        // every single *widget* additionally carries its own
        // `transIn`/`transInDuration`/`transInDirection` and
        // `transOut`/`transOutDuration`/`transOutDirection` in its own
        // `<options>` -- a DIFFERENT, per-widget mechanism this file
        // didn't parse at all before, which is why transitions weren't
        // being honored. Both are now supported: this region-level trio
        // is used only as a *fallback default* for widgets that don't
        // specify their own transIn/transOut (see write_media), matching
        // the community's own description of the region-level setting
        // ("a region transition which allows exit transitions... Is
        // there a way to set the In and Out transitions to a specific
        // setting for all content in that region?").
        let (region_type, region_ms, region_loop) = region.find("options").map(|opts| {
            let ty = opts.find("transitionType").map(|e| e.text().trim().to_string())
                .filter(|s| !s.is_empty());
            let ms: u32 = opts.find("transitionDuration")
                .and_then(|e| e.text().trim().parse().ok()).unwrap_or(0);
            // BUG fix (found from a real report: playlists kept cycling
            // forever regardless of this setting -- on reflection, most
            // likely a single-item region case, or a layout/campaign-
            // level cycle count, given the official docs confirm `loop`
            // "is only applicable when there is only 1 media item in
            // the region"): this was never read at all before. `<loop>`
            // here is the REGION's own single-item loop -- whether,
            // after showing its one media item once, it should reload/
            // restart it (1) or freeze/hold on it until the layout
            // itself finishes (0/absent) -- see region_switch's own
            // handling above, which only applies this when the region
            // has exactly one item; a region with more than one item
            // always keeps cycling through them regardless of this
            // setting, per the same documented semantics. Distinct from
            // a video widget's own `<loop>` in its *own* `<options>`
            // (see write_media's video branch), which governs native
            // single-widget looping.
            let lp = opts.find("loop").map(|e| e.text().trim() == "1").unwrap_or(false);
            (ty, ms, lp)
        }).unwrap_or((None, 0, false));
        let region_fade_in = region_type.as_deref() == Some("fadeIn") && region_ms > 0;
        let region_fade_out = region_type.as_deref() == Some("fadeOut") && region_ms > 0;

        let mut sequence = Vec::new();
        for media in region.find_all("media") {
            match self.write_media(rid, geom, media, (region_fade_in, region_fade_out, region_ms)) {
                Err(e) => log::error!("layout: could not translate media: {:#}", e),
                Ok(None) => continue,
                Ok(Some(res)) => {
                    // res.0 is the widget's mid -- record its (region,
                    // index) now, before `sequence` is drained below,
                    // so navWidget actions can resolve it later.
                    self.widget_regions.insert(res.0, (rid, sequence.len()));
                    sequence.push(res);
                }
            }
        }
        let nitems = sequence.len();

        if nitems == 0 {
            return Ok(());
        }

        writeln!(self.out, "<script type='text/javascript'>")?;
        writeln!(self.out, "window.arexibo.regions[{rid}] = {{")?;
        writeln!(self.out, "  done: false,")?;
        writeln!(self.out, "  cur: null,")?;
        writeln!(self.out, "  timeoutid: null,")?;
        writeln!(self.out, "  total: {nitems},")?;
        writeln!(self.out, "  loop: {region_loop},")?;
        writeln!(self.out, "  media: [")?;

        // for each media, write functions to start/stop displaying it
        for (mid, duration, add_start, add_stop, fade_in, fade_out, transition_ms) in sequence {
            writeln!(self.out, "    [function() {{")?;
            // Diagnostic log (found genuinely useful investigating a
            // real "content renders correctly inside its own iframe but
            // is never visible" report): confirms whether region_switch
            // actually calls this widget's own show function at all.
            // Gated behind --web-debug (see `window.arexiboDebug`,
            // injected profile-wide in gui/lib.cpp's `setup()`) -- on
            // request, so this doesn't add permanent noise to every
            // normal run; combined with LoggingPage (which already
            // captures every frame's own console output including this
            // one when that flag is on), gives a simple way to check
            // "is this specific widget's own show() ever running"
            // without needing network-reachable remote debugging.
            writeln!(self.out, "      if (window.arexiboDebug) console.log(\
                                'arexibo-show: region {rid} widget {mid}');")?;
            let el = format!("document.getElementById('m{mid}')");
            if fade_in {
                // Per the documented semantics ("the media duration
                // should include the in transition"), this animates
                // *within* the widget's own already-scheduled duration
                // countdown in region_switch -- no timing changes are
                // needed there, only here in how "becoming visible" is
                // actually performed.
                writeln!(self.out, "      {{ let el = {el}; \
                                    el.style.transition = 'opacity {transition_ms}ms'; \
                                    el.style.opacity = '0'; \
                                    el.style.visibility = 'visible'; \
                                    void el.offsetWidth; \
                                    el.style.opacity = '1'; }}")?;
            } else {
                writeln!(self.out, "      {el}.style.visibility = 'visible';")?;
            }
            writeln!(self.out, "      {add_start}")?;
            writeln!(self.out, "    }}, function() {{")?;
            // if only one item is present, don't need to hide the others
            if nitems > 1 {
                if fade_out {
                    // Deliberately fire-and-forget: region_switch already
                    // moves on to showing the *next* widget immediately
                    // after calling this, so its own in-transition (if
                    // any) overlaps with this fade-out -- a real
                    // crossfade, rather than the old sequential
                    // fade-to-background-then-fade-in behavior some very
                    // old Xibo player versions were reported to have
                    // (community forum, 2015). The next widget's own
                    // duration countdown is *not* delayed by this timer,
                    // matching "duration should... exclude the out
                    // transition".
                    //
                    // z-index bump is essential, not cosmetic: `.media`
                    // elements are `position: absolute` with no explicit
                    // z-index (see LAYOUT_CSS), so they stack in DOM
                    // order -- the *incoming* widget (written later in
                    // the region's own media list, or simply shown
                    // without any fade-in of its own if its `transIn`
                    // isn't fadeIn) would otherwise paint immediately
                    // on top of this one from the very first frame,
                    // completely hiding the fade-out happening
                    // underneath it -- a real bug found because the
                    // fade was reported as "not happening" even though
                    // the opacity animation itself was verified working
                    // correctly (see this session's QtWebEngine
                    // measurement). Bumping z-index keeps the
                    // *fading-out* widget on top for the duration of its
                    // own fade, so its decreasing opacity visibly
                    // reveals whatever's now underneath, regardless of
                    // DOM order. Not reset back afterwards: once
                    // `visibility: hidden`, the element doesn't
                    // participate in visible stacking at all regardless
                    // of z-index, so there is nothing to clean up.
                    writeln!(self.out, "      {{ let el = {el}; \
                                        el.style.zIndex = '9999'; \
                                        el.style.transition = 'opacity {transition_ms}ms'; \
                                        el.style.opacity = '0'; \
                                        setTimeout(() => {{ el.style.visibility = 'hidden'; }}, \
                                                   {transition_ms}); }}")?;
                } else {
                    writeln!(self.out, "      {el}.style.visibility = 'hidden'; ")?;
                }
            }
            writeln!(self.out, "      {add_stop}")?;
            writeln!(self.out, "    }}, {duration}, {mid}],")?;
        }
        writeln!(self.out, "  ],")?;
        writeln!(self.out, "}};\n</script>")?;
        self.regions.push(rid);
        Ok(())
    }

    fn write_media(&mut self, rid: i32, [x, y, w, h]: [i32; 4],
                   media: &Element, region_fallback: (bool, bool, u32)) -> Result<Option<MediaInfo>> {
        let mid = media.parse_attr("id")?;
        let opts = media.find("options").context("no options")?;
        let mut duration = format!(
            "() => {}", media.def_attr("duration", "").parse::<i32>().unwrap_or(10));
        let mut add_start = String::new();
        let mut add_stop = String::new();

        // Per-widget transition override -- confirmed real (a genuine
        // XLF from a real CMS, shared by the user after reporting
        // transitions weren't being honored): every widget's own
        // `<options>` can carry `transIn`/`transInDuration`/
        // `transInDirection` and `transOut`/`transOutDuration`/
        // `transOutDirection`, DIFFERENT property names from the
        // region-level `transitionType`/`transitionDuration`/
        // `transitionDirection` trio (see write_region) -- this is the
        // one actually populated by the CMS/editor in practice, which is
        // exactly why transitions weren't working before this was added.
        // Falls back to the region-level default (`region_fallback`,
        // itself already reduced to (fade_in, fade_out, ms)) only if
        // this widget doesn't specify its own transIn/transOut at all.
        // Same scope limits as the region-level case: only fadeIn/
        // fadeOut are implemented, "fly" logs a warning and falls back
        // to an instant switch for whichever side (in/out) requested it.
        let trans_in = opts.find("transIn").map(|e| e.text().trim().to_string())
            .filter(|s| !s.is_empty());
        let trans_in_ms: u32 = opts.find("transInDuration")
            .and_then(|e| e.text().trim().parse().ok()).unwrap_or(0);
        let trans_out = opts.find("transOut").map(|e| e.text().trim().to_string())
            .filter(|s| !s.is_empty());
        let trans_out_ms: u32 = opts.find("transOutDuration")
            .and_then(|e| e.text().trim().parse().ok()).unwrap_or(0);

        if trans_in.as_deref() == Some("fly") {
            log::warn!("media {mid}: \"fly\" in-transition requested but not implemented \
                        (only fadeIn/fadeOut are) -- falling back to an instant show");
        }
        if trans_out.as_deref() == Some("fly") {
            log::warn!("media {mid}: \"fly\" out-transition requested but not implemented \
                        (only fadeIn/fadeOut are) -- falling back to an instant hide");
        }

        let (region_fade_in, region_fade_out, region_ms) = region_fallback;
        let (fade_in, fade_out, transition_ms) = if trans_in.is_some() || trans_out.is_some() {
            (trans_in.as_deref() == Some("fadeIn") && trans_in_ms > 0,
             trans_out.as_deref() == Some("fadeOut") && trans_out_ms > 0,
             // fadeIn/fadeOut never coexist on the same widget in
             // practice (one governs showing, the other hiding), so
             // using whichever of the two durations is actually
             // associated with an implemented type is unambiguous; if
             // somehow both ended up set, prefer the in-duration
             // arbitrarily rather than silently pick 0.
             if trans_in.as_deref() == Some("fadeIn") { trans_in_ms } else { trans_out_ms })
        } else {
            (region_fade_in, region_fade_out, region_ms)
        };

        writeln!(self.out, "  <!-- media {mid} -->")?;
        match (media.get_attr("render"), media.get_attr("type")) {
            (Some("html"), _) |
            (_, Some("text" | "ticker" | "embedded" | "datasetview")) => {
                // `embedded` and `datasetview` are, like `text`/`ticker`,
                // core modules whose HTML is generated server-side by the
                // CMS and delivered as a "resource" required-file (see
                // `resource.rs`/`xmds.rs::required_files`), cached locally
                // as `{mid}.html` -- so the same iframe path used for
                // text/ticker applies unchanged; no new download logic
                // needed on the resource side.
                //
                // BUG fix (found from a real report: main layout content
                // intermittently missing/delayed, worse whenever an
                // Overlay Layout was also active): see
                // `server::HTML_SHARD_COUNT`'s own doc comment for the
                // full story (Chromium's hardcoded 6-connections-per-
                // origin limit). Each widget's own `src` is now an
                // *absolute* URL on one of several loopback origins,
                // chosen deterministically from this widget's own `mid`
                // (not randomly -- so repeated translations of the same
                // layout consistently pick the same shard per widget,
                // which doesn't matter for correctness but keeps
                // generated output stable/diffable), instead of a
                // relative path that would always resolve against
                // whichever single origin loaded the *parent* page.
                //
                // Distinctively-named `arexiboShrinkW`/`arexiboShrinkH`
                // query params *in addition to* the original `w`/`h`
                // (kept as-is in case CMS-generated resource templates
                // read those exact names themselves, e.g. via
                // xiboLayoutScaler/bundle.min.js -- not worth risking a
                // regression there) so the profile-level shrink-to-fit
                // script (see gui/lib.cpp, injected into every frame
                // including this one) can recognize *only* arexibo's own
                // resource iframes -- picking a name unlikely to collide
                // with any real external site's own, unrelated `w`/`h`
                // query parameters matters *now* specifically because
                // that script runs in every frame regardless of origin
                // (it has to, now that this iframe may be cross-origin
                // relative to the parent page -- see that script's own
                // doc comment for why the shrink logic moved from
                // parent-reaches-into-child to frame-shrinks-itself).
                let shard = 1 + (mid as u32 % crate::server::HTML_SHARD_COUNT);
                let port = self.html_port;
                writeln!(self.out, "<iframe class='media r{rid}' id='m{mid}' \
                                    src='http://127.0.0.{shard}:{port}/{mid}.html\
                                    ?w={w}&h={h}&arexiboShrinkW={w}&arexiboShrinkH={h}' \
                                    style='left: {x}px; top: {y}px; width: {w}px; \
                                    height: {h}px;'></iframe>")?;
            }
            (_, Some("webpage")) => {
                let url = percent_decode(opts.find("uri").context("no web uri")?.text());
                if media.get_attr("render") == Some("native") {
                    // `render="native"` means a real top-level browser
                    // view, not embedded via iframe -- unlike the
                    // `render="html"`/default iframe path below, this
                    // isn't subject to `X-Frame-Options`/frame-busting
                    // (those only block *embedding*, not top-level
                    // navigation). Emit an empty placeholder here for
                    // the show/hide cycling machinery every widget
                    // already goes through (see region_switch in the
                    // shared SCRIPT), and drive an actual, separate Qt
                    // QWebEngineView overlay via the same add_start/
                    // add_stop mechanism already used for pdf.js and
                    // shellcommand below -- see jsNativeWebShow/Hide
                    // in gui/view.cpp.
                    writeln!(self.out, "<div class='media r{rid}' id='m{mid}' \
                                        style='left: {x}px; top: {y}px; width: {w}px; \
                                        height: {h}px;'></div>")?;
                    add_start = format!(
                        "window.arexiboGui.jsNativeWebShow({mid}, {url:?}, {x}, {y}, {w}, {h});");
                    add_stop = format!("window.arexiboGui.jsNativeWebHide({mid});");
                } else {
                    writeln!(self.out, "<iframe class='media r{rid}' id='m{mid}' src='{url}' \
                                        style='left: {x}px; top: {y}px; width: {w}px; \
                                        height: {h}px;'></iframe>")?;
                }
            }
            (_, Some("pdf")) => {
                let filename = opts.find("uri").context("no pdf uri")?.text();
                let dur = media.def_attr("duration", "").parse::<i32>().unwrap_or(10);
                writeln!(self.out, "<canvas class='media pdf-canvas r{rid}' id='m{mid}' \
                                    style='left: {x}px; top: {y}px; width: {w}px; \
                                    height: {h}px;'></canvas>")?;
                add_start = format!(
                    "window.arexiboPdf.start('m{mid}', '{filename}', {w}, {h}, {dur});");
                add_stop = format!(
                    "window.arexiboPdf.stop('m{mid}');");
            }
            (_, Some("image")) => {
                let filename = opts.find("uri").context("no image uri")?.text();
                writeln!(self.out, "<img class='media r{rid}' id='m{mid}' src='{filename}' \
                                    style='left: {x}px; top: {y}px; width: {w}px; \
                                    height: {h}px;{}{}'>",
                         object_fit(opts), object_pos(opts))?;
            }
            (_, Some("video" | "localvideo")) => {
                let url = percent_decode(opts.find("uri").context("no video uri")?.text());
                let mute = opts.find("mute").is_some_and(|el| el.text() == "1");
                // BUG fix (found from a real report: video "loops, but
                // with quite a long pause between each loop"). This
                // widget's own `<loop>` option (distinct from the
                // *region's* `<loop>`, see write_region) was never read
                // at all before -- every video always had its `duration`
                // overridden to `() => video.duration` regardless, so
                // even a video meant to loop natively would, after
                // exactly one play-through, get caught by
                // region_switch's timer, which would hide/re-show it
                // (re-triggering `.play()` from scratch) -- a
                // JS-driven, timer-mediated restart with real overhead
                // (event round-trip, re-reading `.duration`, DOM
                // updates), instead of the browser's own native video
                // decoder looping seamlessly with the HTML `loop`
                // attribute. Now: `loop=1` sets that attribute (letting
                // the browser handle actual repetition with no JS
                // involvement at all) and *keeps* the widget's own
                // static XLF-declared duration (already `duration`'s
                // default value at this point, same as any other
                // non-video widget type) instead of overriding it with
                // the single-playthrough `video.duration` -- so
                // region_switch only moves on once the widget's own
                // intended on-screen time has elapsed, not after just
                // one internal loop iteration.
                let loop_video = opts.find("loop").is_some_and(|el| el.text().trim() == "1");
                writeln!(self.out, "<video class='media r{rid}' id='m{mid}' src='{url}' {} {} \
                                    style='left: {x}px; top: {y}px; width: {w}px; \
                                    height: {h}px;{}{}'></video>",
                         if mute { "muted" } else { "" },
                         if loop_video { "loop" } else { "" },
                         object_fit(opts), object_pos(opts))?;
                // BUG fix (found from a real report: "Playlist with videos
                // does not obey play time duration set in playlist video
                // properties"). The CMS's own `useDuration` attribute on
                // the <media> node (CONFIRMED REAL, from an official
                // documentation XLF example:
                // `<media type="image" duration="300" useDuration="1">`
                // vs `duration="10" useDuration="0">`) governs exactly
                // this: "1" means play for the CMS-configured `duration`
                // seconds regardless of the video's own natural length
                // (cutting it short, or holding past it); "0" (or absent)
                // means play for the video's own natural length instead,
                // with `duration` only a fallback default. The
                // native-`ended`-event approach below was previously used
                // unconditionally for any non-looping video, silently
                // ignoring an explicit useDuration="1" override entirely.
                let use_duration = media.get_attr("useDuration").is_some_and(|v| v == "1");
                if loop_video || use_duration {
                    add_start = format!("document.getElementById('m{mid}').play();");
                    // `duration` already defaults to the CMS-configured
                    // XLF `duration` attribute (set at the top of this
                    // function, same as any other non-video widget type)
                    // -- nothing further to do here for the useDuration=1
                    // case; for loop_video, the native `loop` attribute on
                    // the <video> element above handles actual repetition,
                    // this just needs *a* duration to know when to move on.
                } else {
                    // BUG fix (found from a real report: a non-looping
                    // video "seems stuck, a screenshot always shows the
                    // same frame"). Reading `video.duration`
                    // *synchronously*, in the very same tick as calling
                    // `.play()`, is unreliable -- video metadata loads
                    // *asynchronously* (the `loadedmetadata` event fires
                    // later), so `.duration` very commonly still reads
                    // as `NaN` at that exact moment. Since
                    // `region_switch`'s `let duration = media[next][2]()
                    // || 1;` treats a NaN result as falsy, this silently
                    // fell back to a **1-second** timer -- meaning the
                    // video got restarted from scratch roughly every
                    // second, often before it had made any real visible
                    // progress at all, giving the appearance of being
                    // frozen on its first frame forever. Fixed by using
                    // the video's own reliable native `ended` event as
                    // the actual mechanism to advance (fired by the
                    // browser only once real playback has genuinely
                    // finished, no race condition), rather than trying
                    // to predict the duration up front. The `duration`
                    // function below is now only a *safety-net* timeout
                    // (24h) in case `ended` never fires for some reason
                    // (e.g. a corrupt file) -- not the primary driver.
                    add_start = format!(
                        "{{ let el = document.getElementById('m{mid}'); \
                           el.play(); \
                           el.onended = () => window.arexibo.region_switch({rid}, -1, false); }}");
                    duration = "() => 86400".to_string();
                }
            }
            (_, Some("audio")) => {
                // Standalone Audio widget (as opposed to audio attached to
                // another widget, which the CMS embeds as <audio> tags
                // inside that widget's own generated HTML and is therefore
                // already handled by the resource/iframe path above).
                // Modeled 1:1 on the video arm above: FLAGGED AS UNVERIFIED
                // -- the `uri`/`mute` option names are carried over from the
                // video module by analogy (same underlying Xibo media
                // options schema) and confirmed only via the XLF developer
                // docs stating a `loop`/`volume` pair also exists for Audio
                // nodes; `loop`/`volume` are not yet wired up here (video
                // doesn't handle them either -- same pre-existing gap,
                // left alone to stay in scope). Verify against a real CMS
                // audio widget before relying on mute/volume behavior.
                let url = percent_decode(opts.find("uri").context("no audio uri")?.text());
                let mute = opts.find("mute").is_some_and(|el| el.text() == "1");
                writeln!(self.out, "<audio class='media r{rid}' id='m{mid}' src='{url}' {}\
                                    ></audio>",
                         if mute { "muted" } else { "" })?;
                add_start = format!("document.getElementById('m{mid}').play();");
                duration = format!("() => document.getElementById('m{mid}').duration");
            }
            (_, Some("shellcommand")) => {
                writeln!(self.out, "<div class='media r{rid}' id='m{mid}' \
                                    style='left: {x}px; top: {y}px; width: {w}px; \
                                    height: {h}px;'></div>")?;

                let is_cmd = opts.req_child("commandType")? == "storedCommand";
                if is_cmd {
                    let code = opts.req_child("commandCode")?;
                    add_start = format!("window.arexiboGui.jsCommand({code:?});");
                } else {
                    let with_shell = opts.req_child("launchThroughCmd")? == "1";
                    let cmd = if opts.req_child("useGlobalCommand")? == "1" {
                        opts.req_child("globalCommand")?
                    } else {
                        opts.req_child("linuxCommand")?
                    };
                    add_start = format!("window.arexiboGui.jsShell({cmd:?}, {with_shell});");

                    let kill = if opts.req_child("terminateCommand")? == "1" {
                        if opts.req_child("useTaskkill")? == "1" { 2 } else { 1 }
                    } else { 0 };
                    add_stop = format!("window.arexiboGui.jsStopShell({kill});");
                }
            }
            (_, Some("ssp")) => {
                // Adspace Exchange, widget-level activation -- see
                // adspace.rs's module-level doc comment for the very
                // significant caveats on this whole feature (untested
                // against a real exchange). Resolved *synchronously*
                // here during translate() -- consistent with the rest
                // of this codebase's blocking-HTTP style (resource.rs's
                // own downloads are blocking too), but does mean a
                // slow/unreachable ad exchange measurably delays
                // translating this one layout. A missing bid ("no
                // fill", a very normal outcome in ad serving, not
                // necessarily an error) or any other failure simply
                // skips the widget entirely (same convention as any
                // other unsupported/unavailable media in this match),
                // rather than failing the whole layout's translation.
                let Some(cfg) = &self.adspace else {
                    log::warn!("ssp widget {mid} skipped: Adspace Exchange is not enabled \
                                on this display");
                    return Ok(None);
                };
                match crate::adspace::resolve_widget_ad(cfg, w) {
                    Ok(creative) => {
                        // Server-relative URL: `download_creative` always
                        // stores into `cfg.cache_dir`, which the caller
                        // (resource::Cache) always sets to
                        // `<cache root>/adspace` -- so the file is
                        // reachable, relative to the cache root the
                        // embedded HTTP server (server.rs) actually
                        // serves from, as "adspace/<filename>".
                        let fname = creative.local_path.file_name()
                            .context("creative path has no filename")?
                            .to_string_lossy();
                        let url = format!("adspace/{fname}");
                        if creative.mime_type.starts_with("video/") {
                            writeln!(self.out, "<video class='media r{rid}' id='m{mid}' \
                                                src='{url}' \
                                                style='left: {x}px; top: {y}px; width: {w}px; \
                                                height: {h}px; object-fit: contain;'></video>")?;
                            add_start = format!("document.getElementById('m{mid}').play();");
                        } else {
                            writeln!(self.out, "<img class='media r{rid}' id='m{mid}' \
                                                src='{url}' \
                                                style='left: {x}px; top: {y}px; width: {w}px; \
                                                height: {h}px; object-fit: contain;'>")?;
                        }
                        if let Some(d) = creative.duration {
                            duration = format!("() => {d}");
                        }
                    }
                    Err(e) => {
                        log::warn!("ssp widget {mid}: no ad shown (bid/creative resolution \
                                    failed, possibly just a no-fill): {e:#}");
                        return Ok(None);
                    }
                }
            }
            _ => {
                log::warn!("unsupported media type: {:?}", media.get_attr("type"));
                return Ok(None);
            }
        }
        Ok(Some((mid, duration, add_start, add_stop, fade_in, fade_out, transition_ms)))
    }
}

fn object_fit(el: &Element) -> &'static str {
    match el.find("scaleType") {
        Some(e) if e.text() == "stretch" => " object-fit: fill;",
        _ => " object-fit: contain;",
    }
}

fn object_pos(el: &Element) -> &'static str {
    match (el.def_attr("align", "center"), el.def_attr("halign", "middle")) {
        ("left", "top") => " object-position: left top;",
        ("left", "bottom") => " object-position: left bottom;",
        ("left", _) => " object-position: left;",
        ("right", "top") => " object-position: right top;",
        ("right", "bottom") => " object-position: right bottom;",
        ("right", _) => " object-position: right;",
        (_, "top") => " object-position: top;",
        (_, "bottom") => " object-position: bottom;",
        _ => "",
    }
}






#[cfg(test)]
mod transition_tests {
    use super::*;
    use std::io::Read;

    fn translate_xlf(xlf: &str) -> String {
        let dir = tempdir();
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        // Same fix as per_widget_transition_tests below: tests run in
        // parallel within the same process, so process::id() alone
        // isn't unique enough between them.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_layout_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    const NO_TRANSITION_XLF: &str = r#"<layout width="1080" height="1920">
        <region id="1" left="0" top="0" width="500" height="500">
            <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
            <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
        </region>
    </layout>"#;

    #[test]
    fn no_transition_configured_is_instant_as_before() {
        let html = translate_xlf(NO_TRANSITION_XLF);
        // Exact previous-behavior lines must still be present verbatim
        // -- this is a regression guard: the overwhelming majority of
        // real-world layouts don't configure a transition at all, and
        // must render identically to before this feature existed.
        assert!(html.contains("document.getElementById('m9001').style.visibility = 'visible';"));
        assert!(html.contains("document.getElementById('m9001').style.visibility = 'hidden'; "));
        assert!(!html.contains("transition = 'opacity"));
    }

    #[test]
    fn fade_in_only_affects_show_not_hide() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <options><transitionType>fadeIn</transitionType><transitionDuration>500</transitionDuration></options>
                <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
                <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("transition = 'opacity 500ms'"));
        // hide path must remain the plain instant one (fadeIn doesn't
        // apply to hiding)
        assert!(html.contains("document.getElementById('m9001').style.visibility = 'hidden'; "));
    }

    #[test]
    fn fade_out_only_affects_hide_not_show() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <options><transitionType>fadeOut</transitionType><transitionDuration>800</transitionDuration></options>
                <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
                <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("transition = 'opacity 800ms'"));
        assert!(html.contains("setTimeout(() => { el.style.visibility = 'hidden'; }, 800)"));
        // show path must remain the plain instant one (fadeOut doesn't
        // apply to showing)
        assert!(html.contains("document.getElementById('m9001').style.visibility = 'visible';"));
    }

    #[test]
    fn zero_duration_transition_is_treated_as_no_transition() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <options><transitionType>fadeOut</transitionType><transitionDuration>0</transitionDuration></options>
                <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
                <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(!html.contains("transition = 'opacity"));
    }

    #[test]
    fn fly_transition_falls_back_to_instant_and_does_not_error() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <options><transitionType>fly</transitionType><transitionDuration>500</transitionDuration>
                <transitionDirection>N</transitionDirection></options>
                <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
                <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        // Must not panic/error -- falls back gracefully to instant switch.
        let html = translate_xlf(xlf);
        assert!(!html.contains("transition = 'opacity"));
        assert!(html.contains("document.getElementById('m9001').style.visibility = 'visible';"));
    }
}

#[cfg(test)]
mod per_widget_transition_tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn translate_xlf(xlf: &str) -> String {
        // Tests run in parallel *within the same process*, so
        // `std::process::id()` alone collides between them (all tests
        // sharing one PID would race on the same temp files) -- an
        // atomic counter makes each call's directory unique regardless.
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_pw_transition_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    #[test]
    fn per_widget_trans_out_fadeout_overrides_empty_region_level() {
        // Mirrors the real XLF the user shared: region-level transition
        // options present but empty, widget carries its own
        // transIn=fly (not implemented) / transOut=fadeOut (implemented).
        let xlf = r#"<layout width="720" height="1280">
            <region id="3488" left="0" top="0" width="250" height="250">
                <options><loop>0</loop><transitionDirection/><transitionDuration/><transitionType/></options>
                <media id="3045" type="image" duration="1">
                    <options><uri>100.png</uri>
                        <transIn>fly</transIn><transInDuration>10000</transInDuration><transInDirection>E</transInDirection>
                        <transOut>fadeOut</transOut><transOutDuration>10000</transOutDuration><transOutDirection>E</transOutDirection>
                    </options>
                </media>
                <media id="3046" type="image" duration="10">
                    <options><uri>105.png</uri>
                        <transIn>fly</transIn><transInDuration>10000</transInDuration><transInDirection>E</transInDirection>
                        <transOut>fadeOut</transOut><transOutDuration>10000</transOutDuration><transOutDirection>E</transOutDirection>
                    </options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // fadeOut with the widget's own 10000ms duration must be applied
        // despite the region-level trio being empty.
        assert!(html.contains("transition = 'opacity 10000ms'"));
        assert!(html.contains("setTimeout(() => { el.style.visibility = 'hidden'; }, 10000)"));
        // "fly" in-transition is not implemented -- show stays instant.
        assert!(html.contains("document.getElementById('m3045').style.visibility = 'visible';"));
    }

    #[test]
    fn per_widget_overrides_region_level_default() {
        // Region has its own real fadeIn default; widget explicitly
        // opts for fadeOut instead -- widget-level must win.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><transitionType>fadeIn</transitionType><transitionDuration>2000</transitionDuration></options>
                <media id="4001" type="image" duration="5">
                    <options><uri>a.png</uri>
                        <transOut>fadeOut</transOut><transOutDuration>500</transOutDuration>
                    </options>
                </media>
                <media id="4002" type="image" duration="5">
                    <options><uri>b.png</uri></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // widget 4001 has its own transOut -> uses 500ms fadeOut, NOT
        // the region's 2000ms fadeIn. widget 4002 (no override) legitimately
        // falls back to the region's 2000ms fadeIn -- so "opacity 2000ms"
        // DOES appear in the file overall (for widget 4002's own block),
        // just not within widget 4001's own generated function block
        // specifically, which is what actually needs checking here.
        // Widget 4001's tuple is generated first and ends distinctly in
        // `4001],` -- slicing up to (and including) that marker isolates
        // exactly its own block, and nothing from widget 4002's.
        assert!(html.contains("setTimeout(() => { el.style.visibility = 'hidden'; }, 500)"));
        let end = html.find("4001],").unwrap() + "4001],".len();
        let widget_4001_block = &html[..end];
        assert!(!widget_4001_block.contains("opacity 2000ms"),
                "widget 4001's own block should not use the region's fadeIn duration");
    }

    #[test]
    fn widget_without_own_transition_falls_back_to_region_default() {
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><transitionType>fadeIn</transitionType><transitionDuration>2000</transitionDuration></options>
                <media id="4001" type="image" duration="5">
                    <options><uri>a.png</uri></options>
                </media>
                <media id="4002" type="image" duration="5">
                    <options><uri>b.png</uri></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("opacity 2000ms"));
    }
}



#[cfg(test)]
mod zindex_regression_test {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    #[test]
    fn fade_out_bumps_zindex_so_it_stays_on_top_while_fading() {
        // Regression test for a real bug: without this, the *incoming*
        // widget (shown instantly, no fade-in of its own) paints on top
        // of the fading-out widget from frame one (DOM order stacking,
        // see LAYOUT_CSS's `.media { position: absolute; }` with no
        // explicit z-index), completely hiding the fade -- reported by
        // the user as "no fading at all" on a real layout even though
        // the opacity animation itself was separately verified working.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_zidx_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><transitionType>fadeOut</transitionType><transitionDuration>800</transitionDuration></options>
                <media id="9101" type="image" duration="2"><options><uri>a.png</uri></options></media>
                <media id="9102" type="image" duration="2"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        assert!(html.contains("el.style.zIndex = '9999'"),
                "fadeOut must bump z-index so the fade is actually visible, \
                 not hidden behind the already-shown next widget");
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn translate_xlf(xlf: &str) -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_loop_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    #[test]
    fn video_with_loop_1_gets_native_loop_attribute_and_static_duration() {
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5001" type="video" duration="60"><options><uri>a.mp4</uri><loop>1</loop></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("<video class='media r1' id='m5001' src='a.mp4'  loop"));
        // must NOT override duration with video.duration when looping --
        // the widget's own static 60s duration must be used instead.
        assert!(!html.contains("document.getElementById('m5001').duration"));
        assert!(html.contains("() => 60"));
    }

    #[test]
    fn video_without_loop_uses_ended_event_not_synchronous_duration_read() {
        // Regression test for a real bug: reading video.duration
        // synchronously right after play() is unreliable (metadata
        // loads asynchronously, often still NaN at that point), which
        // silently fell back to a 1-second timer and made the video
        // restart before it had made any real progress -- appearing
        // permanently stuck on its first frame. Fixed by using the
        // reliable native `ended` event instead.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5002" type="video" duration="0"><options><uri>a.mp4</uri><loop>0</loop></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(!html.contains(" loop "));
        // must NOT read video.duration synchronously anymore
        assert!(!html.contains("document.getElementById('m5002').duration"));
        // must use the native 'ended' event to advance instead
        assert!(html.contains("el.onended = () => window.arexibo.region_switch(1, -1, false);"));
        assert!(html.contains("() => 86400"));
    }

    #[test]
    fn region_switch_only_freezes_on_wrap_for_single_item_regions() {
        // Regression test for a real bug report: "Playlist with 3
        // pictures slideshow timed for 3 seconds each freezes on last
        // picture after 1st run." CONFIRMED via official Xibo
        // documentation: "The Loop option is only applicable when
        // there is only 1 media item in the region" -- verified with a
        // real functional QtWebEngine run during development that a
        // 3-item region with no loop set genuinely keeps cycling
        // (0 -> 1 -> 2 -> 0 -> 1 -> ...) rather than freezing after the
        // first pass; this test checks the generated condition itself.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="1" type="image" duration="3"><options><uri>a.png</uri></options></media>
                <media id="2" type="image" duration="3"><options><uri>b.png</uri></options></media>
                <media id="3" type="image" duration="3"><options><uri>c.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("if (next == 0 && !first && !loop && total <= 1)"));
    }

    #[test]
    fn video_with_use_duration_1_respects_configured_duration_even_without_loop() {
        // Regression test for a real bug report: "Playlist with videos
        // does not obey play time duration set in playlist video
        // properties in CMS backend." Before this fix, any non-looping
        // video always used the native `ended` event to advance,
        // completely ignoring an explicit useDuration="1" + a configured
        // duration shorter (or longer) than the video's own natural
        // length.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5003" type="video" duration="15" useDuration="1">
                    <options><uri>a.mp4</uri><loop>0</loop></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // must NOT be treated as looping (no `loop` HTML attribute) --
        // useDuration=1 without loop still just plays once, but for the
        // *configured* duration, not however long the video naturally is.
        assert!(!html.contains(" loop "));
        // must use the CMS-configured duration, not the ended-event/86400
        // safety-net fallback.
        assert!(html.contains("() => 15"));
        assert!(!html.contains("() => 86400"));
        assert!(!html.contains("el.onended"));
        assert!(html.contains("document.getElementById('m5003').play();"));
    }

    #[test]
    fn region_loop_option_is_wired_into_js_object() {
        let xlf_loop = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><loop>1</loop></options>
                <media id="6001" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="6002" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf_loop);
        assert!(html.contains("loop: true,"));

        let xlf_noloop = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><loop>0</loop></options>
                <media id="6001" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="6002" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf_noloop);
        assert!(html.contains("loop: false,"));

        // absent <loop> entirely -> defaults to false (matches the "0
        // or absent = don't loop" convention)
        let xlf_absent = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="6001" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="6002" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf_absent);
        assert!(html.contains("loop: false,"));
    }
}



#[cfg(test)]
mod action_tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn translate_xlf(xlf: &str) -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_action_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    #[test]
    fn touch_action_also_binds_a_keydown_listener_for_triggercode() {
        // Regression test for a real bug report: "Interactive layout
        // button... did not recognize keyboard space key press... but
        // touch is recognized." Confirmed via a real XLF sample from
        // the CMS that Xibo's "Key Press" trigger (4.4+) reuses
        // triggerType="touch" with triggerCode carrying a keyboard key
        // name (e.g. "Space") as an *alternative* way to fire the same
        // action -- this was previously never implemented, triggerCode
        // was read only for the (unrelated) webhook action type.
        let xlf = r#"<layout width="1080" height="1920" code="defaultxibomultimedia">
            <action layoutCode="test1" target="screen" source="layout"
                    actionType="navLayout" triggerType="touch" triggerCode="Space"
                    id="756" targetId="780" sourceId="780"/>
            <region id="1" left="0" top="0" width="1080" height="1920">
                <media id="100" type="image" duration="5"><options><uri>a.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("document.addEventListener('keydown'"),
                "must bind a keydown listener for the touch+triggerCode Key Press feature");
        assert!(html.contains("e.code === \"Space\""));
        // the existing click/touch handling (source=="layout" -> whole
        // body) must still be present too -- this is an *addition*, not
        // a replacement.
        assert!(html.contains("document.body.addEventListener('click'"));
    }

    #[test]
    fn navlayout_prefers_targetid_over_stale_layoutcode() {
        // Regression test for a real bug report/log: a real XLF sample
        // had targetId="780" (a valid, correct numeric layout id --
        // this layout's own id) but layoutCode="test1", which wasn't
        // present in code_map for the current collection -- the action
        // failed entirely ("unknown layout code") even though targetId
        // alone was already sufficient.
        let xlf = r#"<layout width="1080" height="1920" code="defaultxibomultimedia">
            <action layoutCode="test1" target="screen" source="layout"
                    actionType="navLayout" triggerType="touch" triggerCode="Space"
                    id="756" targetId="780" sourceId="780"/>
            <region id="1" left="0" top="0" width="1080" height="1920">
                <media id="100" type="image" duration="5"><options><uri>a.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // targetId (780) must be used directly as the resolved layout
        // id, not fail on the stale/unassigned layoutCode.
        assert!(html.contains("performAction(\"navLayout\", \"screen\", 780, 780)"));
    }

    #[test]
    fn navlayout_falls_back_to_layoutcode_when_targetid_absent() {
        // The layoutCode-based resolution path must still work for the
        // case it was originally written for: no usable targetId at
        // all, only a layoutCode to resolve via the code map.
        let xlf = r#"<layout width="1080" height="1920" code="mainlayout">
            <action layoutCode="othercode" target="screen" source="layout"
                    actionType="navLayout" triggerType="touch" triggerCode="Space" id="1"/>
            <region id="1" left="0" top="0" width="1080" height="1920">
                <media id="100" type="image" duration="5"><options><uri>a.png</uri></options></media>
            </region>
        </layout>"#;
        let dir = std::env::temp_dir().join(format!("arexibo_action_fallback_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let mut map = HashMap::new();
        map.insert("othercode".to_string(), 999i64);
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        assert!(html.contains("performAction(\"navLayout\", \"screen\", 0, 999)"));
    }
}



#[cfg(test)]
mod html_sharding_tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn translate_xlf_with_port(xlf: &str, port: u16) -> String {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_shard_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, port).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(&html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    #[test]
    fn html_widget_uses_absolute_sharded_url_with_given_port() {
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="9101" type="text" duration="10"><options></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf_with_port(xlf, 12345);
        // mid 9101 % 4 = 1 -> shard 1+1 = 2
        assert!(html.contains("src='http://127.0.0.2:12345/9101.html"),
                "expected shard 2 for mid 9101, got: {html}");
        assert!(html.contains("arexiboShrinkW=") && html.contains("arexiboShrinkH="));
        // old-style parent-side call must be gone
        assert!(!html.contains("autoShrinkIframe"));
    }

    #[test]
    fn different_widget_ids_land_on_different_shards() {
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="1000" type="text" duration="10"><options></options></media>
            </region>
            <region id="2" left="300" top="0" width="250" height="250">
                <media id="1001" type="ticker" duration="10"><options></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf_with_port(xlf, 8080);
        // 1000 % 4 = 0 -> shard 1; 1001 % 4 = 1 -> shard 2
        assert!(html.contains("src='http://127.0.0.1:8080/1000.html"));
        assert!(html.contains("src='http://127.0.0.2:8080/1001.html"));
    }

    #[test]
    fn shard_assignment_is_stable_across_all_four_values() {
        // mid values chosen to land on each of the 4 shards deterministically.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="100" height="100">
                <media id="4000" type="text" duration="5"><options></options></media>
            </region>
            <region id="2" left="100" top="0" width="100" height="100">
                <media id="4001" type="text" duration="5"><options></options></media>
            </region>
            <region id="3" left="200" top="0" width="100" height="100">
                <media id="4002" type="text" duration="5"><options></options></media>
            </region>
            <region id="4" left="300" top="0" width="100" height="100">
                <media id="4003" type="text" duration="5"><options></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf_with_port(xlf, 9999);
        assert!(html.contains("src='http://127.0.0.1:9999/4000.html")); // 4000%4=0 -> shard1
        assert!(html.contains("src='http://127.0.0.2:9999/4001.html")); // 4001%4=1 -> shard2
        assert!(html.contains("src='http://127.0.0.3:9999/4002.html")); // 4002%4=2 -> shard3
        assert!(html.contains("src='http://127.0.0.4:9999/4003.html")); // 4003%4=3 -> shard4
    }
}


