import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "util.js" as U

// Private links: share something new, see what's out there, remove links.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  readonly property var shares: svc ? (svc.status.shares || []) : []
  readonly property int days: svc && svc.status.share_expire_days ? svc.status.share_expire_days : 7
  property string confirmRemove: ""

  spacing: Style.space(10)

  onVisibleChanged: if (!visible) confirmRemove = ""

  function left(expires) {
    var s = expires - Math.floor((svc ? svc.now : Date.now()) / 1000)
    if (s <= 0) return "expired"
    if (s < 3600) return Math.max(1, Math.floor(s / 60)) + "m left"
    if (s < 86400) return Math.floor(s / 3600) + "h left"
    return Math.floor(s / 86400) + "d left"
  }

  function human(n) {
    if (n < 1024) return n + " B"
    if (n < 1048576) return (n / 1024).toFixed(1) + " KB"
    return (n / 1048576).toFixed(1) + " MB"
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: "A private link is a file encrypted on this computer. The key travels in the link itself, so only people you send it to can open it. Links work for " + root.days + " days unless you remove them sooner."
  }

  Row {
    spacing: Style.space(8)
    Button {
      text: "Share a file…"
      iconText: ""
      bordered: true
      foreground: root.foreground
      onClicked: Quickshell.execDetached(["peridot", "share", "--pick", "--notify"])
    }
    Button {
      text: "Clipboard"
      iconText: ""
      bordered: true
      foreground: root.foreground
      onClicked: Quickshell.execDetached(["peridot", "share", "--clipboard", "--notify"])
    }
    Button {
      text: "Last screenshot"
      iconText: "󰹑"
      bordered: true
      foreground: root.foreground
      onClicked: Quickshell.execDetached(["peridot", "share", "--screenshot", "--notify"])
    }
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Also in the Omarchy menu: Share → Private link. The link is copied to your clipboard."
  }

  // Expired links Opal hasn't let us remove yet (it asks about each removal).
  Row {
    spacing: Style.space(8)
    visible: !!root.svc && !!root.svc.opal && (root.svc.opal.shares_waiting || 0) > 0
    Text {
      textFormat: Text.PlainText
      anchors.verticalCenter: parent.verticalCenter
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: (root.svc && root.svc.opal ? root.svc.opal.shares_waiting : 0) + " expired link(s) will be removed once you approve it in Opal"
    }
    Button {
      text: "Remove now"
      foreground: root.foreground
      onClicked: root.svc.run("share.sweep", null)
    }
  }

  PanelSectionHeader {
    visible: root.shares.length > 0
    text: "YOUR LINKS"
    foreground: root.dim
  }
  Repeater {
    model: root.shares
    delegate: Row {
      required property var modelData
      width: root.width
      spacing: Style.space(8)
      Column {
        width: parent.width - shareButtons.width - Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        Text {
          textFormat: Text.PlainText
          width: parent.width
          elide: Text.ElideMiddle
          color: root.foreground
          font.family: Style.font.family
          font.pixelSize: Style.font.body
          text: modelData.name
        }
        Text {
          textFormat: Text.PlainText
          width: parent.width
          elide: Text.ElideRight
          color: root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: root.human(modelData.size) + " · " + root.left(modelData.expires) + " · " + modelData.server
        }
      }
      Row {
        id: shareButtons
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(2)
        PanelActionButton {
          iconText: "󰆏"
          tooltipText: "Copy link"
          onClicked: root.svc.copy(modelData.url, "Link copied")
        }
        PanelActionButton {
          iconText: "󰆴"
          hoverColor: root.urgent
          tooltipText: root.confirmRemove === String(modelData.id) ? "Click again: the link stops working" : "Remove"
          onClicked: {
            if (root.confirmRemove !== String(modelData.id)) { root.confirmRemove = String(modelData.id); return }
            root.confirmRemove = ""
            root.svc.run("share.revoke", { id: modelData.id }, function() { root.svc.message("Removed", false) })
          }
        }
      }
    }
  }
}
