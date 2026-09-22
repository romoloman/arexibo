#include <QApplication>
#include <QIODevice>
#include <QBuffer>
#include <QJsonDocument>
#include <QJsonArray>
#include <QJsonObject>
#include <QMouseEvent>
#include <QTouchEvent>

#include "view.h"

// (Separate QWebEngineProfile for native webpage widgets: tried and
// reverted, see status doc -- didn't fix the freeze and broke
// unrelated sites.)

Window::Window(QString base_uri, QScreen *screen, int inspect, callback cb, void *cb_ptr) :
    QMainWindow(),
    base_uri(base_uri),
    selected_screen(screen),
    cb(cb),
    cb_ptr(cb_ptr),
    layout_width(1920),
    layout_height(1080)
{
    setWindowFlags(windowFlags() | Qt::FramelessWindowHint);
    setWindowIcon(QIcon(":/assets/logo.png"));
    setStyleSheet("background-color: black;");

    // Diagnostic only, for the ongoing Overlay Layout focus
    // investigation -- logs every focus transition in the whole
    // application, identifying which of view/overlay_view/native
    // widget it belongs to. `this->` members accessed inside the
    // lambda are read at call time (when the signal actually fires),
    // not at connection time, so it's safe that view/overlay_view
    // don't exist yet on this exact line.
    connect(qApp, &QApplication::focusChanged, this, [this](QWidget *old, QWidget *now) {
        auto owner = [this](QWidget *w) -> const char* {
            for (QWidget *p = w; p; p = p->parentWidget()) {
                if (p == view) return "view (main layout)";
                if (p == overlay_view) return "overlay_view";
                for (auto nview : native_views) if (p == nview) return "a native_views widget";
                for (auto nview : overlay_native_views) if (p == nview) return "an overlay_native_views widget";
            }
            return "unknown";
        };
        std::cout << "DEBUG: [arexibo::qt] focus changed: " \
                   << (old ? old->metaObject()->className() : "(null)") \
                   << " [" << (old ? owner(old) : "n/a") << "]" \
                   << " -> " \
                   << (now ? now->metaObject()->className() : "(null)") \
                   << " [" << (now ? owner(now) : "n/a") << "]" \
                   << std::endl;
    });

    view = new QWebEngineView(this);
    // Kiosk display: suppress the default context menu entirely --
    // QWebEngineView derives from QWidget and checks this policy inside
    // its own contextMenuEvent() override before showing Chromium's
    // built-in menu, and that override fires the same way for a real
    // right-click and for the touch equivalent (long-press), so this one
    // call covers both input methods.
    view->setContextMenuPolicy(Qt::NoContextMenu);
    // See LoggingPage's own doc comment -- must be set before
    // webChannel/devtools setup below, since those attach to whichever
    // page is current at that point.
    view->setPage(new LoggingPage(view));

    channel = new QWebChannel(this);
    view->page()->setWebChannel(channel);
    auto interface = new JSInterface(this);
    channel->registerObject("arexibo", interface);

    if (inspect) {
        auto devtools_window = new QMainWindow();
        auto devtools = new QWebEngineView();
        devtools_window->setWindowTitle("Arexibo - Inspector");
        devtools_window->setWindowIcon(QIcon(":/assets/logo.png"));
        devtools_window->setCentralWidget(devtools);
        devtools_window->resize(1000, 600);
        devtools_window->show();
        view->page()->setDevToolsPage(devtools->page());
    } else {
        QGuiApplication::setOverrideCursor(Qt::BlankCursor);        
    }

    connect(this, SIGNAL(navigateTo(QString)), this, SLOT(navigateToImpl(QString)));
    connect(this, SIGNAL(screenShot(int)), this, SLOT(screenShotImpl(int)));
    connect(this, SIGNAL(setTitle(QString)), this, SLOT(setWindowTitle(QString)));
    connect(this, SIGNAL(setSize(int, int, int, int)),
            this, SLOT(setSizeImpl(int, int, int, int)));
    connect(this, SIGNAL(runJavascript(QString)),
            this, SLOT(runJavascriptImpl(QString)));
    connect(this, SIGNAL(overlayShow(QString)), this, SLOT(overlayShowImpl(QString)));
    connect(this, SIGNAL(overlayHide()), this, SLOT(overlayHideImpl()));

    view->setUrl(QUrl(base_uri + "0.xlf.html"));
}

void Window::navigateToImpl(QString file) {
    clearNativeViews(false);
    // A new layout is about to load into `view` -- its own real
    // dimensions won't be known/applied again until its own
    // jsLayoutInit fires (see adjustOverlayScale's own doc comment on
    // base_layout_scaled for why this matters).
    base_layout_scaled = false;
    // `view`'s own geometry is about to change (or at least might) --
    // any previously-applied overlay scaling needs redoing against
    // whatever it ends up being for the new layout.
    overlay_scale_applied_this_cycle = false;
    overlay_scale_retry_pending = false;
    view->setUrl(QUrl(base_uri + file));
}

