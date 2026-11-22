// Initializes Qt WebEngine for QML. Must run before the QGuiApplication exists:
// the app hosts Qobuz's official web player in a WebEngineView (see
// qml/engine/QobuzEngine.qml and COMPLIANCE.md at the repository root).
#include <QtWebEngineQuick/qtwebenginequickglobal.h>

extern "C" void qbz_webengine_initialize()
{
    QtWebEngineQuick::initialize();
}
