#ifndef AREXIBO_VIEW_H
#define AREXIBO_VIEW_H

#include <functional>
#include <memory>
#include <QMainWindow>
#include <QScreen>
#include <QMap>
#include <QVector>
#include <QRect>
#include <QEvent>
#include <QtWebEngineWidgets/QWebEngineView>
#include <QtWebEngineCore/QWebEnginePage>
#include <QtWebEngineCore/QWebEngineProfile>
#include <QtWebEngineCore/QWebEngineScript>
#include <QtWebEngineCore/QWebEngineScriptCollection>
#include <QtWebChannel/QWebChannel>
#include <QTimer>
#include <iostream>
#include <cstdlib>

#include "lib.h"

// Logs every JS `console.*` message from a page to arexibo's own stdout
// (same "INFO :"/"WARN :" convention as the rest of gui/*.cpp), including
// messages from *any iframe* within that page -- Chromium's own
// javaScriptConsoleMessage callback fires for the whole page's frame
// tree, not just the top-level document, so setting this on `view`
// alone already covers every widget iframe inside it too (no need to
// hook each iframe separately). Added as a lightweight, always-on
// diagnostic aid (found genuinely useful investigating a real Dataset
// View widget rendering problem: the *first* concrete evidence of
// what's actually failing inside a widget's own JS, without needing
// network-reachable remote debugging -- which, on a real totem where
// the debugging port only binds to 127.0.0.1 and SSH port forwarding
// wasn't reliable, wasn't a practical option at all).
// Controlled by --web-debug (see main.rs's Args -- "Enable debug
// logging of WebEngine messages", threaded through as gui::run's own
// `debug` parameter): whether LoggingPage actually prints anything, and
// whether `window.arexiboDebug` gets injected into every frame (see
// setup()) so JS-side diagnostics (layout.rs's own `arexibo-show:`
// console.log, and the shrink-to-fit script's `arexibo-shrink:` one) are
// gated the same way -- previously all unconditional, adding permanent
// noise to every single run regardless of whether anyone actually
// wanted this level of detail. A single flag, not a whole new one, on
// request: this one's own existing description already matched exactly
// what these do.
static bool g_web_debug_enabled = false;