void Window::screenShotImpl(int max_width)
{
    // Grab the actual screen output via the X11/platform compositor,
    // rather than rendering just the `view` widget -- `view->render()`
    // only captures that one widget's content, missing entirely any
    // separate top-level `webpage render="native"` QWebEngineViews
    // layered on top of it (see jsNativeWebShowImpl above). `screen()`
    // (QWidget::screen(), always valid once the window is shown) is used
    // instead of the `selected_screen` member directly since the latter
    // can be null (no `--screen` given) -- same fallback already relied
    // on in setSizeImpl() above.
    QPixmap pixmap = screen()->grabWindow(0);
    // Confirmed real from a live report (tested against Fedora GNOME
    // Kiosk Wayland, Weston, and WSLg): under Wayland, compositors
    // deliberately block grabWindow(0)'s own X11-era "grab the root
    // window" model for security reasons -- it comes back null
    // (0x0), and saving a null QPixmap silently writes zero bytes,
    // which the CMS then can't even display (crashes its own
    // screenshot viewer on the resulting empty file). Falls back to
    // grabbing this window's own widget hierarchy client-side instead
    // (QWidget::grab(), Qt's own paint engine -- entirely bypasses
    // the compositor's desktop-capture restriction), `view` itself as
    // a last resort if even that somehow comes back null too.
    if (pixmap.isNull()) {
        pixmap = this->grab();
    }
    if (pixmap.isNull() && view) {
        pixmap = view->grab();
    }
    std::cout << "INFO : [arexibo::qt] screenshot capture: (" << pixmap.width() << "x" \
               << pixmap.height() << "), isNull=" << (pixmap.isNull() ? "true" : "false") \
               << std::endl;
    // Respect the CMS's configured ScreenShotSize (PlayerSettings::
    // screenshot_size in Rust) -- previously ignored entirely, always
    // submitting the full captured resolution regardless of what was
    // requested. 0 means "no resize" (submit at full resolution); only
    // downscale, never upscale (a max_width bigger than the actual
    // capture would otherwise blow it up pointlessly).
    if (max_width > 0 && pixmap.width() > max_width) {
        pixmap = pixmap.scaledToWidth(max_width, Qt::SmoothTransformation);
    }
    QByteArray array;
    QBuffer buffer(&array);
    buffer.open(QIODevice::WriteOnly);
    // The CMS itself stores/expects screenshots as .jpg (confirmed
    // real: "{displayId}_screenshot.jpg") -- JPEG (quality 85) also
    // shrinks a typical capture from ~2MB (lossless PNG) down to
    // roughly 75KB. PNG only as a fallback, if the JPEG plugin
    // somehow isn't available.
    if (!pixmap.save(&buffer, "JPEG", 85)) {
        pixmap.save(&buffer, "PNG");
    }
    std::cout << "INFO : [arexibo::qt] screenshot buffer size: " << array.size() << " bytes" \
              << std::endl;
    cb(cb_ptr, CB_SCREENSHOT, (intptr_t)array.constData(), array.size(), 0);
}

void Window::setSizeImpl(int pos_x, int pos_y, int size_x, int size_y)
{
    if (selected_screen)
        setScreen(selected_screen);
    QRect screenGeometry = screen()->geometry();
    int offset_x = screenGeometry.x();
    int offset_y = screenGeometry.y();
    int screen_w = screenGeometry.width();
    int screen_h = screenGeometry.height();

    // need to scale Xibo values (meant to be real pixels) by the device pixel ratio
    auto ratio = screen()->devicePixelRatio();
    pos_x = std::round(pos_x / ratio);
    pos_y = std::round(pos_y / ratio);
    size_x = std::round(size_x / ratio);
    size_y = std::round(size_y / ratio);

    if (size_x == 0) size_x = screen_w;
    if (size_y == 0) size_y = screen_h;

    // calculate window position and size
    if (size_x == screen_w && size_y == screen_h && pos_x == 0 && pos_y == 0) {
        resize(size_x, size_y);
        move(offset_x, offset_y);
        showFullScreen();
        std::cout << "INFO : [arexibo::qt] size: full screen ("
                  << size_x*ratio << "x" << size_y*ratio << ")" << std::endl;
    } else {
        setWindowState(windowState() & ~Qt::WindowFullScreen);
        resize(size_x, size_y);
        move(offset_x + pos_x, offset_y + pos_y);
        std::cout << "INFO : [arexibo::qt] size: windowed ("
                  << size_x*ratio << "x" << size_y*ratio << ")+"
                  << pos_x*ratio << "+" << pos_y*ratio << std::endl;
    }

    adjustScale(layout_width, layout_height);
}

void Window::adjustScale(int layout_w, int layout_h)
{
    layout_width = layout_w;
    layout_height = layout_h;

    // need to scale Xibo values (meant to be real pixels) by the device pixel ratio
    auto ratio = screen()->devicePixelRatio();
    layout_w = std::round(layout_w / ratio);
    layout_h = std::round(layout_h / ratio);

    int window_w = width();
    int window_h = height();

    if (window_w == 0 || window_h == 0 || layout_h == 0 || layout_w == 0)
        return;

    // the easy case: direct match
    if (window_w == layout_w && window_h == layout_h) {
        view->move(0, 0);
        view->resize(layout_w, layout_h);
        view->setZoomFactor(1.0);
        std::cout << "INFO : [arexibo::qt] scale: window = layout ("
                  << layout_w*ratio << "x" << layout_h*ratio << ")" << std::endl;
        return;
    }

    // adjust position of webview within the window, and apply the scale
    double window_aspect = (double)window_w / (double)window_h;
    double layout_aspect = (double)layout_w / (double)layout_h;
    double scale_factor;
    if (window_aspect > layout_aspect) {
        scale_factor = (double)window_h / (double)layout_h;
        int webview_w = (int)((double)layout_w * scale_factor);
        view->move((window_w - webview_w) / 2, 0);
        view->resize(webview_w, window_h);
        view->setZoomFactor(scale_factor);
    } else {
        scale_factor = (double)window_w / (double)layout_w;
        int webview_h = (int)((double)layout_h * scale_factor);
        view->move(0, (window_h - webview_h) / 2);
        view->resize(window_w, webview_h);
        view->setZoomFactor(scale_factor);
    }
    std::cout << "INFO : [arexibo::qt] scale: window ("
              << window_w*ratio << "x" << window_h*ratio << "), layout ("
              << layout_w*ratio << "x" << layout_h*ratio << "), result: ("
              << view->width()*ratio << "x" << view->height()*ratio << ")+"
              << view->x()*ratio << "+" << view->y()*ratio
              << " with zoom " << scale_factor << std::endl;
}

