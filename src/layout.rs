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

    // Loop only applies to single-item regions (confirmed in official
    // Xibo docs) -- a region with 2+ items must keep cycling regardless.
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

  // navWidgetFromDrawer: temporarily swaps a drawer widget (see
  // Rust-side drawer_widgets's own doc comment -- a widget that's
  // never part of any region's own normal item cycling) into a
  // *target* region in place of whatever it's currently showing,
  // for the drawer widget's own configured duration, then reverts.
  // Confirmed real behavior (Xibo's own developer docs): "it should
  // run for its whole duration and on expiry the player should
  // reload its previous state... If a Widget has been loaded its
  // previous state would be the prior widget in that region" -- the
  // real C# client's own equivalent is RegionChangeToWidget.
  // Deliberately doesn't try to track/restore the interrupted
  // widget's own *remaining* time on revert -- it just restarts that
  // widget's own full duration fresh, a reasonable simplification
  // (the official client's own docs only promise resuming the same
  // widget, not preserving exact elapsed time).
  //
  // Resizes to the *target* region rather than the drawer's own
  // native size, confirmed real (Xibo issue #2429: "we need to see
  // if the intended region has been configured and if so go and get
  // the width/height of that region instead of the drawer") --
  // implemented here by actually reparenting the widget's own DOM
  // element into the target region's own wrapper (moved back to the
  // drawer's own wrapper on revert) and overriding its own
  // left/top/width/height to fill whichever wrapper currently
  // contains it, rather than computing/duplicating each region's own
  // geometry again here -- the wrapper's own real size (already
  // correct, written once at translation time) does that for free.
  navWidgetFromDrawer: function(rid, drawerWidgetId) {
    const region = this.regions[rid];
    if (!region) {
      console.warn('navWidgetFromDrawer: unknown target region ' + rid);
      return;
    }
    const drawerItem = this.drawerWidgets && this.drawerWidgets[drawerWidgetId];
    if (!drawerItem) {
      console.warn('navWidgetFromDrawer: unknown drawer widget ' + drawerWidgetId);
      return;
    }
    window.clearTimeout(region.timeoutid);
    const prevCur = region.cur;
    if (prevCur !== null) region.media[prevCur][1](); // hide whatever's currently shown

    const el = document.getElementById('m' + drawerWidgetId);
    const targetWrapper = document.querySelector('.r' + rid);
    if (el && targetWrapper) {
      targetWrapper.appendChild(el);
      el.style.left = '0px';
      el.style.top = '0px';
      el.style.width = '100%';
      el.style.height = '100%';
    } else {
      console.warn('navWidgetFromDrawer: could not find element m' + drawerWidgetId +
                    ' or target wrapper .r' + rid + ' to resize into');
    }

    const [show, hide, duration] = drawerItem;
    show();
    // `duration` is a `() => N` function, same shape as a normal
    // region item's own media[cur][2] (see region_switch's own
    // `media[next][2]()` call) -- not a raw number. Found from a real
    // report: treating it as a plain value here made durationMs come
    // out NaN (a function times 1000), which setTimeout then likely
    // clamped to ~0 -- the widget swapped in and immediately back out
    // again, faster than visibly perceptible, looking exactly like
    // "nothing happened".
    const durationMs = (duration() || 1) * 1000;
    region.timeoutDuration = durationMs;
    region.timeoutStart = Date.now();
    region.timeoutid = window.setTimeout(() => {
      hide();
      // Move the widget's own DOM element back to the drawer -- its
      // own left/top/width/height stay overridden to the *previous*
      // target region's own size until this same widget is swapped
      // in again (harmless: it's hidden, and the next
      // navWidgetFromDrawer call for it always re-applies the sizing
      // above regardless of whatever it was set to before).
      const drawerWrapper = document.querySelector('.drawer');
      if (el && drawerWrapper) {
        drawerWrapper.appendChild(el);
      }
      // Resume the region's own normal cycling by re-showing
      // whichever item was active before this temporary swap-in --
      // `first: true` so this specific re-entry into index 0 (if
      // that's what it was) isn't mistaken for a genuine full cycle
      // completing again (see region_switch's own "region_done"
      // check).
      this.region_switch(rid, prevCur === null ? 0 : prevCur, true);
    }, durationMs);
  },
};
"##;


/// Compass direction for a "fly" transition -- matches the 8 values
/// the CMS itself offers (confirmed in the real CMS source,
/// lib/Controller/Widget.php's own compassPoints array: N/NE/E/SE/S/
/// SW/W/NW).
#[derive(Clone, Copy, PartialEq, Debug)]
enum FlyDir { N, Ne, E, Se, S, Sw, W, Nw }

impl FlyDir {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "N" => Some(Self::N), "NE" => Some(Self::Ne), "E" => Some(Self::E),
            "SE" => Some(Self::Se), "S" => Some(Self::S), "SW" => Some(Self::Sw),
            "W" => Some(Self::W), "NW" => Some(Self::Nw),
            _ => None,
        }
    }

    /// (dx%, dy%) -- a CSS translate() offset, as a percentage of the
    /// element's own size, that moves it fully off-screen in this
    /// compass direction.
    ///
    /// SEMANTICS NOTE: the CMS's own precise meaning of "which way N
    /// moves content" isn't documented anywhere accessible (checked
    /// the real CMS source, including its Designer preview JS -- found
    /// the 8 compass values, not a clear animation-direction spec).
    /// Uses the most intuitive convention: for an in-transition, the
    /// widget arrives *from* this direction; for out, it exits
    /// *toward* it. If a layout's fly direction looks mirrored vs the
    /// CMS Designer's preview, it's this convention, not a broken
    /// transition -- worth revisiting if confirmed against a reference
    /// player.
    fn offset(self) -> (i32, i32) {
        match self {
            Self::N  => (0, -100),
            Self::S  => (0, 100),
            Self::E  => (100, 0),
            Self::W  => (-100, 0),
            Self::Ne => (100, -100),
            Self::Se => (100, 100),
            Self::Sw => (-100, 100),
            Self::Nw => (-100, -100),
        }
    }
}

/// What kind of enter/exit animation a widget uses -- `None` means an
/// instant, non-animated show/hide.
#[derive(Clone, Copy, PartialEq)]
enum Trans {
    None,
    Fade,
    Fly(FlyDir),
}

