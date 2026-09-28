import QtQuick
import qs.Commons
import qs.Ui

// A pairing in progress. On the new computer: the code (and QR) to enter
// on one you already use. On both: the number to compare, and Yes/No.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  // The general state leaves the code out (every listener sees that);
  // the panel asks for it.
  property var full: null
  readonly property var p: {
    var base = svc && svc.pairing ? svc.pairing : ({})
    if (full && base.stage === "waiting" && base.role === "new") {
      var merged = Object.assign({}, base)
      merged.code = full.code
      merged.qr = full.qr
      return merged
    }
    return base
  }
  readonly property bool isNew: p.role === "new"
  property bool holdKey: false

  function refreshCode() {
    var base = svc && svc.pairing ? svc.pairing : null
    if (!base || base.role !== "new" || base.stage !== "waiting") { full = null; return }
    if (full && full.expires_at === base.expires_at) return
    svc.call("pair.view", null, function(err, v) { if (!err && v) root.full = v })
  }

  Connections {
    target: root.svc
    function onStatusChanged() { root.refreshCode() }
  }
  Component.onCompleted: refreshCode()

  spacing: Style.space(10)

  onVisibleChanged: if (!visible) holdKey = false

  PanelSectionHeader {
    text: root.isNew ? "ADD THIS COMPUTER" : "PAIR A NEW COMPUTER"
    foreground: root.dim
  }

  // New computer, waiting: show the code.
  Column {
    width: parent.width
    spacing: Style.space(8)
    visible: root.isNew && root.p.stage === "waiting"
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: "On a computer that already uses Peridot, open Peridot and choose \"Pair a new computer\", then enter this code:"
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      horizontalAlignment: Text.AlignHCenter
      color: root.foreground
      font.family: Style.font.monoFamily || Style.font.family
      font.pixelSize: Style.font.subtitle
      font.bold: true
      text: root.p.code || ""
    }
    Image {
      anchors.horizontalCenter: parent.horizontalCenter
      width: Style.space(160)
      height: width
      visible: !!root.p.qr
      source: root.p.qr || ""
      smooth: false
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: "The code works for 5 minutes and once. Or run `peridot pair " + (root.p.code || "") + "` there."
    }
  }

  // Existing computer, waiting for the new one to answer.
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: !root.isNew && root.p.stage === "waiting"
    wrapMode: Text.Wrap
    color: root.foreground
    font.family: Style.font.family
    font.pixelSize: Style.font.body
    text: "Waiting for the new computer…"
  }

  // Both: compare the numbers and answer here.
  Column {
    width: parent.width
    spacing: Style.space(8)
    visible: root.p.stage === "confirm" || root.p.stage === "waiting_other"
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: "Does " + (root.p.other || "the other computer") + " show this number?"
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      horizontalAlignment: Text.AlignHCenter
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.subtitle * 1.6
      font.bold: true
      text: root.p.number || ""
    }
    Toggle {
      width: parent.width
      visible: !root.isNew && root.p.can_hold_key === true && root.p.stage === "confirm"
      label: "Also keep the key on the new computer"
      description: "On: it holds your key like this one does. Off: it needs Opal with this identity to sync."
      checked: root.holdKey
      foreground: root.foreground
      onClicked: root.holdKey = !root.holdKey
    }
    Row {
      visible: root.p.stage === "confirm"
      spacing: Style.space(8)
      Button {
        text: "Yes, they match"
        iconText: "󰄬"
        bordered: true
        foreground: root.foreground
        onClicked: root.svc.run("pair.confirm", { matches: true, hold_key: root.holdKey, confirm: true })
      }
      Button {
        text: "No"
        foreground: root.urgent
        onClicked: root.svc.run("pair.confirm", { matches: false, confirm: true })
      }
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      visible: root.p.stage === "waiting_other"
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: "Waiting for " + (root.p.other || "the other computer") + " to confirm too…"
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      visible: root.p.stage === "confirm"
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: "Both computers have to say yes. If the numbers differ, someone else may have seen the code: choose No and start again with a new code."
    }
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.p.stage === "sending"
    wrapMode: Text.Wrap
    color: root.foreground
    font.family: Style.font.family
    font.pixelSize: Style.font.body
    text: root.isNew ? "Receiving your settings key…" : "Sending your settings key to " + (root.p.other || "the new computer") + "…"
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.p.stage === "approve"
    wrapMode: Text.Wrap
    color: root.foreground
    font.family: Style.font.family
    font.pixelSize: Style.font.body
    text: "Your identity is held by Opal. Approve Peridot in Opal's bar to finish."
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: ["done", "expired", "cancelled", "aborted", "failed"].indexOf(root.p.stage) !== -1
    wrapMode: Text.Wrap
    color: root.p.stage === "done" ? root.foreground : root.urgent
    font.family: Style.font.family
    font.pixelSize: Style.font.body
    text: {
      switch (root.p.stage) {
      case "done": return root.isNew
        ? "Paired. Your settings are arriving; review them under Changes."
        : "Paired. " + (root.p.other || "The new computer") + " now syncs with your others."
      case "expired": return "The code expired before pairing finished."
      case "cancelled": return "Pairing cancelled. Nothing was shared."
      case "aborted": return (root.p.error || "Pairing stopped.") + " Nothing was shared."
      default: return root.p.error || "Pairing didn't work."
      }
    }
  }

  Button {
    text: ["done", "expired", "cancelled", "aborted", "failed"].indexOf(root.p.stage) !== -1 ? "Close" : "Cancel"
    foreground: root.foreground
    onClicked: root.svc.run("pair.cancel", null)
  }
}
