import QtQuick
import qs.Commons
import qs.Ui

// First run: start fresh, join from a computer you already use, or restore
// from a recovery kit.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  property string mode: ""
  property bool busy: false
  property string error: ""

  spacing: Style.space(10)

  onVisibleChanged: if (!visible) { code.text = ""; words.text = ""; error = ""; mode = "" }

  function para(t) { return t }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.foreground
    font.family: Style.font.family
    font.pixelSize: Style.font.body
    text: "Your look, keyboard shortcuts, terminal, bar and menu follow you to every computer you use. There's no account to create: everything is encrypted on this computer before it leaves, and only your own computers can read it."
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Never synced: passwords, keys, tokens, browser data, your monitor layout. Nothing changes on a computer until you apply it, and every change can be undone."
  }

  Column {
    width: parent.width
    spacing: Style.space(6)
    visible: root.mode === ""

    Button {
      width: parent.width
      leftAlign: true
      bordered: true
      iconText: "󰐕"
      text: "Start fresh on this computer"
      tooltipText: "This is the first computer you use Peridot on"
      foreground: root.foreground
      iconSpinning: root.busy
      onClicked: {
        if (root.busy) return
        root.busy = true
        root.svc.call("setup.start_fresh", null, function(err) {
          root.busy = false
          if (err) root.error = err
        })
      }
    }
    Button {
      width: parent.width
      leftAlign: true
      bordered: true
      iconText: "󰌢"
      text: "I already use Peridot on another computer"
      foreground: root.foreground
      onClicked: root.svc.call("pair.new", null, function(err) { if (err) root.error = err })
    }
    Button {
      width: parent.width
      leftAlign: true
      iconText: "󰁯"
      text: "Use my recovery kit"
      foreground: root.foreground
      onClicked: { root.mode = "restore"; root.error = "" }
    }
  }

  Column {
    width: parent.width
    spacing: Style.space(8)
    visible: root.mode === "restore"

    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.bodySmall
      text: "Enter the recovery code from your kit (it starts with ncryptsec1) and your six words."
    }
    TextField {
      id: code
      width: parent.width
      placeholderText: "Recovery code (ncryptsec1…)"
      foreground: root.foreground
    }
    TextField {
      id: words
      width: parent.width
      password: true
      placeholderText: "Your six words"
      foreground: root.foreground
      onAccepted: restoreButton.clicked()
    }
    Row {
      spacing: Style.space(8)
      Button {
        id: restoreButton
        text: root.busy ? "Restoring…" : "Restore"
        iconText: "󰁯"
        iconSpinning: root.busy
        bordered: true
        foreground: root.foreground
        onClicked: {
          if (root.busy) return
          root.error = ""
          root.busy = true
          root.svc.call("recovery.restore", { code: code.text.trim(), words: words.text }, function(err) {
            root.busy = false
            if (err) { root.error = err; return }
            code.text = ""; words.text = ""
          })
        }
      }
      Button {
        text: "Back"
        foreground: root.foreground
        onClicked: { root.mode = ""; root.error = "" }
      }
    }
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.error !== ""
    wrapMode: Text.Wrap
    color: root.urgent
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: root.error
  }
}