/// (mid, duration expr, add_start, add_stop, trans_in, ms_in, trans_out, ms_out)
/// -- see write_media's transition-resolution logic.
type MediaInfo = (i32, String, String, String, Trans, u32, Trans, u32);

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
    /// Widget ids that live in the layout's own `<drawer>` (a "hidden
    /// reservoir" -- confirmed, Xibo's own developer docs: "a
    /// collection of Widgets with a similar structure to a normal
    /// Region... not shown directly... referenced by Actions elsewhere
    /// in the Layout") rather than any real, directly-shown region --
    /// checked by `write_action`'s own navWidget resolution as a
    /// fallback when a target widget id isn't found in
    /// `widget_regions` at all (a normal region widget). Populated by
    /// `write_drawer`. A plain set, not carrying each widget's own
    /// duration/etc: the generated `navWidgetFromDrawer` JS call looks
    /// that up at runtime instead, from `window.arexibo.drawerWidgets`
    /// (itself populated by `write_drawer` too) -- this field only
    /// needs to answer "is this id a known drawer widget at all".
    drawer_widgets: std::collections::HashSet<i32>,
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
                  widget_regions: HashMap::new(), drawer_widgets: std::collections::HashSet::new(),
                  region_geom: HashMap::new(),
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
        // The drawer's own widgets can carry actions too (confirmed,
        // Xibo's own developer docs: "Parse the XLF for Action Nodes
        // on Layouts, Regions, the Drawer and Widgets") -- collected
        // the same way as a normal region's own nested actions, above.
        if let Some(drawer) = tree.find("drawer") {
            actions.extend(drawer.find_all("action"));
            for media in drawer.find_all("media") {
                actions.extend(media.find_all("action"));
            }
            if let Err(e) = self.write_drawer(drawer) {
                log::error!("layout: could not translate drawer: {:#}", e);
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
            if let Some((rid, index)) = self.widget_regions.get(&widget_id).copied() {
                format!("window.arexibo.navWidget({rid}, {index})")
            } else if self.drawer_widgets.contains(&widget_id) {
                // A drawer widget (see drawer_widgets's own doc
                // comment) -- not part of any region's own item
                // cycling, so the normal (region id, index)
                // resolution above doesn't apply. Confirmed real from
                // a live layout: this kind of action also carries its
                // own targetId, naming the region to temporarily
                // swap this widget into (see navWidgetFromDrawer's
                // own doc comment in the runtime JS) -- unlike the
                // generic target/targetId handling in the `else`
                // branch just below, which doesn't apply here either
                // (this whole branch is navWidget-specific).
                let targetid: i32 = el.parse_attr("targetId")
                    .context("navWidget targeting a drawer widget needs a targetId \
                              (the region to temporarily show it in)")?;
                format!("window.arexibo.navWidgetFromDrawer({targetid}, {widget_id})")
            } else {
                anyhow::bail!("navWidget: unknown target widget id {widget_id} (neither \
                               a region widget nor a drawer widget)");
            }
        } else {
            let target = el.def_attr("target", "screen");
            let targetid = el.def_attr("targetId", "0").parse::<i64>()
                .context("bad targetId")?;
            let mut layoutid = 0;
            if action == "navLayout" {
                layoutid = self.code_map.get(layoutcode).copied()
                        .context("unknown layout code")?;
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
            // Key Press trigger (CMS 4.4+): same triggerType="touch"
            // action, with triggerCode carrying a keyboard key name
            // (KeyboardEvent.code) as an alternative to touch/click.
            // Safe to always add: a non-matching triggerCode simply
            // never fires.
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

        // Fixed, stationary wrapper (real geometry + overflow: hidden)
        // for the "fly" transition -- widgets inside use relative
        // (0,0,100%,100%) coordinates (see media_geom below) so a fly
        // transform moves them only within this clipped viewport.
        // MUST open before the write_media() loop below, not after --
        // otherwise each widget's own HTML ends up as a preceding
        // sibling of the wrapper, not nested inside it, breaking its
        // relative positioning. An empty region gets an empty,
        // harmless wrapper pair (see nitems==0 check below).
        writeln!(self.out, "<div class='r{rid}' style='position: absolute; \
                            left: {x}px; top: {y}px; width: {w}px; height: {h}px; \
                            overflow: hidden;'>")?;

        // A region's own <options> can carry <transitionType>
        // (fadeIn/fadeOut/fly), <transitionDuration> (ms), and
        // <transitionDirection> (compass, fly only) -- but a real CMS
        // XLF showed this region-level trio almost always empty, while
        // every widget carries its own transIn/transInDuration/
        // transInDirection and transOut equivalents in its own
        // <options> -- a different, per-widget mechanism, previously
        // unparsed. Both are supported now: the region-level trio is
        // only a fallback default for widgets without their own
        // transIn/transOut (see write_media).
        // Region-level fallback: (in, out), each (Trans, u32). Unlike
        // widget-level transIn/transOut (independent fields), the
        // region has ONE <transitionType> -- "fadeIn"/"fadeOut" apply
        // to just that side, "fly" doesn't distinguish a side so it
        // applies to both with the same direction.
        let (region_in, region_out): ((Trans, u32), (Trans, u32)) =
            region.find("options").map(|opts| {
                let ty = opts.find("transitionType").map(|e| e.text().trim().to_string())
                    .filter(|s| !s.is_empty());
                let ms: u32 = opts.find("transitionDuration")
                    .and_then(|e| e.text().trim().parse().ok()).unwrap_or(0);
                let dir = opts.find("transitionDirection").map(|e| e.text().trim().to_string())
                    .filter(|s| !s.is_empty());
                match ty.as_deref() {
                    Some("fadeIn") if ms > 0 => ((Trans::Fade, ms), (Trans::None, 0)),
                    Some("fadeOut") if ms > 0 => ((Trans::None, 0), (Trans::Fade, ms)),
                    Some("fly") if ms > 0 => {
                        let d = dir.as_deref().and_then(FlyDir::parse)
                            // Matches the CMS's own default when a
                            // direction isn't set (confirmed in the
                            // real CMS source, Layout.php:
                            // getOptionValue('transInDirection', 'E')).
                            .unwrap_or(FlyDir::E);
                        ((Trans::Fly(d), ms), (Trans::Fly(d), ms))
                    }
                    _ => ((Trans::None, 0), (Trans::None, 0)),
                }
            }).unwrap_or(((Trans::None, 0), (Trans::None, 0)));
        // Region's own single-item loop: whether to restart its one
        // media item after showing it once (1) or freeze on it (0) --
        // only applies with exactly one item (see region_switch above).
        // Distinct from a video widget's own <loop> in write_media.
        let region_loop = region.find("options").and_then(|opts| opts.find("loop"))
            .map(|e| e.text().trim() == "1").unwrap_or(false);

        let mut sequence = Vec::new();
        // [0, 0, w, h], not the real [x, y, w, h] -- widgets are
        // positioned relative to the wrapper div above, not the page.
        let media_geom = [0, 0, w, h];
        for media in region.find_all("media") {
            match self.write_media(rid, media_geom, (x, y), media, (region_in, region_out)) {
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
            // Empty, harmless wrapper (opened above, before this
            // function knew whether there'd be anything to show) --
            // still needs its matching close so the HTML stays
            // balanced. See the wrapper's own doc comment above for why
            // this is simpler than deferring whether to open it at all.
            writeln!(self.out, "</div> <!-- end region {rid} wrapper (empty) -->")?;
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
        for (mid, duration, add_start, add_stop, trans_in, ms_in, trans_out, ms_out) in sequence {
            writeln!(self.out, "    [")?;
            self.write_show_stop_functions(mid, &add_start, &add_stop,
                                            trans_in, ms_in, trans_out, ms_out, nitems > 1)?;
            writeln!(self.out, "    , {duration}, {mid}],")?;
        }
        writeln!(self.out, "  ],")?;
        writeln!(self.out, "}};\n</script>")?;
        writeln!(self.out, "</div> <!-- end region {rid} wrapper -->")?;
        self.regions.push(rid);
        Ok(())
    }

    /// Writes the `function() {{ ...show... }}, function() {{ ...hide...
    /// }}` pair (comma-separated, no trailing comma/bracket -- the
    /// caller wraps this appropriately for its own context: a region's
    /// own `media` array entry gets `, {duration}, {mid}]` appended
    /// after this, while a drawer widget's own standalone entry (see
    /// write_drawer) does not) shared between a region's own normal
    /// item cycling (write_region) and a drawer widget's own temporary
    /// swap-in (write_drawer) -- factored out so the two can never
    /// silently drift apart on the actual show/hide behavior (fades,
    /// flies, z-index handling) for what is, from the widget's own
    /// point of view, an identical "become visible" / "become hidden"
    /// pair either way.
    ///
    /// `always_hide` mirrors the region-cycling case's own `nitems > 1`
    /// check (a single-item, non-looping region skips its own hide
    /// logic since nothing else will ever show in its place) -- a
    /// drawer widget always needs real hide logic, since it always
    /// gets swapped back out again later (see navWidgetFromDrawer's
    /// own doc comment), so callers writing a drawer widget's own
    /// functions should always pass `true` here regardless of the
    /// target region's own item count.
    fn write_show_stop_functions(&mut self, mid: i32, add_start: &str, add_stop: &str,
                                  trans_in: Trans, ms_in: u32, trans_out: Trans, ms_out: u32,
                                  always_hide: bool) -> Result<()> {
        writeln!(self.out, "      function() {{")?;
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
        writeln!(self.out, "        if (window.arexiboDebug) console.log(\
                                    'arexibo-show: widget {mid}');")?;
        let el = format!("document.getElementById('m{mid}')");
        match trans_in {
            Trans::Fade => {
                // Per the documented semantics ("the media duration
                // should include the in transition"), this animates
                // *within* the widget's own already-scheduled duration
                // countdown in region_switch -- no timing changes are
                // needed there, only here in how "becoming visible" is
                // actually performed.
                writeln!(self.out, "        {{ let el = {el}; \
                                    el.style.transition = 'opacity {ms_in}ms'; \
                                    el.style.opacity = '0'; \
                                    el.style.visibility = 'visible'; \
                                    void el.offsetWidth; \
                                    el.style.opacity = '1'; }}")?;
            }
            Trans::Fly(dir) => {
                // Arrives *from* this compass direction (see
                // FlyDir::offset's own doc comment for the
                // semantics/uncertainty note): starts translated
                // fully off-screen in that direction, then animates
                // to its natural position. Same "animate within the
                // widget's own duration countdown" timing as fadeIn.
                let (dx, dy) = dir.offset();
                writeln!(self.out, "        {{ let el = {el}; \
                                    el.style.transition = 'none'; \
                                    el.style.transform = 'translate({dx}%, {dy}%)'; \
                                    el.style.visibility = 'visible'; \
                                    void el.offsetWidth; \
                                    el.style.transition = 'transform {ms_in}ms'; \
                                    el.style.transform = 'translate(0%, 0%)'; }}")?;
            }
            Trans::None => {
                writeln!(self.out, "        {el}.style.visibility = 'visible';")?;
            }
        }
        writeln!(self.out, "        {add_start}")?;
        writeln!(self.out, "      }}, function() {{")?;
        // if only one item is present, don't need to hide the others
        if always_hide {
            match trans_out {
                Trans::Fade => {
                    // Fire-and-forget: region_switch already moves
                    // to the next widget immediately, so its own
                    // in-transition overlaps with this fade-out --
                    // a real crossfade, not sequential fade-out-
                    // then-fade-in. Duration countdown isn't
                    // delayed by this timer.
                    //
                    // z-index bump is essential, not cosmetic:
                    // .media elements are position:absolute with
                    // no explicit z-index, stacking in DOM order --
                    // the incoming widget would otherwise paint on
                    // top from frame one, hiding this fade-out
                    // entirely. Bumping keeps the fading-out widget
                    // on top so its decreasing opacity visibly
                    // reveals what's underneath. Not reset
                    // afterwards -- once visibility:hidden, the
                    // element doesn't participate in stacking
                    // regardless of z-index.
                    writeln!(self.out, "        {{ let el = {el}; \
                                        el.style.zIndex = '9999'; \
                                        el.style.transition = 'opacity {ms_out}ms'; \
                                        el.style.opacity = '0'; \
                                        setTimeout(() => {{ el.style.visibility = 'hidden'; }}, \
                                                   {ms_out}); }}")?;
                }
                Trans::Fly(dir) => {
                    // Exits *toward* this compass direction -- same
                    // z-index reasoning as the fade-out case above
                    // (keeps the leaving widget visibly on top while
                    // it flies away, rather than being instantly
                    // hidden behind the incoming one).
                    let (dx, dy) = dir.offset();
                    writeln!(self.out, "        {{ let el = {el}; \
                                        el.style.zIndex = '9999'; \
                                        el.style.transition = 'transform {ms_out}ms'; \
                                        el.style.transform = 'translate({dx}%, {dy}%)'; \
                                        setTimeout(() => {{ el.style.visibility = 'hidden'; \
                                                             el.style.transform = ''; }}, \
                                                   {ms_out}); }}")?;
                }
                Trans::None => {
                    writeln!(self.out, "        {el}.style.visibility = 'hidden'; ")?;
                }
            }
        }
        writeln!(self.out, "        {add_stop}")?;
        writeln!(self.out, "      }}")?;
        Ok(())
    }

    /// Writes the layout's own `<drawer>` -- "a hidden reservoir" of
    /// widgets (see `drawer_widgets`'s own doc comment), each one
    /// meant to be temporarily swapped into a *target region* by a
    /// `navWidget` action rather than ever shown in place. Modeled
    /// closely on write_region's own geometry/wrapper handling (a
    /// `<drawer>` element carries the same width/height/top/left
    /// attributes a `<region>` does, confirmed from a real XLF), but:
    /// - the wrapper stays `display: none` unconditionally (the
    ///   drawer itself is never shown, only individual widgets pulled
    ///   out of it into some *other* region -- see
    ///   navWidgetFromDrawer's own doc comment in the runtime JS);
    /// - widgets are NOT attached to any `window.arexibo.regions[rid]`
    ///   cycling structure (they don't belong to a real, directly-
    ///   shown region at all) -- instead each one's own show/hide
    ///   functions and duration go into `window.arexibo.
    ///   drawerWidgets[mid]`, a flat, standalone entry.
    ///
    /// Dynamic resizing to the target region (confirmed real, Xibo
    /// issue #2429 -- see navWidgetFromDrawer's own doc comment) is
    /// handled entirely at runtime, in JS -- this function only ever
    /// writes each widget once, at the drawer's own native geometry;
    /// navWidgetFromDrawer overrides that at the moment of swapping a
    /// widget in, rather than this function needing to know in
    /// advance which region(s) might eventually reference it.
    fn write_drawer(&mut self, drawer: &Element) -> Result<()> {
        let x = drawer.parse_attr("left")?;
        let y = drawer.parse_attr("top")?;
        let w = drawer.parse_attr("width")?;
        let h = drawer.parse_attr("height")?;
        // A synthetic region id, only ever used for write_media's own
        // `class='media r{rid}'` CSS class and similarly cosmetic
        // bookkeeping -- drawer widgets are deliberately never
        // inserted into `widget_regions` (that's what lets
        // write_action's own navWidget resolution tell a normal
        // region widget and a drawer widget apart), so this can never
        // collide with or be confused for a genuine region id from
        // the XLF.
        const DRAWER_RID: i32 = -1;

        writeln!(self.out, "<!-- drawer -->")?;
        // Deliberately no `display: none` here (a real bug found this
        // way: an ancestor's display:none hides everything inside it
        // regardless of each child's own visibility -- so toggling a
        // drawer widget's own visibility, as navWidgetFromDrawer
        // already correctly did, could never actually make anything
        // appear on screen at all). Every `.media` element already
        // starts `visibility: hidden` by default (see the shared CSS
        // in write_footer) -- the same mechanism that already governs
        // every normal region widget's own show/hide -- so this
        // wrapper needs no visibility handling of its own at all.
        //
        // z-index: 9999 is essential, not cosmetic -- a second real
        // bug found this way: every real region gets its own explicit
        // z-index (see write_region), but this wrapper previously had
        // none at all, meaning it stacked at the implicit/auto level
        // -- *below* any region with an explicit z-index (which is
        // all of them). A drawer widget could become genuinely
        // visible (the display:none fix above) and still never
        // actually be seen, sitting behind every normal region
        // (including a full-screen "global" one) the whole time.
        // Matches the same "temporarily bump above everything" z-index
        // already used for a fade-out transition elsewhere in this
        // file.
        //
        // pointer-events: none is essential too -- a third real bug
        // found this way: `visibility: hidden` on each *child* widget
        // (the default, see the .media CSS rule) only stops that
        // child from being clickable/visible -- it does nothing to
        // stop the *wrapper* itself from being a solid, event-
        // catching layer. Combined with z-index: 9999 covering the
        // full layout, every touch/click across the entire screen was
        // being silently swallowed by this otherwise-invisible
        // wrapper, disabling every interactive control on the layout.
        // A widget reparented into a target region by
        // navWidgetFromDrawer automatically escapes this (CSS
        // inheritance follows the DOM tree -- once moved to a
        // different parent, it's no longer a descendant of this
        // wrapper at all), so no extra per-widget override is needed
        // for it to remain interactive once actually shown.
        writeln!(self.out, "<div class='drawer' style='position: absolute; \
                            left: {x}px; top: {y}px; width: {w}px; height: {h}px; \
                            overflow: hidden; z-index: 9999; pointer-events: none;'>")?;

        // Write every widget's own DOM element (via write_media) FIRST,
        // collecting the show/hide/duration info to register in a
        // *separate*, later <script> block -- a real bug found this
        // way: an earlier version interleaved each widget's own
        // <img>/<iframe> markup with the drawerWidgets[...] = [...]
        // registration *inside a single already-open <script> tag*.
        // Once a browser's HTML parser enters a <script> tag, it treats
        // everything up to the next </script> as raw JS source text,
        // not HTML to keep parsing -- an <img ...> tag appearing there
        // is invalid JS syntax (a real live report confirmed the exact
        // resulting error: "SyntaxError: Unexpected token '<'"), and a
        // syntax error anywhere in a <script> block means *none* of it
        // runs -- not just that one line. Every drawerWidgets[mid]
        // registration silently never executed at all, on every single
        // layout, regardless of the display/z-index/duration fixes
        // that came before this one -- explaining why none of them
        // ever visibly helped. write_region already avoids this
        // exact trap (all of a region's own DOM elements are written
        // by its own write_media loop *before* that region's own
        // <script>/media array ever opens) -- mirrored here.
        let media_geom = [0, 0, w, h];
        let mut widgets = Vec::new();
        for media in drawer.find_all("media") {
            match self.write_media(DRAWER_RID, media_geom, (x, y), media,
                                    ((Trans::None, 0), (Trans::None, 0))) {
                Err(e) => log::error!("layout: could not translate drawer widget: {:#}", e),
                Ok(None) => continue,
                Ok(Some(res)) => widgets.push(res),
            }
        }

        writeln!(self.out, "<script type='text/javascript'>")?;
        writeln!(self.out, "window.arexibo.drawerWidgets = window.arexibo.drawerWidgets || {{}};")?;
        for (mid, duration, add_start, add_stop, trans_in, ms_in, trans_out, ms_out) in widgets {
            self.drawer_widgets.insert(mid);
            writeln!(self.out, "window.arexibo.drawerWidgets[{mid}] = [")?;
            self.write_show_stop_functions(mid, &add_start, &add_stop,
                                            trans_in, ms_in, trans_out, ms_out, true)?;
            writeln!(self.out, ", {duration}];")?;
        }
        writeln!(self.out, "</script>")?;
        writeln!(self.out, "</div> <!-- end drawer -->")?;
        Ok(())
    }

    fn write_media(&mut self, rid: i32, [x, y, w, h]: [i32; 4], (abs_x, abs_y): (i32, i32),
                   media: &Element, region_fallback: ((Trans, u32), (Trans, u32))) -> Result<Option<MediaInfo>> {
        let mid = media.parse_attr("id")?;
        let opts = media.find("options").context("no options")?;
        let mut duration = format!(
            "() => {}", media.def_attr("duration", "").parse::<i32>().unwrap_or(10));
        let mut add_start = String::new();
        let mut add_stop = String::new();

        // Per-widget transition override -- every widget's own
        // <options> can carry transIn/transInDuration/transInDirection
        // and transOut equivalents, different property names from the
        // region-level transitionType trio (see write_region) -- this
        // is the one actually populated by the CMS/editor in practice.
        // Falls back to region_fallback independently per side, only
        // for whichever side this widget doesn't specify its own.
        fn parse_trans(ty: Option<&str>, dir: Option<&str>, ms: u32) -> (Trans, u32) {
            match ty {
                Some("fadeIn") | Some("fadeOut") if ms > 0 => (Trans::Fade, ms),
                Some("fly") if ms > 0 => {
                    let d = dir.and_then(FlyDir::parse).unwrap_or(FlyDir::E);
                    (Trans::Fly(d), ms)
                }
                _ => (Trans::None, 0),
            }
        }
        let trans_in_ty = opts.find("transIn").map(|e| e.text().trim().to_string())
            .filter(|s| !s.is_empty());
        let trans_in_dir = opts.find("transInDirection").map(|e| e.text().trim().to_string())
            .filter(|s| !s.is_empty());
        let trans_in_ms: u32 = opts.find("transInDuration")
            .and_then(|e| e.text().trim().parse().ok()).unwrap_or(0);
        let trans_out_ty = opts.find("transOut").map(|e| e.text().trim().to_string())
            .filter(|s| !s.is_empty());
        let trans_out_dir = opts.find("transOutDirection").map(|e| e.text().trim().to_string())
            .filter(|s| !s.is_empty());
        let trans_out_ms: u32 = opts.find("transOutDuration")
            .and_then(|e| e.text().trim().parse().ok()).unwrap_or(0);

        let ((region_trans_in, region_ms_in), (region_trans_out, region_ms_out)) = region_fallback;
        let (trans_in, ms_in) = if trans_in_ty.is_some() {
            parse_trans(trans_in_ty.as_deref(), trans_in_dir.as_deref(), trans_in_ms)
        } else {
            (region_trans_in, region_ms_in)
        };
        let (trans_out, ms_out) = if trans_out_ty.is_some() {
            parse_trans(trans_out_ty.as_deref(), trans_out_dir.as_deref(), trans_out_ms)
        } else {
            (region_trans_out, region_ms_out)
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
                // Sharded across multiple loopback origins (see
                // server::HTML_SHARD_COUNT) to work around Chromium's
                // 6-connections-per-origin limit -- chosen
                // deterministically from `mid` for stable output.
                // arexiboShrinkW/H (alongside the original w/h, kept for
                // CMS-template compatibility) let the shrink-to-fit
                // script (gui/lib.cpp) recognize arexibo's own resource
                // iframes across origins.
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
                    // abs_x/abs_y here, NOT the (0,0)-relative x/y used
                    // for the placeholder's CSS above -- jsNativeWebShow
                    // drives a separate Qt QWebEngineView in native
                    // window coordinates, with no wrapper div to be
                    // relative to.
                    add_start = format!(
                        "window.arexiboGui.jsNativeWebShow({mid}, {url:?}, {abs_x}, {abs_y}, {w}, {h});");
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
                // loop=1 uses the native HTML loop attribute (browser
                // handles repetition) and keeps the widget's own static
                // XLF duration, instead of a timer-mediated JS restart
                // with a real pause between loops.
                let loop_video = opts.find("loop").is_some_and(|el| el.text().trim() == "1");
                writeln!(self.out, "<video class='media r{rid}' id='m{mid}' src='{url}' {} {} \
                                    style='left: {x}px; top: {y}px; width: {w}px; \
                                    height: {h}px;{}{}'></video>",
                         if mute { "muted" } else { "" },
                         if loop_video { "loop" } else { "" },
                         object_fit(opts), object_pos(opts))?;
                // useDuration="1" (confirmed real CMS attribute): play
                // for the configured `duration` regardless of the
                // video's natural length, instead of always using the
                // native `ended` event.
                let use_duration = media.get_attr("useDuration").is_some_and(|v| v == "1");
                if loop_video || use_duration {
                    add_start = format!("document.getElementById('m{mid}').play();");
                    // `duration` already defaults to the XLF-configured
                    // value; loop's own repetition is handled by the
                    // native `loop` attribute above.
                } else {
                    // Reading `.duration` synchronously right after
                    // `.play()` is unreliable (metadata loads
                    // asynchronously, often still NaN) -- region_switch
                    // would treat that as falsy and restart the video
                    // every ~1s. Use the native `ended` event instead,
                    // with an 86400s duration as a safety-net timeout only.
                    add_start = format!(
                        "{{ let el = document.getElementById('m{mid}'); \
                           el.play(); \
                           el.onended = () => window.arexibo.region_switch({rid}, -1, false); }}");
                    duration = "() => 86400".to_string();
                }
                // Pause+reset when sent to background (regardless of
                // branch above) -- otherwise the video kept playing
                // invisibly, and would resume from wherever it was left
                // off next time instead of restarting cleanly.
                add_stop = format!(
                    "{{ let el = document.getElementById('m{mid}'); \
                       el.pause(); el.currentTime = 0; }}");
            }
            (_, Some("audio")) => {
                // Standalone Audio widget (audio attached to another
                // widget is embedded as <audio> tags inside that
                // widget's own HTML, handled by the resource/iframe path
                // above). Modeled on the video arm above -- FLAGGED AS
                // UNVERIFIED: uri/mute/loop carried over by analogy;
                // `volume` not wired up (same gap as video). Verify
                // against a real CMS audio widget.
                let url = percent_decode(opts.find("uri").context("no audio uri")?.text());
                let mute = opts.find("mute").is_some_and(|el| el.text() == "1");
                // loop/useDuration/ended-event/add_stop below all mirror
                // the video arm 1:1 -- same rationale, same fixes.
                let loop_audio = opts.find("loop").is_some_and(|el| el.text().trim() == "1");
                writeln!(self.out, "<audio class='media r{rid}' id='m{mid}' src='{url}' {} {}\
                                    ></audio>",
                         if mute { "muted" } else { "" },
                         if loop_audio { "loop" } else { "" })?;
                let use_duration = media.get_attr("useDuration").is_some_and(|v| v == "1");
                if loop_audio || use_duration {
                    add_start = format!("document.getElementById('m{mid}').play();");
                } else {
                    add_start = format!(
                        "{{ let el = document.getElementById('m{mid}'); \
                           el.play(); \
                           el.onended = () => window.arexibo.region_switch({rid}, -1, false); }}");
                    duration = "() => 86400".to_string();
                }
                add_stop = format!(
                    "{{ let el = document.getElementById('m{mid}'); \
                       el.pause(); el.currentTime = 0; }}");
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
        Ok(Some((mid, duration, add_start, add_stop, trans_in, ms_in, trans_out, ms_out)))
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

    #[test]
    fn region_gets_a_fixed_overflow_hidden_wrapper_with_widgets_positioned_relatively() {
        // Regression test for a real report: during a "fly" transition,
        // the incoming widget rendered outside its own region's visual
        // area, then jumped into position, instead of sliding in
        // smoothly clipped to the region -- because each widget used to
        // be its own absolutely-positioned element (matching the
        // region's real geometry directly), so a transform-translate
        // moved the whole box with nothing clipping where it travelled
        // through. Confirms the fix: a real geometry + overflow:hidden
        // wrapper div exists, and the widget inside it uses relative
        // (0,0,100%,100%) coordinates instead of the absolute region
        // position.
        let xlf = r#"<layout width="1920" height="1080">
            <region id="42" left="100" top="200" width="300" height="400">
                <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("class='r42' style='position: absolute; \
                                left: 100px; top: 200px; width: 300px; height: 400px; \
                                overflow: hidden;'"),
                "region wrapper must carry the real geometry and overflow:hidden -- got:\n{html}");
        // The widget itself must use relative (0,0,100%->300px/400px)
        // coordinates, not the absolute region position (100,200) --
        // width/height stay the same (that's the widget's own real
        // size), only the offset changes.
        assert!(html.contains("left: 0px; top: 0px; width: 300px; height: 400px;"),
                "widget must be positioned relative to its wrapper, not the whole page -- got:\n{html}");
        // Critical check, not redundant with the two above: the wrapper
        // must actually come *before* the widget in the real HTML/DOM
        // order, not just exist somewhere in the output -- this is
        // exactly the real bug found on a real totem (the wrapper
        // existed, but the widget's own HTML had already been written
        // as a preceding sibling by the time the wrapper opened).
        let div_pos = html.find("class='r42' style='position: absolute;")
            .expect("wrapper div must exist");
        let img_pos = html.find("<img class='media r42'")
            .expect("widget img must exist");
        assert!(div_pos < img_pos,
                "the wrapper div must open BEFORE the widget's own HTML, so the \
                 widget ends up nested inside it, not as a preceding sibling -- \
                 div at {div_pos}, img at {img_pos}");
    }

    #[test]
    fn an_empty_region_gets_a_harmless_empty_wrapper() {
        // A region with no displayable media still gets its wrapper
        // (opened before knowing whether there'd be anything inside,
        // see write_region's own doc comment on why) -- but it must be
        // empty and properly closed, not left unbalanced.
        let xlf = r#"<layout width="1920" height="1080">
            <region id="42" left="0" top="0" width="300" height="400">
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("class='r42'"), "the wrapper itself is still emitted");
        assert!(html.contains("end region 42 wrapper (empty)"));
    }

    fn translate_xlf(xlf: &str) -> String {
        let dir = tempdir();
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let map = HashMap::new();
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
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
    fn fly_transition_is_now_implemented() {
        // Was previously a fallback-to-instant test ("fly" wasn't
        // implemented) -- now genuinely supported, requested directly.
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <options><transitionType>fly</transitionType><transitionDuration>500</transitionDuration>
                <transitionDirection>N</transitionDirection></options>
                <media id="9001" type="image" duration="10"><options><uri>a.png</uri></options></media>
                <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // N => (0, -100): arrives from above.
        assert!(html.contains("el.style.transform = 'translate(0%, -100%)'"));
        assert!(html.contains("transition = 'transform 500ms'"));
        assert!(html.contains("el.style.transform = 'translate(0%, 0%)';"));
        // No plain "instant visible" line for m9001 -- it's animated.
        assert!(!html.contains("document.getElementById('m9001').style.visibility = 'visible';"));
    }

    #[test]
    fn all_eight_compass_directions_produce_distinct_offsets() {
        for (dir, dx, dy) in [
            ("N", 0, -100), ("S", 0, 100), ("E", 100, 0), ("W", -100, 0),
            ("NE", 100, -100), ("SE", 100, 100), ("SW", -100, 100), ("NW", -100, -100),
        ] {
            let xlf = format!(r#"<layout width="1080" height="1920">
                <region id="1" left="0" top="0" width="500" height="500">
                    <media id="9001" type="image" duration="10">
                        <options><uri>a.png</uri>
                        <transIn>fly</transIn><transInDuration>300</transInDuration>
                        <transInDirection>{dir}</transInDirection></options>
                    </media>
                </region>
            </layout>"#);
            let html = translate_xlf(&xlf);
            let expected = format!("el.style.transform = 'translate({dx}%, {dy}%)'");
            assert!(html.contains(&expected),
                    "direction {dir} should produce offset ({dx}%, {dy}%) -- got:\n{html}");
        }
    }

    #[test]
    fn fly_out_exits_toward_the_direction_and_bumps_zindex() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <media id="9001" type="image" duration="10">
                    <options><uri>a.png</uri>
                    <transOut>fly</transOut><transOutDuration>400</transOutDuration>
                    <transOutDirection>E</transOutDirection></options>
                </media>
                <media id="9002" type="image" duration="10"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("el.style.zIndex = '9999';"));
        assert!(html.contains("transition = 'transform 400ms'"));
        assert!(html.contains("el.style.transform = 'translate(100%, 0%)';"));
    }

    #[test]
    fn fly_direction_defaults_to_east_when_missing() {
        // Matches the CMS's own default (confirmed in the real CMS
        // source, Layout.php: getOptionValue('transInDirection', 'E')).
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1" left="0" top="0" width="500" height="500">
                <media id="9001" type="image" duration="10">
                    <options><uri>a.png</uri>
                    <transIn>fly</transIn><transInDuration>300</transInDuration></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("el.style.transform = 'translate(100%, 0%)'"));
    }
}

#[cfg(test)]
mod native_webpage_tests {
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
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_native_webpage_test_{}_{n}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn native_webpage_js_call_uses_the_regions_real_absolute_position() {
        // Regression test for a real report: after the fly-transition
        // fix introduced a (0,0)-relative wrapper div (see
        // transition_tests's own region_gets_a_fixed_overflow_hidden_
        // wrapper test), the region-relative (0,0) geometry meant for
        // that wrapper's CSS was mistakenly also reused for this
        // widget's own jsNativeWebShow() call -- which drives a
        // completely separate, real Qt QWebEngineView positioned in
        // native window coordinates (gui/view.cpp), with no wrapper div
        // to be relative to at all. That made every native webpage
        // widget land at the same spot (the base view's own top-left
        // corner) regardless of its own region's actual position.
        let xlf = r#"<layout width="1920" height="1080">
            <region id="7" left="150" top="250" width="400" height="300">
                <media id="9001" type="webpage" render="native" duration="10">
                    <options><uri>https://example.com</uri></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // The JS call must use the region's real, absolute (150, 250)
        // position -- not (0, 0), which is what the wrapper-relative
        // geometry alone would produce.
        assert!(html.contains(r#"window.arexiboGui.jsNativeWebShow(9001, "https://example.com", 150, 250, 400, 300);"#),
                "native webpage JS call must use the real absolute region position -- got:\n{html}");
        // Meanwhile the placeholder <div>'s own CSS must still be
        // (0,0)-relative to its wrapper, same as every other widget --
        // this widget type isn't exempt from that part of the fix.
        assert!(html.contains("style='left: 0px; top: 0px; width: 400px; height: 300px;'></div>"),
                "the placeholder div itself must still use wrapper-relative (0,0) CSS -- got:\n{html}");
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
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    #[test]
    fn per_widget_trans_out_fadeout_overrides_empty_region_level() {
        // Mirrors the real XLF the user shared: region-level transition
        // options present but empty, widget carries its own transIn=fly
        // and transOut=fadeOut, both now implemented.
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
        // "fly" in-transition is now implemented -- E => (100%, 0%).
        assert!(html.contains("transition = 'transform 10000ms'"));
        assert!(html.contains("el.style.transform = 'translate(100%, 0%)'"));
        assert!(!html.contains("document.getElementById('m3045').style.visibility = 'visible';"));
    }

    #[test]
    fn per_widget_overrides_region_level_default() {
        // Region has its own real fadeIn default; widget explicitly
        // opts for its own fadeOut on the *out* side only -- its own
        // transOut must win for hiding, while its (unspecified) transIn
        // correctly still falls back to the region's fadeIn default for
        // showing -- the two sides are resolved independently, each
        // falling back to the region's own matching side only when the
        // widget itself doesn't specify that particular side.
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
        // widget 4001: its own 500ms fadeOut for hiding...
        assert!(html.contains("setTimeout(() => { el.style.visibility = 'hidden'; }, 500)"));
        // ...but correctly still the region's 2000ms fadeIn for showing,
        // since it doesn't specify its own transIn at all.
        let end = html.find("4001],").unwrap() + "4001],".len();
        let widget_4001_block = &html[..end];
        assert!(widget_4001_block.contains("opacity 2000ms"),
                "widget 4001 doesn't override transIn, so it must still use \
                 the region's fadeIn default for showing");
        assert!(widget_4001_block.contains("opacity 500ms"),
                "widget 4001's own transOut override must be used for hiding");
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

    #[test]
    fn region_level_fadein_applies_only_to_show_not_hide() {
        // Regression test for a real bug introduced (and caught) while
        // adding fly-transition support in this same session: an
        // earlier version of this refactor applied the region's single
        // fadeIn/fadeOut setting to *both* the show and hide side of
        // any widget falling back to it, when the region's own
        // transitionType string unambiguously means only ONE side
        // ("fadeIn" only ever describes showing, never hiding).
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><transitionType>fadeIn</transitionType><transitionDuration>2000</transitionDuration></options>
                <media id="4001" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="4002" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("transition = 'opacity 2000ms'; \
                                    el.style.opacity = '0'; \
                                    el.style.visibility = 'visible';"),
                "region fadeIn must apply to showing");
        // Hiding must stay the plain instant line -- fadeIn never
        // applies to the hide side, regardless of the region-level
        // fallback.
        assert!(html.contains("document.getElementById('m4001').style.visibility = 'hidden'; "));
    }

    #[test]
    fn region_level_fadeout_applies_only_to_hide_not_show() {
        // Mirror of the test above, for the fadeOut side.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><transitionType>fadeOut</transitionType><transitionDuration>1500</transitionDuration></options>
                <media id="4001" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="4002" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("setTimeout(() => { el.style.visibility = 'hidden'; }, 1500)"),
                "region fadeOut must apply to hiding");
        // Showing must stay the plain instant line.
        assert!(html.contains("document.getElementById('m4001').style.visibility = 'visible';"));
    }

    #[test]
    fn region_level_fly_applies_to_both_show_and_hide() {
        // Unlike fadeIn/fadeOut (each unambiguously one side only), a
        // region-level "fly" doesn't have separate in/out fields at
        // all -- a single whole-region setting, applied to both sides
        // (same direction for both), per this session's own design
        // decision (see the region-level parsing's own doc comment).
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <options><transitionType>fly</transitionType><transitionDuration>600</transitionDuration>
                <transitionDirection>S</transitionDirection></options>
                <media id="4001" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="4002" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // S => (0, 100) -- both show (arrives from) and hide (exits
        // toward) must use this same direction.
        assert!(html.contains("transition = 'transform 600ms'; \
                                    el.style.transform = 'translate(0%, 0%)';")
                || html.contains("el.style.transform = 'translate(0%, 100%)'; \
                                    el.style.visibility = 'visible';"),
                "fly must apply to showing");
        assert!(html.contains("el.style.transform = 'translate(0%, 100%)';"),
                "fly must apply to hiding, exiting toward the same direction");
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
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
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
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
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
    fn audio_uses_ended_event_and_pauses_resets_on_stop_same_as_video() {
        // Regression test for a real question asked directly, right
        // after fixing the exact same two issues for the video widget:
        // audio was modeled on an *earlier* version of that video arm,
        // before either fix existed there. Same two bugs, same fixes:
        // 1. Synchronous `.duration` read right after `.play()` is
        //    unreliable (metadata loads asynchronously) -- must use the
        //    native `ended` event instead, with an 86400s safety-net
        //    duration, not a live `.duration` read.
        // 2. Missing add_stop -- audio kept playing invisibly when sent
        //    to background instead of actually stopping; must pause
        //    *and* reset playback position.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5005" type="audio" duration="0"><options><uri>a.mp3</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        // must NOT read audio.duration synchronously
        assert!(!html.contains("document.getElementById('m5005').duration"));
        // must use the native 'ended' event to advance instead
        assert!(html.contains("el.onended = () => window.arexibo.region_switch(1, -1, false);"));
        assert!(html.contains("() => 86400"));
        // must pause and reset playback position when sent to background
        assert!(html.contains("{ let el = document.getElementById('m5005'); \
                               el.pause(); el.currentTime = 0; }"),
                "audio must pause+reset on stop, same as video -- got:\n{html}");
    }

    #[test]
    fn audio_with_loop_1_gets_native_loop_attribute_and_static_duration() {
        // loop/useDuration support for audio, added on direct request
        // right after the ended-event/add_stop fix above, for full
        // parity with video. Mirrors
        // video_with_loop_1_gets_native_loop_attribute_and_static_duration
        // 1:1.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5006" type="audio" duration="60"><options><uri>a.mp3</uri><loop>1</loop></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(html.contains("<audio class='media r1' id='m5006' src='a.mp3'  loop"));
        // must NOT override duration with audio.duration when looping --
        // the widget's own static 60s duration must be used instead.
        assert!(!html.contains("document.getElementById('m5006').duration"));
        assert!(html.contains("() => 60"));
    }

    #[test]
    fn audio_with_use_duration_1_respects_configured_duration_even_without_loop() {
        // Mirrors video_with_use_duration_1_respects_configured_duration_
        // even_without_loop 1:1.
        let xlf = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5007" type="audio" duration="15" useDuration="1">
                    <options><uri>a.mp3</uri><loop>0</loop></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf);
        assert!(!html.contains(" loop "));
        assert!(html.contains("() => 15"));
        assert!(!html.contains("() => 86400"));
        assert!(!html.contains("el.onended"));
        assert!(html.contains("document.getElementById('m5007').play();"));
        // add_stop must still apply regardless of which branch was taken
        assert!(html.contains("{ let el = document.getElementById('m5007'); \
                               el.pause(); el.currentTime = 0; }"));
    }

    #[test]
    fn video_pauses_and_resets_when_sent_to_background_regardless_of_duration_branch() {
        // Regression test for a real report: a video with an explicit
        // duration set is sent to background when its time is up, but
        // keeps playing invisibly instead of actually stopping. Every
        // other widget type that needs cleanup when hidden (PDF, native
        // webpage) already sets add_stop alongside add_start -- video
        // never did, for either duration-handling branch (useDuration=1/
        // loop, or the native `ended`-event path). Checked for both
        // branches since the fix needed to apply regardless of which
        // one is taken.
        let expected_stop = "{ let el = document.getElementById('m5004'); \
                              el.pause(); el.currentTime = 0; }";

        // Branch 1: useDuration=1 (static duration, no native ended event)
        let xlf_use_duration = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5004" type="video" duration="15" useDuration="1">
                    <options><uri>a.mp4</uri></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf_use_duration);
        assert!(html.contains(expected_stop),
                "useDuration=1 branch must pause+reset on stop -- got:\n{html}");

        // Branch 2: no loop, no useDuration (native `ended`-event path)
        let xlf_ended_event = r#"<layout width="720" height="1280">
            <region id="1" left="0" top="0" width="250" height="250">
                <media id="5004" type="video" duration="0">
                    <options><uri>a.mp4</uri><loop>0</loop></options>
                </media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf_ended_event);
        assert!(html.contains(expected_stop),
                "native ended-event branch must also pause+reset on stop -- got:\n{html}");
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
        //
        // Uses a populated code_map (matching "test1") rather than an
        // empty one -- this test is about the keydown listener
        // specifically, unrelated to how the target layout id itself
        // gets resolved; a layoutCode that fails to resolve would bail
        // out of the whole action (via the `?` on the code_map lookup)
        // before either the click or keydown listener ever gets
        // written, which would fail this test for a completely
        // unrelated reason.
        let xlf = r#"<layout width="1080" height="1920" code="defaultxibomultimedia">
            <action layoutCode="test1" target="screen" source="layout"
                    actionType="navLayout" triggerType="touch" triggerCode="Space"
                    id="756" targetId="780" sourceId="780"/>
            <region id="1" left="0" top="0" width="1080" height="1920">
                <media id="100" type="image" duration="5"><options><uri>a.png</uri></options></media>
            </region>
        </layout>"#;
        let dir = std::env::temp_dir().join(format!("arexibo_action_keydown_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let mut map = HashMap::new();
        map.insert("test1".to_string(), 780i64);
        let t = Translator::new(1, &xlf_path, &html_path, &map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
        assert!(html.contains("document.addEventListener('keydown'"),
                "must bind a keydown listener for the touch+triggerCode Key Press feature");
        assert!(html.contains("e.code === \"Space\""));
        // the existing click/touch handling (source=="layout" -> whole
        // body) must still be present too -- this is an *addition*, not
        // a replacement.
        assert!(html.contains("document.body.addEventListener('click'"));
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
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
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
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
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

#[cfg(test)]
mod drawer_tests {
    use super::*;
    use std::io::Read;

    fn translate_xlf(xlf: &str, code_map: &HashMap<String, LayoutId>, test_name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("arexibo_{test_name}_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let xlf_path = dir.join("test.xlf");
        let html_path = dir.join("test.html");
        fs::write(&xlf_path, xlf).unwrap();
        let t = Translator::new(1, &xlf_path, &html_path, code_map, None, 0).unwrap();
        t.translate().unwrap();
        let mut html = String::new();
        fs::File::open(html_path).unwrap().read_to_string(&mut html).unwrap();
        html
    }

    // Real XLF shared directly by the user (layout 983) -- the exact
    // report this whole feature addresses: a webhook trigger
    // ("apri_home") on an interactive-button widget did nothing at
    // all, because its own navWidget action targets widgetId="4670",
    // a widget that lives in the <drawer>, not any real region --
    // previously never processed at all (confirmed: zero occurrences
    // of "drawer" anywhere in this file before this feature), so
    // write_action's own widget_regions-only lookup always failed,
    // silently dropping the whole action (write_action's own errors
    // are caught and logged, not propagated -- see translate()) and
    // therefore never actually registering
    // window.arexibo.triggers["apri_home"] at all.
    const REAL_LAYOUT_983_XLF: &str = r##"<?xml version="1.0"?>
<layout width="1080" height="1920" bgcolor="#ffffff" schemaVersion="4" enableStat="1"><region id="4654" width="1080" height="1920" top="0" left="0" syncKey="" zindex="2"><options/><media id="4666" schemaVersion="1" type="global" render="html" duration="10" useDuration="1" fromDt="1970-01-01 01:00:00" toDt="2038-01-19 04:14:07" enableStat="1"><options><updateInterval>43200</updateInterval></options><raw/></media></region><region id="4657" width="240" height="255" top="1616" left="40" syncKey="" zindex="5"><options><loop>0</loop><transitionDirection/><transitionDuration/><transitionType/></options><media id="4669" schemaVersion="2" type="interactive-button" render="html" duration="60" useDuration="0" fromDt="1970-01-01 01:00:00" toDt="2038-01-19 04:14:07" enableStat="1"><action layoutCode="" target="region" source="widget" actionType="navWidget" triggerType="webhook" triggerCode="apri_home" id="809" widgetId="4670" targetId="4657" sourceId="4669"/><options><updateInterval>43200</updateInterval></options><raw/></media></region><drawer id="4658" width="1080" height="1920" top="0" left="0" syncKey=""><options/><media id="4670" schemaVersion="1" type="image" render="native" duration="10" useDuration="0" fromDt="1970-01-01 01:00:00" toDt="2038-01-19 04:14:07" enableStat="0" fileId="154"><options><uri>154.jpg</uri><scaleType>center</scaleType><alignId>center</alignId><valignId>middle</valignId></options><raw/></media></drawer><tags/></layout>"##;

    #[test]
    fn real_layout_983_registers_the_trigger_that_previously_did_nothing() {
        let html = translate_xlf(REAL_LAYOUT_983_XLF, &HashMap::new(), "drawer_983_trigger");
        assert!(html.contains(r#"window.arexibo.triggers["apri_home"]"#),
                "the webhook trigger must actually get registered now -- this is the \
                 exact real report this feature addresses");
        assert!(html.contains("window.arexibo.navWidgetFromDrawer(4657, 4670)"),
                "must resolve to the target region (4657) and the drawer widget (4670), \
                 not silently fail");
    }

    #[test]
    fn real_layout_983_drawer_widget_gets_written_hidden() {
        let html = translate_xlf(REAL_LAYOUT_983_XLF, &HashMap::new(), "drawer_983_hidden");
        // Regression test for a real bug: the drawer wrapper must NOT
        // have `display: none` -- an ancestor's display:none hides
        // everything inside it regardless of each child's own
        // visibility, so toggling a drawer widget's own visibility
        // (navWidgetFromDrawer) could never make anything actually
        // appear on screen at all. Every `.media` element already
        // starts hidden via the shared `.media { visibility: hidden }`
        // CSS rule (see write_footer) -- the same mechanism every
        // normal region widget's own show/hide already relies on --
        // so the wrapper itself needs no visibility handling at all.
        assert!(html.contains("class='drawer'"), "the drawer wrapper must exist");
        assert!(!html.contains("class='drawer' style='position: absolute; \
                                left: 0px; top: 0px; width: 1080px; height: 1920px; \
                                overflow: hidden; display: none;'"),
                "the drawer wrapper must NOT have display:none -- that would hide \
                 every widget inside it regardless of its own visibility toggling");
        assert!(html.contains(".media { position: absolute; visibility: hidden; }"),
                "the shared CSS rule that keeps every widget (drawer or not) hidden \
                 by default must still be present");
        // Regression test for a second real bug, found only after the
        // first fix (display:none) let the widget become visible but
        // still invisible in practice: every real region gets its own
        // explicit z-index (see write_region), but the drawer wrapper
        // previously had none at all -- stacking at the implicit/auto
        // level, *below* any region with an explicit z-index (every
        // one of them, including a full-screen "global" widget region
        // that would completely obscure it). A drawer widget must
        // stack above every normal region once swapped in.
        assert!(html.contains("class='drawer'") && html.contains("z-index: 9999;"),
                "the drawer wrapper must have a high explicit z-index -- otherwise a \
                 swapped-in drawer widget can be genuinely visible (visibility: \
                 visible) yet still never actually seen, stacked behind every normal \
                 region");
        // Regression test for a third real bug, found only after the
        // display:none and z-index fixes above finally let a swapped-in
        // drawer widget actually become visible: `visibility: hidden`
        // on each *child* widget (the default) only stops that child
        // from being clickable/visible -- it does nothing to stop the
        // *wrapper* itself from being a solid, event-catching layer.
        // Combined with the same z-index: 9999 covering the full
        // layout, every touch/click across the entire screen was being
        // silently swallowed by this otherwise-invisible wrapper,
        // disabling every interactive control on the layout.
        assert!(html.contains("pointer-events: none;'"),
                "the drawer wrapper must have pointer-events: none -- otherwise, even \
                 though its own contents are invisible, the wrapper itself (full-screen, \
                 stacked above everything via the z-index fix) silently intercepts every \
                 touch/click on the entire layout, disabling all interactive controls");
        // The widget's own content must actually exist in the DOM
        // (id='m4670') -- otherwise there'd be nothing to reveal.
        assert!(html.contains("id='m4670'"),
                "the drawer widget's own DOM element must actually be written");
        assert!(html.contains("window.arexibo.drawerWidgets[4670]"),
                "the drawer widget's own show/hide/duration entry must be registered");
    }

    #[test]
    fn drawer_script_block_contains_no_html_markup() {
        // Regression test for the real, root-cause bug behind every
        // previous "trigger does nothing" report on this feature: an
        // earlier version wrote each widget's own <img>/<iframe>
        // markup *inside* the same already-open <script> block as its
        // own drawerWidgets[...] = [...] registration. Once a
        // browser's HTML parser enters <script>, everything up to the
        // next </script> is raw JS source text, not HTML to keep
        // parsing -- an embedded <img ...> tag is invalid JS syntax
        // (confirmed via a real browser console: "SyntaxError:
        // Unexpected token '<'"), and a syntax error anywhere in a
        // <script> block means *none* of it runs, not just that one
        // line -- so drawerWidgets was silently never populated at
        // all, on every layout, regardless of the display/z-index/
        // duration fixes that came before this one. None of those
        // earlier fixes could ever have been *observed* working,
        // because this bug prevented the registration script from
        // running in the first place. Only actually parsing the
        // generated HTML's own structure (not just checking for
        // substrings, which every earlier test in this file did, and
        // which is exactly why this one slipped through repeatedly)
        // can catch this class of bug.
        let html = translate_xlf(REAL_LAYOUT_983_XLF, &HashMap::new(), "drawer_no_html_in_script");
        let script_start = html.find("<script type='text/javascript'>\nwindow.arexibo.drawerWidgets")
            .expect("the drawer's own registration script must exist");
        let script_end = html[script_start..].find("</script>")
            .expect("the drawer's own registration script must be closed") + script_start;
        let script_body = &html[script_start..script_end];
        assert!(!script_body.contains('<') || script_body.matches("() =>").count() > 0,
                "sanity check: the script body should contain at least the arrow \
                 functions it's expected to, confirming the slice itself is correct");
        for tag in ["<img", "<iframe", "<canvas", "<video", "<audio", "<div"] {
            assert!(!script_body.contains(tag),
                    "the drawer's own <script> block must not contain any HTML \
                     markup ({tag:?} found) -- this breaks JS parsing and silently \
                     prevents the *entire* script block from ever running");
        }
    }

    #[test]
    fn every_script_block_in_the_generated_html_is_syntactically_valid_js() {
        // A much stronger, general version of the substring-based
        // check above -- rather than looking for specific HTML tags
        // that happen to break JS parsing (the exact class of real
        // bug found on this feature), this actually extracts *every*
        // <script> block from a real, full layout's own generated
        // HTML and validates each with `node --check`, a real JS
        // parser -- catching this whole class of bug regardless of
        // what causes it, not just the one specific tag that
        // triggered the real report. Skips gracefully (rather than
        // failing) if `node` isn't available in this environment,
        // since this is a genuinely stronger *additional* check, not
        // the only test covering this -- CI/dev environments without
        // node shouldn't lose an otherwise-unrelated test run over it.
        if std::process::Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipping: node not available in this environment");
            return;
        }
        let html = translate_xlf(REAL_LAYOUT_983_XLF, &HashMap::new(), "drawer_node_check");
        let mut pos = 0;
        let mut checked = 0;
        while let Some(start_rel) = html[pos..].find("<script") {
            let start = pos + start_rel;
            let tag_end = html[start..].find('>').expect("script tag must close") + start + 1;
            let end = html[tag_end..].find("</script>").expect("script must be closed") + tag_end;
            let body = &html[tag_end..end];
            let path = std::env::temp_dir().join(format!(
                "arexibo_node_check_{}_{}_{checked}.js",
                std::process::id(), std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            fs::write(&path, body).unwrap();
            let output = std::process::Command::new("node").arg("--check").arg(&path)
                .output().expect("running node --check");
            assert!(output.status.success(),
                    "script block #{checked} is not valid JS: {}\n--- body ---\n{body}",
                    String::from_utf8_lossy(&output.stderr));
            let _ = fs::remove_file(&path);
            pos = end + "</script>".len();
            checked += 1;
        }
        assert!(checked >= 5, "expected to find and check several script blocks \
                                (regions, drawer, actions, footer) -- only found {checked}, \
                                the extraction logic itself may be broken");
    }

    #[test]
    fn navwidgetfromdrawer_calls_duration_as_a_function_not_a_raw_value() {
        // Regression test for a real report: the trigger fired
        // correctly (window.arexibo.triggers[...] was registered and
        // called navWidgetFromDrawer with the right arguments -- the
        // earlier bug this whole feature already fixed), yet still
        // visibly did nothing. Root cause: drawerWidgets[mid]'s own
        // third element is a `() => N` function (same shape as a
        // normal region item's own media[cur][2], see region_switch's
        // own `media[next][2]()` call) -- the shared runtime JS's own
        // navWidgetFromDrawer treated it as a raw number instead,
        // making its own computed setTimeout delay come out NaN
        // (a function times 1000) -- clamped by the browser to ~0, so
        // the widget swapped in and back out again within
        // milliseconds, imperceptibly fast.
        //
        // This can only be checked at the level of the *shared*
        // runtime JS text itself (not per-layout translated output,
        // which never repeats this function's own body) -- confirms
        // duration is actually *invoked* (parenthesized call), not
        // just referenced as a bare identifier.
        let html = translate_xlf(REAL_LAYOUT_983_XLF, &HashMap::new(), "drawer_duration_call");
        assert!(html.contains("duration() || 1"),
                "navWidgetFromDrawer must call duration() as a function -- it's a \
                 `() => N` expression, not a raw number, same as every normal region \
                 item's own third array element");
    }

    #[test]
    fn a_normal_navwidget_action_targeting_a_real_region_widget_is_unaffected() {
        // Regression safety: the original, already-working case (a
        // navWidget action targeting a widget that's genuinely part
        // of a normal region, resolved via widget_regions) must keep
        // working exactly as before -- this feature only *adds* a
        // fallback for the drawer case, never changes the existing
        // resolution path.
        let xlf = r#"<layout width="1080" height="1920">
            <action target="screen" source="layout" actionType="navWidget"
                    triggerType="webhook" triggerCode="show2" id="1" widgetId="101"/>
            <region id="1" left="0" top="0" width="1080" height="1920">
                <media id="100" type="image" duration="5"><options><uri>a.png</uri></options></media>
                <media id="101" type="image" duration="5"><options><uri>b.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf, &HashMap::new(), "drawer_normal_navwidget");
        assert!(html.contains(r#"window.arexibo.triggers["show2"]"#));
        assert!(html.contains("window.arexibo.navWidget(1, 1)"),
                "widget 101 is the second (index 1) item in region 1 -- must still \
                 resolve via widget_regions, not be mistaken for a drawer widget");
        assert!(!html.contains("window.arexibo.navWidgetFromDrawer("),
                "must not go anywhere near the drawer fallback for a genuinely \
                 normal region widget (navWidgetFromDrawer is always *defined* in the \
                 shared runtime JS regardless -- this checks for an actual *call*)");
    }

    #[test]
    fn an_action_targeting_a_genuinely_unknown_widget_id_is_dropped_and_logged() {
        // Neither a region widget nor a drawer widget -- must still
        // fail closed (the action is silently dropped, logged, see
        // translate()'s own error handling) rather than somehow
        // succeeding or panicking.
        let xlf = r#"<layout width="1080" height="1920">
            <action target="screen" source="layout" actionType="navWidget"
                    triggerType="webhook" triggerCode="ghost" id="1" widgetId="99999"/>
            <region id="1" left="0" top="0" width="1080" height="1920">
                <media id="100" type="image" duration="5"><options><uri>a.png</uri></options></media>
            </region>
        </layout>"#;
        let html = translate_xlf(xlf, &HashMap::new(), "drawer_unknown_widget");
        assert!(!html.contains(r#"window.arexibo.triggers["ghost"]"#),
                "an action targeting a genuinely nonexistent widget id must not \
                 register any trigger at all");
    }
}


