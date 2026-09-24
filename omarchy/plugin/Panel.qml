import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// Bar icon plus popup for the Omarchy Connect host daemon. All state comes
// from `omarchy-connect status --json`; changes go through the same CLI so
// the daemon, the terminal, and this panel never disagree. The PIN is passed
// on stdin, never argv.
Panel {
  id: root
  moduleName: "omarchy-connect.status"
  ipcTarget: "omarchy-connect.status"

  readonly property string cli: Quickshell.env("HOME") + "/.local/bin/omarchy-connect"
  readonly property string stateFile: Quickshell.env("XDG_RUNTIME_DIR") + "/omarchy-connect/state.json"

  property var info: ({})
  property bool loaded: false
  property string message: ""
  property bool messageIsError: false
  property bool busy: false

  readonly property bool running: loaded && info.running === true
  readonly property string status: loaded ? String(info.status || "stopped") : "stopped"
  readonly property bool connected: status === "connected" || status === "sharing"
  readonly property bool unattended: info.unattended === true
  readonly property bool pinSet: info.pin_set === true
  readonly property bool accessOn: running && unattended && pinSet
  readonly property var addresses: Array.isArray(info.addresses) ? info.addresses : []
  readonly property int port: info.port || 47921
  readonly property int fps: info.fps || 15
  readonly property int bitrateKbps: info.bitrate_kbps || 4000

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  readonly property string statusLine: {
    if (!loaded) return "Checking"
    if (!running) return "Host is not running"
    if (status === "sharing") return "Connecting " + (info.peer || "")
    if (status === "connected") return "Connected to " + (info.peer || "a client")
    if (!pinSet) return "Set a PIN to allow connections"
    if (!unattended) return "Unattended access is off"
    return "Waiting for a connection"
  }

  readonly property string barTooltip: {
    var lines = ["Omarchy Connect: " + statusLine]
    if (running && addresses.length > 0) lines.push(addresses[0].ip + ":" + port)
    return lines.join("\n")
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function refresh() {
    if (statusProc.running) return
    statusProc.running = true
  }

  function say(text, isError) {
    message = text
    messageIsError = isError === true
  }

  function run(args, secret) {
    if (actionProc.running) return
    busy = true
    actionProc.secret = secret || ""
    actionProc.command = [cli].concat(args)
    actionProc.running = true
  }

  function savePin() {
    var pin = String(pinField.text || "").trim()
    if (pin.length < 6) {
      say("PIN must be at least 6 characters", true)
      pinField.forceActiveFocus()
      return
    }
    pinField.text = ""
    run(["pin", "set"], pin)
  }

  function setVideo(fps, bitrateKbps) {
    run(["video", "--fps", String(fps), "--bitrate-kbps", String(bitrateKbps)])
  }

  function toggleUnattended() {
    if (!pinSet) {
      say("Set a PIN first", true)
      pinField.forceActiveFocus()
      return
    }
    run(["unattended", unattended ? "off" : "on"])
  }

  function copyAddress(ip) {
    var text = ip + ":" + port
    Quickshell.execDetached(["bash", "-c", "printf %s " + Util.shellQuote(text) + " | wl-copy"])
    say("Copied " + text, false)
  }

  function startHost() {
    busy = true
    startProc.running = true
  }

  onOpenedChanged: if (opened) {
    message = ""
    refresh()
    Qt.callLater(function() { keyCatcher.forceActiveFocus() })
  }

  Process {
    id: statusProc
    command: [root.cli, "status", "--json"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        try {
          root.info = JSON.parse(String(text || "{}"))
          root.loaded = true
        } catch (e) {
          root.info = ({})
          root.loaded = true
        }
      }
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.info = ({})
        root.loaded = true
      }
    }
  }

  Process {
    id: actionProc
    property string secret: ""
    stdinEnabled: true
    onStarted: {
      if (secret !== "") write(secret + "\n")
      secret = ""
    }
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var out = String(text || "").trim()
        if (out !== "") root.say(out, false)
      }
    }
    stderr: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var err = String(text || "").trim().replace(/^Error:\s*/, "")
        if (err !== "") root.say(err, true)
      }
    }
    onExited: {
      root.busy = false
      root.refresh()
    }
  }

  Process {
    id: startProc
    command: ["systemctl", "--user", "start", "omarchy-connect.service"]
    onExited: function(exitCode) {
      root.busy = false
      if (exitCode !== 0) root.say("Could not start omarchy-connect.service", true)
      startRefresh.restart()
    }
  }

  Timer {
    id: startRefresh
    interval: 800
    onTriggered: root.refresh()
  }

  FileView {
    path: root.stateFile
    watchChanges: true
    printErrors: false
    onFileChanged: root.refresh()
  }

  Timer {
    interval: root.opened ? 2000 : 10000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰢹"
    active: root.connected
    useActiveColor: true
    opacity: root.accessOn || root.connected ? 1.0 : 0.5
    tooltipText: root.opened ? "" : root.barTooltip
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton) root.refresh()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(620))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      blocked: pinField.activeFocus
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "r" || t === "R") root.refresh()
        else if (t === "p" || t === "P") pinField.forceActiveFocus()
      }

      Column {
        id: column
        width: parent.width
        spacing: Style.space(12)

        PanelHero {
          id: hero
          width: parent.width
          title: "Omarchy Connect"
          meta: root.statusLine
          foreground: root.foreground
          fontFamily: root.fontFamily
          iconOpacity: root.accessOn || root.connected ? 1.0 : 0.5
          iconComponent: Component {
            Text {
              text: "󰢹"
              color: root.connected ? Color.accent : root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.display
            }
          }
          trailingControl: Component {
            ToggleSwitch {
              id: accessSwitch
              visible: root.running
              checked: root.unattended && root.pinSet
              busy: root.busy
              foreground: hero.foreground
              onToggled: root.toggleUnattended()

              PanelToolTip {
                visible: accessSwitch.containsMouse
                text: root.unattended ? "Turn unattended access off" : "Turn unattended access on"
                fontFamily: hero.fontFamily
              }
            }
          }
        }

        Text {
          textFormat: Text.PlainText
          visible: root.message !== ""
          width: parent.width
          text: root.message
          color: root.messageIsError ? root.urgent : root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WordWrap
        }

        Button {
          visible: root.loaded && !root.running
          width: parent.width
          bordered: true
          iconText: "󰐊"
          text: "Start the host"
          foreground: root.foreground
          fontFamily: root.fontFamily
          enabled: !root.busy
          onClicked: root.startHost()
        }

        // Current session.
        Column {
          visible: root.connected
          width: parent.width
          spacing: Style.space(8)

          PanelSeparator { foreground: root.foreground }

          PanelSectionHeader {
            text: "SESSION"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          RowLayout {
            width: parent.width
            spacing: Style.space(8)

            ColumnLayout {
              Layout.fillWidth: true
              spacing: Style.space(1)

              Text {
                textFormat: Text.PlainText
                Layout.fillWidth: true
                text: (root.info.client || "Client") + " · " + (root.info.peer || "")
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideRight
              }

              Text {
                textFormat: Text.PlainText
                Layout.fillWidth: true
                text: root.status === "sharing"
                  ? "Starting screen capture"
                  : (root.info.since ? "Since " + Qt.formatTime(new Date(root.info.since * 1000), "HH:mm") : "")
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
                elide: Text.ElideRight
              }
            }

            Button {
              text: "Disconnect"
              bordered: true
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy
              onClicked: root.run(["disconnect"])
            }
          }
        }

        // Where to point the client.
        Column {
          visible: root.running
          width: parent.width
          spacing: Style.space(6)

          PanelSeparator { foreground: root.foreground }

          PanelSectionHeader {
            text: "CONNECT TO"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Text {
            textFormat: Text.PlainText
            visible: root.addresses.length === 0
            width: parent.width
            text: "No network address"
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.body
          }

          Repeater {
            model: root.addresses
            AddressRow {
              required property var modelData
              width: column.width
              ip: String(modelData.ip || "")
              iface: String(modelData.interface || "")
            }
          }
        }

        Column {
          visible: root.loaded
          width: parent.width
          spacing: Style.space(8)

          PanelSeparator { foreground: root.foreground }

          PanelSectionHeader {
            text: "VIDEO"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Text {
            textFormat: Text.PlainText
            width: parent.width
            text: "Frame rate"
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }

          Row {
            spacing: Style.space(6)
            Repeater {
              model: [15, 30, 60]
              Button {
                required property int modelData
                text: modelData + " fps"
                bordered: root.fps === modelData
                foreground: root.foreground
                fontFamily: root.fontFamily
                enabled: !root.busy
                onClicked: root.setVideo(modelData, root.bitrateKbps)
              }
            }
          }

          Text {
            textFormat: Text.PlainText
            width: parent.width
            text: "Bitrate"
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }

          Row {
            spacing: Style.space(6)
            Button {
              text: "4 Mb/s"
              bordered: root.bitrateKbps === 4000
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy
              onClicked: root.setVideo(root.fps, 4000)
            }
            Button {
              text: "8 Mb/s"
              bordered: root.bitrateKbps === 8000
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy
              onClicked: root.setVideo(root.fps, 8000)
            }
            Button {
              text: "12 Mb/s"
              bordered: root.bitrateKbps === 12000
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy
              onClicked: root.setVideo(root.fps, 12000)
            }
          }

          Text {
            textFormat: Text.PlainText
            width: parent.width
            text: "Higher frame rate is smoother and lower latency. It applies the next time someone connects."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }
        }

        // Unattended PIN.
        Column {
          visible: root.loaded
          width: parent.width
          spacing: Style.space(8)

          PanelSeparator { foreground: root.foreground }

          PanelSectionHeader {
            text: "UNATTENDED ACCESS PIN"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          RowLayout {
            width: parent.width
            spacing: Style.space(6)

            TextField {
              id: pinField
              Layout.fillWidth: true
              password: true
              placeholderText: root.pinSet ? "PIN is set. Type a new one to change it" : "At least 6 characters"
              font.family: root.fontFamily
              font.pixelSize: Style.font.body
              foreground: root.foreground
              enabled: !root.busy
              onAccepted: root.savePin()
              Keys.onEscapePressed: {
                text = ""
                keyCatcher.forceActiveFocus()
              }
            }

            Button {
              text: "Save"
              bordered: true
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy && pinField.text.length > 0
              onClicked: root.savePin()
            }
          }

          Text {
            textFormat: Text.PlainText
            width: parent.width
            text: root.pinSet
              ? (root.unattended
                  ? "Anyone with this PIN can control this computer without anyone here to accept."
                  : "The PIN is kept, but connections are refused until you turn access on.")
              : "Saving a PIN turns unattended access on."
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          Text {
            textFormat: Text.PlainText
            visible: (root.info.locked_secs || 0) > 0
            width: parent.width
            text: "Locked for " + root.info.locked_secs + " s after wrong PINs"
            color: root.urgent
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          Text {
            textFormat: Text.PlainText
            visible: root.running && root.info.input_ready === false
            width: parent.width
            text: "Remote keyboard and mouse are off: /dev/uinput is not writable. Rerun the installer to add the udev rule, then log out and back in."
            color: root.urgent
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          Row {
            spacing: Style.space(6)

            Button {
              visible: root.pinSet
              text: "Remove PIN"
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy
              onClicked: root.run(["pin", "clear"])
            }

            Button {
              visible: root.info.share_saved === true
              text: "Pick screens again"
              tooltipText: "Forget the saved screen share; the picker shows on the next connection"
              foreground: root.foreground
              fontFamily: root.fontFamily
              enabled: !root.busy
              onClicked: root.run(["reset-share"])
            }
          }
        }
      }
    }
  }

  component AddressRow: CursorSurface {
    id: addressRow
    property string ip: ""
    property string iface: ""

    foreground: root.foreground
    hasCursor: addressMouse.containsMouse
    implicitHeight: addressContent.implicitHeight + Style.spacing.rowPaddingX

    MouseArea {
      id: addressMouse
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onClicked: root.copyAddress(addressRow.ip)
    }

    RowLayout {
      id: addressContent
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      anchors.leftMargin: Style.space(10)
      anchors.rightMargin: Style.space(10)
      spacing: Style.space(8)

      Text {
        textFormat: Text.PlainText
        Layout.fillWidth: true
        text: addressRow.ip + (root.port === 47921 ? "" : ":" + root.port)
        color: root.foreground
        font.family: root.fontFamily
        font.pixelSize: Style.font.subtitle
        font.bold: true
        elide: Text.ElideRight
      }

      Text {
        textFormat: Text.PlainText
        text: addressRow.iface
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.caption
      }

      Text {
        textFormat: Text.PlainText
        text: "󰆏"
        color: root.dim
        font.family: root.fontFamily
        font.pixelSize: Style.font.icon
      }
    }
  }
}
