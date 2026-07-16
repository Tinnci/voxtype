import QtQuick
import QtQuick.Layouts
import QtQuick.Window

Window {
    id: root
    readonly property var args: Qt.application.arguments
    readonly property string stateName: args.length >= 4 ? args[args.length - 4] : "idle"
    readonly property string heading: args.length >= 3 ? args[args.length - 3] : "VoxType"
    readonly property string detail: args.length >= 2 ? args[args.length - 2] : ""
    readonly property int timeoutMs: args.length >= 1 ? Number(args[args.length - 1]) : 2000
    readonly property color accent: stateName === "listening" ? "#ef4444"
        : stateName === "processing" ? "#f59e0b"
        : stateName === "error" ? "#dc2626"
        : stateName === "no-speech" ? "#64748b"
        : stateName === "cancelled" ? "#64748b"
        : stateName === "grammar" ? "#8b5cf6"
        : "#22c55e"
    readonly property string stateIcon: stateName === "listening" ? "●"
        : stateName === "processing" ? "↻"
        : stateName === "done" ? "✓"
        : stateName === "grammar" ? "Aa"
        : stateName === "no-speech" ? "∿"
        : stateName === "cancelled" ? "×" : "!"

    width: 400
    height: detail.length > 0 ? 112 : 82
    x: Math.round((Screen.width - width) / 2)
    y: Screen.height - height - 96
    color: "transparent"
    flags: Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool | Qt.WindowDoesNotAcceptFocus
    visible: true

    Rectangle {
        anchors.fill: parent
        radius: 18
        color: "#e6222228"
        border.color: "#55ffffff"
        border.width: 1

        RowLayout {
            anchors.fill: parent
            anchors.margins: 18
            spacing: 14

            Rectangle {
                Layout.preferredWidth: 42
                Layout.preferredHeight: 42
                radius: 21
                color: root.accent

                Text {
                    anchors.centerIn: parent
                    text: root.stateIcon
                    color: "white"
                    font.pixelSize: 22
                    font.bold: true
                }

                SequentialAnimation on scale {
                    running: root.stateName === "listening"
                    loops: Animation.Infinite
                    NumberAnimation { to: 1.12; duration: 500; easing.type: Easing.InOutQuad }
                    NumberAnimation { to: 1.0; duration: 500; easing.type: Easing.InOutQuad }
                }
                RotationAnimation on rotation {
                    running: root.stateName === "processing"
                    loops: Animation.Infinite
                    from: 0
                    to: 360
                    duration: 900
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 4
                Text {
                    text: root.heading
                    color: "white"
                    font.pixelSize: 17
                    font.bold: true
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
                Text {
                    visible: root.detail.length > 0
                    text: root.detail
                    color: "#d1d5db"
                    font.pixelSize: 13
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
                Rectangle {
                    id: progressTrack
                    visible: root.stateName === "processing"
                    Layout.fillWidth: true
                    Layout.preferredHeight: 3
                    radius: 2
                    color: "#30ffffff"
                    Rectangle {
                        id: progressBar
                        width: parent.width * 0.36
                        height: parent.height
                        radius: parent.radius
                        color: root.accent
                        SequentialAnimation on x {
                            running: root.stateName === "processing"
                            loops: Animation.Infinite
                            NumberAnimation { from: 0; to: progressTrack.width - progressBar.width; duration: 800; easing.type: Easing.InOutQuad }
                            NumberAnimation { from: progressTrack.width - progressBar.width; to: 0; duration: 800; easing.type: Easing.InOutQuad }
                        }
                    }
                }
            }
        }
    }

    Timer {
        interval: root.timeoutMs
        running: root.timeoutMs > 0
        repeat: false
        onTriggered: Qt.quit()
    }
}