class LoggingPage : public QWebEnginePage
{
    Q_OBJECT
public:
    // `profile`, when given, is used explicitly instead of the shared
    // QWebEngineProfile::defaultProfile() implied by the one-argument
    // QWebEnginePage(parent) constructor -- see native_webpage_profile()
    // in view.cpp for why a `webpage render="native"` widget's own page
    // gets a separate profile of its own instead. `view`/`overlay_view`
    // keep using the default profile (pass nullptr, or omit) exactly as
    // before.
    //
    // `hang_watchdog_secs`, when non-zero, forcibly stops (rather than
    // waits out) a navigation that hasn't finished within that many
    // seconds -- found from a real report: a `webpage render="native"`
    // widget's own external URL that accepts a TCP connection attempt
    // but never actually replies can leave loadFinished() simply never
    // firing at all, for as long as the underlying OS/Chromium network
    // stack's own timeout takes (observed: multiple minutes) -- with no
    // recovery of its own in that state (the existing loadFinished(ok=
    // false) retry below never gets a chance to run, since loadFinished
    // never fires while genuinely hung, as opposed to an outright
    // failure). Forcibly triggering Stop makes Chromium finish the
    // navigation as a failure immediately, letting that same retry
    // logic take over from there -- capping how long any single hang
    // can last, regardless of the exact underlying reason. Left at 0
    // (disabled) for `view`/`overlay_view`, which only ever load from
    // arexibo's own fast local server and shouldn't need it.
    LoggingPage(QObject *parent = nullptr, QWebEngineProfile *profile = nullptr,
                int hang_watchdog_secs = 0)
        : QWebEnginePage(profile ? profile : QWebEngineProfile::defaultProfile(), parent)
    {
        // BUG fix (found from a real report: a touch controller
        // intermittently reporting more simultaneous touch points than
        // Chromium's own hardcoded 16-point limit -- an out-of-bounds
        // std::array access deep inside Blink's own touch event
        // handling, std::array<blink::WebTouchPoint, 16>::operator[] --
        // crashed the *renderer* process specifically, on only one
        // totem, intermittently, consistent with a flaky touch
        // controller/cable/grounding issue rather than a genuine
        // 17-finger touch or a systemic software bug). Chromium's own
        // multi-process architecture means this doesn't necessarily
        // bring down arexibo's own top-level process at all -- the
        // GUI's own event loop keeps running, but the QWebEngineView
        // this page belongs to is left showing nothing further (a
        // black screen, reported directly), with no built-in recovery
        // of its own. Connecting to renderProcessTerminated here
        // (rather than at each individual call site) covers every view
        // that uses LoggingPage -- the main view, the overlay view, and
        // every render="native" widget view -- uniformly, in one place.
        //
        // Deliberately exits the *entire* arexibo process outright,
        // rather than trying to reload just the affected page/view in
        // place: a renderer crash this deep inside Chromium's own
        // internals isn't something we have any reliable way to
        // recover from at our level (e.g. Chromium's own GPU/compositor
        // state, shared across every view in the process, could easily
        // be left in a similarly bad state too) -- and arexibo.service
        // already has `Restart=always` (see arexibo.service itself),
        // so a clean, deliberate exit here is picked up automatically,
        // giving a genuinely fresh Xorg + arexibo + Chromium process
        // tree rather than a totem stuck on a black screen indefinitely
        // until someone manually intervenes on site.
        connect(this, &QWebEnginePage::renderProcessTerminated,
                [](QWebEnginePage::RenderProcessTerminationStatus status, int exitCode) {
            std::cout << "ERROR: [arexibo::qt] renderer process terminated "
                       << "(status=" << static_cast<int>(status) << ", exitCode=" << exitCode
                       << ") -- exiting so systemd (Restart=always) can start a fresh instance"
                       << std::endl;
            std::exit(1);
        });

        // A failed page load (network timeout, DNS hiccup) otherwise
        // leaves the view stuck showing a blank/error page forever.
        // Retry indefinitely after a short delay -- context object
        // `this` in singleShot cancels the retry if the page is
        // destroyed first (e.g. widget removed from layout).
        connect(this, &QWebEnginePage::loadFinished, this, [this](bool ok) {
            if (ok) return;
            std::cout << "WARN : [arexibo::qt] page failed to load (" << url().toString().toStdString()
                       << ") -- retrying in 2.5s" << std::endl;
            QTimer::singleShot(2500, this, [this]() {
                triggerAction(QWebEnginePage::Reload);
            });
        });

        if (hang_watchdog_secs > 0) {
            // `generation` distinguishes the current armed timer from a
            // stale one that already fired or was superseded -- bumped
            // every time the watchdog is (re)armed, so a timer whose
            // captured generation no longer matches the current one
            // knows it's obsolete and does nothing when it fires.
            auto generation = std::make_shared<int>(0);
            auto arm_watchdog = std::make_shared<std::function<void()>>();
            *arm_watchdog = [this, generation, hang_watchdog_secs, arm_watchdog]() {
                int my_generation = ++(*generation);
                QTimer::singleShot(hang_watchdog_secs * 1000, this,
                                    [this, generation, my_generation, hang_watchdog_secs]() {
                    if (*generation != my_generation) return;  // superseded already
                    std::cout << "WARN : [arexibo::qt] page still loading (" \
                               << url().toString().toStdString() << ") with no progress for " \
                               << hang_watchdog_secs << "s -- forcing it to stop (letting the " \
                                  "existing failed-load retry take over)" << std::endl;
                    triggerAction(QWebEnginePage::Stop);
                });
            };
            connect(this, &QWebEnginePage::loadStarted, this, [arm_watchdog]() { (*arm_watchdog)(); });
            // Found from a real report: a flat "kill after N seconds no
            // matter what" would also cut off a page that's genuinely
            // loading, just slowly (e.g. a heavy page or a slow but
            // working server) -- every retry would then hit the exact
            // same cutoff again, so it could never actually finish.
            // loadProgress fires repeatedly as real data actually
            // arrives (confirmed distinct from a connection that never
            // gets anywhere: that case never progresses past 0% at
            // all, since Chromium only reports progress once bytes are
            // genuinely flowing) -- re-arming the watchdog here means
            // it only ever fires after hang_watchdog_secs of *no*
            // progress at all, not from total elapsed time, so a slow
            // but actively-loading page can take as long as it
            // genuinely needs.
            connect(this, &QWebEnginePage::loadProgress, this,
                    [arm_watchdog](int) { (*arm_watchdog)(); });
        }
    }

protected:
    void javaScriptConsoleMessage(JavaScriptConsoleMessageLevel level, const QString &message,
                                   int lineNumber, const QString &sourceID) override
    {
        if (!g_web_debug_enabled) return;
        const char *tag = level == ErrorMessageLevel ? "WARN " : "INFO ";
        std::cout << tag << ": [arexibo::qt] JS console [" << sourceID.toStdString()
                   << ":" << lineNumber << "] " << message.toStdString() << std::endl;
    }
};

