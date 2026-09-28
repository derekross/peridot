#!/usr/bin/env bash
source "$T/lib.sh"

case_ "1. the Share menu file is yours: not touched without consent; --menu adds the entries and keeps a backup"
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  // mine\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"; cp "$MENU" "$HOME/menu.before"
r=$(inst)
expect "exit 0" [ "$r" = 0 ] && expect "untouched" same "$HOME/menu.before" "$MENU" && said "Share menu: not touched" && ok
r=$(inst --menu)
expect "exit 0" [ "$r" = 0 ] && expect "entries added" grep -q '"trigger.share.peridot"' "$MENU" && expect "yours kept" grep -q '"trigger.custom"' "$MENU" &&
expect "comma added" grep -q '"trigger.custom": {"label":"Mine"},' "$MENU" &&
expect "backup kept" same "$HOME/menu.before" "$B"/omarchy-menu.jsonc.* && said "kept in $B/omarchy-menu.jsonc." &&
expect "backup recorded" grep -q "^backup	$MENU	" "$M" && ok
r=$(inst --menu)
expect "second run leaves it" [ "$(grep -c 'trigger.share.peridot"' "$MENU")" = 1 ] && said "already in" && ok

case_ "2. uninstall takes out only lines still exactly Peridot's; an edited one and yours stay"
sed -i 's|"label":"Clipboard"|"label":"Paste"|' "$MENU"
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "ours gone" not grep -q '"trigger.share.peridot.file"' "$MENU" && expect "yours kept" grep -q '"trigger.custom"' "$MENU" &&
expect "edited line kept" grep -q '"label":"Paste"' "$MENU" && said "you changed were kept" && ok

case_ "3. a menu file Peridot created is removed on uninstall only while unchanged"
fresh; r=$(inst --menu)
expect "created" [ -f "$MENU" ] && said "created $MENU" && manifest_lists "$MENU" && ok
r=$(uninst); expect "removed" [ ! -e "$MENU" ] && ok
fresh; inst --menu >/dev/null; printf '  "trigger.mine": {"label":"x"},\n' >>"$MENU"
r=$(uninst); expect "kept with your line" [ -f "$MENU" ] && grep -q '"trigger.mine"' "$MENU" && not grep -q 'trigger.share.peridot"' "$MENU" && ok

case_ "4. a symlinked or unreadable-shape menu file is never edited"
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n}\n' >"$HOME/real.jsonc"; ln -s "$HOME/real.jsonc" "$MENU"; r=$(inst --menu)
expect "exit 0" [ "$r" = 0 ] && [ -L "$MENU" ] && [ "$(wc -c <"$HOME/real.jsonc")" = 4 ] && said "a link" && ok
fresh; mkdir -p "$(dirname "$MENU")"; printf 'no brace here\n' >"$MENU"; r=$(inst --menu)
expect "exit 0" [ "$r" = 0 ] && said "has no closing brace" && [ "$(cat "$MENU")" = "no brace here" ] && ok

