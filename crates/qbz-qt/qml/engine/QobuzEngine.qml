// The official-player engine: Qobuz's own web player (play.qobuz.com) in a
// WebEngineView. Qobuz playback and sign-in happen HERE, inside Qobuz's own code:
// upstream QBZ fetched, signed and decrypted Qobuz streams itself, which was
// removed as circumvention (see COMPLIANCE.md). This view only:
//   * injects `engine/qobuz_engine_early.js` (DocumentCreation) and
//     `engine/qobuz_engine_bridge.js` (DocumentReady), which call the player's
//     own action creators and read its public state;
//   * polls `window.__zbq.state()` and hands it to QbzQobuzEngine.onState;
//   * runs the commands Rust sends through QbzQobuzEngine.command.
//
// Full-window while the user signs in on Qobuz's own page (loginOpen); otherwise
// a 1x1 item beneath everything, so the page keeps running and playing audio
// without being seen. With Firefox installed the official player runs there
// (firefox_engine.rs): this view stays blank and hidden, and while Qobuz's sign-in
// page is open in a Firefox window only a banner shows here.
import QtQuick
import QtWebEngine
import com.blitzfc.qbz
import "../theme"

Item {
    id: root

    readonly property bool loginOpen: QbzQobuzEngine.loginOpen && !QbzQobuzEngine.firefoxMode
    readonly property bool firefoxSignIn: QbzQobuzEngine.loginOpen && QbzQobuzEngine.firefoxMode

    x: 0
    y: 0
    width: loginOpen && parent ? parent.width : 1
    height: loginOpen && parent ? parent.height : 1
    z: loginOpen || firefoxSignIn ? 10000 : -1000
    opacity: loginOpen || firefoxSignIn ? 1 : 0.01

    QbzTheme { id: theme }

    WebEngineProfile {
        id: qobuzProfile
        // Persistent: the user's sign-in on Qobuz's page survives restarts, so
        // a returning user is signed in without seeing the page again.
        storageName: "qobuz-official-player"
        offTheRecord: false
        persistentCookiesPolicy: WebEngineProfile.ForcePersistentCookies
    }

    WebEngineView {
        id: view
        anchors.fill: parent
        visible: !QbzQobuzEngine.firefoxMode
        profile: qobuzProfile
        url: QbzQobuzEngine.engineUrl
        settings.playbackRequiresUserGesture: false
        settings.javascriptCanAccessClipboard: false
        settings.localContentCanAccessRemoteUrls: false
        userScripts.collection: [
            {
                sourceCode: QbzQobuzEngine.earlyScript,
                injectionPoint: WebEngineScript.DocumentCreation,
                worldId: WebEngineScript.MainWorld,
                runsOnSubFrames: false
            },
            {
                sourceCode: QbzQobuzEngine.bridgeScript,
                injectionPoint: WebEngineScript.DocumentReady,
                worldId: WebEngineScript.MainWorld,
                runsOnSubFrames: false
            }
        ]
        // Qobuz's sign-in can open a second window (social sign-in); keep it in
        // this view so the token still lands in the official player's session.
        onNewWindowRequested: function (request) {
            view.url = request.requestedUrl
        }
        // A failed load (a DNS blip at startup, a dropped connection) would
        // otherwise leave Qobuz playback dead until restart: retry with backoff.
        onLoadingChanged: function (info) {
            if (info.status === WebEngineView.LoadFailedStatus) {
                reloadTimer.interval = Math.min(60000, 2000 * Math.pow(2, reloadTimer.attempt))
                console.warn("[qobuz-engine] page load failed: " + info.errorString
                             + "; retrying in " + (reloadTimer.interval / 1000) + " s")
                reloadTimer.attempt++
                reloadTimer.restart()
            } else if (info.status === WebEngineView.LoadSucceededStatus) {
                reloadTimer.attempt = 0
            }
        }
        // The page's own console is noise; only the bridge's lines are kept.
        onJavaScriptConsoleMessage: function (level, message, lineNumber, sourceId) {
            if (message.indexOf("[zbq]") === 0)
                console.info("[qobuz-engine] " + message)
        }
    }

    Timer {
        id: reloadTimer
        property int attempt: 0
        repeat: false
        onTriggered: view.reload()
    }

    Timer {
        interval: 500
        running: !QbzQobuzEngine.firefoxMode
        repeat: true
        onTriggered: view.runJavaScript(
            "window.__zbq ? window.__zbq.state() : ''",
            function (result) {
                if (result)
                    QbzQobuzEngine.onState(result)
            })
    }

    Connections {
        target: QbzQobuzEngine
        function onCommand(name, args) {
            // `name` is one of the bridge's own method names; `args` is a JSON
            // array built in Rust (qobuz_engine.rs), never user text.
            view.runJavaScript("window.__zbq && window.__zbq[" + JSON.stringify(name)
                               + "] && window.__zbq[" + JSON.stringify(name) + "].apply(null, "
                               + args + ")")
        }
    }

    // Firefox host: Qobuz's sign-in page is in a Firefox window; say so, and offer
    // the same way back as the overlay.
    Rectangle {
        visible: root.firefoxSignIn
        x: root.parent ? (root.parent.width - width) / 2 : 0
        y: 16
        width: bannerRow.implicitWidth + 32
        height: 44
        radius: 22
        color: theme.surfaceElevated
        border.width: 1
        border.color: theme.borderSubtle
        Row {
            id: bannerRow
            anchors.centerIn: parent
            spacing: 16
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: QbzSession.tr("Sign in on Qobuz's page in the Firefox window, then close it.", QbzSession.trRev)
                color: theme.textPrimary
                font.pixelSize: 13
            }
            Text {
                anchors.verticalCenter: parent.verticalCenter
                text: QbzSession.tr("Cancel", QbzSession.trRev)
                color: bannerCancel.containsMouse ? theme.accent : theme.textMuted
                font.pixelSize: 13
                MouseArea {
                    id: bannerCancel
                    anchors.fill: parent
                    anchors.margins: -6
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: QbzQobuzEngine.cancelLogin()
                }
            }
        }
    }

    // The sign-in overlay's only chrome: a way back without signing in.
    Rectangle {
        visible: root.loginOpen
        anchors.top: parent.top
        anchors.right: parent.right
        anchors.margins: 16
        width: cancelLabel.implicitWidth + 32
        height: 36
        radius: 18
        color: theme.surfaceElevated
        border.width: 1
        border.color: theme.borderSubtle
        Text {
            id: cancelLabel
            anchors.centerIn: parent
            text: QbzSession.tr("Cancel sign-in", QbzSession.trRev)
            color: theme.textPrimary
            font.pixelSize: 13
        }
        MouseArea {
            anchors.fill: parent
            cursorShape: Qt.PointingHandCursor
            onClicked: QbzQobuzEngine.cancelLogin()
        }
    }
}
