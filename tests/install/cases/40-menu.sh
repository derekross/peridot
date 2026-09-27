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

finish
