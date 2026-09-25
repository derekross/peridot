import QtQuick
import qs.Commons
import qs.Ui
import "util.js" as U

// What's waiting from your other computers, conflicts, themes and plugins
// to install, and recent applies (with undo).
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  spacing: Style.space(10)

  readonly property var files: svc ? svc.files : []
  readonly property var incoming: files.filter(function(f) { return f.status === "incoming" })
  readonly property var conflicts: files.filter(function(f) { return f.status === "conflict" })
  readonly property var outgoing: files.filter(function(f) { return f.status === "outgoing" })
  readonly property var kept: files.filter(function(f) { return f.status === "kept" })
  readonly property var offers: svc ? svc.offers : []
  readonly property var history: svc ? svc.history : []
  readonly property int inSync: svc && svc.counts ? (svc.counts.in_sync || 0) : 0
  readonly property bool anyCommands: incoming.some(function(f) { return f.runs_commands })

  // ── Nothing waiting ─────────────────────────────────────────────
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.incoming.length === 0 && root.conflicts.length === 0 && root.offers.length === 0
    wrapMode: Text.Wrap
    color: root.foreground
    font.family: Style.font.family
    font.pixelSize: Style.font.body
    text: root.inSync > 0
      ? "Everything matches. " + root.inSync + " setting" + (root.inSync === 1 ? "" : "s") + " in sync."
      : "Nothing to sync yet. Change a setting and it will appear here."
  }

  // ── Incoming ────────────────────────────────────────────────────
  PanelSectionHeader {
    visible: root.incoming.length > 0
    text: "FROM YOUR OTHER COMPUTERS"
    foreground: root.dim
  }
  Repeater {
    model: root.incoming
    delegate: FileRow {
      required property var modelData
      width: root.width
      file: modelData
      foreground: root.foreground
      urgent: root.urgent
      note: (modelData.deleted ? "Deleted on " : "Changed on ") + (modelData.from || "another computer")
        + (modelData.runs_commands ? " · can run commands" : "")
      actions: [{ label: "Apply", icon: "󰄬", run: function() { root.svc.run("apply", { paths: [modelData.path] }) } }]
    }
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.anyCommands
    wrapMode: Text.Wrap
    color: root.urgent
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Some of these can run commands on this computer. Apply them only if you made the change yourself."
  }
  Row {
    visible: root.incoming.length > 1
    spacing: Style.space(8)
    Button {
      text: "Apply all " + root.incoming.length
      iconText: "󰄬"
      bordered: true
      foreground: root.foreground
      onClicked: root.svc.run("apply", { paths: [] }, function(r) {
        root.svc.message("Applied " + ((r && r.applied) ? r.applied.length : 0) + ". Undo below if needed.", false)
      })
    }
  }

  // ── Conflicts ───────────────────────────────────────────────────
  PanelSectionHeader {
    visible: root.conflicts.length > 0
    text: "CHANGED HERE AND ELSEWHERE"
    foreground: root.dim
  }
  Repeater {
    model: root.conflicts
    delegate: FileRow {
      required property var modelData
      width: root.width
      file: modelData
      foreground: root.foreground
      urgent: root.urgent
      note: "Changed here and on " + (modelData.from || "another computer")
      actions: [
        { label: "Keep mine", icon: "󰆓", run: function() { root.svc.run("conflict.keep_local", { path: modelData.path }) } },
        { label: "Use theirs", icon: "󰄬", run: function() { root.svc.run("apply", { paths: [modelData.path] }) } }
      ]
    }
  }

  // ── Kept after an undo ──────────────────────────────────────────
  PanelSectionHeader {
    visible: root.kept.length > 0
    text: "KEPT ON THIS COMPUTER"
    foreground: root.dim
  }
  Repeater {
    model: root.kept
    delegate: FileRow {
      required property var modelData
      width: root.width
      file: modelData
      foreground: root.foreground
      urgent: root.urgent
      note: "You undid " + (modelData.from || "another computer") + "'s version"
      actions: [{ label: "Use theirs", icon: "󰄬", run: function() { root.svc.run("apply", { paths: [modelData.path] }) } }]
    }
  }

  // ── Themes and plugins ──────────────────────────────────────────
  PanelSectionHeader {
    visible: root.offers.length > 0
    text: "THEMES AND PLUGINS"
    foreground: root.dim
  }
  Repeater {
    model: root.offers
    delegate: Item {
      id: offer
      required property var modelData
      width: root.width
      implicitHeight: offerRow.implicitHeight
      readonly property string what: modelData.kind === "theme"
        ? "Switch to the " + modelData.name + " theme"
        : modelData.kind === "install_theme"
          ? "Install the " + modelData.name + " theme"
          : "Install the " + modelData.name + " plugin"
      readonly property string where: modelData.kind === "theme"
        ? "In use on " + modelData.from
        : "Installed on " + modelData.from + " · " + modelData.url
      Row {
        id: offerRow
        width: parent.width
        spacing: Style.space(8)
        Column {
          width: parent.width - offerButtons.width - Style.space(8)
          anchors.verticalCenter: parent.verticalCenter
          Text {
            textFormat: Text.PlainText
            width: parent.width
            elide: Text.ElideRight
            color: root.foreground
            font.family: Style.font.family
            font.pixelSize: Style.font.body
            text: offer.what
          }
          Text {
            textFormat: Text.PlainText
            width: parent.width
            elide: Text.ElideMiddle
            color: root.dim
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            text: offer.where
          }
        }
        Row {
          id: offerButtons
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.space(2)
          PanelActionButton {
            iconText: "󰄬"
            tooltipText: modelData.kind === "theme" ? "Switch" : "Install"
            onClicked: {
              root.svc.message(modelData.kind === "theme" ? "Switching theme…" : "Installing…", false)
              root.svc.run("offer.accept", modelData, function() { root.svc.message("Done", false) })
            }
          }
          PanelActionButton {
            iconText: "󰅖"
            tooltipText: "Not now"
            onClicked: root.svc.run("offer.dismiss", modelData)
          }
        }
      }
    }
  }

  // ── Sending ─────────────────────────────────────────────────────
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.outgoing.length > 0 || (root.svc && root.svc.status.waiting_to_send > 0)
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Sending your latest changes…"
  }

  // ── Recent ──────────────────────────────────────────────────────
  PanelSectionHeader {
    visible: root.history.length > 0
    text: "RECENT"
    foreground: root.dim
  }
  Repeater {
    model: root.history.slice(0, 5)
    delegate: Row {
      required property var modelData
      required property int index
      width: root.width
      spacing: Style.space(8)
      Text {
        textFormat: Text.PlainText
        width: parent.width - undoButton.width - Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        elide: Text.ElideRight
        color: modelData.undone ? root.dim : root.foreground
        font.family: Style.font.family
        font.pixelSize: Style.font.bodySmall
        text: modelData.summary + " · " + U.ago(modelData.at, root.svc ? root.svc.now : 0)
          + (modelData.undone ? " (undone)" : "")
      }
      PanelActionButton {
        id: undoButton
        visible: !modelData.undone
        iconText: "󰕌"
        tooltipText: "Undo: put back what this computer had"
        onClicked: root.svc.run("history.undo", { id: modelData.id }, function() {
          root.svc.message("Put back on this computer. Your other computers keep theirs.", false)
        })
      }
    }
  }
}
