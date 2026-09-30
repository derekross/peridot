import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "util.js" as U

// The Gallery: themes, plugins and whole setups from other Omarchy users,
// with likes and reviews from real people, ranked by the ones you follow.
Column {
  id: root
  property var svc: null
  property color foreground: Color.foreground
  property color urgent: Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)

  readonly property var gallery: svc && svc.status.gallery ? svc.status.gallery : ({})
  readonly property bool needsEnable: gallery.needs_enable === true
  property bool enabling: false
  function enable() {
    if (enabling) return
    enabling = true
    svc.message("Approve Peridot in Opal's bar…", false)
    svc.call("gallery.enable", null, function(err) {
      root.enabling = false
      if (err) root.svc.message(err, true)
      else root.reload(false)
    })
  }
  readonly property bool catalogued: (gallery.themes || 0) + (gallery.plugins || 0) > 0
  // A name is needed before the first like, review or setup.
  readonly property bool needsName: gallery.profile === null

  property string section: "theme"
  property string sort: "top"
  property var items: []
  property int total: 0
  property int shown: 0
  property bool loading: false
  property var setups: []
  property var mine: null
  // Candidates you switched off (by url), and whether the theme name goes in.
  property var leftOut: ({})
  property bool includeTheme: true
  function candidateOn(c) { return !!c.url && !leftOut[c.url] }
  function toggleCandidate(c) {
    if (!c.url) return
    var m = Object.assign({}, leftOut)
    if (m[c.url]) delete m[c.url]; else m[c.url] = true
    leftOut = m
  }
  readonly property int chosenCount: {
    var n = 0
    var list = mine && mine.candidates ? mine.candidates : []
    for (var i = 0; i < list.length; i++) if (candidateOn(list[i])) n++
    return n
  }

  property string openReviews: ""
  property var reviews: []
  property int reviewRating: 0
  property string busyUrl: ""

  property string confirmInstall: ""
  property string confirmRemove: ""
  property bool screenshotOn: false
  property bool publishing: false

  // What to do once a name is picked.
  property var pending: null
  property bool askingName: false
  // Clears the review field, which lives inside a row.
  signal clearDraft()

  readonly property int pageSize: 30

  spacing: Style.space(10)

  onVisibleChanged: {
    if (visible) { reload() }
    else { openReviews = ""; confirmInstall = ""; confirmRemove = ""; askingName = false; pending = null }
  }

  Connections {
    target: root.svc
    function onGalleryChanged() { if (root.visible) root.reload(true) }
  }

  Timer {
    id: searchDebounce
    interval: 300
    onTriggered: root.reload()
  }

  function reload(light) {
    if (!svc || !svc.connected) return
    if (section === "setup") {
      svc.call("gallery.setups", { query: search.text.trim() }, function(err, r) {
        if (!err) root.setups = r || []
      })
      if (!light) svc.call("gallery.setup.mine", null, function(err, r) { if (!err) root.mine = r })
      return
    }
    var want = light ? Math.max(root.shown, root.pageSize) : root.pageSize
    if (!light) root.loading = true
    svc.call("gallery.list", { kind: section, query: search.text.trim(), sort: sort, offset: 0, limit: want }, function(err, r) {
      root.loading = false
      if (err) { root.svc.message(err, true); return }
      root.items = r.items || []
      root.total = r.total || 0
      root.shown = root.items.length
    })
    if (light && openReviews !== "") loadReviews(openReviews)
  }

  function more() {
    if (loading) return
    loading = true
    svc.call("gallery.list", { kind: section, query: search.text.trim(), sort: sort, offset: shown, limit: pageSize }, function(err, r) {
      root.loading = false
      if (err) { root.svc.message(err, true); return }
      root.items = root.items.concat(r.items || [])
      root.total = r.total || 0
      root.shown = root.items.length
    })
  }

  function loadReviews(url) {
    svc.call("gallery.reviews", { url: url }, function(err, r) {
      if (!err && root.openReviews === url) root.reviews = r || []
    })
  }

  function toggleReviews(url) {
    if (openReviews === url) { openReviews = ""; reviews = []; return }
    openReviews = url
    reviews = []
    reviewRating = 0
    clearDraft()
    loadReviews(url)
  }

  // Runs `fn` now, or after a name is picked.
  function withName(fn) {
    if (needsName) { pending = fn; askingName = true; return }
    fn()
  }

  function pickName() {
    var n = nameField.text.trim()
    if (n === "") return
    svc.call("profile.set", { name: n }, function(err) {
      if (err) { root.svc.message(err, true); return }
      root.askingName = false
      nameField.text = ""
      var fn = root.pending
      root.pending = null
      if (fn) fn()
    })
  }

  function like(item) {
    withName(function() {
      var on = !item.liked
      svc.run("gallery.like", { url: item.url, on: on }, function() {
        root.svc.message(on ? "Liked " + item.name : "Like taken back", false)
        root.reload(true)
      })
    })
  }

  function likeSetup(s) {
    withName(function() {
      var on = !s.liked
      svc.run("gallery.setup.like", { coordinate: s.coordinate, on: on }, function() {
        root.svc.message(on ? "Liked " + s.title : "Like taken back", false)
        root.reload(true)
      })
    })
  }

  function install(item) {
    if (busyUrl !== "") return
    busyUrl = item.url
    svc.message("Installing " + item.name + "…", false)
    svc.consent(item.kind + ":" + item.url, function(ok) {
      if (!ok) { root.busyUrl = ""; return }
      svc.call("gallery.install", { url: item.url, kind: item.kind, confirm: true }, function(err) {
        root.busyUrl = ""
        if (err) { root.svc.message(err, true); return }
        root.svc.message("Installed " + item.name, false)
        root.reload(true)
      })
    })
  }

  function postReview(url, text) {
    var t = (text || "").trim()
    if (t === "") { svc.message("Write something first", true); return }
    withName(function() {
      var p = { url: url, text: t }
      if (root.reviewRating > 0) p.rating = root.reviewRating
      svc.run("gallery.review", p, function() {
        root.clearDraft()
        root.reviewRating = 0
        root.svc.message("Review posted", false)
        root.loadReviews(url)
        root.reload(true)
      })
    })
  }

  function follow(pubkey, on, who) {
    withName(function() {
      svc.run("gallery.follow", { pubkey: pubkey, on: on }, function() {
        root.svc.message(on ? "Following " + who : "Unfollowed " + who, false)
        root.reload(true)
      })
    })
  }

  // A setup is installed one step at a time, each its own confirmed
  // Omarchy command (plugins are code; Omarchy's own prompt shows too).
  function installSetup(s) {
    if (confirmInstall !== s.coordinate) { confirmInstall = s.coordinate; return }
    confirmInstall = ""
    if (busyUrl !== "") return
    busyUrl = s.coordinate
    svc.call("gallery.setup.steps", { coordinate: s.coordinate }, function(err, steps) {
      if (err) { root.busyUrl = ""; root.svc.message(err, true); return }
      root.runSteps(s, steps || [], 0, 0)
    })
  }

  function runSteps(s, steps, i, failed) {
    if (i >= steps.length) {
      root.busyUrl = ""
      root.svc.message(failed > 0 ? "Done, but " + failed + " step(s) failed" : "Done", failed > 0)
      root.reload(true)
      return
    }
    var step = steps[i]
    var what = step.url || step.name || ""
    root.svc.message("Step " + (i + 1) + " of " + steps.length + ": " + what + "…", false)
    var consent = step.kind === "install_theme" ? "theme:" + step.url
                : step.kind === "install_plugin" ? "plugin:" + step.url
                : "theme-set:" + step.name
    svc.consent(consent, function(ok) {
      if (!ok) { root.runSteps(s, steps, i + 1, failed + 1); return }
      svc.call("gallery.setup.install", { coordinate: s.coordinate, step: step, confirm: true }, function(err) {
        if (err) { root.svc.message(err, true); failed++ }
        root.runSteps(s, steps, i + 1, failed)
      })
    })
  }

  function removeSetup(s) {
    if (confirmRemove !== s.coordinate) { confirmRemove = s.coordinate; return }
    confirmRemove = ""
    svc.run("gallery.setup.remove", { coordinate: s.coordinate }, function() {
      root.svc.message("Removed", false)
      root.reload(true)
    })
  }

  function publish() {
    var t = setupTitle.text.trim()
    if (t === "") { svc.message("Give your setup a title", true); return }
    withName(function() {
      if (root.publishing) return
      root.publishing = true
      var p = { title: t, summary: setupSummary.text.trim() }
      if (root.screenshotOn && root.mine && root.mine.screenshot) p.screenshot = root.mine.screenshot
      var include = []
      var list = root.mine && root.mine.candidates ? root.mine.candidates : []
      for (var i = 0; i < list.length; i++) if (root.candidateOn(list[i])) include.push(list[i].url)
      p.include = include
      p.without_theme = !root.includeTheme
      svc.call("gallery.setup.publish", p, function(err) {
        root.publishing = false
        if (err) { root.svc.message(err, true); return }
        setupTitle.text = ""
        setupSummary.text = ""
        root.screenshotOn = false
        root.svc.message("Your setup is published", false)
        root.reload(true)
      })
    })
  }

  function stars(n) {
    var s = ""
    for (var i = 1; i <= 5; i++) s += i <= Math.round(n) ? "★" : "☆"
    return s
  }

  function shortUrl(u) {
    return (u || "").replace(/^https:\/\/(www\.)?/, "")
  }

  // ── Search and sections ──────────────────────────────────────────
  Text {
    textFormat: Text.PlainText
    width: parent.width
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: "Themes, plugins and whole setups from other Omarchy users. Likes come from real people; the ones you follow count for more."
  }
  // With Opal holding the key, the Gallery's kinds are declared on first
  // use: Opal asks once.
  Column {
    width: parent.width
    visible: root.needsEnable
    spacing: Style.space(6)
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: "Liking, reviewing and publishing here signs as you through Opal. Turn the Gallery on once, and Opal will ask what Peridot may sign."
    }
    Button {
      text: root.enabling ? "Waiting for Opal…" : "Turn on the Gallery"
      iconText: "󰇈"
      iconSpinning: root.enabling
      enabled: !root.enabling
      bordered: true
      foreground: root.foreground
      onClicked: root.enable()
    }
  }

  Row {
    width: parent.width
    spacing: Style.space(8)
    TextField {
      id: search
      width: parent.width - refreshButton.width - Style.space(8)
      placeholderText: root.section === "setup" ? "Search setups" : "Search by name, author or tag"
      foreground: root.foreground
      onTextChanged: searchDebounce.restart()
      onAccepted: root.reload()
    }
    PanelActionButton {
      id: refreshButton
      anchors.verticalCenter: parent.verticalCenter
      iconText: "󰑐"
      tooltipText: "Check for new likes and listings"
      onClicked: root.svc.run("gallery.refresh", null, function(r) {
        root.svc.message(r && r.registry_error ? r.registry_error : "Up to date", !!(r && r.registry_error))
        root.reload()
      })
    }
  }

  ButtonGroup {
    width: parent.width
    options: [
      { value: "theme", label: "Themes" },
      { value: "plugin", label: "Plugins" },
      { value: "setup", label: "Setups" }
    ]
    value: root.section
    foreground: root.foreground
    onChanged: function(v) {
      root.section = v
      root.openReviews = ""
      root.confirmInstall = ""
      root.confirmRemove = ""
      root.reload()
    }
  }

  ButtonGroup {
    width: parent.width
    visible: root.section !== "setup"
    options: [
      { value: "top", label: "Top" },
      { value: "stars", label: "Stars" },
      { value: "name", label: "Name" }
    ]
    value: root.sort
    foreground: root.foreground
    onChanged: function(v) { root.sort = v; root.reload() }
  }

  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: !!root.gallery.error
    wrapMode: Text.Wrap
    color: root.urgent
    font.family: Style.font.family
    font.pixelSize: Style.font.caption
    text: root.gallery.error || ""
  }

  // ── Pick a name (first like, review or setup) ────────────────────
  Column {
    width: parent.width
    visible: root.askingName
    spacing: Style.space(6)
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.foreground
      font.family: Style.font.family
      font.pixelSize: Style.font.body
      text: "Pick a name others will see next to your likes and reviews."
    }
    Row {
      width: parent.width
      spacing: Style.space(8)
      TextField {
        id: nameField
        width: parent.width - nameButton.width - cancelName.width - Style.space(16)
        placeholderText: "Your name"
        foreground: root.foreground
        onAccepted: root.pickName()
      }
      Button {
        id: nameButton
        text: "Use this name"
        iconText: "󰄬"
        bordered: true
        foreground: root.foreground
        onClicked: root.pickName()
      }
      Button {
        id: cancelName
        text: "Not now"
        foreground: root.foreground
        onClicked: { root.askingName = false; root.pending = null }
      }
    }
  }

  // ── Empty states ─────────────────────────────────────────────────
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.section !== "setup" && !root.catalogued
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: "Loading the catalogs…"
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.section !== "setup" && root.catalogued && !root.loading && root.items.length === 0
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: search.text.trim() !== "" ? "Nothing matches that." : "Nothing here yet."
  }

  // ── Themes and plugins ───────────────────────────────────────────
  Repeater {
    model: root.section !== "setup" ? root.items : []
    delegate: Column {
      id: row
      required property var modelData
      width: root.width
      spacing: Style.space(4)
      readonly property bool open: root.openReviews === modelData.url
      readonly property string meta: {
        var parts = []
        if (modelData.author) parts.push(modelData.author)
        if (modelData.category) parts.push(modelData.category)
        parts.push("★ " + (modelData.stars || 0))
        parts.push("♥ " + (modelData.likes || 0))
        if (modelData.rating !== undefined && modelData.rating !== null)
          parts.push(Number(modelData.rating).toFixed(1) + " ☆ · " + modelData.reviews + " review" + (modelData.reviews === 1 ? "" : "s"))
        else if (modelData.reviews > 0)
          parts.push(modelData.reviews + " review" + (modelData.reviews === 1 ? "" : "s"))
        if (modelData.installed) parts.push("Installed")
        return parts.join(" · ")
      }

      Row {
        width: parent.width
        spacing: Style.space(8)
        Column {
          width: parent.width - itemButtons.width - Style.space(8)
          anchors.verticalCenter: parent.verticalCenter
          Text {
            textFormat: Text.PlainText
            width: parent.width
            elide: Text.ElideRight
            color: root.foreground
            font.family: Style.font.family
            font.pixelSize: Style.font.body
            font.bold: true
            text: modelData.name
          }
          Text {
            textFormat: Text.PlainText
            width: parent.width
            elide: Text.ElideRight
            color: root.dim
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            text: row.meta
          }
          Text {
            textFormat: Text.PlainText
            width: parent.width
            visible: !!modelData.description && modelData.description !== ""
            elide: Text.ElideRight
            maximumLineCount: 2
            wrapMode: Text.Wrap
            color: root.dim
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            text: modelData.description || ""
          }
          Text {
            textFormat: Text.PlainText
            width: parent.width
            visible: (modelData.liked_by || []).length > 0
            elide: Text.ElideRight
            color: root.foreground
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            text: "Liked by " + (modelData.liked_by || []).join(", ")
          }
        }
        Row {
          id: itemButtons
          anchors.verticalCenter: parent.verticalCenter
          spacing: Style.space(2)
          PanelActionButton {
            iconText: modelData.liked ? "󰋑" : "󰋕"
            tooltipText: modelData.liked ? "Unlike" : "Like"
            onClicked: root.like(modelData)
          }
          PanelActionButton {
            visible: modelData.installable && !modelData.installed
            iconText: root.busyUrl === modelData.url ? "󰑐" : "󰄬"
            tooltipText: root.busyUrl === modelData.url ? "Installing…" : "Install"
            onClicked: root.install(modelData)
          }
          PanelActionButton {
            iconText: "󰆉"
            tooltipText: row.open ? "Hide reviews" : "Reviews"
            onClicked: root.toggleReviews(modelData.url)
          }
          PanelActionButton {
            iconText: "󰆏"
            tooltipText: "Copy the repository link"
            onClicked: root.svc.copy(modelData.url, "Link copied")
          }
        }
      }

      // ── Reviews ───────────────────────────────────────────────
      Column {
        width: parent.width
        visible: row.open
        spacing: Style.space(6)
        leftPadding: Style.space(8)

        Text {
          textFormat: Text.PlainText
          width: parent.width - Style.space(8)
          visible: root.reviews.length === 0
          color: root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: "No reviews yet. Be the first."
        }
        Repeater {
          model: row.open ? root.reviews : []
          delegate: Row {
            id: reviewRow
            required property var modelData
            width: row.width - Style.space(8)
            spacing: Style.space(8)
            Column {
              width: parent.width - reviewButtons.width - Style.space(8)
              Text {
                textFormat: Text.PlainText
                width: parent.width
                elide: Text.ElideRight
                color: root.dim
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                text: (reviewRow.modelData.mine ? "You" : reviewRow.modelData.author)
                  + (reviewRow.modelData.rating ? " · " + root.stars(reviewRow.modelData.rating) : "")
                  + " · " + U.ago(reviewRow.modelData.created_at, root.svc ? root.svc.now : 0)
                  + (reviewRow.modelData.following ? " · following" : "")
              }
              Text {
                textFormat: Text.PlainText
                width: parent.width
                wrapMode: Text.Wrap
                color: root.foreground
                font.family: Style.font.family
                font.pixelSize: Style.font.bodySmall
                text: reviewRow.modelData.text
              }
            }
            Row {
              id: reviewButtons
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(2)
              PanelActionButton {
                visible: !reviewRow.modelData.mine
                iconText: reviewRow.modelData.following ? "󰄬" : "󰀔"
                tooltipText: reviewRow.modelData.following ? "Unfollow " + reviewRow.modelData.author : "Follow " + reviewRow.modelData.author
                onClicked: root.follow(reviewRow.modelData.pubkey, !reviewRow.modelData.following, reviewRow.modelData.author)
              }
              PanelActionButton {
                visible: reviewRow.modelData.mine
                iconText: "󰆴"
                hoverColor: root.urgent
                tooltipText: "Remove your review"
                onClicked: root.svc.run("gallery.unreview", { id: reviewRow.modelData.id }, function() {
                  root.loadReviews(row.modelData.url)
                  root.reload(true)
                })
              }
            }
          }
        }

        Row {
          spacing: Style.space(2)
          Text {
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            color: root.dim
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            text: "Your rating: "
          }
          Repeater {
            model: 5
            delegate: PanelActionButton {
              required property int index
              iconText: index < root.reviewRating ? "★" : "☆"
              tooltipText: (index + 1) + " of 5"
              onClicked: root.reviewRating = (root.reviewRating === index + 1) ? 0 : index + 1
            }
          }
        }
        Row {
          width: parent.width - Style.space(8)
          spacing: Style.space(8)
          TextField {
            id: reviewText
            width: parent.width - postButton.width - Style.space(8)
            placeholderText: "Write a short review"
            foreground: root.foreground
            onAccepted: root.postReview(row.modelData.url, text)
            Connections {
              target: root
              function onClearDraft() { reviewText.text = "" }
            }
          }
          Button {
            id: postButton
            text: "Post"
            iconText: "󰐕"
            bordered: true
            foreground: root.foreground
            onClicked: root.postReview(row.modelData.url, reviewText.text)
          }
        }
      }
    }
  }

  Button {
    visible: root.section !== "setup" && root.shown < root.total
    text: root.loading ? "Loading…" : "Show more (" + (root.total - root.shown) + " left)"
    iconSpinning: root.loading
    foreground: root.foreground
    onClicked: root.more()
  }

  // ── Setups ───────────────────────────────────────────────────────
  PanelSectionHeader {
    visible: root.section === "setup"
    text: "YOUR SETUP"
    foreground: root.dim
  }
  Column {
    width: parent.width
    visible: root.section === "setup"
    spacing: Style.space(6)
    Text {
      textFormat: Text.PlainText
      width: parent.width
      wrapMode: Text.Wrap
      color: root.dim
      font.family: Style.font.family
      font.pixelSize: Style.font.bodySmall
      text: {
        var m = root.mine
        if (!m) return "Looking at this computer…"
        if (!m.can_publish) return "Nothing to publish yet: install a theme or plugin from a repository first."
        return "Share what this computer runs. Tick what goes in; others can install it in one go. Only repository addresses are shared, never files."
      }
    }
    Toggle {
      width: parent.width
      visible: !!root.mine && root.mine.can_publish && !!root.mine.theme
      label: "The " + (root.mine ? root.mine.theme : "") + " theme"
      description: "The theme in use"
      checked: root.includeTheme
      foreground: root.foreground
      onClicked: root.includeTheme = !root.includeTheme
    }
    Repeater {
      model: root.mine && root.mine.can_publish ? (root.mine.candidates || []) : []
      delegate: Toggle {
        required property var modelData
        width: parent ? parent.width : 0
        enabled: !!modelData.url
        label: modelData.title + (modelData.kind === "theme" ? " theme" : "")
        description: !modelData.url ? "No public source, so it can't be shared"
          : modelData.how === "linked" ? modelData.url + " · a linked checkout (one you develop?)"
          : modelData.how === "catalogue" ? modelData.url + " · from the catalog"
          : modelData.url
        checked: root.candidateOn(modelData)
        foreground: root.foreground
        onClicked: root.toggleCandidate(modelData)
      }
    }
    TextField {
      id: setupTitle
      width: parent.width
      visible: !!root.mine && root.mine.can_publish
      placeholderText: "A title, like \"Quiet desk\""
      foreground: root.foreground
    }
    TextField {
      id: setupSummary
      width: parent.width
      visible: !!root.mine && root.mine.can_publish
      placeholderText: "A line about it (optional)"
      foreground: root.foreground
      onAccepted: root.publish()
    }
    Toggle {
      width: parent.width
      visible: !!root.mine && root.mine.can_publish && !!root.mine.screenshot
      label: "Include my latest screenshot (public)"
      description: root.mine && root.mine.screenshot ? "Anyone can see it: " + root.mine.screenshot.split("/").pop() : ""
      checked: root.screenshotOn
      foreground: root.foreground
      onClicked: root.screenshotOn = !root.screenshotOn
    }
    Button {
      visible: !!root.mine && root.mine.can_publish
      text: root.publishing ? "Publishing…" : (root.chosenCount + (root.includeTheme && root.mine && root.mine.theme ? 1 : 0)) === 0 ? "Nothing chosen" : "Publish"
      enabled: !root.publishing && (root.chosenCount + (root.includeTheme && root.mine && root.mine.theme ? 1 : 0)) > 0
      iconText: "󰐕"
      iconSpinning: root.publishing
      bordered: true
      foreground: root.foreground
      onClicked: root.publish()
    }
  }

  PanelSectionHeader {
    visible: root.section === "setup"
    text: "SETUPS"
    foreground: root.dim
  }
  Text {
    textFormat: Text.PlainText
    width: parent.width
    visible: root.section === "setup" && root.setups.length === 0
    wrapMode: Text.Wrap
    color: root.dim
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
    text: search.text.trim() !== "" ? "Nothing matches that." : "No setups published yet. Yours could be the first."
  }
  Repeater {
    model: root.section === "setup" ? root.setups : []
    delegate: Row {
      id: setupRow
      required property var modelData
      width: root.width
      spacing: Style.space(8)
      Column {
        width: parent.width - setupButtons.width - Style.space(8)
        anchors.verticalCenter: parent.verticalCenter
        Text {
          textFormat: Text.PlainText
          width: parent.width
          elide: Text.ElideRight
          color: root.foreground
          font.family: Style.font.family
          font.pixelSize: Style.font.body
          font.bold: true
          text: modelData.title
        }
        Text {
          textFormat: Text.PlainText
          width: parent.width
          elide: Text.ElideRight
          color: root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: {
            var parts = [modelData.mine ? "You" : modelData.author]
            if (modelData.theme) parts.push(modelData.theme + " theme")
            parts.push("♥ " + modelData.likes)
            parts.push("Installed " + modelData.installed + " of " + modelData.total)
            if (modelData.following) parts.push("following")
            parts.push(U.ago(modelData.created_at, root.svc ? root.svc.now : 0))
            return parts.join(" · ")
          }
        }
        Text {
          textFormat: Text.PlainText
          width: parent.width
          visible: !!modelData.summary && modelData.summary !== ""
          wrapMode: Text.Wrap
          maximumLineCount: 2
          elide: Text.ElideRight
          color: root.dim
          font.family: Style.font.family
          font.pixelSize: Style.font.caption
          text: modelData.summary || ""
        }
      }
      Row {
        id: setupButtons
        anchors.verticalCenter: parent.verticalCenter
        spacing: Style.space(2)
        PanelActionButton {
          visible: !modelData.mine
          iconText: modelData.liked ? "󰋑" : "󰋕"
          tooltipText: modelData.liked ? "Unlike" : "Like"
          onClicked: root.likeSetup(modelData)
        }
        PanelActionButton {
          visible: modelData.installed < modelData.total || !!modelData.theme
          iconText: root.busyUrl === modelData.coordinate ? "󰑐" : "󰄬"
          tooltipText: root.busyUrl === modelData.coordinate ? "Installing…"
            : root.confirmInstall === modelData.coordinate ? "Click again: installs everything and switches the theme"
            : "Install everything"
          onClicked: root.installSetup(modelData)
        }
        PanelActionButton {
          visible: !modelData.mine
          iconText: modelData.following ? "󰄬" : "󰀔"
          tooltipText: modelData.following ? "Unfollow " + modelData.author : "Follow " + modelData.author
          onClicked: root.follow(modelData.pubkey, !modelData.following, modelData.author)
        }
        PanelActionButton {
          visible: !!modelData.image
          iconText: "󰋩"
          tooltipText: "Copy the screenshot link"
          onClicked: root.svc.copy(modelData.image, "Link copied")
        }
        PanelActionButton {
          visible: modelData.mine
          iconText: "󰆴"
          hoverColor: root.urgent
          tooltipText: root.confirmRemove === modelData.coordinate ? "Click again to take it down" : "Remove"
          onClicked: root.removeSetup(modelData)
        }
      }
    }
  }
}
