import QtQuick
import Quickshell
import Quickshell.Io

// One connection to peridotd for the whole shell. The bar widget (one per
// monitor) reads state from here and sends requests through call().
Item {
  id: root

  // Injected by omarchy-shell.
  property var shell: null
  property var manifest: null

  readonly property string pluginId: (manifest && manifest.id) || "derekross.peridot"
  readonly property string socketPath: Quickshell.env("XDG_RUNTIME_DIR") + "/peridot.sock"

  // Our own record of the link: Socket.connected is also the *requested*
  // state, so it can read true after a failed attempt.
  property bool linked: false
  readonly property bool connected: linked

  // Daemon state (see peridotd `status`).
  property var status: ({})
  readonly property bool setUp: status.set_up === true
  readonly property var counts: status.counts || ({})
  readonly property var files: status.files || []
  readonly property var offers: status.offers || []
  readonly property var devices: status.devices || []
  readonly property var history: status.history || []
  readonly property var pairing: status.pairing || null
  readonly property bool paused: status.paused === true
  // The pairing with Opal, when Opal holds the identity (see peridotd `opal`).
  readonly property var opal: status.opal || null
  // Things that want you: changes to apply, conflicts, offers, a pairing
  // waiting for your answer, Opal needing to pair again.
  readonly property int attention: (counts.incoming || 0) + (counts.conflicts || 0) + (counts.offers || 0)
    + (pairing && pairing.stage === "confirm" ? 1 : 0)
    + (opal && opal.needs_pairing ? 1 : 0)

  signal message(string text, bool isError)
  signal openRequested()

  property int _nextId: 1
  property var _callbacks: ({})

  function call(method, params, done) {
    var sock = root.sock
    if (!root.linked || !sock) {
      if (done) done("Peridot isn't running", null)
      return
    }
    var id = _nextId++
    if (done) _callbacks[id] = done
    sock.write(JSON.stringify({ id: id, method: method, params: params === undefined ? null : params }) + "\n")
    sock.flush()
  }

  // call() with a toast on error and an optional success callback.
  function run(method, params, onOk) {
    call(method, params, function(err, result) {
      if (err) root.message(err, true)
      else if (onOk) onOk(result)
    })
  }

  function startDaemon() {
    Quickshell.execDetached(["systemctl", "--user", "start", "peridot.service"])
    reconnectNow()
  }

  function copy(text, what) {
    Quickshell.execDetached(["wl-copy", "--", text])
    message((what || "Copied") + " to the clipboard", false)
  }

  function handle(msg) {
    if (msg.id !== undefined && msg.id !== null) {
      var cb = _callbacks[msg.id]
      if (cb) {
        delete _callbacks[msg.id]
        cb(msg.error || null, msg.result)
      }
      return
    }
    if (msg.event === "state") status = msg.data || {}
  }

  // Quickshell's Socket won't retry after a failed attempt, so each attempt
  // gets a fresh Socket, built from a string so it still works after the
  // plugin is updated (this service stays loaded; its component cache
  // doesn't).
  property var sock: null
  readonly property string sockQml: "import QtQuick; import Quickshell.Io; Socket {"
    + " property var owner: null;"
    + " parser: SplitParser { onRead: function(line) { if (owner) owner.onSocketLine(line) } }"
    + " onConnectedChanged: if (owner) owner.onSocketConnected(connected);"
    + " onError: if (owner) owner.onSocketConnected(false) }"

  function onSocketLine(line) {
    var msg
    try { msg = JSON.parse(line) } catch (e) { return }
    root.handle(msg)
  }

  function onSocketConnected(up) {
    if (up === root.linked) return
    root.linked = up
    if (up) {
      root._callbacks = ({})
      root.call("subscribe", null, function(err, s) { if (!err) root.status = s || {} })
    } else {
      root.status = ({})
    }
  }

  Timer {
    interval: 2000
    running: !root.linked
    repeat: true
    triggeredOnStart: true
    onTriggered: root.reconnectNow()
  }

  function reconnectNow() {
    if (root.linked) return
    if (root.sock) {
      root.sock.owner = null
      root.sock.destroy()
      root.sock = null
    }
    try {
      var s = Qt.createQmlObject(root.sockQml, root, "PeridotSocket")
      s.owner = root
      s.path = root.socketPath
      root.sock = s
      s.connected = true
    } catch (e) {
      console.warn("peridot: could not create a socket: " + e)
    }
  }

  // "Last synced 3m ago" stays fresh.
  property double now: Date.now()
  Timer {
    interval: 30000
    running: true
    repeat: true
    onTriggered: root.now = Date.now()
  }

  // `omarchy-shell derekross.peridot <fn>`; notifications open the panel.
  IpcHandler {
    target: "derekross.peridot"
    function open(): string { root.openRequested(); return "ok" }
    function sync(): string { root.call("sync.now", null); return "ok" }
    function waiting(): string { return String(root.attention) }
  }
}
