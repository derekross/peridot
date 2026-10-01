#!/usr/bin/env bash
# Remove Peridot: the service, binaries, shell plugin and its Share menu
# entries. Your settings files stay as they are. This computer's Peridot
# identity (so it can rejoin) is kept unless you pass --purge.
#
#   ./dist/uninstall.sh           remove the program, keep the identity
#   ./dist/uninstall.sh --purge   also delete the identity, history and backups
#
# Only what Peridot can prove it wrote is removed (the rule is in
# dist/lib.sh): a unit or plugin file you changed stays and is named, a
# masked or linked unit is left alone, folders keep anything you added, only
# menu lines still exactly Peridot's are taken out, and backups Peridot made
# are never deleted, not even with --purge.
set -euo pipefail

cd "$(dirname "$0")/.."
source dist/lib.sh || { echo "dist/lib.sh is missing: run this from an Peridot checkout" >&2; exit 1; }

PURGE=0
for arg in "$@"; do
  case $arg in
    --purge) PURGE=1 ;;
    -h | --help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "Unknown option: $arg (only --purge)" >&2; exit 2 ;;
  esac
done

if (( PURGE )); then
  [[ -t 0 ]] || die "--purge needs a terminal to confirm on."
  say "This deletes this computer's Peridot identity, history and undo backups:"
  note "$DATADIR, $CONFIGDIR, $CACHEDIR, and the keyring items"
  say "Your settings files stay. Your other computers keep syncing."
  say "If this is your only computer, make a recovery kit first ('peridot recovery')."
  read -r -p "Type 'delete' to continue: " reply </dev/tty
  [[ $reply == "delete" ]] || { say "Cancelled."; exit 1; }
fi

prepare_state
load_manifest
load_known
print_paths