void Window::adjustOverlayScale(int layout_w, int layout_h)
{
    // Mirrors adjustScale() above, but sized/positioned to exactly match
    // the MAIN view's own geometry (view->x()/y()/width()/height()) rather
    // than the window -- an overlay is meant to sit on top of the main
    // layout, not to be independently letterboxed against the physical
    // screen, which could otherwise misalign it against the content it's
    // supposed to overlay if the main layout's aspect ratio doesn't match
    // the screen's.
    overlay_layout_width = layout_w;
    overlay_layout_height = layout_h;

    auto ratio = screen()->devicePixelRatio();
    layout_w = std::round(layout_w / ratio);
    layout_h = std::round(layout_h / ratio);

    int area_w = view->width();
    int area_h = view->height();

    std::cout << "DEBUG: [arexibo::qt] adjustOverlayScale(" << layout_w << "x" << layout_h \
               << "), view area is (" << area_w << "x" << area_h << "), base_layout_scaled=" \
               << (base_layout_scaled ? "true" : "false") << ", retry_count=" \
               << overlay_scale_retry_count << std::endl;

    // Found from a real report: with every file already cached (so
    // every page loads near-instantly), this can genuinely be called
    // -- both from overlayShowImpl (with the last-known/default
    // dimensions, right as the overlay's own page starts loading) and
    // later from jsLayoutInit's own overlay branch (with the real
    // ones, once that page reports it) -- before `view` itself
    // (the *main* layout) has applied its own *current* layout's real
    // dimensions yet. Checking area_w/area_h alone isn't enough on its
    // own -- a real report showed `view` genuinely non-zero at the
    // exact moment this ran, but still sized from an earlier generic/
    // default resize event (before any real layout had reported its
    // own size at all), not this specific layout's own real
    // dimensions -- `base_layout_scaled` (see its own doc comment in
    // view.h) tracks that distinction directly instead. Previously
    // this returned silently when unready and nothing ever retried
    // it, leaving the overlay sized wrong (or not at all) for its
    // entire lifetime -- with `--clear` (real downloads, always some
    // delay), the base layout's own real sizing was in practice
    // already applied by the time this ran, so the race went
    // unnoticed. Retries shortly afterward instead, bounded so a
    // genuinely pathological case (no main layout at all) can't retry
    // forever.
    if (area_w == 0 || area_h == 0 || !base_layout_scaled) {
        // At most one retry timer in flight at a time -- found from a
        // real report: every single failed call (whether from this
        // retry lambda itself, or from a *different* call site like
        // jsLayoutInit's own overlay branch, which calls this directly
        // with no guard of its own at all) used to schedule its own
        // brand new timer, regardless of whether one was already
        // pending from an earlier failed call. With several callers
        // failing in quick succession (a real, confirmed sequence:
        // overlayShowImpl's own initial call, several of this
        // function's own retries, and jsLayoutInit's own direct call,
        // all within the same ~200ms base-layout-loading window),
        // multiple independent timers ended up in flight
        // simultaneously -- and since nothing stopped them once one
        // eventually succeeded, the other, still-pending ones fired
        // afterward too, redundantly reapplying the exact same
        // geometry a second (or third) time regardless of the
        // overlay_scale_applied_this_cycle guard, since none of THEM
        // needed a fresh schedule at all, just a chance to fire once.
        if (!overlay_scale_retry_pending && overlay_scale_retry_count < 20) {
            overlay_scale_retry_count++;
            overlay_scale_retry_pending = true;
            std::cout << "DEBUG: [arexibo::qt] adjustOverlayScale: base layout not (yet) " \
                         "sized for real, scheduling retry " << overlay_scale_retry_count \
                      << "/20" << std::endl;
            // Deliberately reads overlay_layout_width/height fresh
            // inside the lambda body at *fire* time, NOT captured by
            // value here at schedule time -- found from a real report:
            // capturing by value meant that if the overlay's own real
            // dimensions arrived (via jsLayoutInit) *after* this retry
            // was scheduled but *before* it fired, the retry still
            // fired with the original, stale target dimensions this
            // call started with (e.g. still the generic 1920x1080
            // default from before the overlay's own page had reported
            // anything), briefly applying a wrong, letterboxed
            // geometry moments before the real call corrected it again
            // -- two full setGeometry()/setZoomFactor() calls on
            // overlay_view within a few milliseconds where one, with
            // the right values from the start, would do.
            QTimer::singleShot(50, this, [this]() {
                overlay_scale_retry_pending = false;
                if (overlay_view && !overlay_scale_applied_this_cycle) {
                    adjustOverlayScale(overlay_layout_width, overlay_layout_height);
                }
            });
        } else if (overlay_scale_retry_count >= 20) {
            std::cout << "WARN : [arexibo::qt] adjustOverlayScale: giving up after 20 " \
                         "retries, base layout still not sized for real" << std::endl;
        }
        return;
    }
    overlay_scale_retry_count = 0;

    if (layout_h == 0 || layout_w == 0)
        return;

    if (area_w == layout_w && area_h == layout_h) {
        std::cout << "DEBUG: [arexibo::qt] adjustOverlayScale: exact match, no scaling needed" \
                   << std::endl;
        overlay_view->setGeometry(view->x(), view->y(), layout_w, layout_h);
        overlay_view->setZoomFactor(1.0);
        overlay_scale_applied_this_cycle = true;
        return;
    }

    double area_aspect = (double)area_w / (double)area_h;
    double layout_aspect = (double)layout_w / (double)layout_h;
    double scale_factor;
    int ov_x, ov_y, ov_w, ov_h;
    if (area_aspect > layout_aspect) {
        scale_factor = (double)area_h / (double)layout_h;
        ov_w = (int)((double)layout_w * scale_factor);
        ov_h = area_h;
        ov_x = view->x() + (area_w - ov_w) / 2;
        ov_y = view->y();
    } else {
        scale_factor = (double)area_w / (double)layout_w;
        ov_w = area_w;
        ov_h = (int)((double)layout_h * scale_factor);
        ov_x = view->x();
        ov_y = view->y() + (area_h - ov_h) / 2;
    }
    std::cout << "DEBUG: [arexibo::qt] adjustOverlayScale: applying geometry (" << ov_x << "," \
               << ov_y << " " << ov_w << "x" << ov_h << ") with zoom " << scale_factor \
               << std::endl;
    overlay_view->setGeometry(ov_x, ov_y, ov_w, ov_h);
    overlay_view->setZoomFactor(scale_factor);
    overlay_scale_applied_this_cycle = true;
}

