.pragma library

function ago(secs, nowMs) {
  if (!secs) return "never"
  var d = Math.max(0, Math.floor((nowMs || Date.now()) / 1000) - secs)
  if (d < 60) return "just now"
  if (d < 3600) return Math.floor(d / 60) + "m ago"
  if (d < 86400) return Math.floor(d / 3600) + "h ago"
  if (d < 86400 * 30) return Math.floor(d / 86400) + "d ago"
  return new Date(secs * 1000).toLocaleDateString()
}

// Friendly names for what syncs.
var names = [
  [".config/hypr/bindings.lua", "Keyboard shortcuts"],
  [".config/hypr/looknfeel.lua", "Look and feel"],
  [".config/hypr/hyprland.lua", "Window manager settings"],
  [".config/hypr/autostart.lua", "Apps that start at login"],
  [".config/hypr/hyprsunset.conf", "Night light"],
  [".config/omarchy/shell.json", "Bar layout"],
  [".config/omarchy/extensions/", "Menu additions"],
  [".config/omarchy/branding/", "Branding"],
  [".config/omarchy/themed/", "Theme templates"],
  [".config/omarchy/hooks/", "Omarchy hooks"],
  [".config/alacritty/", "Alacritty terminal"],
  [".config/ghostty/", "Ghostty terminal"],
  [".config/kitty/", "Kitty terminal"],
  [".config/foot/", "Foot terminal"],
  [".config/btop/", "btop"],
  [".config/starship.toml", "Shell prompt"],
  [".config/tmux/", "tmux"],
  [".config/lazygit/", "lazygit"],
  [".config/mpv/", "mpv"],
  [".config/nvim/", "Neovim"],
  [".config/git/config", "Git settings"],
  [".config/mimeapps.list", "Default apps"],
  [".bashrc", "Shell startup"],
  [".XCompose", "Compose key"]
]

function label(path) {
  for (var i = 0; i < names.length; i++) {
    var p = names[i][0]
    if (path === p || (p.charAt(p.length - 1) === "/" && path.indexOf(p) === 0)) return names[i][1]
  }
  return path
}

// The part of the path worth showing under the friendly name.
function detail(path) {
  var home = "~/" + path
  return home
}

// What syncs only when you turn it on (these can run commands).
var optIn = [
  { pattern: ".config/hypr/autostart.lua", label: "Apps that start at login" },
  { pattern: ".bashrc", label: "Shell startup (.bashrc)" },
  { pattern: ".config/omarchy/hooks/**", label: "Omarchy hooks" },
  { pattern: ".config/nvim/**", label: "Neovim" },
  { pattern: ".config/git/config", label: "Git settings" },
  { pattern: ".config/mimeapps.list", label: "Default apps" }
]

function skipReason(s) {
  switch (s.why) {
  case "linked": return "managed by a dotfile tool (a link)"
  case "secret": return "looks like it holds " + (s.what || "a secret")
  case "binary": return "not a text file"
  case "too_big": return "too big"
  default: return s.error || "can't be read"
  }
}
