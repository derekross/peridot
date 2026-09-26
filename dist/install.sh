#!/usr/bin/env bash
# Install Peridot for the current user: the peridotd service and peridot
# command (~/.local/bin), a systemd user service, and the Omarchy shell
# plugin.
#
#   ./dist/install.sh             build with cargo if Rust is installed,
#                                 otherwise download the release binaries
#   ./dist/install.sh --build     always build from source
#   ./dist/install.sh --prebuilt  always download the release binaries
#   ./dist/install.sh --no-build  install already-built binaries
#
# Release binaries are built by GitHub Actions from the tag matching this
# checkout's version, and checked against the release's SHA256SUMS (and its
# build attestation too, when the GitHub CLI is signed in).
#
# Run it again after `git pull` / `omarchy plugin update` to update.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$PWD"
BINDIR="$HOME/.local/bin"
UNITDIR="$HOME/.config/systemd/user"
PLUGINDIR="$HOME/.config/omarchy/plugins"
PLUGIN_ID="$(jq -r .id manifest.json)"
PLUGIN_PATH="$PLUGINDIR/$PLUGIN_ID"
VERSION="$(jq -r .version manifest.json)"
GITHUB_REPO="derekross/peridot"

MARKER=".installed-by-peridot"
UNIT="$UNITDIR/peridot.service"

die() { echo "$*" >&2; exit 1; }

# Peridot only replaces what it installed itself. Anything else at these
# paths (another program's `peridot` command, your own peridot.service, a
# plugin checkout) is left alone and the install stops.
our_binary() { [[ ! -e $1 ]] || grep -qa "$2" "$1"; }
our_unit() { [[ ! -e $UNIT ]] || grep -q "https://github.com/derekross/peridot" "$UNIT"; }
our_plugin_copy() {
  local dir=$1
  [[ -d $dir && ! -L $dir && -f $dir/$MARKER ]]
}

# Installed with `omarchy plugin add`, this checkout *is* the plugin. Build
# outside it: the shell reloads plugins whenever files change in there.
FROM_PLUGIN_CHECKOUT=0
if [[ "$REPO" == "$(realpath -m "$PLUGIN_PATH")" ]]; then
  FROM_PLUGIN_CHECKOUT=1
  export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/peridot/build"
fi
TARGET="${CARGO_TARGET_DIR:-$REPO/target}"

MODE="${1:-auto}"
case $MODE in
  auto) command -v cargo >/dev/null && MODE=--build || MODE=--prebuilt ;;
  --build | --prebuilt | --no-build) ;;
  *) echo "Unknown option: $MODE (use --build, --prebuilt or --no-build)" >&2; exit 2 ;;
esac

download_release() {
  local arch name base dir
  arch="$(uname -m)"
  [[ $arch == x86_64 || $arch == aarch64 ]] || { echo "No release build for $arch; install Rust to build Peridot." >&2; exit 1; }
  name="peridot-v$VERSION-$arch-linux"
  base="https://github.com/$GITHUB_REPO/releases/download/v$VERSION"
  dir="${XDG_CACHE_HOME:-$HOME/.cache}/peridot/release"
  rm -rf "${dir:?}" && mkdir -p "$dir"
  echo "Downloading Peridot v$VERSION ($arch)"
  curl -fsSL --proto '=https' --tlsv1.2 -o "$dir/$name.tar.gz" "$base/$name.tar.gz"
  curl -fsSL --proto '=https' --tlsv1.2 -o "$dir/SHA256SUMS" "$base/SHA256SUMS"
  (cd "$dir" && grep -E "  $name\.tar\.gz\$" SHA256SUMS | sha256sum --check --status) \
    || { echo "Checksum mismatch for $name.tar.gz; not installing it." >&2; exit 1; }
  echo "  Checksum OK"
  if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$dir/$name.tar.gz" --repo "$GITHUB_REPO" >/dev/null \
      || { echo "Build attestation check failed; not installing it." >&2; exit 1; }
    echo "  Built by GitHub Actions from $GITHUB_REPO (attestation verified)"
  fi
  tar -xzf "$dir/$name.tar.gz" -C "$dir"
  BIN_SRC="$dir/$name"
}

BIN_SRC="$TARGET/release"
case $MODE in
  --build)
    command -v cargo >/dev/null || {
      echo "Rust is needed to build Peridot: sudo pacman -S --needed rustup && rustup default stable" >&2
      exit 1
    }
    cargo build --locked --release -p peridotd -p peridot-cli
    ;;
  --prebuilt) download_release ;;
esac

# Check everything before changing anything.
our_binary "$BINDIR/peridotd" "Peridot daemon" \
  || die "$BINDIR/peridotd exists and isn't Peridot's. Move it aside, then run this again."
our_binary "$BINDIR/peridot" "can't reach Peridot at" \
  || die "$BINDIR/peridot exists and isn't Peridot's. Move it aside, then run this again."
our_unit || die "$UNIT exists and isn't Peridot's. Move it aside, then run this again."
INSTALL_PLUGIN=1
if (( FROM_PLUGIN_CHECKOUT )); then
  INSTALL_PLUGIN=0
elif [[ -e $PLUGIN_PATH || -L $PLUGIN_PATH ]] && ! our_plugin_copy "$PLUGIN_PATH"; then
  INSTALL_PLUGIN=0
  echo "Note: $PLUGIN_PATH exists and wasn't installed by this script"
  echo "  (e.g. added with 'omarchy plugin add'); leaving it as it is."
fi

echo "Installing binaries to $BINDIR"
install -Dm755 "$BIN_SRC/peridotd" "$BINDIR/peridotd"
install -Dm755 "$BIN_SRC/peridot" "$BINDIR/peridot"

echo "Installing the systemd user service"
install -Dm644 dist/peridot.service "$UNIT"
systemctl --user daemon-reload
systemctl --user enable peridot.service >/dev/null
systemctl --user restart peridot.service

if (( FROM_PLUGIN_CHECKOUT )); then
  echo "Shell plugin: installed by 'omarchy plugin add' ($PLUGIN_ID)"
elif (( INSTALL_PLUGIN )); then
  echo "Installing the Omarchy shell plugin ($PLUGIN_ID)"
  mkdir -p "$PLUGINDIR"
  # Copied, not linked: the shell's file watcher doesn't follow symlinks.
  # Built next to the destination, then swapped in.
  staging="$(mktemp -d "$PLUGINDIR/.$PLUGIN_ID.XXXXXX")"
  cp -r shell-plugin/. "$staging/"
  # The repo's single manifest points into shell-plugin/; here the files sit
  # at the top of the plugin folder.
  jq '.entryPoints |= with_entries(.value |= ltrimstr("shell-plugin/"))' manifest.json \
    >"$staging/manifest.json"
  echo "https://github.com/derekross/peridot" >"$staging/$MARKER"
  chmod 755 "$staging"
  if [[ -e $PLUGIN_PATH ]]; then
    # Only reached for our own earlier copy (checked above).
    rm -rf "${PLUGIN_PATH:?}"
  fi
  mv "$staging" "$PLUGIN_PATH"
fi
if command -v omarchy >/dev/null; then
  omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
  omarchy plugin enable "$PLUGIN_ID" --section right --before omarchy.tray >/dev/null 2>&1 \
    || omarchy plugin enable "$PLUGIN_ID" --section right >/dev/null 2>&1 || true
fi

echo
systemctl --user --no-pager --lines=0 status peridot.service | head -3
echo
echo "Done. Click the Peridot icon in the bar (or run 'peridot') to get started."