void Window::runJavascriptImpl(QString js)
{
    // Gated behind --web-debug (see g_web_debug_enabled's own doc
    // comment in view.h) -- found from a real report: this fires on
    // *every single* JS execution through this path (region switches,
    // ReloadWidget, navWidget, Interactive Control duration changes,
    // etc.), printing the full JS source each time, which was
    // previously unconditional and adds up to a fair amount of noise
    // over a long-running session.
    if (g_web_debug_enabled) {
        std::cout << "INFO : [arexibo::qt] run JavaScript: " << js.toStdString() << std::endl;
    }
    view->page()->runJavaScript(js);
}

void Window::clearNativeViews(bool overlay)
{
    auto &views = overlay ? overlay_native_views : native_views;
    for (auto nview : views) {
        nview->deleteLater();
    }
    views.clear();
}

void Window::ensureOverlayView()
{
    if (overlay_view) return;

    overlay_view = new QWebEngineView(this);
    overlay_view->setContextMenuPolicy(Qt::NoContextMenu);
    overlay_view->setPage(new LoggingPage(overlay_view));
    // Transparent background so parts of the overlay layout that don't
    // paint anything (e.g. a region-less area of a PNG-with-alpha
    // banner) let the main layout underneath show through, rather than
    // compositing as opaque white/black.
    overlay_view->page()->setBackgroundColor(Qt::transparent);
    overlay_view->setAttribute(Qt::WA_AlwaysStackOnTop);
    overlay_view->setAttribute(Qt::WA_TranslucentBackground);

    // BUG fix (found from a real report: an Overlay Layout with only a
    // few small regions rendered correctly, but hid the main layout
    // underneath entirely). The `Qt::transparent`/`WA_TranslucentBackground`
    // pair above only takes effect for *unpainted* areas -- but this
    // layout's own XLF `bgcolor`/`background` attribute (see layout.rs's
    // `write_layout`, which applies it unconditionally, with no way to
    // know at translation time whether a given layout will be shown
    // normally or as an overlay) gets baked into the page's own CSS as
    // an *opaque* `body { background-color: ...; }` rule -- painted by
    // the page itself, completely defeating any Qt-level transparency
    // regardless of what's underneath at the OS compositing level. This
    // exact scenario is documented Xibo behavior, confirmed via the
    // official manual: "Xibo will not render the background on Players
    // when a Layout is scheduled as an Overlay Layout" -- i.e. the
    // *player*, not the CMS, is responsible for suppressing it, and only
    // for layouts actually being shown *as* an overlay (the same layout
    // could in principle also be scheduled normally at another time,
    // where its own declared background should still apply -- so this
    // is deliberately a page-specific script attached to `overlay_view`'s
    // own page, not a change to the shared `QWebEngineProfile` that
    // would also strip the main view's own layouts' backgrounds).
    // Deliberately does NOT use `runsOnSubFrames` (defaults to false) --
    // only the overlay layout's own top-level body background should be
    // stripped, not individual widgets' own legitimate background colors
    // (e.g. a colored rectangle "global element"), which live in
    // separate iframes untouched by this.
    const char *stripOverlayBackgroundScript = R"JS(
        (function() {
            function strip() {
                if (document.body) {
                    document.body.style.backgroundColor = 'transparent';
                    document.body.style.backgroundImage = 'none';
                }
            }
            if (document.readyState === 'loading') {
                document.addEventListener('DOMContentLoaded', strip);
            } else {
                strip();
            }
        })();
    )JS";
    QWebEngineScript stripBg;
    stripBg.setName(QStringLiteral("arexibo-strip-overlay-background"));
    stripBg.setSourceCode(QString::fromUtf8(stripOverlayBackgroundScript));
    stripBg.setInjectionPoint(QWebEngineScript::DocumentReady);
    stripBg.setWorldId(QWebEngineScript::MainWorld);
    overlay_view->page()->scripts().insert(stripBg);

    overlay_channel = new QWebChannel(this);
    overlay_view->page()->setWebChannel(overlay_channel);
    auto interface = new JSInterface(this, /*is_overlay=*/true);
    overlay_channel->registerObject("arexibo", interface);

    overlay_view->hide();
}

void Window::overlayShowImpl(QString file)
{
    ensureOverlayView();
    clearNativeViews(/*overlay=*/true);
    overlay_view->setUrl(QUrl(base_uri + file));
    // A fresh overlay show cycle -- any earlier successful scaling no
    // longer applies (see overlay_scale_applied_this_cycle's own doc
    // comment in view.h).
    overlay_scale_applied_this_cycle = false;
    overlay_scale_retry_pending = false;
    adjustOverlayScale(overlay_layout_width, overlay_layout_height);
    overlay_view->show();
    overlay_view->raise();

    // Touch/click passthrough -- see overlay_region_rects's own doc
    // comment in view.h. Installed on focusProxy(), not overlay_view
    // itself (confirmed via research: installing on the view directly
    // doesn't work at all, QTBUG-43602). Installed HERE, after show(),
    // not in ensureOverlayView() (right after construction) -- found
    // from a real report/log that focusProxy() is genuinely still null
    // that early, confirming the contingency this comment used to only
    // flag as a possibility. `overlay_filter_installed` guards against
    // installing more than once on the same overlay_view instance
    // (overlayShowImpl can run again for the same instance, e.g. the
    // overlay's own content refreshing, without a full hide/show
    // teardown in between).
    if (!overlay_filter_installed) {
        if (auto *proxy = overlay_view->focusProxy()) {
            proxy->installEventFilter(this);
            overlay_filter_installed = true;
            std::cout << "DEBUG: [arexibo::qt] touch/click passthrough event filter " \
                         "installed on overlay focusProxy" << std::endl;
        } else {
            std::cout << "WARN : [arexibo::qt] overlay_view->focusProxy() was still null " \
                         "after show() -- touch/click passthrough will not work for " \
                         "this overlay" << std::endl;
        }
    }
}

void Window::overlayHideImpl()
{
    if (!overlay_view) return;
    clearNativeViews(/*overlay=*/true);
    // Full teardown, not just hide() -- an overlay is meant to be a
    // transient, occasional thing (an alert, an ad-hoc announcement),
    // not a second permanent renderer process idling in the background
    // between uses. ensureOverlayView() recreates everything from
    // scratch on the next overlayLayout action.
    overlay_view->deleteLater();
    overlay_view = nullptr;
    overlay_channel->deleteLater();
    overlay_channel = nullptr;
    overlay_region_rects.clear();
    overlay_filter_installed = false;
    overlay_reraise_scheduled = false;
    overlay_scale_retry_count = 0;
}

