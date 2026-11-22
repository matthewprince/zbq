// LyraLyricsView: the current track's lyrics drawn by Lyra (the karaoke
// renderer in `crates/qbz-qt/lyra/`, shipped verbatim) in a web view. The page
// comes from `QbzLyrics.lyraPage()`; this item feeds it the document, the
// cover and the playback clock, and turns a click on a line into a seek.
// Hosts: the lyrics panel (compact) and Immersive's split lyrics.
//
// The clock: the page extrapolates from the last position it was given and
// Lyra eases toward that (snapping on a jump), so a push every 150 ms is
// plenty. It runs only while the view is on screen.

import QtQuick
import QtWebEngine
import com.blitzfc.qbz

Item {
    id: root

    /// Panel-width typography (the side panel is a few hundred px wide).
    property bool compact: false
    /// Lyra's own cover background; off where the host paints one.
    property bool background: true
    /// On screen: the page animates only while this is true.
    readonly property bool live: root.visible && root.width > 0 && root.height > 0

    signal seekRequested(real timeMs)

    /// The lyrics text-size setting (S/M/L/XL) as a factor on Lyra's type.
    readonly property real textScale: [0.85, 1.0, 1.15, 1.3][Math.max(0, Math.min(3, QbzLyrics.sizeIndex))]

    property bool pageReady: false

    // The app's focus item before a press moved focus into the page, and the
    // way back to it.
    property Item appFocus: null
    function insideView(item) {
        for (var i = item; i; i = i.parent)
            if (i === view)
                return true
        return false
    }
    function returnFocus() {
        if (!view.activeFocus)
            return
        if (root.appFocus)
            root.appFocus.forceActiveFocus()
        else
            view.focus = false
    }
    Connections {
        target: root.Window.window
        function onActiveFocusItemChanged() {
            const w = root.Window.window
            const item = w ? w.activeFocusItem : null
            if (item && !root.insideView(item))
                root.appFocus = item
        }
    }
    property string loadedSource: ""

    function js(code) {
        if (root.pageReady)
            view.runJavaScript(code)
    }
    function pushDocument() {
        if (!root.pageReady)
            return
        const source = QbzLyrics.lyraSource()
        if (source === root.loadedSource)
            return
        root.loadedSource = source
        if (source === "") {
            view.runJavaScript("zbq.status(null)")
            return
        }
        view.runJavaScript("zbq.load(" + JSON.stringify(source) + "," + QbzLyrics.lyraMeta() + ")")
        const cover = QbzLyrics.lyraCover()
        if (cover !== "")
            view.runJavaScript("zbq.cover(" + JSON.stringify(cover) + ")")
    }
    function pushClock() {
        root.js("zbq.clock(" + Math.round(QbzLyrics.positionMs()) + ","
                + (QbzLyrics.playing() ? "true" : "false") + ")")
    }

    WebEngineView {
        id: view
        anchors.fill: parent
        // Qt WebEngine hands mouse PRESSES to the page only when the view may
        // take focus on press: with this false, Lyra's Resume button and line
        // clicks never arrived (wheel did). So the view takes focus on a press
        // and gives it straight back, so the app's keys (space = play/pause,
        // AppShell's dispatcher) keep working.
        activeFocusOnPress: true
        onActiveFocusChanged: if (activeFocus) Qt.callLater(root.returnFocus)
        backgroundColor: "transparent"
        settings.showScrollBars: false
        settings.focusOnNavigationEnabled: false
        settings.navigateOnDropEnabled: false
        onContextMenuRequested: function (request) { request.accepted = true }
        onLoadingChanged: function (info) {
            if (info.status === WebEngineView.LoadSucceededStatus)
                view.runJavaScript("zbq.init({compact:" + root.compact + ",background:" + root.background
                                   + ",scale:" + root.textScale + "})")
        }
        onJavaScriptConsoleMessage: function (level, message, lineNumber, sourceId) {
            if (message === "zbq:ready:1") {
                root.pageReady = true
                root.loadedSource = ""
                root.pushClock()
                root.pushDocument()
                if (!root.live)
                    view.runJavaScript("zbq.stop()")
            } else if (message.indexOf("zbq:seek:") === 0) {
                root.seekRequested(Number(message.substring(9)))
            } else if (message === "zbq:refetch:1") {
                // Lyra's "Wrong lyrics?" pill; the answer comes back below.
                QbzLyrics.refetch()
            }
        }
        Component.onCompleted: view.loadHtml(QbzLyrics.lyraPage(), "https://lyra.zbq.invalid/")
    }

    onCompactChanged: root.js("zbq.compact(" + root.compact + ")")
    onTextScaleChanged: root.js("zbq.scale(" + root.textScale + ")")
    onLiveChanged: {
        if (root.live) {
            root.pushClock()
            root.js("zbq.start()")
        } else {
            root.js("zbq.stop()")
        }
    }

    Connections {
        target: QbzLyrics
        function onDocJsonChanged() { root.pushDocument() }
        function onRefetchFinished(message) { root.js("zbq.refetchDone(" + JSON.stringify(message) + ")") }
    }

    Timer {
        interval: 150
        repeat: true
        running: root.pageReady && root.live
        onTriggered: root.pushClock()
    }
}
