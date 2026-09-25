import QtQuick
import qs.Commons
import qs.Ui
import "util.js" as U

// One synced file: its friendly name, where the change is from, and
// buttons.
Item {
  id: root
  property var file: ({})
  property string note: ""
  // [{ label, icon, run }]
  property var actions: []
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  implicitHeight: row.implicitHeight

  Row {
    id: row
    width: parent.width
    spacing: Style.space(8)

    Column {
      width: parent.width - buttons.width - Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(1)
      Text {
        textFormat: Text.PlainText
        width: parent.width
        elide: Text.ElideRight
        color: root.foreground
        font.family: Style.font.family
        font.pixelSize: Style.font.body
        text: U.label(root.file.path || "")
      }
      Text {
        textFormat: Text.PlainText
        width: parent.width
        elide: Text.ElideRight
        color: root.file.runs_commands ? root.urgent : root.dim
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
        text: root.note
      }
      Text {
        textFormat: Text.PlainText
        width: parent.width
        elide: Text.ElideMiddle
        color: root.dim
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
        text: "~/" + (root.file.path || "")
      }
    }

    Row {
      id: buttons
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(4)
      Repeater {
        model: root.actions
        delegate: Button {
          required property var modelData
          text: modelData.label
          iconText: modelData.icon
          bordered: true
          foreground: root.foreground
          onClicked: modelData.run()
        }
      }
    }
  }
}
