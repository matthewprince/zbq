pragma Singleton
import QtQuick
QtObject {
property string docJson: '{"lines":[],"synced":false}'
property bool showTranslation: false
property bool liteFill: false
property int sizeIndex: 1
property bool lyra: true
// The clock is two invokables in the app (positionMs(), playing()).
property real testPositionMs: 0
property bool testPlaying: false
function positionMs() { return testPositionMs }
function playing() { return testPlaying }
function activeIndexAt(ms) { return -1 }
function fillFractionAt(index, ms) { return 0 }
// Lyra (LyraLyricsView): the test supplies the page and the document.
property string testPage: ""
property string testSource: ""
function lyraPage() { return testPage }
function lyraSource() { return testSource }
function lyraMeta() { return '{"name":"","artist":""}' }
signal refetchFinished(string message)
property int refetchCalls: 0
function refetch() { refetchCalls += 1 }
function lyraCover() { return "" }
}
