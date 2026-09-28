#!/usr/bin/env bash
source "$T/lib.sh"

# The download itself needs the network; the check it must pass doesn't.
check() { (cd "$ROOT" && source dist/lib.sh && RELEASE_CHECKSUMS="$HOME/pins.tsv" && verify_pinned "$@" >"$HOME/out.txt" 2>&1); }

case_ "1. a tarball is accepted only when it matches the hash pinned in the checkout"
fresh; echo "release bytes" >"$HOME/asset.tar.gz"; h=$(sha256sum "$HOME/asset.tar.gz" | cut -c1-64)
printf '# pins\n%s\tperidot-v9.9.9-x86_64-linux.tar.gz\tdeadbeef\t%s\n' "$h" "$(stat -c %s "$HOME/asset.tar.gz")" >"$HOME/pins.tsv"
expect "matching" check "$HOME/asset.tar.gz" peridot-v9.9.9-x86_64-linux.tar.gz && ok
echo "tampered" >>"$HOME/asset.tar.gz"
expect "oversized refused before hashing" not check "$HOME/asset.tar.gz" peridot-v9.9.9-x86_64-linux.tar.gz && said "isn't the pinned size" && ok
printf 'release bytez\n' >"$HOME/asset.tar.gz"
expect "same size, other bytes refused" not check "$HOME/asset.tar.gz" peridot-v9.9.9-x86_64-linux.tar.gz && said "doesn't match the checksum pinned" && ok
expect "unpinned asset refused" not check "$HOME/asset.tar.gz" peridot-v9.9.9-aarch64-linux.tar.gz && said "no pinned checksum" && ok

case_ "2. every pinned tarball is well formed and names its tag's commit; the manifest's version is pinned once released"
v="$(jq -r .version "$ROOT/manifest.json")"
n=0
while IFS=$'\t' read -r h a c z; do
  [[ $h == \#* || -z $a ]] && continue
  n=$((n + 1)); tag="${a#opal-}"; tag="${tag%%-*}"
  expect "hash shape ($a)" grep -qE '^[0-9a-f]{64}$' <<<"$h" &&
  expect "size pinned ($a)" grep -qE '^[0-9]+$' <<<"$z" &&
  { ! (cd "$ROOT" && git rev-parse -q --verify "$tag^{commit}" >/dev/null 2>&1) \
    || expect "commit is the tag's ($a)" [ "$c" = "$(cd "$ROOT" && git rev-parse "$tag^{commit}")" ]; }
done <"$ROOT/dist/release-checksums.tsv"
expect "at least one pinned release" [ "$n" -gt 0 ] && ok
if grep -qF "	peridot-v$v-x86_64-linux.tar.gz	" "$ROOT/dist/release-checksums.tsv"; then
  expect "both architectures pinned for $v" grep -qF "	peridot-v$v-aarch64-linux.tar.gz	" "$ROOT/dist/release-checksums.tsv" && ok
else
  echo "   ok (v$v isn't pinned yet: dist/pin-release.sh runs after the release is published)"
fi

case_ "3. install --prebuilt refuses before downloading when this version isn't pinned (--dev reads another table)"
fresh; export PERIDOT_RELEASE_CHECKSUMS="$HOME/empty.tsv"; : >"$HOME/empty.tsv"
r=$( (cd "$ROOT" && ./dist/install.sh --dev --prebuilt </dev/null >"$HOME/out.txt" 2>&1); echo $?)
expect "stopped" [ "$r" = 1 ] && said "--dev: pinned releases are read from $HOME/empty.tsv" && said "has no pinned checksum" &&
expect "nothing written" [ ! -e "$BIN/peridotd" ] && not_logged "curl" && ok

case_ "4. without --dev, PERIDOT_RELEASE_CHECKSUMS is ignored and said so; the checkout's own table is used"
# The download is a stub (tests/install/stubs/curl) that never matches a pin.
fresh; export PERIDOT_RELEASE_CHECKSUMS="$HOME/empty.tsv"; : >"$HOME/empty.tsv"
r=$( (cd "$ROOT" && ./dist/install.sh --prebuilt </dev/null >"$HOME/out.txt" 2>&1); echo $?)
expect "stopped" [ "$r" = 1 ] && said "Ignoring PERIDOT_RELEASE_CHECKSUMS (only honoured with --dev)" &&
expect "the real table decided" bash -c 'grep -qF "has no pinned checksum" "$1" || grep -qF "isn'"'"'t the pinned size" "$1"' _ "$HOME/out.txt" &&
expect "nothing written" [ ! -e "$BIN/peridotd" ] && ok
unset PERIDOT_RELEASE_CHECKSUMS

case_ "5. a signed pin table is checked with minisign before it is trusted, once the checkout carries the key"
sigcheck() { (cd "$ROOT" && source dist/lib.sh && RELEASE_CHECKSUMS="$HOME/pins.tsv" && check_pin_signature >"$HOME/out.txt" 2>&1); }
fresh; printf '# pins\n' >"$HOME/pins.tsv"; printf 'untrusted comment: x\nsig\n' >"$HOME/pins.tsv.minisig"
expect "no key yet: not checked, said" sigcheck && said "carries no signing key yet" && not_logged "minisign" && ok
key="RW$(head -c 40 /dev/zero | base64 | cut -c1-54)"
export PIN_SIGNING_PUBKEY="$key"
expect "good signature accepted" sigcheck && said "verified (minisign)" && logged "minisign -V -q -P $key -m $HOME/pins.tsv -x $HOME/pins.tsv.minisig" && ok
FAKE_MINISIGN_RC=1; export FAKE_MINISIGN_RC
expect "bad signature stops" not sigcheck && said "doesn't verify" && ok
unset FAKE_MINISIGN_RC; rm "$HOME/pins.tsv.minisig"
expect "unsigned table: said, checksums still stand" sigcheck && said "isn't signed" && ok
unset PIN_SIGNING_PUBKEY
# install.sh itself carries the (still empty) key and runs the check before reading pins.
fresh; printf '# pins\n' >"$HOME/pins.tsv"; printf 'x\n' >"$HOME/pins.tsv.minisig"; export PERIDOT_RELEASE_CHECKSUMS="$HOME/pins.tsv"
r=$( (cd "$ROOT" && ./dist/install.sh --dev --prebuilt </dev/null >"$HOME/out.txt" 2>&1); echo $?)
expect "said before pins" [ "$r" = 1 ] && said "carries no signing key yet" && said "has no pinned checksum" && ok
unset PERIDOT_RELEASE_CHECKSUMS

finish