bool Window::eventFilter(QObject *watched, QEvent *event)
{
    // "Focus follows touch" for every native widget (both the main
    // layout's own native_views and the overlay's own
    // overlay_native_views) -- see jsNativeWebShowImpl's own doc
    // comment on installEventFilter for the full story. Deliberately
    // observes only -- calls setFocus() as a side effect and always
    // falls through to normal handling afterward (never returns true
    // here), so gesture recognition (drag, pinch, double-tap-to-zoom)
    // proceeds exactly as it would without this filter; setFocus()
    // itself doesn't consume or alter the event in any way.
    if (event->type() == QEvent::MouseButtonPress || event->type() == QEvent::TouchBegin) {
        // Diagnostic only, for the ongoing investigation into why a
        // real report contradicts the assumption that
        // WA_AlwaysStackOnTop also guarantees which widget *receives*
        // a touch, not just which one paints on top -- identifies
        // exactly which widget `watched` corresponds to, since a
        // wrong answer here means the touch itself is reaching the
        // wrong widget at the OS/compositor level, which no amount of
        // setFocus() afterward could fix.
        const char *which = "UNKNOWN (none of view/overlay_view/any native view)";
        if (view && watched == view->focusProxy()) which = "view (main layout)";
        else if (overlay_view && watched == overlay_view->focusProxy()) which = "overlay_view";
        else {
            for (auto nview : native_views) {
                if (watched == nview->focusProxy()) { which = "a native_views widget"; break; }
            }
            for (auto nview : overlay_native_views) {
                if (watched == nview->focusProxy()) { which = "an overlay_native_views widget"; break; }
            }
        }
        std::cout << "DEBUG: [arexibo::qt] press/touch actually delivered to: " << which \
                  << std::endl;

        for (auto nview : native_views) {
            if (watched == nview->focusProxy()) { nview->setFocus(); break; }
        }
        for (auto nview : overlay_native_views) {
            if (watched == nview->focusProxy()) { nview->setFocus(); break; }
        }
    }

    // Touch/click passthrough for an active Overlay Layout -- see
    // overlay_region_rects's own doc comment in view.h for the full
    // story, and its own installation point in ensureOverlayView().
    // Compiles cleanly against real Qt6 6.4.2 headers, but that's not
    // the same as verified runtime behavior on a real touchscreen --
    // needs real hardware testing before trusting it.
    if (!overlay_view || watched != overlay_view->focusProxy()) {
        return QMainWindow::eventFilter(watched, event);
    }

    // A forwarded gesture is already in progress (started on a press
    // that landed in an empty area) -- its own move/release events must
    // ALSO be forwarded to `view`, not just the initial press. Found
    // from a real report: forwarding only the press left `view` with a
    // mousedown but no matching mouseup, so a button's own click never
    // actually registered there at all (a DOM click needs both on the
    // same element). Consumed here either way, so the overlay's own
    // page never sees any part of a gesture that isn't its own.
    if (overlay_forwarding_active) {
        QEvent::Type t = event->type();
        if (t == QEvent::MouseMove || t == QEvent::MouseButtonRelease) {
            forwardMouseEventToView(t, static_cast<QMouseEvent*>(event)->position().toPoint());
            if (t == QEvent::MouseButtonRelease) overlay_forwarding_active = false;
            return true;
        }
        if (t == QEvent::TouchUpdate || t == QEvent::TouchEnd) {
            auto *te = static_cast<QTouchEvent*>(event);
            if (!te->points().isEmpty()) {
                QEvent::Type mouseType = (t == QEvent::TouchEnd)
                    ? QEvent::MouseButtonRelease : QEvent::MouseMove;
                forwardMouseEventToView(mouseType, te->points().first().position().toPoint());
            }
            if (t == QEvent::TouchEnd) overlay_forwarding_active = false;
            return true;
        }
        // Some other, unrelated event type arriving mid-gesture --
        // leave it alone.
        return QMainWindow::eventFilter(watched, event);
    }

    // Only a *press* starts a new decision -- see above for why its
    // own follow-up events are then tracked via overlay_forwarding_active
    // instead of being independently re-decided here.
    QPoint pos;
    if (event->type() == QEvent::MouseButtonPress) {
        pos = static_cast<QMouseEvent*>(event)->position().toPoint();
    } else if (event->type() == QEvent::TouchBegin) {
        auto *te = static_cast<QTouchEvent*>(event);
        if (te->points().isEmpty()) {
            return QMainWindow::eventFilter(watched, event);
        }
        pos = te->points().first().position().toPoint();
    } else {
        return QMainWindow::eventFilter(watched, event);
    }
    std::cout << "DEBUG: [arexibo::qt] overlay press/touch at (" << pos.x() << "," \
               << pos.y() << ")" << std::endl;

    for (const QRect &r : overlay_region_rects) {
        if (r.contains(pos)) {
            // Real overlay content at this exact point -- give the
            // overlay's own main view focus (same "focus follows
            // touch" reasoning as above, just for overlay_view itself
            // rather than one of its native widgets), then let
            // Chromium handle the event completely normally.
            overlay_view->setFocus();
            std::cout << "DEBUG: [arexibo::qt] -> inside known overlay content rect (" \
                       << r.x() << "," << r.y() << " " << r.width() << "x" << r.height() \
                       << "), letting Chromium handle it normally" << std::endl;
            return QMainWindow::eventFilter(watched, event);
        }
    }

    // An empty area of the overlay -- forward the whole gesture (this
    // press, and its own move/release above) to the main layout
    // underneath instead of letting the overlay silently absorb it doing
    // nothing. `overlay_view` and `view` share the exact same on-screen
    // geometry (see overlayShowImpl's own setGeometry call, matching
    // `view`'s own), so the same local point applies to both unchanged.
    // Deliberately simplified to synthesized *mouse* events even for a
    // touch gesture -- faithfully reconstructing Qt's own full
    // multi-point touch event machinery here is considerably more
    // involved, and a plain click/drag already achieves the practical
    // outcome a kiosk touchscreen needs (activating/dragging whatever's
    // underneath).
    std::cout << "DEBUG: [arexibo::qt] -> outside every known overlay content rect, " \
                 "forwarding to the main layout" << std::endl;
    overlay_forwarding_active = true;
    forwardMouseEventToView(QEvent::MouseButtonPress, pos);
    return true; // consume the original event -- the overlay must not also react to it
}

