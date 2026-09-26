#!/usr/bin/env bash
# Remove Peridot: the service, binaries and shell plugin. Your settings files
# stay as they are. The Peridot identity (so this computer can rejoin) is kept
# unless you pass --purge.
#
#   ./dist/uninstall.sh           remove the program, keep the identity
#   ./dist/uninstall.sh --purge   also delete the identity, history and backups
set -euo pipefail

cd "$(dirname "$0")/.."
PLUGIN_ID="$(jq -r .id manifest.json 2>/dev/null || echo derekross.peridot)"
PLUGIN_PATH="$HOME/.config/omarchy/plugins/$PLUGIN_ID"
UNIT="$HOME/.config/systemd/user/peridot.service"
PURGE=0
[[ "${1:-}" == "--purge" ]] && PURGE=1

if (( PURGE )); then
  echo "This deletes this computer's Peridot identity, history and undo backups."
  echo "Your settings files stay. Your other computers keep syncing."
  echo "If this is your only computer, make a recovery kit first ('peridot recovery')."
  read -r -p "Type 'delete' to continue: " reply
  [[ $reply == "delete" ]] || { echo "Cancelled."; exit 1; }
fi

echo "Stopping the service"
# Only what Peridot installed is removed (see install.sh).
if [[ -e $UNIT ]] && ! grep -q "https://github.com/derekross/peridot" "$UNIT"; then
  echo "  $UNIT isn't Peridot's; leaving it alone."
else
  systemctl --user disable --now peridot.service >/dev/null 2>&1 || true
  rm -f "$UNIT"
  systemctl --user daemon-reload
fi

echo "Removing binaries"
for bin in "peridotd:Peridot daemon" "peridot:can't reach Peridot at"; do
  path="$HOME/.local/bin/${bin%%:*}"
  if [[ -e $path ]] && grep -qa "${bin#*:}" "$path"; then
    rm -f "$path"
  elif [[ -e $path ]]; then
    echo "  $path isn't Peridot's; leaving it alone."
  fi
done

echo "Removing the shell plugin"
if command -v omarchy >/dev/null; then
  omarchy plugin disable "$PLUGIN_ID" >/dev/null 2>&1 || true
fi
if [[ -d $PLUGIN_PATH && ! -L $PLUGIN_PATH && -f $PLUGIN_PATH/.installed-by-peridot ]]; then
  rm -rf "${PLUGIN_PATH:?}"
elif [[ -e $PLUGIN_PATH ]]; then
  # e.g. added with `omarchy plugin add`: that command removes it.
  echo "  $PLUGIN_PATH wasn't installed by install.sh; remove it with: omarchy plugin remove $PLUGIN_ID"
fi
omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true

if (( PURGE )); then
  echo "Deleting the identity, history and backups"
  secret-tool clear application peridot 2>/dev/null || true
  rm -rf "$HOME/.local/share/peridot" "$HOME/.config/peridot" "$HOME/.cache/peridot"
else
  echo
  echo "Kept: this computer's Peridot identity (keyring), ~/.local/share/peridot, ~/.config/peridot."
  echo "Run with --purge to delete those too."
fi
echo "Peridot removed."
