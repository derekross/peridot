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
  readonly property var opalAccounts: svc && svc.status.opal_accounts ? svc.status.opal_accounts : []
  property string opalPick: ""
  readonly property var opalChosen: {
    for (var i = 0; i < opalAccounts.length; i++) if (opalAccounts[i].pubkey === opalPick) return opalAccounts[i]
    for (var j = 0; j < opalAccounts.length; j++) if (opalAccounts[j].current) return opalAccounts[j]
    return opalAccounts.length > 0 ? opalAccounts[0] : null
  }

  spacing: Style.space(10)

  onVisibleChanged: if (!visible) { code.text = ""; words.text = ""; secret.text = ""; secretPass.text = ""; error = ""; mode = "" }

  function start(method, params) {
    if (busy) return
    error = ""
    busy = true
    svc.call(method, params, function(err) {
      root.busy = false
      if (err) root.error = err
      else { secret.text = ""; secretPass.text = "" }
    })
  }

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

  // Opal is installed and has a key: the natural identity to use.
  Column {
    width: parent.width
    spacing: Style.space(6)
    visible: root.mode === "" && root.opalAccounts.length > 0
    PanelSectionHeader { text: "YOUR OPAL IDENTITY"; foreground: root.dim }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.caption
      text: "Your key stays in Opal. Opal will ask you to approve Peridot; tick \"Application data\" there so syncing doesn't ask every time. Relay logins and share uploads may still ask. Syncing out pauses while Opal is locked."
    }
    Text {
      textFormat: Text.PlainText
      width: parent.width
      visible: root.busy && !!root.svc && !!root.svc.opal && root.svc.opal.waiting_approval
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.bodySmall
      text: "Approve Peridot in Opal's bar (top of the screen)…"
    }
    ButtonGroup {
      width: parent.width
      visible: root.opalAccounts.length > 1
      options: root.opalAccounts.map(function(a) { return { value: a.pubkey, label: a.label } })
      value: root.opalChosen ? root.opalChosen.pubkey : ""
      foreground: root.foreground
      onChanged: function(v) { root.opalPick = v }
    }
    Button {
      width: parent.width
      leftAlign: true
      bordered: true
      iconText: "󰇈"
      text: "Use " + (root.opalChosen ? root.opalChosen.label : "your Opal identity")
      foreground: root.foreground
      iconSpinning: root.busy
      onClicked: root.start("setup.use_opal", { pubkey: root.opalChosen ? root.opalChosen.pubkey : null })
    }
  }

  Column {
    width: parent.width
    spacing: Style.space(6)
    visible: root.mode === ""

    PanelSectionHeader {
      visible: root.opalAccounts.length > 0
      text: "OR"
      foreground: root.dim
    }
    Button {
      width: parent.width
      leftAlign: true
      bordered: true
      iconText: "󰐕"
      text: "Start fresh with a new identity"
      tooltipText: "Makes a new key just for you"
      foreground: root.foreground
      iconSpinning: root.busy
      onClicked: root.start("setup.start_fresh", null)
    }
    Button {
      width: parent.width
      leftAlign: true
      bordered: true
      iconText: "󰌆"
      text: "Use a key I already have"
      tooltipText: "An nsec or ncryptsec from another Nostr app"
      foreground: root.foreground
      onClicked: { root.mode = "import"; root.error = "" }
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
    visible: root.mode === "import"

    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.bodySmall
      text: "Paste your key. It's kept in this computer's keyring, protected by your login. If the same key has used Peridot before, its settings are picked up."
    }
    TextField {
      id: secret
      width: parent.width
      password: true
      placeholderText: "nsec, ncryptsec or recovery phrase"
      foreground: root.foreground
      onAccepted: importButton.clicked()
    }
    TextField {
      id: secretPass
      width: parent.width
      visible: secret.text.trim().indexOf("ncryptsec1") === 0
      password: true
      placeholderText: "Password of that ncryptsec"
      foreground: root.foreground
      onAccepted: importButton.clicked()
    }
    Row {
      spacing: Style.space(8)
      Button {
        id: importButton
        text: root.busy ? "Setting up…" : "Use this key"
        iconText: "󰌆"
        iconSpinning: root.busy
        bordered: true
        foreground: root.foreground
        onClicked: {
          if (secret.text.trim() === "") { root.error = "Paste your key first."; return }
          var p = { secret: secret.text.trim() }
          if (secretPass.visible) p.password = secretPass.text
          root.start("setup.import", p)
        }
      }
      Button {
        text: "Back"
        foreground: root.foreground
        onClicked: { root.mode = ""; root.error = ""; secret.text = ""; secretPass.text = "" }
      }
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
