import QtQuick
import qs.Commons
import qs.Ui
import "util.js" as U

// Your computers, pairing a new one, and this computer's name.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  property string confirmRemove: ""
  property bool renaming: false
  property string error: ""

  spacing: Style.space(10)

  onVisibleChanged: if (!visible) { confirmRemove = ""; renaming = false; code.text = ""; error = "" }

  Repeater {
    model: root.svc ? root.svc.devices : []
    delegate: Row {
      required property var modelData
      width: root.width
      spacing: Style.space(8)
      Text {
        text: modelData.this ? "󰌢" : "󰍹"
        color: root.foreground
        font.family: Style.font.family
        font.pixelSize: Style.font.icon
        anchors.verticalCenter: parent.verticalCenter
      }
      Column {
        width: parent.width - Style.space(40) - removeButton.width
        anchors.verticalCenter: parent.verticalCenter
        Text {
          textFormat: Text.PlainText
          width: parent.width
          elide: Text.ElideRight
          color: root.foreground
          font.family: Style.font.family
          font.pixelSize: Style.font.body
          font.bold: modelData.this
          text: modelData.name + (modelData.this ? "  (this computer)" : "")
        }
        Text {
          textFormat: Text.PlainText
          width: parent.width
          color: root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: modelData.this ? "Syncing" : "Last seen " + U.ago(modelData.last_seen, root.svc ? root.svc.now : 0)
        }
      }
      PanelActionButton {
        id: removeButton
        visible: !modelData.this
        iconText: "󰆴"
        hoverColor: root.urgent
        tooltipText: root.confirmRemove === modelData.id ? "Click again to remove" : "Remove from your computers"
        onClicked: {
          if (root.confirmRemove !== modelData.id) { root.confirmRemove = modelData.id; return }
          root.confirmRemove = ""
          root.svc.run("device.remove", { id: modelData.id })
        }
      }
    }
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.confirmRemove !== ""
    wrapMode: Text.Wrap
    color: root.urgent
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Removing hides it from your list. It keeps what it already has; to stop it syncing, choose \"Stop syncing here\" on that computer."
  }

  PanelSectionHeader { text: "PAIR A NEW COMPUTER"; foreground: root.dim }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: "Install Peridot on the new computer and choose \"I already use Peridot on another computer\". Enter the code it shows here."
  }
  Row {
    width: parent.width
    spacing: Style.space(8)
    TextField {
      id: code
      width: parent.width - pairButton.width - Style.space(8)
      placeholderText: "PDT-XXXX-XXXX-XXXX-XXXX"
      foreground: root.foreground
      onAccepted: pairButton.clicked()
    }
    Button {
      id: pairButton
      text: "Pair"
      iconText: "󰌢"
      bordered: true
      foreground: root.foreground
      onClicked: {
        root.error = ""
        root.svc.call("pair.join", { code: code.text }, function(err) {
          if (err) { root.error = err; return }
          code.text = ""
        })
      }
    }
  }

  PanelSectionHeader { text: "THIS COMPUTER"; foreground: root.dim }
  Row {
    width: parent.width
    spacing: Style.space(8)
    visible: !root.renaming
    Text {
      textFormat: Text.PlainText
      width: parent.width - renameButton.width - Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      elide: Text.ElideRight
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: root.svc ? (root.svc.status.device_name || "") : ""
    }
    PanelActionButton {
      id: renameButton
      iconText: "󰏫"
      tooltipText: "Rename"
      onClicked: { nameField.text = root.svc.status.device_name || ""; root.renaming = true }
    }
  }
  Row {
    width: parent.width
    spacing: Style.space(8)
    visible: root.renaming
    TextField {
      id: nameField
      width: parent.width - saveName.width - Style.space(8)
      placeholderText: "Name your other computers see"
      foreground: root.foreground
      onAccepted: saveName.clicked()
    }
    Button {
      id: saveName
      text: "Save"
      bordered: true
      foreground: root.foreground
      onClicked: root.svc.run("device.rename", { name: nameField.text }, function() { root.renaming = false })
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
