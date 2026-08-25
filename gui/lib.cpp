#include <cstdlib>
#include <QApplication>
#include <QMainWindow>
#include <QScreen>
#include <QtWebEngineCore/QWebEngineProfile>
#include <QtWebEngineCore/QWebEngineScript>
#include <QtWebEngineCore/QWebEngineScriptCollection>
#include <QtWebEngineCore/QWebEngineSettings>
#include <QtWebEngineWidgets/QWebEngineView>

#include "lib.h"
#include "view.h"

// For some reason, this constructor is not automatically called
int qInitResources_res();

QApplication *the_app = nullptr;
Window *the_wnd = nullptr;

int fake_argc = 1;
char *fake_argv[] = {(char *)"arexibo", nullptr};

void setup(const char *base_uri, const char *screen, int inspect, int debug,
           callback cb, void *cb_ptr) {
    if (the_wnd) return;

    // Kiosk display: touch input is multipoint-capable, and QtWebEngine
    // (Chromium) enables pinch-to-zoom on multipoint touch by default --
    // there is no QWebEngineSettings attribute to turn this off (unlike
    // e.g. WebView2's IsPinchZoomEnabled on Windows), so it has to go
    // through the Chromium command-line flag instead. Must be set via
    // env var before QApplication/QtWebEngine initializes below --
    // setting it later (e.g. as a runtime attribute) has no effect.
    //
    // BUG fix (found while writing the deployment guide, considering
    // suggesting QTWEBENGINE_CHROMIUM_FLAGS=--use-gl=disabled as a
    // workaround for a real GPU rendering quirk seen in testing --
    // realized that advice wouldn't actually work): this used to
    // qputenv() our own flags unconditionally, silently discarding any
    // value already set externally (e.g. via a systemd unit's own
    // Environment= line, exactly what a deployment guide would
    // reasonably suggest for troubleshooting). Now appends to whatever
    // is already present instead, so an externally-set value and our
    // own required flags can coexist.
    QByteArray chromium_flags = "--disable-pinch";
    QByteArray existing_flags = qgetenv("QTWEBENGINE_CHROMIUM_FLAGS");
    if (!existing_flags.isEmpty()) {
        chromium_flags = existing_flags + " " + chromium_flags;
    }
    if (debug)
        chromium_flags += " --single-process --enable-logging --log-level=0 --v=1";
    qputenv("QTWEBENGINE_CHROMIUM_FLAGS", chromium_flags);

    qInitResources_res();

    QCoreApplication::setOrganizationName("arexibo");
    the_app = new QApplication(fake_argc, fake_argv);

    auto screens = QApplication::screens();
    QScreen *selected_screen = nullptr;

    if (strcmp(screen, "list") == 0) {
        std::cout << "INFO : [arexibo::qt] listing screens:" << std::endl;
        int i = 1;
        foreach (auto scr, screens) {
            std::cout << "number " << i << " - name " << scr->name().toStdString() << std::endl;
            i++;
        }
    } else {
        int n = atoi(screen);
        if (n > 0 && n <= screens.length())
            selected_screen = screens[n - 1];
        else
            foreach (auto scr, screens)
                if (scr->name() == screen)
                    selected_screen = scr;
    }
    if (selected_screen)
        std::cout << "INFO : [arexibo::qt] selected screen: " <<
            selected_screen->name().toStdString() << std::endl;

    auto settings = QWebEngineProfile::defaultProfile()->settings();
    settings->setAttribute(QWebEngineSettings::ScreenCaptureEnabled, true);
    settings->setAttribute(QWebEngineSettings::PlaybackRequiresUserGesture, false);

    // Temporary diagnostic knob (found genuinely useful investigating a
    // real report: "RSS marquee scroller does not display text"), NOT
    // meant to be a permanent feature -- QTWEBENGINE_CHROMIUM_FLAGS'
    // own "--user-agent=" is silently overridden by QtWebEngine's own
    // default UA construction later, so this has to go through
    // QWebEngineProfile's dedicated API instead to actually take
    // effect. Lets us test whether bundle.min.js (or a third-party
    // jQuery plugin bundled inside it) does *any* UA-based platform
    // detection that happens to affect Linux differently, without
    // needing a real Windows machine to compare against.
    const char *fakeUserAgentEnv = std::getenv("AREXIBO_FAKE_USERAGENT");
    if (fakeUserAgentEnv && *fakeUserAgentEnv) {
        std::cout << "INFO : [arexibo::qt] overriding navigator.userAgent via \\
                     AREXIBO_FAKE_USERAGENT (diagnostic only): " << fakeUserAgentEnv << std::endl;
        QWebEngineProfile::defaultProfile()->setHttpUserAgent(QString(fakeUserAgentEnv));
    }

    // Pragmatic, deliberately-not-root-caused font size calibration:
    // user-reported (and independently re-confirmed via a real
    // side-by-side comparison) that text renders visibly larger under
    // QtWebEngine than under other Xibo players (CEF/Windows client,
    // the Go player) on the same physical screen, both in arexibo's own
    // "resource" widgets (e.g. the clock-digital widget, investigated
    // at length in an earlier session) and in `webpage render="native"`
    // widgets showing real external sites. DPI/devicePixelRatio/the
    // widget-box-overflow issue, and separately QT_FONT_DPI (changes
    // what QScreen reports but was empirically confirmed to have zero
    // effect on QtWebEngine's own actual rendered CSS font-size), were
    // all investigated and ruled out --
    // the likely remaining explanation (QtWebEngine not honoring
    // mobile-style meta viewport tags the way CEF apparently does) was
    // identified but not implemented (would need a Chrome DevTools
    // Protocol client issuing Emulation.setDeviceMetricsOverride, a
    // materially bigger, still-unverified undertaking).
    //
    // Controlled by the AREXIBO_FONT_SCALE environment variable
    // (requested explicitly by the user, to allow experimenting with
    // the right factor on real hardware without needing a rebuild each
    // time) -- unset or empty: no script is injected at all, i.e. today's
    // default, unmodified behavior. Set to a number (e.g. "0.95"): every
    // element's own *computed* font size, in every frame of every
    // WebEngine view (main layout page, overlay, "native" webpage
    // widgets, AND -- critically, since this uses the profile-level
    // script mechanism with runsOnSubFrames enabled -- any iframe within
    // them regardless of origin, including "resource" widgets like the
    // clock which fetch their HTML directly from the CMS and would
    // otherwise be unreachable by a same-origin-restricted content
    // script injected from the parent page alone) is scaled by that
    // factor. A value outside (0, 2] is rejected (logged, no injection)
    // as almost certainly a typo rather than an intentional extreme
    // request.
    //
    // Deliberately NOT scaling anything else (images, layout boxes,
    // etc.) -- only `font-size`, matching the user's own observation
    // that specifically (and only) text appeared larger, not other
    // content.
    const char *fontScaleEnv = std::getenv("AREXIBO_FONT_SCALE");
    if (fontScaleEnv && fontScaleEnv[0] != '\0') {
        char *end = nullptr;
        double factor = std::strtod(fontScaleEnv, &end);
        if (end == fontScaleEnv || *end != '\0' || !(factor > 0.0 && factor <= 2.0)) {
            std::cout << "WARN : [arexibo::qt] ignoring invalid AREXIBO_FONT_SCALE=\"" <<
                fontScaleEnv << "\" (must be a number in (0, 2]), font scaling disabled" <<
                std::endl;
        } else {
            std::cout << "INFO : [arexibo::qt] font scaling enabled via AREXIBO_FONT_SCALE, \
                            factor=" << factor << std::endl;
            // Two-pass (read every element's current computed size
            // BEFORE writing any of them) to avoid compounding: naively
            // scaling as you go would make deeply-nested text shrink by
            // the factor *per ancestor level* rather than a flat amount
            // overall. Each scaled element is marked (a data attribute)
            // so later reruns (on 'load', and on any DOM mutation, to
            // catch content that loads/renders asynchronously --
            // tickers, async widgets, SPA-like external pages) don't
            // re-shrink something already scaled.
            QString fontScaleScript = QStringLiteral(R"JS(
                (function() {
                    var FONT_SCALE_FACTOR = %1;
                    var MARK = 'data-arexibo-fs';
                    function scaleAll(root) {
                        var els;
                        try { els = (root || document).querySelectorAll('*'); }
                        catch (e) { return; }
                        var todo = [];
                        for (var i = 0; i < els.length; i++) {
                            var el = els[i];
                            if (el.hasAttribute(MARK)) continue;
                            var cs;
                            try { cs = window.getComputedStyle(el); } catch (e) { continue; }
                            var px = parseFloat(cs.fontSize);
                            if (!isNaN(px) && px > 0) todo.push([el, px]);
                        }
                        for (var i = 0; i < todo.length; i++) {
                            todo[i][0].setAttribute(MARK, '1');
                            todo[i][0].style.fontSize = (todo[i][1] * FONT_SCALE_FACTOR) + 'px';
                        }
                    }
                    function run() { try { scaleAll(document); } catch (e) {} }
                    if (document.readyState === 'loading') {
                        document.addEventListener('DOMContentLoaded', run);
                    } else {
                        run();
                    }
                    window.addEventListener('load', run);
                    try {
                        var mo = new MutationObserver(function() { run(); });
                        mo.observe(document.documentElement || document,
                                   { childList: true, subtree: true });
                    } catch (e) {}
                })();
            )JS").arg(factor);
            QWebEngineScript fontScale;
            fontScale.setName(QStringLiteral("arexibo-font-scale"));
            fontScale.setSourceCode(fontScaleScript);
            fontScale.setInjectionPoint(QWebEngineScript::DocumentReady);
            fontScale.setWorldId(QWebEngineScript::MainWorld);
            fontScale.setRunsOnSubFrames(true);
            QWebEngineProfile::defaultProfile()->scripts()->insert(fontScale);
        }
    }

    // Corrective fallback for widgets whose CMS-rendered HTML (delivered
    // via GetResource -- text/ticker/embedded/datasetview/clock-digital/
    // etc.) overflows the box it's actually shown in. The CMS's own
    // client-side scaling helper (`xiboLayoutScaler`, part of
    // bundle.min.js) is supposed to handle this by comparing the
    // widget's declared "original" design size against
    // `$(window).width()/.height()` -- but since arexibo renders each
    // such widget in an `<iframe>` sized to *exactly* its XLF region
    // dimensions, that comparison is always 1:1 by construction, so
    // xiboLayoutScaler always computes a no-op scale factor of 1 and
    // never shrinks anything, regardless of whether the content itself
    // (fixed font-sizes authored against some other assumed canvas)
    // actually fits. Investigated at length against a real
    // clock-digital widget overflowing its box on a real CMS --
    // confirmed harness-independent (reproduced the exact
    // xiboLayoutScaler no-op mathematically, and confirmed via a
    // standalone QtWebEngine test that the `<meta viewport>` angle is a
    // dead end: desktop QtWebEngine ignores it entirely).
    //
    // Architecture note (why this lives here, at the profile level, and
    // not as a parent-page function like the original implementation):
    // widgets are now served from one of several loopback origins (see
    // server::HTML_SHARD_COUNT) to work around a *different* real bug
    // (Chromium's per-origin connection limit starving a busy layout's
    // own requests) -- meaning a widget's own iframe can be genuinely
    // cross-origin relative to the page that embeds it, and the
    // browser's same-origin policy would block the parent from reaching
    // into `iframe.contentDocument` to measure/shrink it directly (this
    // is exactly the approach the original implementation used, before
    // sharding was introduced). Injecting a *self-contained* script into
    // every frame (main layout, overlay, and every widget iframe alike,
    // via runsOnSubFrames below) sidesteps this entirely: each frame
    // reads its own intended box size from its own URL's query string
    // (`arexiboShrinkW`/`arexiboShrinkH`, see layout.rs's `write_media`)
    // and shrinks its own `document.body` if needed -- no cross-frame
    // reaching-in required at all, regardless of origin. The distinct
    // query param names (not just `w`/`h`) matter here specifically
    // because this script now runs in *every* frame including native
    // `webpage render="native"` widgets showing real external sites --
    // picking a name unlikely to collide with any such site's own,
    // unrelated query parameters avoids ever acting on them by mistake.
    //
    // Deliberately NEVER scales *up* (only ever shrinks, scale <= 1) --
    // content that already fits is left exactly as authored, to avoid
    // surprising already-correct widgets. Re-checked on every DOM
    // mutation (via MutationObserver, debounced) rather than only once
    // at load, since some widgets populate their real content
    // asynchronously after the page's `load` event (confirmed: the
    // clock-digital template renders its own placeholder spans empty at
    // first, filling in the actual time/date text only slightly later).
    const char *shrinkToFitScript = R"JS(
        (function() {
            if (window.self === window.top) return;
            var params;
            try { params = new URLSearchParams(window.location.search); }
            catch (e) { return; }
            if (!params.has('arexiboShrinkW') || !params.has('arexiboShrinkH')) return;
            var w = parseFloat(params.get('arexiboShrinkW'));
            var h = parseFloat(params.get('arexiboShrinkH'));
            if (!(w > 0) || !(h > 0)) return;

            function tryShrink() {
                var body = document.body;
                if (!body) return;
                body.style.transform = '';
                var sw = body.scrollWidth, sh = body.scrollHeight;
                if (sw <= 0 || sh <= 0) return;
                var scale = Math.min(1, w / sw, h / sh);
                // BUG fix (found from a real report: "RSS marquee scroller
                // does not display text" -- confirmed via real DevTools
                // inspection on a real widget: body.scrollWidth measured
                // in the ~1,000,000px range for a horizontally-scrolling
                // marquee ticker, whose plugin duplicates its own content
                // several times over to create a seamless scrolling loop
                // -- entirely intentional, not oversized content to fix).
                // This shrink mechanism was designed for a different,
                // genuine bug (a widget like a clock rendering 2-3x too
                // large due to a font/CSS sizing issue) -- a MODEST
                // overflow ratio. A scale this extreme (over ~5x too
                // "big") is a strong signal the content is *designed* to
                // overflow (a scrolling ticker/marquee, not a sizing bug),
                // and forcibly shrinking it to fit -- besides visually
                // squashing it into imperceptibility -- also repeatedly
                // resets and re-measures `body.style.transform` on every
                // DOM mutation (see armObserver below), which can race
                // with a marquee plugin's *own* width measurement/
                // animation setup happening at the same time. Below this
                // threshold, skip shrinking entirely and leave the
                // content exactly as the CMS/widget itself intended.
                //
                // HISTORY: briefly set to 1 (disabling this mechanism
                // entirely) after a deliberate decision to trade the
                // clock-sizing fix for reduced risk of unknown
                // interactions with other, untested third-party widget
                // plugins. Reverted immediately -- confirmed via a direct
                // Linux-vs-Windows screenshot comparison from the user
                // that this reintroduced the exact original clock bug
                // (Europe/Rome digital clock rendering correctly, cleanly
                // sized on Windows -- a completely separate CEF-based
                // client architecture that never went through this
                // mechanism at all -- but overflowing into a clipped,
                // near-black box on Linux/arexibo the moment this was
                // disabled). 0.2 is confirmed, via real functional
                // testing, to fix both cases correctly at once: the clock
                // (a modest, genuine sizing bug) gets shrunk; the marquee
                // (content intentionally far larger than its box) does
                // not.
                var MIN_SHRINK_SCALE = 0.2;
                if (scale < MIN_SHRINK_SCALE) {
                    if (window.arexiboDebug) {
                        console.log('arexibo-shrink: target=' + w + 'x' + h +
                                    ' measured=' + sw + 'x' + sh + ' scale=' + scale +
                                    ' -- skipped, below MIN_SHRINK_SCALE (likely ' +
                                    'intentionally-overflowing content, e.g. a marquee)');
                    }
                    return;
                }
                if (scale < 1) {
                    // Diagnostic log (found genuinely useful investigating
                    // a real "content generated correctly but invisible"
                    // report -- caught here via LoggingPage, since this
                    // otherwise silent transform is exactly the kind of
                    // thing that can go wrong for widget content whose
                    // natural size assumptions don't match its own
                    // region's declared box, e.g. a full-canvas "Elements"
                    // widget). Gated behind --web-debug (see
                    // `window.arexiboDebug`, injected below) -- useful for
                    // troubleshooting, but noisy for every normal run.
                    if (window.arexiboDebug) {
                        console.log('arexibo-shrink: target=' + w + 'x' + h +
                                    ' measured=' + sw + 'x' + sh + ' scale=' + scale);
                    }
                    body.style.transformOrigin = '0 0';
                    body.style.transform = 'scale(' + scale + ')';
                    return;
                }
                // BUG fix (found from a real report: a worldclock-digital-
                // date widget rendering as a small dark box in the corner
                // of its own much larger region, rather than filling it --
                // confirmed via real DevTools inspection: the widget's own
                // handlebars template hardcodes data-width="200"
                // data-height="80" as its native design size, while its
                // actual region was 711x241 -- body.scrollWidth/Height
                // measured exactly 200x80, confirming it never gets scaled
                // UP to fill the larger box at all). Same root cause as the
                // shrink case above (xiboLayoutScaler comparing container
                // size against globalOptions.originalWidth/Height, which
                // arexibo always sets equal to the container by
                // construction, so the ratio it computes is always exactly
                // 1 and it never does anything) -- just the opposite
                // direction: undersized content that needs to grow to fill
                // its box, rather than oversized content that needs to
                // shrink. A generous but bounded cap (MAX_GROW_SCALE) avoids
                // grotesquely scaling up content that's tiny/broken for an
                // unrelated reason (e.g. a genuinely empty/errored widget)
                // rather than merely designed smaller than its region.
                var MAX_GROW_SCALE = 10;
                var growScale = Math.min(MAX_GROW_SCALE, w / sw, h / sh);
                if (growScale > 1) {
                    if (window.arexiboDebug) {
                        console.log('arexibo-shrink: target=' + w + 'x' + h +
                                    ' measured=' + sw + 'x' + sh + ' growScale=' + growScale);
                    }
                    body.style.transformOrigin = '0 0';
                    body.style.transform = 'scale(' + growScale + ')';
                }
            }
            function run() { try { tryShrink(); } catch (e) {} }
            function armObserver() {
                try {
                    var mo = new MutationObserver(function() {
                        clearTimeout(window._arexiboShrinkTimer);
                        window._arexiboShrinkTimer = setTimeout(run, 50);
                    });
                    mo.observe(document.body, { childList: true, subtree: true,
                                                 characterData: true });
                } catch (e) {}
            }
            if (document.readyState === 'loading') {
                document.addEventListener('DOMContentLoaded', function() { run(); armObserver(); });
            } else {
                run();
                armObserver();
            }
            window.addEventListener('load', run);
        })();
    )JS";
    QWebEngineScript shrinkToFit;
    shrinkToFit.setName(QStringLiteral("arexibo-shrink-to-fit"));
    shrinkToFit.setSourceCode(QString::fromUtf8(shrinkToFitScript));
    shrinkToFit.setInjectionPoint(QWebEngineScript::DocumentCreation);
    shrinkToFit.setWorldId(QWebEngineScript::MainWorld);
    shrinkToFit.setRunsOnSubFrames(true);
    QWebEngineProfile::defaultProfile()->scripts()->insert(shrinkToFit);

    // --web-debug (see main.rs's Args) gates all of this session's own
    // JS-side diagnostics -- LoggingPage's console output (view.h),
    // this flag itself, and layout.rs's own `arexibo-show:` console.log
    // generated into every widget's `show()` function -- previously all
    // unconditional, adding permanent noise to every single run
    // regardless of whether anyone actually wanted this level of
    // detail. `window.arexiboDebug` is injected into every frame (main,
    // overlay, and every widget iframe alike) so the same flag reaches
    // JS generated at very different points (translated layout HTML,
    // this file's own inline scripts) without needing to duplicate the
    // debug flag into each of them separately.
    g_web_debug_enabled = debug;
    QWebEngineScript debugFlag;
    debugFlag.setName(QStringLiteral("arexibo-debug-flag"));
    debugFlag.setSourceCode(QString("window.arexiboDebug = %1;")
                             .arg(debug ? "true" : "false"));
    debugFlag.setInjectionPoint(QWebEngineScript::DocumentCreation);
    debugFlag.setWorldId(QWebEngineScript::MainWorld);
    debugFlag.setRunsOnSubFrames(true);
    QWebEngineProfile::defaultProfile()->scripts()->insert(debugFlag);

    the_wnd = new Window(base_uri, selected_screen, inspect, cb, cb_ptr);
    the_wnd->show();
}

void run() {
    if (!the_app) return;
    the_app->exec();
}

void quit() {
    if (!the_app) return;
    the_app->quit();
}

void navigate(const char *file) {
    if (!the_wnd) return;
    emit the_wnd->navigateTo(file);
}

void screenshot(int max_width) {
    if (!the_wnd) return;
    emit the_wnd->screenShot(max_width);
}

void set_title(const char *title) {
    if (!the_wnd) return;
    emit the_wnd->setTitle(title);
}

void set_size(int pos_x, int pos_y, int size_x, int size_y) {
    if (!the_wnd) return;
    emit the_wnd->setSize(pos_x, pos_y, size_x, size_y);
}

void run_js(const char *js) {
    if (!the_wnd) return;
    emit the_wnd->runJavascript(js);
}

void overlay_show(const char *file) {
    if (!the_wnd) return;
    emit the_wnd->overlayShow(file);
}

void overlay_hide() {
    if (!the_wnd) return;
    emit the_wnd->overlayHide();
}