class Window : public QMainWindow
{
    Q_OBJECT
    friend class JSInterface;

public:
    Window(QString, QScreen *, int, callback, void *);

private:
    QWebEngineView *view;
    QWebChannel *channel;
    QString base_uri;
    QScreen *selected_screen;

    callback cb;
    void *cb_ptr;

    int layout_width;
    int layout_height;

    // One additional, non-iframed QWebEngineView per `render="native"`
    // webpage widget currently on screen in the MAIN layout -- keyed by
    // the widget's XLF media id. See jsNativeWebShowImpl/jsNativeWebHideImpl
    // in view.cpp for why this exists instead of just using an iframe
    // (X-Frame-Options and similar frame-busting headers only block
    // *embedding*, not a real top-level browser view).
    QMap<int, QWebEngineView*> native_views;

    // --- Overlay layout support (XMR `overlayLayout` action) ---
    // A second, independent QWebEngineView/QWebChannel/JSInterface stack,
    // shown on top of the main view without interrupting it (unlike
    // `changeLayout`, which replaces what's on screen -- see
    // xmr::Message::OverlayLayout/ChangeLayout in mainloop.rs). Lazily
    // created on first use via ensureOverlayView(); torn down (not just
    // hidden) in overlayHideImpl() so an idle overlay doesn't keep a live
    // QWebEngineView/renderer process around indefinitely between uses.
    QWebEngineView *overlay_view = nullptr;
    QWebChannel *overlay_channel = nullptr;
    int overlay_layout_width = 1920;
    int overlay_layout_height = 1080;
    // Bounds adjustOverlayScale's own retry when `view` isn't sized
    // yet -- see its own doc comment for the full story.
    int overlay_scale_retry_count = 0;
    // At most one retry timer in flight at a time -- see
    // adjustOverlayScale's own doc comment on this for why multiple
    // parallel ones caused real, observed redundant re-application of
    // the same geometry.
    bool overlay_scale_retry_pending = false;

    // One genuinely *new* native widget waiting to be created -- see
    // jsNativeWebShowImpl's own doc comment for why this is queued
    // instead of created immediately. Plain data, not a std::function,
    // so the queue itself stays simple to inspect/reason about.
    struct PendingNativeWebShow {
        bool overlay;
        int mediaId;
        QString url;
        int x, y, w, h;
    };
    QVector<PendingNativeWebShow> pending_native_web_shows;
    // Whether processNextPendingNativeWebShow's own 20ms-spaced chain
    // is currently running -- guards against starting a second,
    // parallel chain if jsNativeWebShowImpl is called again while one
    // is already in progress.
    bool native_web_show_stagger_active = false;
    // Whether adjustOverlayScale has already successfully applied
    // geometry once since the overlay was last (re)shown or the base
    // layout last changed -- lets a still-pending, now-redundant retry
    // skip itself instead of unnecessarily calling
    // setGeometry()/setZoomFactor() on overlay_view again for no
    // reason (found from a real report: even on a run where scaling
    // ultimately succeeded, a stale retry firing afterward still
    // reapplied the same geometry a second time, one more
    // GPU-compositor-affecting call in an already busy window).
    bool overlay_scale_applied_this_cycle = false;
    // Whether the *current* base layout has had its own real
    // jsLayoutInit-reported dimensions applied to `view` yet (see
    // adjustOverlayScale's own doc comment for why this, not just
    // checking view's area is non-zero, is what's actually needed --
    // a real report showed `view` non-zero but still sized from an
    // earlier generic/default resize, not this layout's own real
    // size, at the exact moment this was checked). Reset to false in
    // navigateToImpl (a new layout is about to load, so `view`'s
    // current sizing is now stale for it), set true at the end of
    // jsLayoutInit's own non-overlay branch.
    bool base_layout_scaled = false;
    // Own set of native_views for `webpage render="native"` widgets that
    // happen to be inside the overlay layout itself -- kept separate
    // from the main view's `native_views` so overlayHideImpl() tears down
    // exactly its own without touching the main layout's.
    QMap<int, QWebEngineView*> overlay_native_views;

