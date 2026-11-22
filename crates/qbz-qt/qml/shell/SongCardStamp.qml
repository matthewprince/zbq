// SongCardStamp: the audio stamp inside the song card, pinned to its right
// edge, in the NEW (0) and CLASSIC (1) now-playing bars. The SMALL (2) and
// LARGE (3) bars mount shell/AudioStamp.qml instead; they stay separate.
//
// ZBQ redesign (2026-11-21, "still really awkward and hard to see"). QBZ's
// stamp was a 38px chip (an 8px tracked-caps tier over a 9px muted format
// line) with a second row of 9px LED rings and 8px tracked-caps codes
// (PIPEWIRE, BITPERF) floating under it. It is now ONE card:
//
//   [mark]  24-bit / 96 kHz        12px, primary
//           Bit-perfect            11px, success green when the path is exact
//
// The tier is the mark itself (Hi-Res, DSD, or the CD/MP3 glyph). The output
// codes stay output_labels.rs's tested contract and are shown here as plain
// words; the backend and device move into the tooltip, under the source and
// output detail.
//
// While the stream is downgraded the card shows the DELIVERED detail with an
// amber arrow, and the catalog max moves into the tooltip's "Source" line next
// to the cause.

import QtQuick
import QtQuick.Controls
import com.blitzfc.qbz
import "../controls"
import "../theme"

