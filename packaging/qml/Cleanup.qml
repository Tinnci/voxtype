import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ApplicationWindow {
    id: root
    readonly property var args: Qt.application.arguments
    readonly property string apiBase: args.length >= 2 ? args[args.length - 2] : ""
    readonly property string sessionToken: args.length >= 1 ? args[args.length - 1] : ""
    property var report: null
    property string loadError: ""
    property string actionMessage: ""
    property bool actionError: false
    property bool busy: false
    property bool applied: false

    width: 760
    height: 640
    minimumWidth: 560
    minimumHeight: 420
    visible: true
    title: qsTr("VoxType 文本整理审阅")
    color: "#111318"
    flags: Qt.Tool | Qt.WindowStaysOnTopHint | Qt.WindowDoesNotAcceptFocus

    function callApi(method, path, callback) {
        root.busy = true
        const request = new XMLHttpRequest()
        request.open(method, apiBase + path + "?token=" + sessionToken)
        request.onreadystatechange = function() {
            if (request.readyState !== XMLHttpRequest.DONE)
                return
            root.busy = false
            let result = null
            try { result = JSON.parse(request.responseText) } catch (error) {}
            if (request.status >= 200 && request.status < 300) {
                root.actionError = false
                callback(result)
            } else {
                root.actionError = true
                root.actionMessage = result && result.error
                    ? result.error : qsTr("文本整理操作失败")
            }
        }
        request.send()
    }

    header: ToolBar {
        background: Rectangle { color: "#191c23" }
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 20
            anchors.rightMargin: 12
            Label {
                text: qsTr("文本整理审阅")
                color: "white"
                font.pixelSize: 18
                font.bold: true
                Layout.fillWidth: true
            }
            Button {
                text: qsTr("关闭")
                onClicked: root.close()
            }
        }
    }

    ScrollView {
        anchors.fill: parent
        contentWidth: availableWidth

        ColumnLayout {
            width: Math.max(0, parent.width - 44)
            x: 22
            spacing: 16

            Rectangle {
                Layout.fillWidth: true
                Layout.preferredHeight: 58
                radius: 12
                color: "#20242d"
                border.color: "#394150"
                RowLayout {
                    anchors.fill: parent
                    anchors.margins: 14
                    Label {
                        text: root.loadError.length > 0
                            ? root.loadError
                            : root.report && root.report.clean
                                ? qsTr("没有本地整理建议")
                                : qsTr("%1 项安全排版建议 · %2 项需要人工确认")
                                    .arg(root.report ? root.report.safe_edit_count : 0)
                                    .arg(root.report ? root.report.review_edit_count : 0)
                        color: root.loadError.length > 0 ? "#f87171" : "#e5e7eb"
                        font.pixelSize: 15
                        Layout.fillWidth: true
                    }
                    Label {
                        visible: root.report && root.report.source
                        text: root.report && root.report.source
                            ? String(root.report.source.program || root.report.source.kind || "") : ""
                        color: "#9ca3af"
                    }
                }
            }

            Label {
                text: qsTr("建议文本")
                color: "#f3f4f6"
                font.pixelSize: 15
                font.bold: true
            }
            TextArea {
                Layout.fillWidth: true
                Layout.preferredHeight: Math.max(110, Math.min(230, contentHeight + 26))
                readOnly: true
                selectByMouse: true
                wrapMode: TextEdit.Wrap
                text: root.report ? String(root.report.suggested || "") : ""
                color: "#e5e7eb"
                selectionColor: "#7c3aed"
                selectedTextColor: "white"
                background: Rectangle {
                    radius: 12
                    color: "#191c23"
                    border.color: "#394150"
                }
            }

            Label {
                text: qsTr("逐项建议")
                color: "#f3f4f6"
                font.pixelSize: 15
                font.bold: true
            }

            Repeater {
                model: root.report && root.report.edits ? root.report.edits : []
                delegate: Rectangle {
                    required property var modelData
                    Layout.fillWidth: true
                    Layout.preferredHeight: editColumn.implicitHeight + 28
                    radius: 12
                    color: "#191c23"
                    border.color: modelData.safety === "safe" ? "#356b55" : "#6b5735"

                    ColumnLayout {
                        id: editColumn
                        anchors.fill: parent
                        anchors.margins: 14
                        spacing: 8
                        RowLayout {
                            Layout.fillWidth: true
                            Rectangle {
                                Layout.preferredWidth: badgeText.implicitWidth + 14
                                Layout.preferredHeight: 24
                                radius: 12
                                color: modelData.safety === "safe" ? "#214d3c" : "#5a4523"
                                Label {
                                    id: badgeText
                                    anchors.centerIn: parent
                                    text: modelData.safety === "safe" ? qsTr("安全排版") : qsTr("需要确认")
                                    color: modelData.safety === "safe" ? "#86efac" : "#fcd34d"
                                    font.pixelSize: 12
                                }
                            }
                            Label {
                                text: String(modelData.rule_id || "")
                                color: "#9ca3af"
                                font.family: "monospace"
                                font.pixelSize: 12
                                Layout.fillWidth: true
                                elide: Text.ElideRight
                            }
                        }
                        Label {
                            text: String(modelData.message || "")
                            color: "#e5e7eb"
                            wrapMode: Text.Wrap
                            Layout.fillWidth: true
                        }
                        Label {
                            text: qsTr("“%1” → “%2”")
                                .arg(String(modelData.original || ""))
                                .arg(String(modelData.replacement || ""))
                            color: "#c4b5fd"
                            wrapMode: Text.WrapAnywhere
                            Layout.fillWidth: true
                        }
                    }
                }
            }

            Rectangle {
                visible: root.actionMessage.length > 0
                Layout.fillWidth: true
                Layout.preferredHeight: actionLabel.implicitHeight + 24
                radius: 10
                color: root.actionError ? "#3b1d24" : "#16382d"
                border.color: root.actionError ? "#7f303d" : "#2f765e"
                Label {
                    id: actionLabel
                    anchors.fill: parent
                    anchors.margins: 12
                    text: root.actionMessage
                    color: root.actionError ? "#fca5a5" : "#86efac"
                    wrapMode: Text.Wrap
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.bottomMargin: 20
                Label {
                    Layout.fillWidth: true
                    text: root.report && root.report.source
                        ? qsTr("写回前会再次校验 Fcitx 焦点、generation 和原文。")
                        : qsTr("最近一次转写没有可安全定位的输入框，因此只提供审阅。")
                    color: "#9ca3af"
                    wrapMode: Text.Wrap
                }
                Button {
                    text: qsTr("撤销")
                    enabled: root.applied && !root.busy
                    onClicked: root.callApi("POST", "/undo", function() {
                        root.applied = false
                        root.actionMessage = qsTr("已撤销，原文已恢复")
                    })
                }
                Button {
                    text: root.busy ? qsTr("正在校验…") : qsTr("应用建议")
                    highlighted: true
                    enabled: root.report && root.report.source
                        && !root.report.clean && !root.applied && !root.busy
                    onClicked: root.callApi("POST", "/apply", function() {
                        root.applied = true
                        root.actionMessage = qsTr("建议已写回原输入框；如内容未继续变化，可在此撤销")
                    })
                }
            }
        }
    }

    Component.onCompleted: {
        if (root.apiBase.length === 0 || root.sessionToken.length === 0) {
            root.loadError = qsTr("缺少整理报告")
            return
        }
        root.callApi("GET", "/report", function(result) {
            root.report = result
        })
    }
}