# Only paths this script would write itself are taken from the record;
# a record that was edited or restored from elsewhere can't point it at
# anything else.
for p in "${!MANIFEST_HASH[@]}"; do
  case $p in
    "$BINDIR/peridotd" | "$BINDIR/peridot" | "$INSTALLER" | "$UNIT" | "$PROXY_UNIT" | "$SOCKET_UNIT" | "$INSTALL_UNIT" | "$MENU" | "$PLUGIN_PATH"/*) ;;
    *) note "ignoring a record line for $p: not a path this script writes"; unset 'MANIFEST_HASH[$p]' ;;
  esac
done
for k in "${!MENU_LINE_HASH[@]}"; do
  [[ $k == "$MENU"$'\t'* ]] || { note "ignoring a menu record line for ${k%%$'\t'*}: not the menu file this script edits"; unset 'MENU_LINE_HASH[$k]'; }
done

# ── The service ────────────────────────────────────────────────────────
inspect_unit
say "Stopping the service"
case $UNIT_STATE in
  owned)
    systemctl_user disable peridot.service
    if unit_runs_our_binary; then scoped stop peridot.service || true
    elif unit_is_active; then note "peridot.service is running but doesn't start $BINDIR/peridotd (a drop-in?); not stopped."; fi
    remove_owned "$UNIT" "$(file_hash "$UNIT")"
    systemctl_user daemon-reload ;;
  edited)
    if unit_runs_our_binary && scoped stop peridot.service; then
      note "$UNIT is kept: you changed it. It starts the binary this removes, so it is stopped but not disabled;"
      note "when you're done with it: systemctl --user disable peridot.service && rm $UNIT"
    elif unit_runs_our_binary; then
      note "$UNIT is kept: you changed it. It starts the binary this removes; stop it yourself when you're done with it."
    else
      note "$UNIT is kept: you changed it, and it doesn't start $BINDIR/peridotd, so it is left alone."
    fi ;;
  elsewhere)
    if unit_runs_our_binary && scoped stop peridot.service; then note "peridot.service comes from $UNIT_FRAGMENT (not Peridot's); stopped because it starts the binary this removes, otherwise left alone."
    elif unit_runs_our_binary; then note "peridot.service comes from $UNIT_FRAGMENT (not Peridot's); it starts the binary this removes, so stop it yourself."
    else note "peridot.service comes from $UNIT_FRAGMENT; not Peridot's, leaving it alone."; fi ;;
  symlink)
    note "$UNIT is a symbolic link (masked or linked); not Peridot's, leaving it alone."
    if [[ -f $UNIT ]] && grep -qF -- "$BINDIR/peridotd" "$UNIT" 2>/dev/null; then note "it still starts Peridot's binary, which this removes: disable or fix it yourself."; fi ;;
  foreign) note "$UNIT isn't Peridot's; leaving it alone." ;;
  other) note "$UNIT isn't a regular file; leaving it alone." ;;
  missing) ;;
esac
(( UNIT_DROPIN )) && note "$UNIT.d/ drop-ins are yours; not touched."

# The bus proxy: normally already stopped with the service (PartOf=); a
# stop of the service went ahead only if this was in scope too. This covers
# a service that wasn't Peridot's to stop.
inspect_proxy_unit
case $PROXY_UNIT_STATE in
  owned)
    proxy_serves_peridot && { scoped stop peridot-dbus-proxy.service || true; }
    remove_owned "$PROXY_UNIT" "$(file_hash "$PROXY_UNIT")"
    systemctl_user daemon-reload ;;
  edited)
    if proxy_serves_peridot; then
      scoped stop peridot-dbus-proxy.service || true
      note "$PROXY_UNIT is kept: you changed it. Only peridot.service uses it; remove it yourself when you're done with it."
    else
      note "$PROXY_UNIT is kept: you changed it, and it no longer serves Peridot's bus socket, so it is left alone."
    fi ;;
  elsewhere) note "peridot-dbus-proxy.service comes from $PROXY_UNIT_FRAGMENT; not Peridot's, leaving it alone." ;;
  symlink) note "$PROXY_UNIT is a symbolic link (masked or linked); not Peridot's, leaving it alone." ;;
  foreign) note "$PROXY_UNIT isn't Peridot's; leaving it alone." ;;
  other) note "$PROXY_UNIT isn't a regular file; leaving it alone." ;;
  missing) ;;
esac

# The install socket and its instances: stopped (so nothing new starts,
# and nothing running stays) before the units go.
inspect_socket_unit
case $SOCKET_UNIT_STATE in
  owned)
    socket_serves_peridot && { scoped stop peridot-install.socket || true; }
    stop_install_instances
    remove_owned "$SOCKET_UNIT" "$(file_hash "$SOCKET_UNIT")"
    systemctl_user daemon-reload ;;
  edited)
    if socket_serves_peridot; then
      scoped stop peridot-install.socket || true
      stop_install_instances
      note "$SOCKET_UNIT is kept: you changed it. It starts the script this removes; remove it yourself when you're done with it."
    else
      note "$SOCKET_UNIT is kept: you changed it, and it no longer starts Peridot's install script from Peridot's socket, so it is left alone."
    fi ;;
  elsewhere) note "peridot-install.socket comes from $SOCKET_UNIT_FRAGMENT; not Peridot's, leaving it alone." ;;
  symlink) note "$SOCKET_UNIT is a symbolic link (masked or linked); not Peridot's, leaving it alone." ;;
  foreign) note "$SOCKET_UNIT isn't Peridot's; leaving it alone." ;;
  other) note "$SOCKET_UNIT isn't a regular file; leaving it alone." ;;
  missing) ;;
esac

# The install template: any instance still running is stopped with it.
inspect_install_unit
case $INSTALL_UNIT_STATE in
  owned)
    stop_install_instances
    remove_owned "$INSTALL_UNIT" "$(file_hash "$INSTALL_UNIT")"
    systemctl_user daemon-reload ;;
  edited) note "$INSTALL_UNIT is kept: you changed it. It runs the script this removes; remove it yourself when you're done with it." ;;
  elsewhere) note "peridot-install@.service comes from $INSTALL_UNIT_FRAGMENT; not Peridot's, leaving it alone." ;;
  symlink) note "$INSTALL_UNIT is a symbolic link (masked or linked); not Peridot's, leaving it alone." ;;
  foreign) note "$INSTALL_UNIT isn't Peridot's; leaving it alone." ;;
  other) note "$INSTALL_UNIT isn't a regular file; leaving it alone." ;;
  missing) ;;
esac

# ── Binaries ───────────────────────────────────────────────────────────
say "Removing binaries"
for bin in peridotd peridot peridot-install; do
  p="$BINDIR/$bin"
  case "$(binary_state "$p")" in
    owned) remove_owned "$p" "$(file_hash "$p")" ;;
    unrecorded) note "$p isn't recorded as installed by Peridot ($(describe_file "$p")); leaving it." ;;
    symlink) note "$p is a link; not Peridot's, leaving it." ;;
    other) note "$p isn't a regular file; leaving it." ;;
  esac
done

# ── The Share menu ─────────────────────────────────────────────────────
say "Removing the Private link entries from the Share menu"
inspect_menu
case $MENU_STATE in
  present)
    if owned_file "$MENU" menu; then
      # A file Peridot created and nobody changed since.
      remove_owned "$MENU" "$(file_hash "$MENU")"
      note "removed $MENU (Peridot created it and it was unchanged)"
    else
      menu_remove_entries
    fi ;;
  edited)
    note "$MENU has Peridot's entries, but you changed them; they're yours now and stay. Edit them out yourself if you like."
    forget_menu_lines ;;
  absent | missing) forget_menu_lines ;;
  other) note "$MENU is a link or not a file; not Peridot's, leaving it." ;;
esac

# ── The shell plugin ───────────────────────────────────────────────────
say "Removing the shell plugin"
load_shipped
for entry in "$PLUGIN_PATH:$PLUGIN_ID"; do
  dir=${entry%%:*}; id=${entry#*:}
  case "$(plugin_dir_state "$dir")" in
    missing) ;;
    checkout) note "$dir is a checkout (has .git); remove it with: omarchy plugin remove $id" ;;
    symlink) note "$dir is a link; not Peridot's, leaving it." ;;
    other) note "$dir isn't a folder; not Peridot's, leaving it." ;;
    dir)
      if remove_plugin_copy "$dir"; then
        command -v omarchy >/dev/null && { omarchy plugin disable "$id" >/dev/null 2>&1 || true; }
      else
        note "the shell may still list it; disable it with: omarchy plugin disable $id"
      fi ;;
  esac
done
command -v omarchy-shell >/dev/null && { omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true; }

# ── The record, and --purge ────────────────────────────────────────────
# Drop record lines for files that are gone (kept files keep their lines
# only while they still match, which they don't if you changed them).
for p in "${!MANIFEST_HASH[@]}"; do
  [[ -f $p && ! -L $p && "$(file_hash "$p")" == "${MANIFEST_HASH[$p]}" ]] || unset 'MANIFEST_HASH[$p]'
done
write_manifest

if (( PURGE )); then
  say "Deleting the identity, history and backups"
  # Only Peridot's own item kinds, never everything tagged application=peridot.
  before="$(secret-tool search --all application peridot 2>/dev/null | grep -c '^\[' || true)"
  for kind in device-identity sync-secret app-token; do
    secret-tool clear application peridot kind "$kind" 2>/dev/null || note "keyring: clearing '$kind' items failed; they may still be there."
  done
  after="$(secret-tool search --all application peridot 2>/dev/null | grep -c '^\[' || true)"
  note "keyring: ${before:-?} Peridot item(s) before, ${after:-?} left$( (( ${after:-0} > 0 )) && printf ' (not Peridot'\''s kinds, or clearing failed: check with Seahorse)')"
  for d in "$DATADIR" "$CONFIGDIR" "$CACHEDIR"; do
    if [[ -L $d ]]; then note "$d is a link; not followed, not removed."
    elif [[ -d $d ]]; then rm -rf -- "$d"; note "removed $d"
    else note "$d: nothing there"; fi
  done
  rm -f -- "$MANIFEST" "$STATEDIR/.lock"
  rmdir -- "$STATEDIR" 2>/dev/null || true
else
  say
  say "Kept: this computer's Peridot identity (keyring), $DATADIR, $CONFIGDIR."
  say "Run with --purge to delete those too."
fi
if [[ -d $BACKUPDIR ]] && [[ -n "$(ls -A -- "$BACKUPDIR" 2>/dev/null)" ]]; then
  say "Backups Peridot made are in $BACKUPDIR (Peridot never deletes them):"
  for f in "$BACKUPDIR"/*; do note "$f"; done
fi
say "Peridot removed."
