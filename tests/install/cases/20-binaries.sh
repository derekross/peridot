#!/usr/bin/env bash
source "$T/lib.sh"

case_ "1. an unrecorded peridotd (a 0.1.x source build): non-interactive run stops before any write"
fresh; printf 'old build\n' >"$BIN/peridotd"; chmod +x "$BIN/peridotd"; cp "$BIN/peridotd" "$HOME/old"; r=$(inst)
expect "exit 1" [ "$r" = 1 ] &&
expect "names it" said "$BIN/peridotd exists but isn't recorded" &&
expect "names the flag" said "--replace-existing=$BIN/peridotd" &&
expect "untouched" same "$HOME/old" "$BIN/peridotd" &&
expect "nothing else written" [ ! -e "$U" ] && [ ! -e "$P" ] && ok

case_ "2. --replace-existing=<path>: replaced, the old file kept as a backup, recorded"
r=$(inst "--replace-existing=$BIN/peridotd")
expect "exit 0" [ "$r" = 0 ] &&
expect "new binary" same "$BINS/release/peridotd" "$BIN/peridotd" &&
expect "backup exists" [ -n "$(ls "$B"/peridotd.* 2>/dev/null)" ] &&
expect "backup content" same "$HOME/old" "$B"/peridotd.* &&
expect "backup recorded" grep -q "^backup	$BIN/peridotd	$B/peridotd\." "$M" &&
expect "said where" said "moved the previous $BIN/peridotd to $B/peridotd." && ok

case_ "3. the flag consents to one path only"
fresh; printf 'old\n' >"$BIN/peridotd"; printf 'old\n' >"$BIN/peridot"; r=$(inst "--replace-existing=$BIN/peridotd")
expect "stops on the other" [ "$r" = 1 ] && expect "names opal" said "$BIN/peridot exists but isn't recorded" && ok

case_ "4. the interactive prompt: y replaces, n stops"
fresh; printf 'old\n' >"$BIN/peridotd"
if command -v script >/dev/null; then
  (cd "$ROOT" && printf 'n\n' | CARGO_TARGET_DIR="$BINS" script -qec "./dist/install.sh --no-build" /dev/null >"$HOME/out.txt" 2>&1); r=$?
  expect "declined stops" [ "$r" = 1 ] && expect "untouched" [ "$(cat "$BIN/peridotd")" = old ] && ok
  (cd "$ROOT" && printf 'y\n' | CARGO_TARGET_DIR="$BINS" script -qec "./dist/install.sh --no-build" /dev/null >"$HOME/out.txt" 2>&1); r=$?
  expect "accepted installs" [ "$r" = 0 ] && expect "replaced" same "$BINS/release/peridotd" "$BIN/peridotd" && expect "backup" [ -n "$(ls "$B"/peridotd.* 2>/dev/null)" ] && ok
else
  echo "   ok (skipped: no 'script' command)"
fi

case_ "5. a symlink or a folder at a binary path stops the install, never followed"
fresh; printf 'target\n' >"$HOME/real"; ln -s "$HOME/real" "$BIN/peridotd"; r=$(inst)
expect "exit 1" [ "$r" = 1 ] && expect "message" said "is a symbolic link" && expect "target untouched" [ "$(cat "$HOME/real")" = target ] && expect "still a link" [ -L "$BIN/peridotd" ] && ok
fresh; mkdir "$BIN/peridot"; r=$(inst)
expect "exit 1" [ "$r" = 1 ] && expect "message" said "isn't a regular file" && ok

case_ "6. uninstall keeps an unrecorded binary and a link, removes ours, lists backups"
fresh; inst "--replace-existing=$BIN/peridotd" >/dev/null 2>&1 || true; printf 'old\n' >"$BIN/peridotd"; inst "--replace-existing=$BIN/peridotd" >/dev/null
printf 'theirs\n' >"$BIN/peridot"; ln -sf /bin/true "$BIN/peridotd"  # both now not ours
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] &&
expect "unrecorded kept" [ "$(cat "$BIN/peridot")" = theirs ] && said "$BIN/peridot isn't recorded" &&
expect "link kept" [ -L "$BIN/peridotd" ] && said "$BIN/peridotd is a link" &&
expect "backups listed" said "Backups Peridot made are in $B" && [ -n "$(ls "$B"/peridotd.* 2>/dev/null)" ] && ok

case_ "7. a file that changed after the check is kept as a backup, not overwritten"
fresh; inst >/dev/null
# Simulate the race: the record says one thing, the file another.
printf 'edited after check\n' >>"$BIN/peridot"
(cd "$ROOT" && source dist/lib.sh && prepare_state && load_manifest && replace_owned "$BINS/release/peridot" "$BIN/peridot" 755 "${MANIFEST_HASH[$BIN/peridot]}" >"$HOME/out.txt" 2>&1)
expect "new file in place" same "$BINS/release/peridot" "$BIN/peridot" &&
expect "old kept" grep -q "edited after check" "$B"/peridot.* &&
expect "said" said "changed after it was checked" && ok

finish
