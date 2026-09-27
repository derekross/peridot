#!/usr/bin/env bash
source "$T/lib.sh"

case_ "1. --purge without a terminal is refused; nothing is deleted"
fresh; inst >/dev/null; r=$(uninst --purge)
expect "refused" [ "$r" = 1 ] && said "needs a terminal" && expect "binary still there" [ -f "$BIN/peridotd" ] && ok

case_ "2. without --purge, keys and data stay and the resolved paths are named"
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && said "Kept: this computer's Peridot identity" && said "$HOME/.local/share/peridot" && ok

case_ "3. --purge: XDG-resolved folders removed, a decoy untouched, a linked folder not followed, backups kept"
fresh; printf 'old\n' >"$BIN/peridotd"; inst "--replace-existing=$BIN/peridotd" >/dev/null
export XDG_DATA_HOME="$HOME/xdg-data" XDG_CONFIG_HOME="$HOME/xdg-cfg" XDG_CACHE_HOME="$HOME/xdg-cache" XDG_STATE_HOME="$HOME/xdg-state"
mkdir -p "$XDG_DATA_HOME/peridot" "$XDG_CONFIG_HOME/peridot" "$HOME/.local/share/peridot" "$HOME/real-cache" "$XDG_CACHE_HOME"
echo db >"$XDG_DATA_HOME/peridot/opal.db"; echo decoy >"$HOME/.local/share/peridot/decoy"; ln -s "$HOME/real-cache" "$XDG_CACHE_HOME/peridot"
mkdir -p "$XDG_STATE_HOME/peridot/backup"; echo b >"$XDG_STATE_HOME/peridot/backup/peridotd.20200101T000000"
if command -v script >/dev/null; then
  (cd "$ROOT" && printf 'delete\n' | script -qec "./dist/uninstall.sh --purge" /dev/null >"$HOME/out.txt" 2>&1); r=$?
  expect "exit 0" [ "$r" = 0 ] &&
  expect "data removed" [ ! -e "$XDG_DATA_HOME/peridot" ] && said "removed $XDG_DATA_HOME/peridot" &&
  expect "config removed" [ ! -e "$XDG_CONFIG_HOME/peridot" ] &&
  expect "decoy untouched" [ -f "$HOME/.local/share/peridot/decoy" ] &&
  expect "link not followed" [ -L "$XDG_CACHE_HOME/peridot" ] && [ -d "$HOME/real-cache" ] && said "is a link; not followed" &&
  expect "backup kept" [ -f "$XDG_STATE_HOME/peridot/backup/peridotd.20200101T000000" ] && said "Backups Peridot made are in" &&
  expect "keyring cleared per kind" logged "clear application peridot kind device-identity" && logged "clear application peridot kind sync-secret" && [ ! -s "$FAKE_SECRETS" ] &&
  expect "reported" said "2 Peridot item(s) before, 0 left" && ok
else
  echo "   ok (skipped: no 'script' command)"
fi
unset XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME

case_ "4. a keyring that refuses is reported, not hidden"
fresh; inst >/dev/null
if command -v script >/dev/null; then
  (cd "$ROOT" && printf 'delete\n' | FAKE_SECRET_RC=1 script -qec "./dist/uninstall.sh --purge" /dev/null >"$HOME/out.txt" 2>&1); r=$?
  expect "exit 0" [ "$r" = 0 ] && said "clearing 'device-identity' items failed" && said "2 left" && ok
else
  echo "   ok (skipped: no 'script' command)"
fi

case_ "5. the typed phrase is required"
fresh; inst >/dev/null
if command -v script >/dev/null; then
  (cd "$ROOT" && printf 'delete my keys\n' | script -qec "./dist/uninstall.sh --purge" /dev/null >"$HOME/out.txt" 2>&1); r=$?
  expect "cancelled" [ "$r" = 1 ] && said "Cancelled" && [ -f "$BIN/peridotd" ] && ok
else
  echo "   ok (skipped: no 'script' command)"
fi

finish