// Builds and posts a single synthesized mouse event of the given type,
// at `pos` (already in `view`'s own local coordinates -- see
// eventFilter's own doc comment for why no translation is needed), to
// the main layout. Shared by every step of a forwarded gesture
// (press/move/release) so they all agree on the same button/modifier
// state a real gesture would have throughout.
//
// Posted to whichever *actual* widget occupies this exact point --
// NOT unconditionally `view` itself. Found from a real report: a
// point outside every known overlay content rect was being forwarded
// straight to `view->focusProxy()`, even when that exact spot is
// really occupied by one of the main layout's own *native* widgets
// (`webpage render="native"`, e.g. an embedded page) -- a separate
// sibling QWebEngineView in `native_views`, not part of `view`'s own
// content at all, so the forwarded event never reached it. Checks
// each native_views widget's own (window-relative) geometry first;
// falls back to `view` only if none of them contains this point.
//
// Posted to that widget's own focusProxy(), NOT the widget itself --
// confirmed from the real Qt6 header (qwebengineview.h):
// QWebEngineView overrides only the generic `event(QEvent*)`, never
// mousePressEvent/mouseMoveEvent/mouseReleaseEvent specifically,
// delegating real mouse handling entirely to its own internal
// focusProxy() child widget instead (the exact same QTBUG-43602
// mechanism already confirmed for *installing* the event filter on
// the overlay's own side -- missed here at first, applied
// inconsistently, until a real report showed the forwarded click
// simply never registering on `view`'s own page at all).
void Window::forwardMouseEventToView(QEvent::Type type, QPoint pos)
{
    if (!view) {
        std::cout << "WARN : [arexibo::qt] forwardMouseEventToView: no `view` to forward to" \
                  << std::endl;
        return;
    }
    // `pos` arrives in the same local coordinate space as
    // overlay_view/view themselves (see eventFilter's own doc
    // comment on why no translation is needed there) -- converted to
    // a window-relative point here, since native_views' own geometry
    // (set in jsNativeWebShowImpl, `base->x() + ...`) is
    // window-relative too.
    QPoint windowPos = view->pos() + pos;

    QWebEngineView *target_view = view;
    for (auto nview : native_views) {
        if (nview->geometry().contains(windowPos)) {
            target_view = nview;
            break;
        }
    }

    QWidget *target = target_view->focusProxy();
    if (!target) target = target_view;
    QPoint localPos = windowPos - target_view->pos();
    std::cout << "DEBUG: [arexibo::qt] forwarding event type " << type << " at (" \
               << localPos.x() << "," << localPos.y() << ") to " \
               << (target_view == view ? "view" : "a native_views widget") \
               << (target == target_view ? " (focusProxy was null)" : "->focusProxy()") \
               << std::endl;
    auto *forwarded = new QMouseEvent(type, QPointF(localPos), target->mapToGlobal(localPos),
                                       Qt::LeftButton, Qt::LeftButton, Qt::NoModifier);
    QCoreApplication::postEvent(target, forwarded);
}

// Found from a real report, backed by a real timing comparison
// between a successful and a failed run: creating several
// QWebEngineView widgets back-to-back within a few tens of
// milliseconds (as happens whenever a layout with several native
// widgets first loads, doubly so with an Overlay Layout ALSO doing
// the same at the same time) appears to occasionally overwhelm
// Chromium's own compositor, leaving the *entire* main layout
// unrendered (a plain background-colour screen) for that whole
// session -- the failed run's own native widgets were created
// measurably more tightly clustered together (as little as 3ms
// between the last two) than the successful run's (54ms). Spreading
// genuinely *new* widget creation out over time (existing widgets
// simply being updated -- a new URL/geometry for an already-created
// one -- are NOT staggered at all, since no new QWebEngineView is
// being created for those) trades a small, likely imperceptible delay
// (native widgets appearing one after another rather than all at
// once) for -- hopefully -- materially more reliable rendering.
void Window::jsNativeWebShowImpl(bool overlay, int mediaId, QString url, int x, int y, int w, int h)
{
    auto &views = overlay ? overlay_native_views : native_views;
    bool already_exists = views.contains(mediaId);
    std::cout << "DEBUG: [arexibo::qt] jsNativeWebShowImpl mediaId=" << mediaId \
               << " overlay=" << overlay << " already_exists=" << already_exists \
               << " queue_size=" << pending_native_web_shows.size() \
               << " stagger_active=" << native_web_show_stagger_active << std::endl;
    if (already_exists) {
        // Already exists -- no new QWebEngineView is being created, so
        // there's nothing to stagger; apply the update immediately.
        jsNativeWebShowImplNow(overlay, mediaId, url, x, y, w, h);
        return;
    }
    pending_native_web_shows.append({overlay, mediaId, url, x, y, w, h});
    if (!native_web_show_stagger_active) {
        native_web_show_stagger_active = true;
        // 1s delay before the first native widget navigation, so it
        // doesn't compete with the initial page-load burst (see status doc).
        QTimer::singleShot(1000, this, [this]() { processNextPendingNativeWebShow(); });
    }
}

void Window::processNextPendingNativeWebShow()
{
    if (pending_native_web_shows.isEmpty()) {
        native_web_show_stagger_active = false;
        return;
    }
    auto item = pending_native_web_shows.takeFirst();
    std::cout << "DEBUG: [arexibo::qt] creating staggered native widget " << item.mediaId \
               << " (" << pending_native_web_shows.size() << " more queued)" << std::endl;
    jsNativeWebShowImplNow(item.overlay, item.mediaId, item.url, item.x, item.y, item.w, item.h);
    QTimer::singleShot(20, this, [this]() { processNextPendingNativeWebShow(); });
}

