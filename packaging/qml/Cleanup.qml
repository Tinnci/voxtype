import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ApplicationWindow {
    id: root
    readonly property var args: Qt.application.arguments
    readonly property string reportFile: args.length >= 1 ? args[args.length - 1] : ""
    property var report: null
    property string loadError: ""

    width: 760
    height: 640
    minimumWidth: 560
    minimumHeight: 420
    visible: true
    title: qsTr("VoxType 文本整理审阅")
    color: "#111318"

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

            Label {
                Layout.fillWidth: true
                text: qsTr("当前窗口只提供审阅，不会修改输入框。待 Fcitx generation 校验写回协议完成后再启用应用与撤销。")
                color: "#9ca3af"
                wrapMode: Text.Wrap
                bottomPadding: 20
            }
        }
    }

    Component.onCompleted: {
        if (root.reportFile.length === 0) {
            root.loadError = qsTr("缺少整理报告")
            return
        }
        const request = new XMLHttpRequest()
        request.open("GET", "file://" + root.reportFile)
        request.onreadystatechange = function() {
            if (request.readyState !== XMLHttpRequest.DONE)
                return
            try {
                root.report = JSON.parse(request.responseText)
            } catch (error) {
                root.loadError = qsTr("无法读取整理报告")
            }
        }
        request.send()
    }
}
