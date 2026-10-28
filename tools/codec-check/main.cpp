// Asks the Qt WebEngine it is built with which video formats it can play
// (the check of the MP4 note in the README): exit code 0 if an MP4 with H.264
// and AAC can be played, 1 if it cannot, 2 if the answer did not come.

#include <QCoreApplication>
#include <QGuiApplication>
#include <QJsonDocument>
#include <QJsonObject>
#include <QTextStream>
#include <QTimer>
#include <QWebEnginePage>

static const char *SCRIPT = R"JS(
(function () {
  var v = document.createElement('video');
  return JSON.stringify({
    mp4_h264_aac: v.canPlayType('video/mp4; codecs="avc1.42E01E, mp4a.40.2"'),
    mp4_plain: v.canPlayType('video/mp4'),
    webm_vp9: v.canPlayType('video/webm; codecs="vp9"'),
    webm_vp8: v.canPlayType('video/webm; codecs="vp8"')
  });
})()
)JS";

int main(int argc, char **argv)
{
    // Headless, as a CI machine is; the sandbox does not work as root.
    if (qEnvironmentVariableIsEmpty("QT_QPA_PLATFORM"))
        qputenv("QT_QPA_PLATFORM", "offscreen");
    if (qEnvironmentVariableIsEmpty("QTWEBENGINE_CHROMIUM_FLAGS"))
        qputenv("QTWEBENGINE_CHROMIUM_FLAGS", "--disable-gpu --no-sandbox");
    QCoreApplication::setAttribute(Qt::AA_ShareOpenGLContexts);

    QGuiApplication app(argc, argv);
    QTextStream out(stdout);
    QWebEnginePage page;

    QObject::connect(&page, &QWebEnginePage::loadFinished, &app, [&](bool ok) {
        if (!ok) {
            out << "the test page did not load" << Qt::endl;
            app.exit(2);
            return;
        }
        page.runJavaScript(QString::fromLatin1(SCRIPT), [&](const QVariant &result) {
            const QJsonObject answers = QJsonDocument::fromJson(result.toString().toUtf8()).object();
            for (auto it = answers.begin(); it != answers.end(); ++it)
                out << it.key() << ": '" << it.value().toString() << "'" << Qt::endl;
            const bool mp4 = !answers.value("mp4_h264_aac").toString().isEmpty();
            out << (mp4 ? "RESULT: MP4 (H.264 + AAC) can be played"
                        : "RESULT: MP4 (H.264 + AAC) CANNOT be played") << Qt::endl;
            app.exit(mp4 ? 0 : 1);
        });
    });
    QTimer::singleShot(60000, &app, [&] {
        out << "no answer after 60 seconds" << Qt::endl;
        app.exit(2);
    });

    page.setHtml("<!doctype html><title>codec-check</title>");
    return app.exec();
}
