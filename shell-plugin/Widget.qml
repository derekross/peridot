import QtQuick
import Quickshell
import Quickshell.Hyprland
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
  Component.onCompleted: findService()
  onBarChanged: findService()
  Timer {
    interval: 500
    running: !root.svc
    repeat: true
    onTriggered: root.findService()
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
      if (!root.daemonUp) return "Peridot isn't running"
      if (!root.svc.setUp) return "Peridot: keep your computers matching"
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
      foreground: root.foreground
      urgent: root.urgent
      onCloseRequested: root.close()
    }
  }
}
