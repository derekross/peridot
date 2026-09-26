import QtQuick
import QtQuick.Controls
import Quickshell
import qs.Commons
import qs.Ui
import "util.js" as U

// Popup content: welcome until this computer is set up, then what's
// changed, your computers and settings.
Item {
  id: root

  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  signal closeRequested()

  property alias keyTarget: keyCatcher
  property string tab: "changes"
  property string toast: ""
  property bool toastError: false

  readonly property bool up: !!svc && svc.connected
  readonly property bool editing: {
    var f = Window.activeFocusItem
    return !!f && f.echoMode !== undefined
  }
  readonly property var tabs: [
    { value: "changes", label: up && svc.attention > 0 ? "Changes " + svc.attention : "Changes" },
    { value: "computers", label: "Computers" },
    { value: "settings", label: "Settings" }
  ]

  implicitHeight: column.implicitHeight

  function showToast(text, isError) {
    toast = text
    toastError = isError
    toastTimer.restart()
  }

  Connections {
    target: root.svc
    function onMessage(text, isError) { root.showToast(text, isError) }
  }

  Timer {
    id: toastTimer
    interval: 3500
    onTriggered: root.toast = ""
  }

  PanelKeyCatcher {
    id: keyCatcher
    anchors.fill: parent
    blocked: root.editing
    onCloseRequested: root.closeRequested()
    onMoveRequested: function(dx, dy) {
      if (dx === 0 || !root.up || !root.svc.setUp) return
      var i = 0
      for (; i < root.tabs.length; i++) if (root.tabs[i].value === root.tab) break
      root.tab = root.tabs[(i + dx + root.tabs.length) % root.tabs.length].value
    }

    Flickable {
      id: flick
      anchors.fill: parent
      contentWidth: width
      contentHeight: column.implicitHeight
      clip: true
      boundsBehavior: Flickable.StopAtBounds
      interactive: contentHeight > height
      ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

      Column {
        id: column
        width: flick.width
        spacing: Style.space(12)

        // ── Header ────────────────────────────────────────────────
        Item {
          width: parent.width
          implicitHeight: headerText.implicitHeight

          Column {
            id: headerText
            anchors.left: parent.left
            anchors.right: headerButtons.left
            anchors.rightMargin: Style.space(8)
            spacing: Style.space(2)
            Text {
              textFormat: Text.PlainText
              width: parent.width
              elide: Text.ElideRight
              color: root.foreground
              font.family: Style.font.family
              font.pixelSize: Style.font.subtitle
              font.bold: true
              text: "Peridot"
            }
            Text {
              textFormat: Text.PlainText
              width: parent.width
              elide: Text.ElideRight
              color: root.dim
              font.family: Style.font.family
              font.pixelSize: Style.font.caption
              text: {
                if (!root.up) return "Not running"
                var s = root.svc
                if (!s.setUp) return "Keep your Omarchy computers matching"
                var id = s.status.identity || {}
                var who = id.name || U.shortKey(id.npub || "")
                if (id.mode === "opal") who += " via Opal"
                if (s.paused) return (s.status.device_name || "This computer") + " · " + who + " · paused"
                var parts = [s.status.device_name || "This computer", who]
                var waiting = s.attention
                parts.push(waiting > 0 ? waiting + " waiting" : "everything matches")
                if (s.status.last_sync) parts.push("checked " + U.ago(s.status.last_sync, s.now))
                return parts.join(" · ")
              }
            }
          }

          Row {
            id: headerButtons
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)
            visible: root.up && root.svc.setUp
            PanelActionButton {
              iconText: "󰓦"
              tooltipText: "Check for changes now"
              onClicked: root.svc.run("sync.now", null, function() { root.showToast("Checking…", false) })
            }
          }
        }

        // ── Not running ───────────────────────────────────────────
        Column {
          width: parent.width
          visible: !root.up
          spacing: Style.space(8)
          Text {
            textFormat: Text.PlainText
            width: parent.width
            wrapMode: Text.Wrap
            color: root.foreground
            font.family: Style.font.family
            font.pixelSize: Style.font.body
            text: "Peridot's background service isn't running."
          }
          Button {
            text: "Start Peridot"
            iconText: "󰐊"
            bordered: true
            foreground: root.foreground
            onClicked: if (root.svc) root.svc.startDaemon()
          }
        }

        // ── Pairing in progress (either side) ─────────────────────
        PairCard {
          width: parent.width
          visible: root.up && !!root.svc.pairing
          svc: root.svc
          foreground: root.foreground
          urgent: root.urgent
        }

        // ── Not set up yet ────────────────────────────────────────
        WelcomeView {
          width: parent.width
          visible: root.up && !root.svc.setUp && !root.svc.pairing
          svc: root.svc
          foreground: root.foreground
          urgent: root.urgent
        }

        // ── Set up ────────────────────────────────────────────────
        ButtonGroup {
          width: parent.width
          visible: root.up && root.svc.setUp
          options: root.tabs
          value: root.tab
          foreground: root.foreground
          onChanged: function(v) { root.tab = v }
        }

        ChangesView {
          width: parent.width
          visible: root.up && root.svc.setUp && root.tab === "changes"
          svc: root.svc
          foreground: root.foreground
          urgent: root.urgent
        }

        ComputersView {
          width: parent.width
          visible: root.up && root.svc.setUp && root.tab === "computers"
          svc: root.svc
          foreground: root.foreground
          urgent: root.urgent
        }

        SettingsView {
          width: parent.width
          visible: root.up && root.svc.setUp && root.tab === "settings"
          svc: root.svc
          foreground: root.foreground
          urgent: root.urgent
        }

        Text {
          textFormat: Text.PlainText
          width: parent.width
          visible: root.up && !!root.svc.status.error
          wrapMode: Text.Wrap
          color: root.urgent
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: root.up ? (root.svc.status.error || "") : ""
        }

        Text {
          textFormat: Text.PlainText
          width: parent.width
          visible: root.toast !== ""
          wrapMode: Text.Wrap
          color: root.toastError ? root.urgent : root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: root.toast
        }
      }
    }
  }
}