void Window::jsNativeWebShowImplNow(bool overlay, int mediaId, QString url, int x, int y, int w, int h)
{
    auto &views = overlay ? overlay_native_views : native_views;
    QWebEngineView *nview = views.value(mediaId, nullptr);
    if (!nview) {
        nview = new QWebEngineView(this);
        nview->setContextMenuPolicy(Qt::NoContextMenu);
        nview->setPage(new LoggingPage(nview, /*profile=*/nullptr, /*hang_watchdog_secs=*/15));
        // BUG fix (found from a real report: an overlay's own native
        // widget -- a `webpage render="native"`/interactive-button
        // *inside* the Overlay Layout itself -- wasn't visible, hidden
        // behind the overlay's own background). `overlay_view` has
        // `Qt::WA_AlwaysStackOnTop` set (see ensureOverlayView) so that
        // it reliably paints above the *main* layout's `view` and its
        // own native widgets -- but that attribute specifically means
        // "always above ordinary sibling widgets that don't ALSO have
        // it", regardless of raise()/lower() order. A plain `nview`
        // (this one, without the attribute) would therefore always
        // render *underneath* `overlay_view`, no matter how many times
        // `raise()` is called on it -- exactly backwards for a native
        // widget that's meant to be part of/on top of the overlay's own
        // content. Giving the overlay's own native widgets the same
        // attribute puts them on the same "always on top" tier as
        // `overlay_view`, where normal raise() ordering between the two
        // then determines which is on top of the other -- and since
        // this is only ever called (with a real URL) after the overlay
        // page has already loaded and started running its own widgets,
        // by which point `overlay_view->raise()` has already happened
        // (see overlayShowImpl), this widget's own `raise()` below
        // correctly ends up on top. WA_AlwaysStackOnTop is NOT set for
        // `native_views` (the main layout's own native widgets) --
        // this was originally reasoned to be unnecessary ("ordinary
        // raise()-order already puts them above it with no special
        // handling"), but a real report and its own diagnostic
        // logging directly contradicted that: WA_AlwaysStackOnTop
        // evidently does not reliably guarantee which widget actually
        // *receives* touch/click input over another, at least not
        // consistently on its own. See this function's own explicit
        // re-raise of the overlay after a main-layout native widget's
        // own raise() below, added because of exactly this.
        if (overlay) {
            nview->setAttribute(Qt::WA_AlwaysStackOnTop);
        }
        views[mediaId] = nview;
    }
    // Position in the same coordinate space the relevant base view
    // (`view` for the main layout, `overlay_view` for the overlay) itself
    // already uses -- its `x()`/`y()` is where the (possibly letterboxed)
    // view sits within the window, and its `zoomFactor()` is the same
    // scale factor adjustScale()/adjustOverlayScale() applies to it, so
    // an XLF region's raw (x, y, w, h) lines up with that layout exactly
    // by re-using both here, instead of re-deriving the transform
    // separately.
    QWebEngineView *base = overlay ? overlay_view : view;
    double zoom = base->zoomFactor();
    nview->setGeometry(
        base->x() + (int)(x * zoom),
        base->y() + (int)(y * zoom),
        (int)(w * zoom),
        (int)(h * zoom)
    );
    if (nview->url().toString() != url) {
        nview->setUrl(QUrl(url));
    }
    nview->show();
    nview->raise();
    // Found from a real report, confirmed directly via diagnostic
    // logging that a touch on the overlay's own area was genuinely
    // delivered to this *main-layout* native widget instead: contrary
    // to this function's own earlier assumption above ("ordinary
    // raise()-order already puts them above it with no special
    // handling" for the overlay), Qt::WA_AlwaysStackOnTop evidently
    // does not reliably guarantee which widget actually *receives*
    // touch/click input, at least not on its own -- only, at best,
    // paint order. A main-layout native widget being shown or
    // refreshed (e.g. a periodically-updating dataset widget) calls
    // raise() on itself here, which can apparently still let it
    // receive input over the overlay's own content in the same
    // screen area. Explicitly re-raising the overlay (and its own
    // native widgets) again right afterward, whenever one is
    // currently active, restores it -- the same remedy
    // navigateToImpl already applies after a full layout switch,
    // extended here to cover this other, previously-missed path.
    //
    // Debounced (see overlay_reraise_scheduled's own doc comment in
    // view.h) -- found from a real report: doing this unconditionally
    // on every single call broke rendering of most of the main layout
    // during its own initial load, when several of its own native
    // widgets are created back-to-back (at the same time an Overlay
    // Layout's own several native widgets are ALSO being created),
    // producing dozens of raise() calls within a few hundred
    // milliseconds. A single deferred re-raise, 150ms after the last
    // of a whole burst of calls, still restores the overlay's own
    // stacking (the periodically-refreshing-widget case this was
    // written for is far slower than 150ms between occurrences, so it
    // still gets its own individual re-raise each time) without
    // disrupting the initial simultaneous-creation window.
    if (!overlay && overlay_view && !overlay_reraise_scheduled) {
        overlay_reraise_scheduled = true;
        QTimer::singleShot(150, this, [this]() {
            overlay_reraise_scheduled = false;
            if (overlay_view) {
                overlay_view->raise();
                for (auto onview : overlay_native_views) {
                    onview->raise();
                }
            }
        });
    }
    // "Focus follows touch" -- see eventFilter's own doc comment for
    // the full story (a real report: several native widgets created
    // back-to-back in one Overlay Layout left whichever one was shown
    // *last* holding focus indefinitely, with a genuine subsequent
    // touch on a different one, e.g. a map, never registering at
    // all). Installed after show() rather than right after
    // construction -- focusProxy() has been confirmed genuinely null
    // that early elsewhere in this file (ensureOverlayView). Qt
    // tolerates installing the same filter on the same object more
    // than once (this runs on every call, not just first creation --
    // e.g. a dataset widget refreshing periodically calls this again
    // with the same nview) -- harmless here, since the filter's own
    // handling (setFocus(), never consuming) is naturally idempotent.
    if (auto *proxy = nview->focusProxy()) {
        proxy->installEventFilter(this);
    }
}

