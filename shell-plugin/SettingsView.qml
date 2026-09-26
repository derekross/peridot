import QtQuick
import qs.Commons
import qs.Ui
import "util.js" as U

// What syncs, the recovery kit, and how syncing behaves here.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  readonly property var choices: svc && svc.status.choices ? svc.status.choices : ({ enabled: [], excluded: [] })
  readonly property var syncing: svc ? svc.files.filter(function(f) { return f.status !== "conflict" }) : []
  readonly property var skipped: svc ? (svc.status.skipped || []) : []

  property var kit: null
  property bool making: false
  property string kitPath: ""
  property bool confirmLeave: false
  readonly property var relays: svc && svc.status.relays ? svc.status.relays : []
  readonly property bool viaOpal: !!svc && !!svc.status.identity && svc.status.identity.mode === "opal"
  property bool checkingRelay: false
  property string relayError: ""
  property string confirmRelayRemove: ""

  function addRelay() {
    var url = newRelay.text.trim().replace(/\/+$/, "")
    if (url === "") return
    if (url.indexOf("wss://") !== 0) { relayError = "Relay addresses start with wss://"; return }
    if (checkingRelay) return
    relayError = ""
    checkingRelay = true
    svc.call("relays.check", { url: url }, function(err, r) {
      if (err || !r || !r.reachable) {
        root.checkingRelay = false
        root.relayError = err || ("Couldn't connect to " + url)
        return
      }
      var list = root.relays.slice()
      if (list.indexOf(url) === -1) list.push(url)
      svc.call("relays.set", { relays: list }, function(err2) {
        root.checkingRelay = false
        if (err2) { root.relayError = err2; return }
        newRelay.text = ""
        root.svc.message("Added " + url, false)
      })
    })
  }

  function removeRelay(url) {
    if (confirmRelayRemove !== url) { confirmRelayRemove = url; return }
    confirmRelayRemove = ""
    var list = root.relays.filter(function(r) { return r !== url })
    svc.run("relays.set", { relays: list })
  }

  spacing: Style.space(10)

  onVisibleChanged: if (!visible) { kit = null; kitPath = ""; confirmLeave = false }

  function setOptIn(pattern, on) {
    var enabled = (choices.enabled || []).filter(function(p) { return p !== pattern })
    if (on) enabled.push(pattern)
    svc.run("items.set", { enabled: enabled, excluded: choices.excluded || [] })
  }

  // ── What syncs ─────────────────────────────────────────────────
  PanelSectionHeader { text: "SYNCING"; foreground: root.dim }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.foreground
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: root.syncing.length > 0
      ? root.syncing.map(function(f) { return U.label(f.path) })
          .filter(function(v, i, a) { return a.indexOf(v) === i }).join(", ")
      : "Nothing yet."
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Never synced: passwords, keys, tokens, browser data, monitor layout and input devices."
  }

  PanelSectionHeader { text: "ALSO SYNC (THESE CAN RUN COMMANDS)"; foreground: root.dim }
  Repeater {
    model: U.optIn
    delegate: Toggle {
      required property var modelData
      width: root.width
      label: modelData.label
      checked: (root.choices.enabled || []).indexOf(modelData.pattern) !== -1
      foreground: root.foreground
      onClicked: root.setOptIn(modelData.pattern, !checked)
    }
  }

  PanelSectionHeader {
    visible: root.skipped.length > 0
    text: "LEFT OUT"
    foreground: root.dim
  }
  Repeater {
    model: root.skipped
    delegate: Text {
      required property var modelData
      textFormat: Text.PlainText
      width: root.width
      elide: Text.ElideMiddle
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: "~/" + modelData.path + ": " + U.skipReason(modelData)
    }
  }

  // ── Behaviour ──────────────────────────────────────────────────
  PanelSectionHeader { text: "ON THIS COMPUTER"; foreground: root.dim }
  Toggle {
    width: parent.width
    label: "Apply changes automatically"
    description: "Settings from your other computers apply without asking (never ones that can run commands)"
    checked: !!root.svc && root.svc.status.auto_apply === true
    foreground: root.foreground
    onClicked: root.svc.run("sync.auto_apply", { on: !checked })
  }
  Toggle {
    width: parent.width
    label: "Pause"
    description: "Changes here aren't sent until you resume"
    checked: !!root.svc && root.svc.paused
    foreground: root.foreground
    onClicked: root.svc.run("sync.pause", { paused: !checked })
  }

  // ── Servers ────────────────────────────────────────────────────
  PanelSectionHeader { text: "SERVERS"; foreground: root.dim }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Your encrypted settings are stored on each of these. Add your own if you run one."
  }
  Repeater {
    model: root.relays
    delegate: Row {
      required property var modelData
      width: root.width
      spacing: Style.space(8)
      Text {
        textFormat: Text.PlainText
        width: parent.width - relayRemove.width - Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        elide: Text.ElideMiddle
        color: root.foreground
        font.family: Style.font.family
        font.pixelSize: Style.font.bodySmall
        text: modelData
      }
      PanelActionButton {
        id: relayRemove
        iconText: "󰆴"
        hoverColor: root.urgent
        tooltipText: root.confirmRelayRemove === modelData ? "Click again to remove" : "Remove"
        onClicked: root.removeRelay(modelData)
      }
    }
  }
  Row {
    width: parent.width
    spacing: Style.space(8)
    TextField {
      id: newRelay
      width: parent.width - addRelayButton.width - Style.space(8)
      placeholderText: "wss://relay.example.com"
      foreground: root.foreground
      onAccepted: root.addRelay()
    }
    Button {
      id: addRelayButton
      text: root.checkingRelay ? "Checking…" : "Add"
      iconText: "󰐕"
      iconSpinning: root.checkingRelay
      bordered: true
      foreground: root.foreground
      onClicked: root.addRelay()
    }
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.relayError !== ""
    wrapMode: Text.Wrap
    color: root.urgent
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: root.relayError
  }

  // ── Recovery kit ───────────────────────────────────────────────
  PanelSectionHeader { text: "RECOVERY KIT"; foreground: root.dim }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    visible: !root.kit
    text: root.viaOpal
      ? "Opal holds your key, so your Opal backup (the ncryptsec you can copy in Opal's Profiles) is your recovery kit. On a new computer, restore it in Opal, then choose \"Use your Opal identity\" here. Peridot is listed under Apps in Opal, where you can see what it signed, change what it may do, or revoke it."
      : "If you ever lose every computer, a recovery kit brings your settings back: a page to save or print, and six words to write down."
  }
  Button {
    visible: !root.kit && !root.viaOpal
    text: root.making ? "Making your kit…" : "Make a recovery kit"
    iconText: "󰁯"
    iconSpinning: root.making
    bordered: true
    foreground: root.foreground
    onClicked: {
      if (root.making) return
      root.making = true
      root.svc.call("recovery.create", null, function(err, r) {
        if (err) { root.making = false; root.svc.message(err, true); return }
        root.kit = r
        root.svc.call("recovery.save_page", { code: r.code }, function(err2, saved) {
          root.making = false
          if (!err2) root.kitPath = saved.path
        })
      })
    }
  }
  Column {
    width: parent.width
    spacing: Style.space(6)
    visible: !!root.kit
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: "Write down these six words. They're shown only now:"
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      horizontalAlignment: Text.AlignHCenter
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.subtitle
      font.bold: true
      text: root.kit ? root.kit.words : ""
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: root.kitPath !== ""
        ? "Your recovery page is saved at " + root.kitPath + ". Print it or keep a copy somewhere other than this computer. The words aren't on it: you need both."
        : "Saving your recovery page…"
    }
    Button {
      text: "I've written them down"
      iconText: "󰄬"
      bordered: true
      foreground: root.foreground
      onClicked: { root.kit = null }
    }
  }

  // ── Leave ──────────────────────────────────────────────────────
  PanelSectionHeader { text: "STOP SYNCING"; foreground: root.dim }
  Button {
    text: root.confirmLeave ? "Click again to stop syncing on this computer" : "Stop syncing here"
    iconText: "󰅖"
    foreground: root.confirmLeave ? root.urgent : root.foreground
    onClicked: {
      if (!root.confirmLeave) { root.confirmLeave = true; return }
      root.confirmLeave = false
      root.svc.run("setup.leave", null)
    }
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Your settings here stay as they are, and your other computers keep syncing."
  }
}
