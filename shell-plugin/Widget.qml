import QtQuick
import Quickshell
import Quickshell.Hyprland
import Quickshell.Io
import qs.Commons
import qs.Ui

// Bar icon + popup. One instance per monitor; all state lives in
// PeridotService.
Panel {
  id: root
  manageIpc: false

  // The service loads once for the whole shell; it may appear after us.
  property var svc: null
  function findService() {
    if (!svc && bar && bar.shell && typeof bar.shell.serviceFor === "function")
      svc = bar.shell.serviceFor(root.moduleName || "derekross.peridot")
  }
  Component.onCompleted: { findService(); checkApproval() }
  onBarChanged: findService()
  Timer {
    interval: 500
    running: !root.svc
    repeat: true
    onTriggered: root.findService()
  }

  // ── Third-party bars ─────────────────────────────────────────
  // The shell hands plugin services only to its built-in bar. On a custom
  // bar serviceFor() stays null, so (with the user's OK) the widget runs its
  // own PeridotService, which talks to peridotd directly.
  property bool serviceBlocked: false   // no shell service after a grace period
  property bool approved: false
  property bool approvalChecked: false
  readonly property string approvalFile:
    (Quickshell.env("XDG_STATE_HOME") || (Quickshell.env("HOME") + "/.local/state")) + "/peridot/third-party-bar-approved"
  readonly property bool needsApproval: serviceBlocked && approvalChecked && !approved

  Timer {
    interval: 3000
    running: !root.svc && !root.serviceBlocked
    onTriggered: if (!root.svc) root.serviceBlocked = true
  }
  Process {
    id: checkProc
    command: ["/usr/bin/test", "-f", root.approvalFile]
    onExited: function(code) { root.approved = code === 0; root.approvalChecked = true }
  }
  Process {
    id: approveProc
    onExited: function(code) { if (code === 0) root.approved = true }
  }
  function checkApproval() { checkProc.running = true }
  function approve() {
    approveProc.command = ["/usr/bin/sh", "-c",
      'umask 077 && /usr/bin/mkdir -p "$(/usr/bin/dirname "$1")" && /usr/bin/date +%s > "$1"',
      "sh", root.approvalFile]
    approveProc.running = true
  }
  Loader {
    active: root.serviceBlocked && root.approved
    sourceComponent: PeridotService { ipcEnabled: false }
    onLoaded: if (!root.svc) root.svc = item
  }

  // A notification click or `omarchy-shell derekross.peridot open` opens
  // the popup on the focused monitor.
  Connections {
    target: root.svc
    function onOpenRequested() {
      var win = root.QsWindow.window
      var focused = Hyprland.focusedMonitor
      if (!win || !win.screen || !focused || win.screen.name === focused.name) root.open()
    }
  }

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property bool daemonUp: !!svc && svc.connected
  readonly property int attention: svc ? svc.attention : 0

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰓦"
    active: root.attention > 0
    dimmed: !root.daemonUp || !root.svc.setUp || root.svc.paused
    tooltipText: {
      if (root.needsApproval) return "Peridot: click to allow it on this custom bar"
      if (!root.daemonUp) return "Peridot isn't running"
      if (!root.svc.setUp) return "Peridot: keep your computers matching"
      if (root.svc.opal && root.svc.opal.needs_pairing) return "Peridot: pair with Opal to keep syncing"
      if (root.attention > 0) return root.attention + " waiting from your other computers"
      if (root.svc.paused) return "Peridot: paused"
      return "Peridot: everything matches"
    }
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton && root.svc) root.svc.call("sync.now", null)
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: content.keyTarget
    contentWidth: panel.fittedContentWidth(Style.space(440))
    contentHeight: panel.fittedContentHeight(content.implicitHeight, Style.space(660))

    PeridotPanel {
      id: content
      anchors.fill: parent
      svc: root.svc
      needsApproval: root.needsApproval
      onApproveRequested: root.approve()
      foreground: root.foreground
      urgent: root.urgent
      onCloseRequested: root.close()
    }
  }
}