void Window::jsNativeWebHideImpl(bool overlay, int mediaId)
{
    auto &views = overlay ? overlay_native_views : native_views;
    if (auto nview = views.value(mediaId, nullptr)) {
        std::cout << "DEBUG: [arexibo::qt] jsNativeWebHideImpl mediaId=" << mediaId \
                   << " overlay=" << overlay << std::endl;
        nview->hide();
    }
}

// Callbacks from JavaScript

void JSInterface::jsLayoutInit(int id, int width, int height, QString region_geometry_json)
{
    if (is_overlay) {
        std::cout << "INFO : [arexibo::qt] overlay layout " << id << " initialized" << std::endl;
        wnd->adjustOverlayScale(width, height);
        // Touch/click passthrough -- see overlay_region_rects's own doc
        // comment in view.h. Parsed here (not at HTML-generation time,
        // Rust side) since this is the C++-side data structure the
        // event filter actually reads from at click-time.
        wnd->overlay_region_rects.clear();
        QJsonParseError err;
        QJsonDocument doc = QJsonDocument::fromJson(region_geometry_json.toUtf8(), &err);
        if (err.error == QJsonParseError::NoError && doc.isArray()) {
            for (const QJsonValue &v : doc.array()) {
                QJsonObject o = v.toObject();
                wnd->overlay_region_rects.append(QRect(
                    o.value("x").toInt(), o.value("y").toInt(),
                    o.value("w").toInt(), o.value("h").toInt()));
            }
        } else {
            std::cout << "WARN : [arexibo::qt] failed to parse overlay region geometry -- " \
                         "touch/click passthrough will treat this whole overlay as empty" \
                      << std::endl;
        }
        std::cout << "DEBUG: [arexibo::qt] overlay_region_rects now has " \
                  << wnd->overlay_region_rects.size() << " rect(s):";
        for (const QRect &r : wnd->overlay_region_rects) {
            std::cout << " (" << r.x() << "," << r.y() << " " << r.width() << "x" \
                       << r.height() << ")";
        }
        std::cout << std::endl;
        wnd->cb(wnd->cb_ptr, CB_OVERLAY_LAYOUT_INIT, id, width, height);
        return;
    }
    // Splash screen (id 0) declares a fixed 1920x1080 size regardless
    // of real screen orientation -- on portrait screens this makes
    // adjustScale() letterbox it, showing black bars instead of the
    // splash's own white background. Override with the real window
    // size so it always fills the screen exactly.
    if (id == 0) {
        auto ratio = wnd->screen()->devicePixelRatio();
        width = std::round(wnd->width() * ratio);
        height = std::round(wnd->height() * ratio);
    }
    std::cout << "INFO : [arexibo::qt] layout " << id << " initialized" << std::endl;
    wnd->adjustScale(width, height);
    // `view` now genuinely reflects this layout's own real dimensions
    // -- see adjustOverlayScale's own doc comment on base_layout_scaled
    // for why this distinct signal is needed (view's own area being
    // merely non-zero isn't enough on its own: a real report showed it
    // non-zero but still sized from an earlier generic/default resize
    // event, not this specific layout's own real size, at the exact
    // moment an overlay's own scaling read it).
    wnd->base_layout_scaled = true;
    wnd->cb(wnd->cb_ptr, CB_LAYOUT_INIT, id, width, height);
}

void JSInterface::jsLayoutDone(int id)
{
    // For the overlay, there is no cross-layout cycling to do -- it's a
    // single standalone layout, not one of several concurrently
    // scheduled top-level layouts (that's what CB_LAYOUT_NEXT/Schedule<T>
    // in gui.rs are for). Its own regions keep looping via their own JS
    // timers (region_switch in layout.rs) regardless of this signal, for
    // as long as the overlay stays visible -- so this is simply a no-op.
    if (is_overlay) return;
    wnd->cb(wnd->cb_ptr, CB_LAYOUT_NEXT, id, 0, 0);
}

void JSInterface::jsLayoutPrev(int id)
{
    // No defined semantics for "previous layout" from inside a standalone
    // overlay (there's no history to go back to) -- ignored, rather than
    // guessing at behavior that could affect the main view unexpectedly.
    if (is_overlay) return;
    wnd->cb(wnd->cb_ptr, CB_LAYOUT_PREV, id, 0, 0);
}

void JSInterface::jsLayoutJump(int id, int which)
{
    // JUDGEMENT CALL, not verified against the C# client: a touch-driven
    // navLayout action targeting a different layout from *inside* an
    // overlay is interpreted here as "replace the overlay's own content
    // with that layout" (stays an overlay), rather than affecting the
    // main view underneath -- this seemed like the least surprising
    // interpretation for a self-contained overlay, but it's a guess.
    if (is_overlay) {
        wnd->overlayShowImpl(QString("%1.xlf.html").arg(which));
        return;
    }
    wnd->cb(wnd->cb_ptr, CB_LAYOUT_JUMP, id, which, 0);
}

void JSInterface::jsCommand(QString code)
{
    // Shell/system commands are process-level side effects, not tied to
    // whichever view triggered them -- shared with the main view's path.
    std::string std_code = code.toStdString();
    wnd->cb(wnd->cb_ptr, CB_COMMAND, (intptr_t)std_code.c_str(), 0, 0);
}

void JSInterface::jsShell(QString command, int with_shell)
{
    std::string std_cmd = command.toStdString();
    wnd->cb(wnd->cb_ptr, CB_SHELL, (intptr_t)std_cmd.c_str(), with_shell, 0);
}

void JSInterface::jsStopShell(int kill_mode)
{
    wnd->cb(wnd->cb_ptr, CB_STOPSHELL, kill_mode, 0, 0);
}

void JSInterface::jsNativeWebShow(int mediaId, QString url, int x, int y, int w, int h)
{
    wnd->jsNativeWebShowImpl(is_overlay, mediaId, url, x, y, w, h);
}

void JSInterface::jsNativeWebHide(int mediaId)
{
    wnd->jsNativeWebHideImpl(is_overlay, mediaId);
}