    // Touch/click passthrough for an active Overlay Layout (found from
    // a real report, compared directly against the Windows client's own
    // WH_MOUSE_LL global hook): the overlay's own real content zones (one
    // rectangle per region that has at least one widget in it), parsed
    // from the JSON jsLayoutInit's own overlay call carries (see
    // JSInterface::jsLayoutInit and layout.rs's own region_geometry doc
    // comment for where this comes from) -- consumed by eventFilter()
    // below to decide whether a click/tap lands on real overlay content
    // (let it through to Chromium normally) or an empty area (forward it
    // to the main layout underneath instead). Deliberately NOT fixing the
    // separate native-view stacking-order bug (see this codebase's own
    // status notes) without this passthrough existing first -- doing so
    // alone would make an Overlay Layout block the entire main layout
    // underneath even where it draws nothing, a real regression risk
    // flagged before starting this.
    QVector<QRect> overlay_region_rects;
    // Whether a click/tap that started in an empty overlay area is
    // currently being forwarded to `view` -- see eventFilter's own doc
    // comment for why its own move/release events need this to keep
    // being forwarded too, not just the initial press.
    bool overlay_forwarding_active = false;
    // Whether the event filter has already been installed on this
    // overlay_view instance's own focusProxy -- see its own
    // installation point in overlayShowImpl for why this guard exists.
    bool overlay_filter_installed = false;
    // Debounces the "re-raise the overlay after a main-layout native
    // widget shows/refreshes" remedy (see jsNativeWebShowImpl) --
    // found from a real report: firing it unconditionally on every
    // single native widget shown, during the initial burst where a
    // layout's own several native widgets are all created back-to-back
    // (at the same time an Overlay Layout's own several native widgets
    // are ALSO being created), produced dozens of raise() calls within
    // a few hundred milliseconds and broke rendering of most of the
    // main layout. Coalesces that whole burst into a single deferred
    // re-raise instead.
    bool overlay_reraise_scheduled = false;
    void forwardMouseEventToView(QEvent::Type type, QPoint pos);

    void ensureOverlayView();

    void adjustScale(int, int);
    void adjustOverlayScale(int, int);

protected:
    // Installed on overlay_view->focusProxy() (NOT overlay_view itself --
    // confirmed via research, QTBUG-43602: QWebEngineView overrides
    // event() and delegates internally to Chromium, bypassing Qt's own
    // event filter dispatch entirely for the view itself). Compiles
    // cleanly against real Qt6 6.4.2 headers (-Wall -pedantic, zero
    // warnings from this code) -- but that only confirms the API usage
    // is syntactically/type-correct, not the actual runtime behavior on
    // a real touchscreen (whether focusProxy() is genuinely non-null
    // this early, whether overlay_view and view truly share one
    // coordinate space at click time, etc.) -- still needs real
    // hardware testing before trusting it, same as the rest of this
    // feature.
    bool eventFilter(QObject *watched, QEvent *event) override;

public:
    // `overlay` selects which view/geometry/native-view-map a call
    // applies to -- see jsNativeWebShow/jsNativeWebHide in JSInterface,
    // which is itself bound to a specific view via its own `is_overlay`
    // flag and passes it straight through.
    void jsNativeWebShowImpl(bool overlay, int, QString, int, int, int, int);
    // The actual, previously-inline body of jsNativeWebShowImpl -- see
    // its own doc comment for why creating a genuinely *new* native
    // widget is now staggered through pending_native_web_shows instead
    // of calling this directly every time.
    void jsNativeWebShowImplNow(bool overlay, int, QString, int, int, int, int);
    void processNextPendingNativeWebShow();
    void jsNativeWebHideImpl(bool overlay, int);
    // Destroys any native_views left over from the *previous* layout --
    // called before navigating to a new one, since those widgets don't
    // exist in the new page at all and would otherwise leak/linger on
    // screen indefinitely.
    void clearNativeViews(bool overlay);

signals:
    void navigateTo(QString);
    void screenShot(int max_width);
    void setTitle(QString);
    void setSize(int, int, int, int);
    void runJavascript(QString);
    void overlayShow(QString);
    void overlayHide();

public slots:
    void navigateToImpl(QString);
    void screenShotImpl(int max_width);
    void setSizeImpl(int, int, int, int);
    void runJavascriptImpl(QString);
    void overlayShowImpl(QString);
    void overlayHideImpl();
};

class JSInterface : public QObject
{
    Q_OBJECT

public:
    // `is_overlay` distinguishes which view/QWebChannel this instance is
    // bound to -- see the members it guards in view.cpp (e.g. jsLayoutInit
    // must NOT report the overlay's own layout id as "the current layout"
    // to the CMS; jsLayoutDone has no cross-layout cycling to do for a
    // standalone overlay).
    JSInterface(Window *wnd, bool is_overlay = false) :
        QObject(wnd), wnd(wnd), is_overlay(is_overlay) {}

private:
    Window *wnd;
    bool is_overlay;

public slots:
    void jsLayoutInit(int, int, int, QString);
    void jsLayoutDone(int);
    void jsLayoutPrev(int);
    void jsLayoutJump(int, int);
    void jsCommand(QString);
    void jsShell(QString, int);
    void jsStopShell(int);
    void jsNativeWebShow(int, QString, int, int, int, int);
    void jsNativeWebHide(int);
};

#endif
