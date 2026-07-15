import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

ApplicationWindow {
    id: root
    readonly property var args: Qt.application.arguments
    readonly property string apiBase: args.length >= 2 ? args[args.length - 2] : ""
    readonly property string sessionToken: args.length >= 1 ? args[args.length - 1] : ""
    property var state: null
    property string message: qsTr("正在读取设置…")
    property bool messageError: false

    width: 960
    height: 720
    minimumWidth: 760
    minimumHeight: 560
    visible: true
    title: qsTr("VoxType 设置")

    function callApi(method, path, body, callback) {
        const separator = path.indexOf("?") >= 0 ? "&" : "?"
        const request = new XMLHttpRequest()
        request.open(method, apiBase + path + separator + "token=" + sessionToken)
        if (method === "POST")
            request.setRequestHeader("Content-Type", "text/plain;charset=UTF-8")
        request.onreadystatechange = function() {
            if (request.readyState !== XMLHttpRequest.DONE)
                return
            let result = null
            try { result = JSON.parse(request.responseText) } catch (error) {}
            if (request.status >= 200 && request.status < 300) {
                callback(result)
            } else {
                root.messageError = true
                root.message = result && result.error ? result.error : qsTr("设置请求失败")
            }
        }
        request.send(body || "")
    }

    function refresh(successMessage) {
        callApi("GET", "/state", "", function(result) {
            root.state = result
            root.messageError = false
            root.message = successMessage || qsTr("设置已同步")
        })
    }

    function nullableInteger(text) {
        const trimmed = text.trim()
        return trimmed.length === 0 ? null : Number(trimmed)
    }

    function totalUsage(field) {
        if (!state)
            return 0
        let total = 0
        for (let i = 0; i < state.providers.length; ++i)
            total += Number(state.providers[i].usage[field] || 0)
        return total
    }

    function configuredKeyCount() {
        if (!state)
            return 0
        let count = 0
        for (let i = 0; i < state.providers.length; ++i) {
            if (state.providers[i].secret_state === "configured")
                ++count
        }
        return count
    }

    function formatAudio(millis) {
        return (Number(millis) / 1000).toFixed(1) + " s"
    }

    Component.onCompleted: refresh("")

    component SummaryCard: Frame {
        property string heading: ""
        property string valueText: ""
        property string detail: ""
        Layout.fillWidth: true
        ColumnLayout {
            anchors.fill: parent
            spacing: 3
            Label { text: heading; opacity: 0.7 }
            Label { text: valueText; font.pixelSize: 24; font.bold: true }
            Label { text: detail; opacity: 0.65; elide: Text.ElideRight; Layout.fillWidth: true }
        }
    }

    component Meter: Rectangle {
        property real fraction: 0
        property bool known: true
        implicitHeight: 8
        radius: 4
        color: palette.mid
        opacity: 0.65
        Rectangle {
            width: parent.width * Math.max(0, Math.min(1, parent.fraction))
            height: parent.height
            radius: parent.radius
            color: !parent.known ? palette.mid : parent.fraction >= 1 ? "#ef4444"
                : parent.fraction >= 0.8 ? "#f59e0b" : palette.highlight
            Behavior on width { NumberAnimation { duration: 180 } }
        }
    }

    header: ToolBar {
        implicitHeight: 68
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 20
            anchors.rightMargin: 14
            spacing: 12
            Rectangle {
                Layout.preferredWidth: 40
                Layout.preferredHeight: 40
                radius: 12
                color: palette.highlight
                Label {
                    anchors.centerIn: parent
                    text: "●"
                    color: palette.highlightedText
                    font.pixelSize: 18
                }
            }
            ColumnLayout {
                spacing: 1
                Layout.fillWidth: true
                Label { text: qsTr("VoxType 设置"); font.pixelSize: 20; font.bold: true }
                Label { text: qsTr("KDE / Wayland 优先的语音输入"); opacity: 0.65 }
            }
            Button { text: qsTr("刷新"); onClicked: root.refresh(qsTr("设置已刷新")) }
            Button {
                text: qsTr("打开配置文件")
                onClicked: root.callApi("POST", "/open-config", "", function() {
                    root.messageError = false
                    root.message = qsTr("已打开配置文件")
                })
            }
        }
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: 0

        Rectangle {
            Layout.fillWidth: true
            implicitHeight: messageLabel.implicitHeight + 18
            color: root.messageError ? "#24ef4444" : "#2422c55e"
            Label {
                id: messageLabel
                anchors.fill: parent
                anchors.leftMargin: 20
                anchors.rightMargin: 20
                verticalAlignment: Text.AlignVCenter
                text: root.message
                color: root.messageError ? "#ef4444" : palette.text
                wrapMode: Text.Wrap
            }
        }

        TabBar {
            id: tabs
            Layout.fillWidth: true
            TabButton { text: qsTr("常规") }
            TabButton { text: qsTr("服务与密钥") }
            TabButton { text: qsTr("用量与限额") }
        }

        StackLayout {
            currentIndex: tabs.currentIndex
            Layout.fillWidth: true
            Layout.fillHeight: true

            ScrollView {
                contentWidth: availableWidth

                ColumnLayout {
                    width: parent.width
                    spacing: 14

                    RowLayout {
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        Layout.topMargin: 18
                        spacing: 12
                        SummaryCard {
                            heading: qsTr("Provider")
                            valueText: root.state ? String(root.state.providers.length) : "—"
                            detail: qsTr("已配置的识别后端")
                        }
                        SummaryCard {
                            heading: qsTr("API 密钥")
                            valueText: root.state ? String(root.configuredKeyCount()) : "—"
                            detail: qsTr("保存在 Secret Service / KWallet")
                        }
                        SummaryCard {
                            heading: qsTr("本次会话请求")
                            valueText: root.state ? String(root.totalUsage("requests")) : "—"
                            detail: root.state ? root.formatAudio(root.totalUsage("audio_millis")) : "—"
                        }
                    }

                    GroupBox {
                        title: qsTr("输入与录音")
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        enabled: root.state !== null

                        GridLayout {
                            anchors.fill: parent
                            columns: 2
                            columnSpacing: 24
                            rowSpacing: 10

                            Label { text: qsTr("默认配置") }
                            ComboBox {
                                id: profileBox
                                Layout.fillWidth: true
                                model: root.state ? root.state.general.profiles : []
                                currentIndex: root.state ? model.indexOf(root.state.general.default_profile) : -1
                            }

                            Label { text: qsTr("文本注入后端") }
                            ComboBox {
                                id: backendBox
                                Layout.fillWidth: true
                                model: ["fcitx", "auto", "clipboard"]
                                currentIndex: root.state ? model.indexOf(root.state.general.insertion_backend) : 0
                            }

                            Label { text: qsTr("最短录音") }
                            RowLayout {
                                SpinBox {
                                    id: minimumDuration
                                    from: 50
                                    to: 10000
                                    value: root.state ? root.state.general.minimum_duration_millis : 250
                                }
                                Label { text: qsTr("毫秒"); opacity: 0.7 }
                            }

                            Label { text: qsTr("本地 VAD") }
                            CheckBox {
                                id: vadEnabled
                                text: qsTr("过滤静音和无语音录音")
                                checked: root.state ? root.state.general.vad_enabled : true
                            }

                            Label { text: qsTr("VAD RMS 阈值") }
                            SpinBox {
                                id: vadThreshold
                                from: 1
                                to: 10000
                                value: root.state ? root.state.general.vad_rms_threshold : 300
                            }

                            Label { text: qsTr("最少语音帧") }
                            SpinBox {
                                id: vadFrames
                                from: 1
                                to: 100
                                value: root.state ? root.state.general.vad_minimum_voiced_frames : 2
                            }
                        }
                    }

                    GroupBox {
                        title: qsTr("隐私与回退")
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        enabled: root.state !== null
                        ColumnLayout {
                            anchors.fill: parent
                            CheckBox {
                                id: restoreClipboard
                                text: qsTr("使用剪贴板回退后恢复原内容")
                                checked: root.state ? root.state.general.restore_clipboard : true
                            }
                            CheckBox {
                                id: retainRecordings
                                text: qsTr("保留录音文件（仅建议调试时启用）")
                                checked: root.state ? root.state.general.retain_recordings : false
                            }
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        Layout.bottomMargin: 20
                        Label {
                            Layout.fillWidth: true
                            text: root.state ? qsTr("配置：") + root.state.config_path : ""
                            elide: Text.ElideMiddle
                            opacity: 0.6
                        }
                        Button {
                            text: qsTr("保存常规设置")
                            highlighted: true
                            enabled: root.state !== null
                            onClicked: {
                                const payload = {
                                    default_profile: profileBox.currentText,
                                    insertion_backend: backendBox.currentText,
                                    restore_clipboard: restoreClipboard.checked,
                                    retain_recordings: retainRecordings.checked,
                                    vad_enabled: vadEnabled.checked,
                                    vad_rms_threshold: vadThreshold.value,
                                    vad_minimum_voiced_frames: vadFrames.value,
                                    minimum_duration_millis: minimumDuration.value
                                }
                                root.callApi("POST", "/general", JSON.stringify(payload), function(result) {
                                    root.refresh(result.daemon_reloaded
                                        ? qsTr("常规设置已保存并重新载入")
                                        : qsTr("设置已保存；daemon 当前忙碌或不可用，稍后请重新载入"))
                                })
                            }
                        }
                    }
                }
            }

            ScrollView {
                contentWidth: availableWidth

                ColumnLayout {
                    width: parent.width
                    spacing: 14

                    Label {
                        Layout.fillWidth: true
                        Layout.leftMargin: 22
                        Layout.rightMargin: 22
                        Layout.topMargin: 18
                        text: qsTr("识别服务与 API 密钥")
                        font.pixelSize: 19
                        font.bold: true
                    }
                    Label {
                        Layout.fillWidth: true
                        Layout.leftMargin: 22
                        Layout.rightMargin: 22
                        text: qsTr("端点和模型来自配置文件。密钥只写入 Secret Service / KWallet，保存后不会回显，也不会写入 TOML 或命令行。")
                        wrapMode: Text.Wrap
                        opacity: 0.75
                    }

                    Repeater {
                        model: root.state ? root.state.providers : []
                        delegate: GroupBox {
                            required property var modelData
                            title: modelData.id
                            Layout.fillWidth: true
                            Layout.leftMargin: 20
                            Layout.rightMargin: 20

                            ColumnLayout {
                                anchors.fill: parent
                                spacing: 9
                                RowLayout {
                                    Layout.fillWidth: true
                                    Label { text: modelData.kind; font.bold: true }
                                    Item { Layout.fillWidth: true }
                                    Label {
                                        text: modelData.secret_state === "configured" ? qsTr("密钥已配置")
                                            : modelData.secret_state === "missing" ? qsTr("密钥缺失") : qsTr("无需密钥")
                                        color: modelData.secret_state === "configured" ? "#22c55e"
                                            : modelData.secret_state === "missing" ? "#ef4444" : palette.text
                                    }
                                }
                                Label {
                                    visible: modelData.kind === "command"
                                    text: qsTr("程序：") + modelData.endpoint
                                    wrapMode: Text.WrapAnywhere
                                    Layout.fillWidth: true
                                    opacity: 0.8
                                }
                                GridLayout {
                                    visible: modelData.kind === "openai-compatible"
                                    columns: 3
                                    Layout.fillWidth: true
                                    columnSpacing: 10
                                    rowSpacing: 8
                                    Label { text: qsTr("API 端点") }
                                    TextField {
                                        id: endpointInput
                                        Layout.fillWidth: true
                                        text: modelData.endpoint
                                        placeholderText: "https://…/v1/audio/transcriptions"
                                    }
                                    Item { Layout.preferredWidth: 1 }
                                    Label { text: qsTr("模型") }
                                    TextField {
                                        id: modelInput
                                        Layout.fillWidth: true
                                        text: modelData.model
                                    }
                                    Item { Layout.preferredWidth: 1 }
                                    Label { text: qsTr("超时") }
                                    SpinBox {
                                        id: timeoutInput
                                        from: 1
                                        to: 300
                                        value: modelData.timeout_seconds || 30
                                    }
                                    Button {
                                        text: qsTr("保存 API 设置")
                                        enabled: endpointInput.text.length > 0 && modelInput.text.length > 0
                                        onClicked: {
                                            const payload = {
                                                endpoint: endpointInput.text.trim(),
                                                model: modelInput.text.trim(),
                                                timeout_seconds: timeoutInput.value
                                            }
                                            root.callApi("POST", "/provider/" + encodeURIComponent(modelData.id),
                                                JSON.stringify(payload), function(result) {
                                                    root.refresh(result.daemon_reloaded
                                                        ? qsTr("API 设置已保存")
                                                        : qsTr("API 设置已保存；daemon 稍后需要重新载入"))
                                                })
                                        }
                                    }
                                }
                                Rectangle {
                                    visible: modelData.secret_ref.length > 0
                                    Layout.fillWidth: true
                                    implicitHeight: keyRow.implicitHeight + 20
                                    radius: 8
                                    color: palette.alternateBase
                                    RowLayout {
                                        id: keyRow
                                        anchors.fill: parent
                                        anchors.margins: 10
                                        Label { text: modelData.secret_ref; font.family: "monospace" }
                                        TextField {
                                            id: secretInput
                                            Layout.fillWidth: true
                                            placeholderText: qsTr("输入新 API 密钥")
                                            echoMode: TextInput.Password
                                            passwordMaskDelay: 0
                                        }
                                        Button {
                                            text: qsTr("安全保存")
                                            enabled: secretInput.text.length > 0
                                            onClicked: {
                                                const key = secretInput.text
                                                root.callApi("POST", "/secret/" + encodeURIComponent(modelData.secret_ref),
                                                    key, function() {
                                                        secretInput.clear()
                                                        root.refresh(qsTr("API 密钥已保存到 Secret Service / KWallet"))
                                                    })
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    Item { Layout.fillHeight: true; Layout.minimumHeight: 20 }
                }
            }

            ScrollView {
                contentWidth: availableWidth

                ColumnLayout {
                    width: parent.width
                    spacing: 14

                    Label {
                        Layout.fillWidth: true
                        Layout.leftMargin: 22
                        Layout.rightMargin: 22
                        Layout.topMargin: 18
                        text: qsTr("用量与软限额")
                        font.pixelSize: 19
                        font.bold: true
                    }
                    Label {
                        Layout.fillWidth: true
                        Layout.leftMargin: 22
                        Layout.rightMargin: 22
                        text: qsTr("范围：当前 daemon 会话。本地请求数和音频时长是可靠统计；Token 仅在 API 明确返回 usage 字段时显示。软限额由用户配置，不代表供应商账单或账户余额。")
                        wrapMode: Text.Wrap
                        opacity: 0.75
                    }

                    Repeater {
                        model: root.state ? root.state.providers : []
                        delegate: GroupBox {
                            required property var modelData
                            title: modelData.id + " · " + modelData.kind
                            Layout.fillWidth: true
                            Layout.leftMargin: 20
                            Layout.rightMargin: 20

                            ColumnLayout {
                                anchors.fill: parent
                                spacing: 12

                                RowLayout {
                                    Layout.fillWidth: true
                                    spacing: 18
                                    Label { text: qsTr("尝试 ") + modelData.usage.attempts }
                                    Label { text: qsTr("请求 ") + modelData.usage.requests; font.bold: true }
                                    Label { text: qsTr("成功 ") + modelData.usage.successes }
                                    Label { text: qsTr("失败 ") + modelData.usage.failures }
                                    Label { text: qsTr("音频 ") + root.formatAudio(modelData.usage.audio_millis) }
                                    Label {
                                        text: modelData.usage.token_reports > 0
                                            ? qsTr("API Token ") + modelData.usage.reported_tokens
                                            : qsTr("API Token 未报告")
                                    }
                                    Item { Layout.fillWidth: true }
                                }

                                GridLayout {
                                    columns: 3
                                    Layout.fillWidth: true
                                    columnSpacing: 12
                                    rowSpacing: 7

                                    Label { text: qsTr("请求") }
                                    Meter {
                                        Layout.fillWidth: true
                                        fraction: modelData.quota.request_limit
                                            ? modelData.usage.requests / modelData.quota.request_limit : 0
                                        known: modelData.quota.request_limit !== null
                                    }
                                    Label {
                                        text: modelData.quota.request_limit
                                            ? modelData.usage.requests + " / " + modelData.quota.request_limit : qsTr("未设置")
                                    }

                                    Label { text: qsTr("音频") }
                                    Meter {
                                        Layout.fillWidth: true
                                        fraction: modelData.quota.audio_seconds_limit
                                            ? modelData.usage.audio_millis / 1000 / modelData.quota.audio_seconds_limit : 0
                                        known: modelData.quota.audio_seconds_limit !== null
                                    }
                                    Label {
                                        text: modelData.quota.audio_seconds_limit
                                            ? root.formatAudio(modelData.usage.audio_millis) + " / "
                                                + modelData.quota.audio_seconds_limit + " s" : qsTr("未设置")
                                    }

                                    Label { text: qsTr("Token") }
                                    Meter {
                                        Layout.fillWidth: true
                                        fraction: modelData.quota.token_limit && modelData.usage.token_reports > 0
                                            ? modelData.usage.reported_tokens / modelData.quota.token_limit : 0
                                        known: modelData.quota.token_limit !== null && modelData.usage.token_reports > 0
                                    }
                                    Label {
                                        text: modelData.quota.token_limit
                                            ? (modelData.usage.token_reports > 0
                                                ? modelData.usage.reported_tokens + " / " + modelData.quota.token_limit
                                                : qsTr("API 未报告 / ") + modelData.quota.token_limit)
                                            : qsTr("未设置")
                                    }
                                }

                                RowLayout {
                                    Layout.fillWidth: true
                                    Label { text: qsTr("软限额") }
                                    TextField {
                                        id: requestLimit
                                        placeholderText: qsTr("请求数")
                                        text: modelData.quota.request_limit === null ? "" : String(modelData.quota.request_limit)
                                        validator: IntValidator { bottom: 1 }
                                        Layout.preferredWidth: 130
                                    }
                                    TextField {
                                        id: audioLimit
                                        placeholderText: qsTr("音频秒")
                                        text: modelData.quota.audio_seconds_limit === null ? "" : String(modelData.quota.audio_seconds_limit)
                                        validator: IntValidator { bottom: 1 }
                                        Layout.preferredWidth: 130
                                    }
                                    TextField {
                                        id: tokenLimit
                                        placeholderText: qsTr("Token")
                                        text: modelData.quota.token_limit === null ? "" : String(modelData.quota.token_limit)
                                        validator: IntValidator { bottom: 1 }
                                        Layout.preferredWidth: 130
                                    }
                                    Item { Layout.fillWidth: true }
                                    Button {
                                        text: qsTr("保存限额")
                                        onClicked: {
                                            const payload = {
                                                request_limit: root.nullableInteger(requestLimit.text),
                                                audio_seconds_limit: root.nullableInteger(audioLimit.text),
                                                token_limit: root.nullableInteger(tokenLimit.text)
                                            }
                                            root.callApi("POST", "/quota/" + encodeURIComponent(modelData.id),
                                                JSON.stringify(payload), function(result) {
                                                    root.refresh(result.daemon_reloaded
                                                        ? qsTr("软限额已保存")
                                                        : qsTr("软限额已保存；daemon 稍后需要重新载入"))
                                                })
                                        }
                                    }
                                }
                            }
                        }
                    }

                    Item { Layout.fillHeight: true; Layout.minimumHeight: 20 }
                }
            }
        }
    }
}