case_ "5. every line Peridot added is recorded by its hash; uninstall forgets them; ownership needs no marker"
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"
r=$(inst --menu)
n_lines=$(wc -l <"$ROOT/dist/omarchy-menu.jsonc")
expect "exit 0" [ "$r" = 0 ] && expect "one record per line" [ "$(grep -c "^menu	$MENU	[0-9a-f]\{64\}$" "$M")" = "$n_lines" ] &&
expect "the record verifies" manifest_verifies && ok
# A line that carries Peridot's marker but isn't one Peridot wrote is never removed.
sed -i 's|"trigger.custom": {"label":"Mine"}|"trigger.custom": {"label":"Mine"},\n  "trigger.share.peridot.mine": {"label":"Looks like Peridot"}|' "$MENU"
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "marker-only line kept" grep -q '"trigger.share.peridot.mine"' "$MENU" &&
expect "Peridot's lines gone" [ "$(grep -c 'trigger.share.peridot"' "$MENU")" = 0 ] && said "removed $n_lines Private link line(s)" &&
expect "records forgotten" not grep -q "^menu	" "$M" 2>/dev/null && ok

case_ "6. a change made while the file is being edited is noticed: nothing is written, and it says so"
# The install snapshot is taken before the new content is composed; a
# write that lands in between must be caught before and after the swap.
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"
r=$( (cd "$ROOT" && source dist/lib.sh && load_manifest && inspect_menu \
  && menu_lines() { printf '  "trigger.other": 1,\n' >>"$MENU"; cat dist/omarchy-menu.jsonc; } \
  && menu_add_entries) </dev/null >"$HOME/out.txt" 2>&1; echo $?)
expect "refused" [ "$r" = 1 ] && said "changed while it was being edited; not touched" &&
expect "the other write is intact" [ "$(tail -n1 "$MENU")" = '  "trigger.other": 1,' ] &&
expect "nothing of Peridot's added" not grep -q 'trigger.share.peridot' "$MENU" &&
expect "no backup made" [ ! -d "$B" ] && expect "no temp left" [ -z "$(ls -A "$(dirname "$MENU")" | grep '^\.peridot')" ] && ok
# The same guard on removal.
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"; inst --menu >/dev/null; cp "$MENU" "$HOME/menu.before"
r=$( (cd "$ROOT" && source dist/lib.sh && load_manifest && inspect_menu \
  && eval "orig_$(declare -f menu_line_owned)" \
  && menu_line_owned() { [[ -e $HOME/.once ]] || { : >"$HOME/.once"; printf '  "trigger.other": 1,\n' >>"$MENU"; }; orig_menu_line_owned "$@"; } \
  && menu_remove_entries) </dev/null >"$HOME/out.txt" 2>&1; echo $?)
expect "refused" [ "$r" = 1 ] && said "changed while it was being edited; not touched" &&
expect "Peridot's lines still there" grep -q '"trigger.share.peridot.file"' "$MENU" &&
expect "the other write is intact" [ "$(tail -n1 "$MENU")" = '  "trigger.other": 1,' ] && ok
# And the swap itself: a file that isn't the expected bytes any more is put back.
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n}\n' >"$MENU"; printf '{\n  "x": 1\n}\n' >"$(dirname "$MENU")/.peridot.new"
r=$( (cd "$ROOT" && source dist/lib.sh && menu_swap_in "$(dirname "$MENU")/.peridot.new" "$(printf 'something else' | sha256sum | cut -c1-64)") </dev/null >"$HOME/out.txt" 2>&1; echo $?)
expect "refused" [ "$r" = 1 ] && said "changed while it was being edited" && expect "file untouched" [ "$(cat "$MENU")" = "$(printf '{\n}')" ] &&
expect "new content discarded" [ ! -e "$(dirname "$MENU")/.peridot.new" ] && ok

case_ "7. when every Peridot line was changed, install adds nothing and uninstall removes nothing"
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"; inst --menu >/dev/null
sed -i 's|"Private link"|"My link"|; s|"File"|"A file"|; s|"Clipboard"|"Paste"|; s|"Last screenshot"|"Shot"|; s|"Manage links"|"Links"|; s|^  // Peridot|  // mine|' "$MENU"
cp "$MENU" "$HOME/menu.before"
r=$(inst --menu)
expect "exit 0" [ "$r" = 0 ] && said "changed by you; not touched" && expect "untouched" same "$HOME/menu.before" "$MENU" && ok
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && said "they're yours now and stay" && expect "untouched" same "$HOME/menu.before" "$MENU" &&
expect "stale records dropped" not grep -q "^menu	" "$M" 2>/dev/null && ok

case_ "8. an install recorded before menu lines were recorded is still cleaned up: the lines are this checkout's bytes"
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"; inst --menu >/dev/null
sed -i '/^menu	/d' "$M"
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && expect "ours gone" not grep -q 'trigger.share.peridot' "$MENU" && expect "yours kept" grep -q '"trigger.custom"' "$MENU" && ok
# A record line for some other menu file is ignored and named, never acted on.
fresh; mkdir -p "$(dirname "$MENU")"; printf '{\n  "trigger.custom": {"label":"Mine"}\n}\n' >"$MENU"; inst --menu >/dev/null
printf 'menu\t%s/elsewhere.jsonc\t%s\n' "$HOME" "$(printf 'x' | sha256sum | cut -c1-64)" >>"$M"
r=$(uninst)
expect "exit 0" [ "$r" = 0 ] && said "ignoring a menu record line for $HOME/elsewhere.jsonc" && ok

finish
