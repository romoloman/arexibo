#include <QApplication>
#include <QIODevice>
#include <QBuffer>
#include <QJsonDocument>
#include <QJsonArray>
#include <QJsonObject>
#include <QMouseEvent>
#include <QTouchEvent>

#include "view.h"

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

    if (area_w == 0 || area_h == 0 || layout_h == 0 || layout_w == 0)
        return;

    if (area_w == layout_w && area_h == layout_h) {
        overlay_view->setGeometry(view->x(), view->y(), layout_w, layout_h);
        overlay_view->setZoomFactor(1.0);
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
    overlay_view->setGeometry(ov_x, ov_y, ov_w, ov_h);
    overlay_view->setZoomFactor(scale_factor);
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
}

bool Window::eventFilter(QObject *watched, QEvent *event)
{
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
            // Real overlay content at this exact point -- let Chromium
            // handle it completely normally, as if this filter didn't
            // exist at all.
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
// Posted to `view->focusProxy()`, NOT `view` itself -- confirmed from
// the real Qt6 header (qwebengineview.h): QWebEngineView overrides only
// the generic `event(QEvent*)`, never mousePressEvent/mouseMoveEvent/
// mouseReleaseEvent specifically, delegating real mouse handling
// entirely to its own internal focusProxy() child widget instead (the
// exact same QTBUG-43602 mechanism already confirmed for *installing*
// the event filter on the overlay's own side -- missed here at first,
// applied inconsistently, until a real report showed the forwarded
// click simply never registering on `view`'s own page at all).
void Window::forwardMouseEventToView(QEvent::Type type, QPoint pos)
{
    if (!view) {
        std::cout << "WARN : [arexibo::qt] forwardMouseEventToView: no `view` to forward to" \
                  << std::endl;
        return;
    }
    QWidget *target = view->focusProxy();
    if (!target) target = view;
    std::cout << "DEBUG: [arexibo::qt] forwarding event type " << type << " at (" \
               << pos.x() << "," << pos.y() << ") to " \
               << (target == view ? "view (focusProxy was null)" : "view->focusProxy()") \
               << std::endl;
    auto *forwarded = new QMouseEvent(type, QPointF(pos), target->mapToGlobal(pos),
                                       Qt::LeftButton, Qt::LeftButton, Qt::NoModifier);
    QCoreApplication::postEvent(target, forwarded);
}

void Window::jsNativeWebShowImpl(bool overlay, int mediaId, QString url, int x, int y, int w, int h)
{
    auto &views = overlay ? overlay_native_views : native_views;
    QWebEngineView *nview = views.value(mediaId, nullptr);
    if (!nview) {
        nview = new QWebEngineView(this);
        nview->setContextMenuPolicy(Qt::NoContextMenu);
        nview->setPage(new LoggingPage(nview));
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
        // correctly ends up on top. Not needed for `native_views` (the
        // main layout's own native widgets): `view` itself has no
        // AlwaysStackOnTop attribute, so ordinary raise()-order already
        // puts them above it with no special handling.
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
}

void Window::jsNativeWebHideImpl(bool overlay, int mediaId)
{
    auto &views = overlay ? overlay_native_views : native_views;
    if (auto nview = views.value(mediaId, nullptr)) {
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
