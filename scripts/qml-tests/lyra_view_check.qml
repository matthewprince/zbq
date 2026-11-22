// LyraLyricsView end to end in a real web view (run by test-lyra-view-qml.sh
// with the `qml` tool, which can host Qt WebEngine; qmltestrunner cannot): the
// page loads and reports ready, a document becomes Lyra's lines, the clock
// reaches the page, a click on a line comes back as a seek (lead-corrected),
// mouse presses reach the page, the "Wrong lyrics?" pill reaches the app and
// shows its answer, a hidden view stops animating, and the text-size setting
// reaches the page.
// Prints one line per check and exits non-zero on any failure.

import QtQuick
import QtQuick.Window
import com.blitzfc.qbz
import "../../crates/qbz-qt/qml/shell" as Shell

Window {
    id: win
    width: 420
    height: 640
    visible: true
    color: "#101010"

    property var args: Qt.application.arguments
    property string pagePath: args[args.length - 2]
    property string fixturePath: args[args.length - 1]
    property int failures: 0
    property var seeks: []
    property int step: 0

    function readFile(path) {
        const x = new XMLHttpRequest()
        x.open("GET", "file://" + path, false)
        x.send()
        return x.responseText
    }
    function check(ok, what) {
        console.log((ok ? "ok   " : "FAIL ") + what)
        if (!ok)
            win.failures += 1
    }
    function finish() {
        console.log(win.failures === 0 ? "Lyra view: all checks passed" : "Lyra view: " + win.failures + " check(s) failed")
        Qt.exit(win.failures === 0 ? 0 : 1)
    }
    function js(code, then) {
        loader.item.children[0].runJavaScript(code, then)
    }

    Component.onCompleted: {
        QbzLyrics.testPage = readFile(win.pagePath)
        check(QbzLyrics.testPage.indexOf("window.zbq") > 0 && QbzLyrics.testPage.indexOf("Lyra.create") > 0,
              "the page carries Lyra and the glue")
        loader.active = true
    }

    Loader {
        id: loader
        anchors.fill: parent
        active: false
        sourceComponent: Shell.LyraLyricsView {
            compact: true
            onSeekRequested: function (timeMs) { win.seeks.push(timeMs) }
        }
    }

    // A small script of steps, one per tick; each waits for what it needs.
    property int waited: 0
    Timer {
        interval: 200
        repeat: true
        running: true
        onTriggered: {
            const view = loader.item
            win.waited += 1
            if (win.waited > 150) {          // 30 s for the whole run
                win.check(false, "finished in time (stuck at step " + win.step + ")")
                win.finish()
                return
            }
            switch (win.step) {
            case 0:
                if (view && view.pageReady) {
                    win.check(true, "the page loaded and reported ready")
                    QbzLyrics.testSource = win.readFile(win.fixturePath)
                    QbzLyrics.testPositionMs = 13000
                    QbzLyrics.testPlaying = true
                    QbzLyrics.docJson = '{"status":2,"lines":[{"text":"x"}],"synced":true}'
                    win.step = 1
                }
                break
            case 1:
                win.js("document.querySelectorAll('.lyra-line').length", function (n) {
                    if (n > 0) {
                        win.check(n === 6, "the document became Lyra's six lines (got " + n + ")")
                        win.step = 3
                    }
                })
                win.step = 2
                break
            case 2:
                win.step = 1                  // poll again next tick
                break
            case 3:
                // The clock reaches the page and Lyra animates.
                win.js("JSON.parse(zbq.stats()).frames", function (frames) {
                    win.check(frames > 0, "Lyra animates (" + frames + " frames)")
                    win.js("document.querySelector('.lyra-line.lyra-active') ? document.querySelector('.lyra-line.lyra-active').textContent : ''",
                           function (text) {
                        win.check(text.indexOf("Every wave") >= 0, "the line at 13 s is active (\"" + text + "\")")
                        win.js("document.querySelectorAll('.lyra-line')[3].click()")
                        win.step = 4
                    })
                })
                win.step = 99
                break
            case 4:
                if (win.seeks.length > 0) {
                    // Line 4 starts at 16.000 s; the view seeks Lyra's 110 ms lead earlier.
                    win.check(Math.abs(win.seeks[0] - 15890) <= 1, "a click on a line seeks to it (" + win.seeks[0] + " ms)")
                    win.js("getComputedStyle(document.documentElement).getPropertyValue('--zbq-scale').trim()", function (s) {
                        win.check(s === "" || Number(s) === 1, "the medium size is unscaled")
                        win.step = 10
                    })
                    win.step = 99
                }
                break
            case 10:
                // Qt WebEngine hands mouse presses to the page only when the view
                // may take focus on press: false made Resume and line clicks dead.
                win.check(view.children[0].activeFocusOnPress === true, "mouse presses reach the page")
                win.js("document.querySelector('.lyra-refetch.lyra-refetch-on') !== null", function (shown) {
                    win.check(shown === true, "the Wrong lyrics? pill is shown")
                    win.js("document.querySelector('.lyra-refetch').click()")
                    win.step = 11
                })
                win.step = 99
                break
            case 11:
                if (QbzLyrics.refetchCalls > 0) {
                    win.check(QbzLyrics.refetchCalls === 1, "the pill asks the app for another version")
                    QbzLyrics.refetchFinished("Switched to a different version")
                    win.step = 12
                }
                break
            case 12:
                win.js("(document.querySelector('.lyra-toast.lyra-toast-on') || {}).textContent || ''", function (text) {
                    win.check(text === "Switched to a different version", "the app's answer is the pill's message (\"" + text + "\")")
                    QbzLyrics.sizeIndex = 3
                    win.step = 5
                })
                win.step = 99
                break
            case 5:
                win.js("getComputedStyle(document.documentElement).getPropertyValue('--zbq-scale').trim()", function (s) {
                    win.check(Number(s) === 1.3, "the XL size reaches the page (" + s + ")")
                    loader.item.visible = false
                    win.step = 6
                })
                win.step = 99
                break
            case 6:
                win.js("JSON.parse(zbq.stats()).frames", function (frames) {
                    win.framesAtHide = frames
                    win.step = 7
                })
                win.step = 99
                break
            case 7:
                win.js("JSON.parse(zbq.stats()).frames", function (frames) {
                    win.check(frames === win.framesAtHide, "a hidden view stops animating (" + win.framesAtHide + " -> " + frames + ")")
                    win.finish()
                })
                win.step = 99
                break
            }
        }
    }
    property int framesAtHide: -1
}
