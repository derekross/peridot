import QtQuick
import Quickshell
import Quickshell.Io
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
  // Your identity: the card (npub, QR) and the move into Opal.
  property var card: null
  property bool showQr: false
  property var move: null          // {code, words} while the move card is open
  property string moveState: ""    // "", "waiting", "others", "found", "finishing"
  property bool finishing: false
  readonly property var identity: svc && svc.status.identity ? svc.status.identity : ({})
  readonly property bool canMove: !viaOpal && identity.opal_installed === true

  function loadCard() {
    if (!svc || !svc.connected) return
    svc.call("identity.card", null, function(err, r) { if (!err) root.card = r })
  }

  // Secrets go to the clipboard over stdin, never argv (argv is readable
  // by every process of this user).
  Process {
    id: copier
    property string pending: ""
    command: ["/usr/bin/wl-copy"]
    stdinEnabled: true
    function copyText(text) {
      running = false
      pending = text
      stdinEnabled = true
      running = true
    }
    onStarted: {
      write(pending)
      pending = ""
      stdinEnabled = false   // closes stdin, so wl-copy sees the end
    }
  }

  function openProfile() {
    if (!card) return
    var link = "https://njump.me/" + card.nprofile
    Quickshell.execDetached(["/usr/bin/xdg-open", link])
    svc.message("Opening your profile in the browser", false)
  }

  function startMove() {
    svc.call("identity.move.start", null, function(err, r) {
      if (err) { root.svc.message(err, true); return }
      root.move = r
      root.moveState = "waiting"
      copier.copyText(r.code)
      root.svc.message("The code is on your clipboard", false)
      root.checkMove()
    })
  }

  function checkMove() {
    if (!svc || !svc.connected || root.moveState === "" || root.moveState === "finishing") return
    svc.call("opal.accounts", null, function(err, list) {
      if (err || root.moveState === "" || root.moveState === "finishing") return
      var mine = root.identity.pubkey
      var found = false
      for (var i = 0; i < (list || []).length; i++) if (list[i].pubkey === mine) found = true
      root.moveState = found ? "found" : ((list || []).length > 0 ? "others" : "waiting")
    })
  }

  function finishMove() {
    if (root.finishing) return
    root.finishing = true
    root.moveState = "finishing"
    svc.call("identity.move.finish", null, function(err, r) {
      root.finishing = false
      if (err) { root.moveState = "found"; root.svc.message(err, true); return }
      Quickshell.execDetached(["/usr/bin/wl-copy", "--clear"])
      root.move = null
      root.moveState = ""
      root.svc.message("Opal now holds your identity", false)
      root.loadCard()
    })
  }

  function cancelMove() {
    root.move = null
    root.moveState = ""
    Quickshell.execDetached(["/usr/bin/wl-copy", "--clear"])
  }

  Timer {
    interval: 3000
    running: root.visible && root.moveState !== "" && root.moveState !== "finishing"
    repeat: true
    onTriggered: root.checkMove()
  }

  property bool confirmLeave: false
  readonly property var relays: svc && svc.status.relays ? svc.status.relays : []
  readonly property bool viaOpal: !!svc && !!svc.status.identity && svc.status.identity.mode === "opal"
  property bool checkingRelay: false
  property string relayError: ""
  property string confirmRelayRemove: ""
  property bool tidying: false

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

  onVisibleChanged: {
    if (!visible) { kit = null; kitPath = ""; confirmLeave = false; showQr = false; cancelMove() }
    else loadCard()
  }

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
      // What the daily check found for this server.
      readonly property var health: {
        var list = root.svc && root.svc.status.servers ? (root.svc.status.servers.relays || []) : []
        for (var i = 0; i < list.length; i++) if (list[i].url === modelData) return list[i]
        return null
      }
      Column {
        width: parent.width - relayRemove.width - Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        Text {
          textFormat: Text.PlainText
          width: parent.width
          elide: Text.ElideMiddle
          color: root.foreground
          font.family: Style.font.family
          font.pixelSize: Style.font.bodySmall
          text: modelData
        }
        Text {
          textFormat: Text.PlainText
          width: parent.width
          visible: !!parent.parent.health
          elide: Text.ElideRight
          color: parent.parent.health && (!parent.parent.health.reachable || parent.parent.health.missing > 0) ? root.urgent : root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: {
            var h = parent.parent.health
            if (!h) return ""
            if (!h.reachable) return "Couldn't be reached at the last check"
            if (h.missing > 0) return "Was missing " + h.missing + " item(s); sent again"
            return "Complete · " + h.items + " items"
          }
        }
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
    spacing: Style.space(8)
    visible: !!root.svc && !!root.svc.status.servers
    Text {
      textFormat: Text.PlainText
      anchors.verticalCenter: parent.verticalCenter
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: {
        var a = root.svc && root.svc.status.servers ? root.svc.status.servers : null
        if (!a) return ""
        var s = "Checked " + U.ago(a.at, root.svc.now)
        if (a.stale_chunks > 0) s += " · " + a.stale_chunks + " old piece(s) to remove"
        return s
      }
    }
    Button {
      text: root.tidying ? "Checking…" : "Check now"
      iconText: "󰓦"
      iconSpinning: root.tidying
      enabled: !root.tidying
      foreground: root.foreground
      onClicked: {
        root.tidying = true
        root.svc.call("servers.audit", null, function(err, r) {
          root.tidying = false
          if (err) root.svc.message(err, true)
          else root.svc.message("Servers checked: " + r.resent + " sent again, " + r.refreshed + " refreshed, " + r.removed_chunks + " old piece(s) removed", false)
        })
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

  // ── Your identity ──────────────────────────────────────────────
  PanelSectionHeader { text: "YOUR IDENTITY"; foreground: root.dim }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: "Your public address on Nostr. People can follow you with it, and other Nostr apps show the same name, likes and follows. Sharing it ties your Gallery name to it."
  }
  Row {
    width: parent.width
    spacing: Style.space(8)
    Column {
      width: parent.width - identityButtons.width - Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      Text {
        textFormat: Text.PlainText
        width: parent.width
        elide: Text.ElideRight
        color: root.foreground
        font.family: Style.font.family
        font.pixelSize: Style.font.body
        text: root.card && root.card.name ? root.card.name : (root.viaOpal ? "Held by Opal" : "This computer holds the key")
      }
      Text {
        textFormat: Text.PlainText
        width: parent.width
        elide: Text.ElideMiddle
        color: root.dim
        font.family: Style.font.family
        font.pixelSize: Style.font.caption
        text: root.card ? root.card.npub : (root.identity.npub || "")
      }
    }
    Row {
      id: identityButtons
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(2)
      PanelActionButton {
        iconText: "󰆏"
        tooltipText: "Copy your npub"
        onClicked: root.svc.copy(root.card ? root.card.npub : (root.identity.npub || ""), "Your npub")
      }
      PanelActionButton {
        iconText: "󰐲"
        tooltipText: root.showQr ? "Hide the QR code" : "Show as a QR code (scan it with a phone app)"
        onClicked: root.showQr = !root.showQr
      }
      PanelActionButton {
        iconText: "󰏌"
        tooltipText: root.card && root.card.has_profile ? "Open your profile in the browser" : "Pick a name in the Gallery first, then open your profile"
        onClicked: root.openProfile()
      }
    }
  }
  Image {
    visible: root.showQr && !!root.card
    anchors.horizontalCenter: parent.horizontalCenter
    width: Style.space(160)
    height: width
    fillMode: Image.PreserveAspectFit
    smooth: false
    source: root.showQr && root.card ? root.card.qr : ""
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.canMove && !root.move
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: "Opal is installed. Move your key into it and Opal holds it from now on: it keeps signing for Peridot, and you can use the same identity in other Nostr apps."
  }
  Button {
    visible: root.canMove && !root.move
    text: "Move it into Opal"
    iconText: "󰇈"
    bordered: true
    foreground: root.foreground
    onClicked: root.startMove()
  }
  Column {
    width: parent.width
    spacing: Style.space(6)
    visible: !!root.move
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: "Your key is on the clipboard as a one-time code. In Opal: Profiles → Add account → paste it. When Opal asks for the code's password, type these six words exactly, dashes included:"
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
      text: root.move ? root.move.words : ""
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.moveState === "found" ? root.foreground : root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: {
        switch (root.moveState) {
        case "found": return "Opal has your key. Finish the move: Peridot pairs with Opal (approve it in the bar) and forgets the key here. Your other computers keep their own copy."
        case "others": return "Opal has accounts, but not this one yet. Waiting…"
        case "finishing": return "Approve Peridot in Opal's bar…"
        default: return "Waiting for Opal to have it… Nothing changes until you finish."
        }
      }
    }
    Row {
      spacing: Style.space(8)
      Button {
        visible: root.moveState === "found" || root.moveState === "finishing"
        text: root.finishing ? "Finishing…" : "Finish"
        iconText: "󰄬"
        iconSpinning: root.finishing
        enabled: !root.finishing
        bordered: true
        foreground: root.foreground
        onClicked: root.finishMove()
      }
      Button {
        visible: !root.finishing
        text: "Copy the code again"
        iconText: "󰆏"
        foreground: root.foreground
        onClicked: { copier.copyText(root.move.code); root.svc.message("Copied", false) }
      }
      Button {
        visible: !root.finishing
        text: "Cancel"
        iconText: "󰅖"
        foreground: root.dim
        onClicked: root.cancelMove()
      }
    }
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
      ? "Opal holds your key, so your Opal backup (the ncryptsec you can copy in Opal's Profiles) is your recovery kit. On a new computer, restore it in Opal, then choose \"Use your Opal identity\" here. Your other computers keep their own copy of the key. Peridot is listed under Apps in Opal, where you can see what it signed, change what it may do, or revoke it."
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
