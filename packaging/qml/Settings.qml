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
    property var calibration: null
    property bool calibrating: false

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
                root.calibrating = false
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
                                model: ["fcitx", "auto", "clipboard", "copy"]
                                currentIndex: root.state ? model.indexOf(root.state.general.insertion_backend) : 0
                            }

                            Item { Layout.preferredWidth: 1 }
                            Label {
                                Layout.fillWidth: true
                                text: backendBox.currentText === "fcitx"
                                    ? qsTr("锁定录音开始时的 Fcitx 输入上下文；焦点变化或密码字段会拒绝提交。")
                                    : backendBox.currentText === "copy"
                                        ? qsTr("只复制识别结果，不模拟按键；适合无法安全注入的应用。")
                                        : backendBox.currentText === "clipboard"
                                            ? qsTr("通过剪贴板和 ydotool 粘贴，兼容性较高但不提供原生焦点锁定。")
                                            : qsTr("优先使用 Fcitx；桥接不可用时只复制结果，不会自动模拟粘贴。")
                                wrapMode: Text.Wrap
                                opacity: 0.65
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

                            Label { text: qsTr("最长录音") }
                            RowLayout {
                                SpinBox {
                                    id: maximumDuration
                                    from: 5
                                    to: 3600
                                    value: root.state ? root.state.general.maximum_duration_seconds : 120
                                }
                                Label { text: qsTr("秒"); opacity: 0.7 }
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
                            Label {
                                Layout.fillWidth: true
                                text: qsTr("仅恢复可安全读取的文本；如果期间复制了新内容则保留新内容，非文本剪贴板不会被覆盖。")
                                wrapMode: Text.Wrap
                                opacity: 0.65
                            }
                            CheckBox {
                                id: retainRecordings
                                text: qsTr("保留录音文件（仅建议调试时启用）")
                                checked: root.state ? root.state.general.retain_recordings : false
                            }
                            CheckBox {
                                id: transcriptHistory
                                text: qsTr("在内存中保留最近 20 条语音输入，用于本地文本整理")
                                checked: root.state ? root.state.general.transcript_history_enabled : false
                            }
                            Label {
                                Layout.fillWidth: true
                                text: qsTr("历史记录默认关闭，只存在于当前 daemon 会话；关闭并保存时会立即清除。")
                                wrapMode: Text.Wrap
                                opacity: 0.65
                            }
                        }
                    }

                    GroupBox {
                        title: qsTr("麦克风与 VAD 校准")
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        ColumnLayout {
                            anchors.fill: parent
                            Label {
                                Layout.fillWidth: true
                                text: qsTr("录制 2.5 秒本地样本，估算环境噪声和动态阈值。样本不会上传，分析后立即删除。开始时先保持安静，再说一句短句。")
                                wrapMode: Text.Wrap
                                opacity: 0.75
                            }
                            RowLayout {
                                Layout.fillWidth: true
                                Button {
                                    text: root.calibrating ? qsTr("正在校准…") : qsTr("开始校准")
                                    enabled: !root.calibrating
                                    onClicked: {
                                        root.calibrating = true
                                        root.message = qsTr("正在录制校准样本…")
                                        root.callApi("POST", "/calibrate", "", function(result) {
                                            root.calibration = result
                                            root.calibrating = false
                                            root.messageError = false
                                            root.message = qsTr("校准完成；请确认建议值后再保存")
                                        })
                                    }
                                }
                                Label {
                                    Layout.fillWidth: true
                                    visible: root.calibration !== null
                                    text: root.calibration ? qsTr("噪声 %1 · 动态阈值 %2 · 峰值 %3 · 语音占比 %4%")
                                        .arg(root.calibration.noise_floor)
                                        .arg(root.calibration.adaptive_threshold)
                                        .arg(root.calibration.peak)
                                        .arg(Math.round(root.calibration.speech_ratio * 100)) : ""
                                    elide: Text.ElideRight
                                }
                                Button {
                                    visible: root.calibration !== null
                                    text: qsTr("采用建议阈值")
                                    onClicked: vadThreshold.value = root.calibration.suggested_threshold
                                }
                            }
                            Label {
                                Layout.fillWidth: true
                                visible: root.calibration !== null
                                text: !root.calibration ? ""
                                    : root.calibration.level_status === "clipping"
                                        ? qsTr("检测到削波：请降低麦克风增益或稍微远离麦克风")
                                        : root.calibration.level_status === "too-quiet"
                                            ? qsTr("输入过低：请检查设备、增益或靠近麦克风")
                                            : qsTr("输入电平正常")
                                color: root.calibration && root.calibration.level_status === "ok"
                                    ? "#22c55e" : "#ef4444"
                                wrapMode: Text.Wrap
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
                                    transcript_history_enabled: transcriptHistory.checked,
                                    vad_enabled: vadEnabled.checked,
                                    vad_rms_threshold: vadThreshold.value,
                                    vad_minimum_voiced_frames: vadFrames.value,
                                    minimum_duration_millis: minimumDuration.value,
                                    maximum_duration_seconds: maximumDuration.value
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
                    Label {
                        Layout.fillWidth: true
                        Layout.leftMargin: 22
                        Layout.rightMargin: 22
                        text: qsTr("隐私提示：选择 OpenAI-compatible 或 Deepgram 云服务时，麦克风录音会发送给对应服务。只有配置 buffered-with-consent 时才允许将同一录音重放给 fallback Provider。")
                        wrapMode: Text.Wrap
                        color: "#f59e0b"
                    }

                    Rectangle {
                        visible: root.state !== null && root.state.onboarding_needed
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        implicitHeight: onboardingLabel.implicitHeight + 24
                        radius: 10
                        color: "#24f59e0b"
                        Label {
                            id: onboardingLabel
                            anchors.fill: parent
                            anchors.margins: 12
                            text: qsTr("当前只有本地演示 Provider：它会返回固定文本，不执行语音识别。请添加并配置真实云服务后再评估 ASR 效果。")
                            wrapMode: Text.Wrap
                            color: "#f59e0b"
                        }
                    }

                    GroupBox {
                        title: qsTr("添加真实云服务")
                        Layout.fillWidth: true
                        Layout.leftMargin: 20
                        Layout.rightMargin: 20
                        GridLayout {
                            anchors.fill: parent
                            columns: 3
                            columnSpacing: 10
                            rowSpacing: 8

                            Label { text: qsTr("标识") }
                            TextField {
                                id: newProviderId
                                Layout.fillWidth: true
                                placeholderText: "work-asr"
                                validator: RegularExpressionValidator { regularExpression: /[a-z0-9-]{1,64}/ }
                            }
                            Label { text: qsTr("小写字母、数字和连字符"); opacity: 0.6 }

                            Label { text: qsTr("协议") }
                            ComboBox {
                                id: newProviderKind
                                Layout.fillWidth: true
                                model: ["openai-compatible", "deepgram"]
                            }
                            Button {
                                text: qsTr("填充官方默认")
                                onClicked: {
                                    if (newProviderKind.currentText === "deepgram") {
                                        newProviderEndpoint.text = "https://api.deepgram.com/v1/listen"
                                        newProviderModel.text = "nova-3"
                                    } else {
                                        newProviderEndpoint.text = "https://api.openai.com/v1/audio/transcriptions"
                                        newProviderModel.text = "gpt-4o-mini-transcribe"
                                    }
                                }
                            }

                            Label { text: qsTr("API 端点") }
                            TextField {
                                id: newProviderEndpoint
                                Layout.fillWidth: true
                                placeholderText: "https://…"
                            }
                            Item { Layout.preferredWidth: 1 }

                            Label { text: qsTr("模型") }
                            TextField {
                                id: newProviderModel
                                Layout.fillWidth: true
                                placeholderText: newProviderKind.currentText === "deepgram" ? "nova-3" : "gpt-4o-mini-transcribe"
                            }
                            Item { Layout.preferredWidth: 1 }

                            Label { text: qsTr("语言") }
                            TextField {
                                id: newProviderLanguage
                                Layout.fillWidth: true
                                text: "zh"
                            }
                            CheckBox {
                                id: newProviderDefault
                                text: qsTr("设为默认配置")
                                checked: true
                            }

                            Item { Layout.preferredWidth: 1 }
                            Label {
                                Layout.fillWidth: true
                                text: qsTr("创建后在下方安全保存 API 密钥。音频只有在实际使用该配置时才会上传。")
                                wrapMode: Text.Wrap
                                opacity: 0.65
                            }
                            Button {
                                text: qsTr("创建 Provider 和配置")
                                highlighted: true
                                enabled: newProviderId.acceptableInput
                                    && newProviderEndpoint.text.trim().length > 0
                                    && newProviderModel.text.trim().length > 0
                                    && newProviderLanguage.text.trim().length > 0
                                onClicked: {
                                    const payload = {
                                        id: newProviderId.text.trim(),
                                        kind: newProviderKind.currentText,
                                        endpoint: newProviderEndpoint.text.trim(),
                                        model: newProviderModel.text.trim(),
                                        language: newProviderLanguage.text.trim(),
                                        make_default: newProviderDefault.checked
                                    }
                                    root.callApi("POST", "/provider", JSON.stringify(payload), function(result) {
                                        newProviderId.clear()
                                        root.refresh(result.daemon_reloaded
                                            ? qsTr("Provider 和配置已创建；请继续保存 API 密钥")
                                            : qsTr("Provider 已创建；daemon 稍后需要重新载入"))
                                    })
                                }
                            }
                        }
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
                                    Label {
                                        visible: modelData.kind === "mock"
                                        text: qsTr("演示固定文本 · 不执行 ASR")
                                        color: "#f59e0b"
                                    }
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
                                        || modelData.kind === "deepgram"
                                    columns: 3
                                    Layout.fillWidth: true
                                    columnSpacing: 10
                                    rowSpacing: 8
                                    Label { text: qsTr("API 端点") }
                                    TextField {
                                        id: endpointInput
                                        Layout.fillWidth: true
                                        text: modelData.endpoint
                                        placeholderText: modelData.kind === "deepgram"
                                            ? "https://api.deepgram.com/v1/listen"
                                            : "https://…/v1/audio/transcriptions"
                                    }
                                    Item { Layout.preferredWidth: 1 }
                                    Label { text: qsTr("模型") }
                                    TextField {
                                        id: modelInput
                                        Layout.fillWidth: true
                                        text: modelData.model
                                    }
                                    Item { Layout.preferredWidth: 1 }
                                    Label {
                                        visible: modelData.kind === "deepgram"
                                        text: qsTr("智能格式化")
                                    }
                                    CheckBox {
                                        id: smartFormatInput
                                        visible: modelData.kind === "deepgram"
                                        text: qsTr("启用标点和易读格式")
                                        checked: modelData.smart_format === null ? true : modelData.smart_format
                                    }
                                    Item {
                                        visible: modelData.kind === "deepgram"
                                        Layout.preferredWidth: 1
                                    }
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
                                                timeout_seconds: timeoutInput.value,
                                                smart_format: modelData.kind === "deepgram"
                                                    ? smartFormatInput.checked : null
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