Item {
    id: root

    QbzTheme { id: theme }

    // Kept for SongCard's call site; this card shows a multi-line tooltip, which
    // the shared one-line bubble cannot, so it uses its own (a Popup, drawn
    // above the window's other items, so the bar never clips it).
    property Item tooltipHost: null

    readonly property bool showDelivered: QbzPlayer.npQualityDowngraded
        && QbzPlayer.npQualityTrueDetail !== ""
    readonly property string tier: showDelivered ? QbzPlayer.npQualityEffectiveTier
                                                 : QbzPlayer.npQualityTier
    readonly property string detail: showDelivered ? QbzPlayer.npQualityTrueDetail
                                                   : QbzPlayer.npQualityDetail
    readonly property string qualityCauseLine: {
        var c = QbzPlayer.npQualityLimitCause
        if (c === 1) return QbzSession.tr("Limited by your streaming quality setting", QbzSession.trRev)
        if (c === 2) return QbzSession.tr("Limited by your output device's limit", QbzSession.trRev)
        if (c === 3) return QbzSession.tr("Limited by the quality cap for {}", QbzSession.trRev)
            .replace("{}", QbzPlayer.npCastTarget)
        if (c === 4) return QbzSession.tr("Qobuz has no higher quality for this track", QbzSession.trRev)
        return ""
    }
    readonly property string qualityStampTooltip: {
        if (!showDelivered) return QbzPlayer.npQualityDetail
        var t = QbzSession.tr("Source: {}", QbzSession.trRev).replace("{}", QbzPlayer.npQualityDetail)
            + "\n"
            + QbzSession.tr("Output: {}", QbzSession.trRev).replace("{}", QbzPlayer.npQualityTrueDetail)
        return qualityCauseLine === "" ? t : t + "\n" + qualityCauseLine
    }

    // Plain words for output_labels.rs's codes.
    function backendName(code) {
        switch (code) {
        case "PIPEWIRE": return "PipeWire"
        case "ALSA": return "ALSA"
        case "JACK": return "JACK"
        case "PULS": return "PulseAudio"
        case "WASAPI": return "WASAPI"
        case "SYST": return QbzSession.tr("System default", QbzSession.trRev)
        default: return QbzSession.tr("Auto", QbzSession.trRev)
        }
    }
    function modeName(code) {
        switch (code) {
        case "BITPERF": return QbzSession.tr("Bit-perfect", QbzSession.trRev)
        case "DACPASS": return QbzSession.tr("DAC passthrough", QbzSession.trRev)
        case "EXCL": return QbzSession.tr("Exclusive mode", QbzSession.trRev)
        case "DIRECT": return QbzSession.tr("Direct", QbzSession.trRev)
        case "LOCKED": return QbzSession.tr("Locked", QbzSession.trRev)
        case "ROUTED": return QbzSession.tr("Routed", QbzSession.trRev)
        case "SHARED": return QbzSession.tr("Shared", QbzSession.trRev)
        default: return QbzSession.tr("Default", QbzSession.trRev)
        }
    }

    // A peer owns playback (show its name), a local cast (Chromecast / DLNA),
    // or the local output path: the same three cases the LED row had.
    readonly property bool remotePeer: QbzPlayer.npIsRemote && QbzPlayer.npCastTarget !== ""
    readonly property bool casting: QbzPlayer.npCastActive && !QbzPlayer.npIsRemote
    readonly property string statusText: remotePeer ? QbzPlayer.npCastTarget
        : (casting ? QbzSession.tr("Casting", QbzSession.trRev)
                     + (QbzPlayer.npCastProtocol === "dlna" ? " · DLNA" : "")
                   : modeName(QbzPlayer.npOutputModeLabel))
    readonly property color statusColor: remotePeer ? "#e0b341"
        : (casting ? "#a855f7"
                   : (QbzPlayer.npOutputModeActive ? theme.success : theme.textSecondary))
    // The backend tooltip starts with the code ("PIPEWIRE", then the device);
    // show the name instead.
    readonly property string outputTooltip: {
        if (remotePeer || casting) return ""
        var code = QbzPlayer.npOutputBackendLabel
        var tip = QbzPlayer.npOutputBackendTooltip
        return tip.indexOf(code) === 0 ? backendName(code) + tip.slice(code.length) : tip
    }
    readonly property string tooltipText: {
        var q = qualityStampTooltip
        var o = outputTooltip
        return q === "" ? o : (o === "" ? q : q + "\n" + o)
    }

    readonly property bool brandMark: tier === "hires" || tier === "dsd"
    readonly property real markSide: brandMark ? 26 : 18
    readonly property real padH: 10
    readonly property real gap: 8

    implicitWidth: Math.ceil(padH + (markItem.visible ? markSide + gap : 0)
                             + textCol.width + padH)
    implicitHeight: 44
    width: implicitWidth
    height: implicitHeight

    // The tooltip bubble of shell/TooltipOverlay.slint: surface-elevated,
    // radius 4, no border, plus the downward caret. The bubble's bottom edge
    // sits 9px above the anchor and the 5px caret pokes below it.
    component TipBubble: Item {
        Rectangle {
            anchors.fill: parent
            anchors.bottomMargin: 5
            color: theme.surfaceElevated
            radius: 4
        }
        Rectangle {
            width: 7.07
            height: 7.07
            rotation: 45
            antialiasing: true
            color: theme.surfaceElevated
            x: Math.round((parent.width - width) / 2)
            y: parent.height - 5 - height / 2
        }
    }

    Rectangle {
        anchors.fill: parent
        radius: 6
        color: theme.surfaceHover
        border.width: 1
        border.color: theme.borderSubtle
    }

    // Format mark. The Hi-Res brand mark keeps its own colours; DSD is the
    // themed 2:1 mark; CD / MP3 / lossless use the muted glyph.
    Item {
        id: markItem
        visible: root.tier !== ""
        x: root.padH
        width: root.markSide
        height: root.markSide
        anchors.verticalCenter: parent.verticalCenter

        Image {
            visible: root.tier === "hires"
            anchors.fill: parent
            source: "qrc:/qt/qml/com/blitzfc/qbz/qml/assets/hi-res.svg"
            fillMode: Image.PreserveAspectFit
            sourceSize: Qt.size(Math.round(root.markSide * 2), Math.round(root.markSide * 2))
        }
        Image {
            visible: root.tier === "dsd"
            anchors.centerIn: parent
            source: theme.isDark
                ? "qrc:/qt/qml/com/blitzfc/qbz/qml/assets/dsd-light.svg"
                : "qrc:/qt/qml/com/blitzfc/qbz/qml/assets/dsd-dark.svg"
            width: root.markSide
            height: Math.round(root.markSide / 2)
            fillMode: Image.PreserveAspectFit
            sourceSize: Qt.size(Math.round(root.markSide * 2), Math.round(root.markSide))
        }
        QbzIcon {
            visible: !root.brandMark
            anchors.centerIn: parent
            name: root.tier === "mp3" ? "mp3" : "cd"
            tintName: "muted"
            width: root.markSide
            height: root.markSide
        }
    }

    Column {
        id: textCol
        x: root.padH + (markItem.visible ? root.markSide + root.gap : 0)
        anchors.verticalCenter: parent.verticalCenter
        spacing: 2
        // Explicit, from the children's natural widths (unwrapped Texts, so
        // their implicitWidth never depends on this width: no binding loop).
        width: Math.max(detailRow.visible ? detailRow.implicitWidth : 0,
                        statusLabel.implicitWidth > 160 ? 160 : statusLabel.implicitWidth)

        Row {
            id: detailRow
            visible: root.detail !== ""
            spacing: 4
            Text {
                text: root.detail
                color: theme.textPrimary
                font.pixelSize: 12
                font.weight: theme.weightMedium
            }
            // Downgrade arrow (glyph, not a pill: ADR-008), amber so it reads
            // as "adjusted", not an error; the tooltip carries the cause.
            Text {
                visible: QbzPlayer.npQualityDowngraded
                text: "↓"
                color: "#eab308"
                font.pixelSize: 12
                font.weight: Font.Bold
            }
        }
        Text {
            id: statusLabel
            width: Math.min(implicitWidth, 160)
            text: root.statusText
            color: root.statusColor
            font.pixelSize: 11
            font.weight: theme.weightMedium
            elide: Text.ElideRight
        }
    }

    MouseArea {
        id: stampHover
        anchors.fill: parent
        hoverEnabled: true
    }
    ToolTip {
        id: stampTip
        visible: stampHover.containsMouse && root.tooltipText !== ""
        text: root.tooltipText
        delay: 0
        timeout: -1
        // Geometry pinned to the text (see controls/QualityInline.qml).
        padding: 0
        leftPadding: 0
        rightPadding: 0
        topPadding: 0
        bottomPadding: 0
        leftInset: 0
        rightInset: 0
        topInset: 0
        bottomInset: 0
        margins: 0
        width: contentItem ? contentItem.implicitWidth : 0
        height: contentItem ? contentItem.implicitHeight : 0
        x: Math.round((root.width - width) / 2)
        y: -height - 4
        contentItem: Text {
            text: stampTip.text
            color: theme.textPrimary
            font.pixelSize: 11
            font.weight: theme.weightMedium
            leftPadding: 9
            rightPadding: 9
            topPadding: 5
            bottomPadding: 10
        }
        background: TipBubble {}
    }
}
