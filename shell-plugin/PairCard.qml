import QtQuick
import qs.Commons
import qs.Ui

// A pairing in progress. On the new computer: the code (and QR) to enter
// on one you already use. On the existing one: the number to compare.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  readonly property var p: svc && svc.pairing ? svc.pairing : ({})
  readonly property bool isNew: p.role === "new"

  spacing: Style.space(10)

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
      text: "The code works for 5 minutes. Or run `peridot pair " + (root.p.code || "") + "` there."
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

  // Both: compare the numbers.
  Column {
    width: parent.width
    spacing: Style.space(8)
    visible: root.p.stage === "confirm"
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: root.isNew
        ? "Check that " + (root.p.other || "your other computer") + " shows this number, and confirm there:"
        : "Does " + (root.p.other || "the new computer") + " show this number?"
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
    Row {
      visible: !root.isNew
      spacing: Style.space(8)
      Button {
        text: "Yes, they match"
        iconText: "󰄬"
        bordered: true
        foreground: root.foreground
        onClicked: root.svc.run("pair.confirm", { matches: true })
      }
      Button {
        text: "No"
        foreground: root.urgent
        onClicked: root.svc.run("pair.confirm", { matches: false })
      }
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      visible: !root.isNew
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: "If the numbers differ, someone else may have seen the code. Choose No and start again."
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
    text: "Sending your settings key to " + (root.p.other || "the new computer") + "…"
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: ["done", "expired", "cancelled", "failed"].indexOf(root.p.stage) !== -1
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
      default: return root.p.error || "Pairing didn't work."
      }
    }
  }

  Button {
    text: ["done", "expired", "cancelled", "failed"].indexOf(root.p.stage) !== -1 ? "Close" : "Cancel"
    foreground: root.foreground
    onClicked: root.svc.run("pair.cancel", null)
  }
}
