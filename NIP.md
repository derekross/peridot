# Peridot Sync and Pairing Protocol

`draft` `optional`

Version 2 of the protocol Peridot uses to keep a person's computers matching over Nostr relays, and to bring a new computer into the set. This document is written so that another client can interoperate with Peridot, or reuse the construction for its own private, multi-device sync. The reference implementation is the `peridot-sync` crate in this repository; where this text and the code disagree, the code is the specification and this text has a bug.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are to be interpreted as in RFC 2119. `‖` is byte concatenation. Hashes are SHA-256, HMAC is HMAC-SHA256, HKDF is HKDF-SHA256. Public keys in hashes and derivations are the 32-byte x-only form; hex is lower-case; big-endian integers are 8-byte `u64` unless stated.

## Contents

1. Overview
2. Keys
3. Item events
4. Reading: subscriptions, ingest and file status
5. The root event
6. Epochs: rotation, the window, removal
7. The legacy epoch and migration
8. Publishing
9. Relays
10. The daily audit
11. Pairing
12. The recovery kit and restore
13. Security considerations
14. Reference implementation

## 1. Overview

A person's computers share one Nostr **identity key** and one 32-byte **sync secret**. The secret is numbered in **epochs**: a new identity starts at epoch 1, and removing a computer, or rotating on request, mints epoch 2, 3, and so on. From an epoch's secret four things are derived:

- a **naming key**, so every item has an opaque keyed name;
- a **content key**, a NIP-44 conversation key that seals every item;
- a **signing key** `K_e`, which signs every item of the epoch, so relays never see the identity on sync traffic;
- a **rekey address** `R_{e+1}`, the Nostr key under which the rotation to the next epoch is announced.

Each computer also has a **device key** `D` that never leaves it. When an epoch is rotated, the next secret is delivered to each remaining computer encrypted to its device key. A removed computer gets no such delivery and stops.

The identity key signs exactly one event: the **root event**, which carries the current secret encrypted to the identity itself, so a computer that has only the identity key (a recovery kit) can rejoin.

All sync events are kind `30078` (NIP-78 application data). Pairing uses the ephemeral kind `21078`. Deletions use kind `5` (NIP-09). Relay authentication uses kind `22242` (NIP-42).

## 2. Keys

### 2.1 Identity key

A secp256k1 Nostr key pair. The client MAY hold it or MAY delegate to an external signer (Peridot uses Opal; the client then never sees the secret key and asks the signer for signatures and NIP-44 self-encryption). It signs the root event (section 5), and in the legacy epoch 0 only, items, deletions and relay logins (section 7).

### 2.2 Device key

A random secp256k1 key pair generated once per computer and kept in the local keyring. It MUST NOT leave the computer and MUST NOT sign public events. Its public key is published inside the computer's device entry (section 3.4) so that a rotating computer can wrap the next secret to it (section 6.2). A computer restored from a recovery kit generates a fresh device key: it is a new device.

### 2.3 Sync secret

```
SyncSecret = { epoch: u64, bytes: [u8; 32] }
```

`bytes` come from the operating system's random source. `FIRST_EPOCH` is 1. Epoch 0 denotes the first protocol version (section 7).

Text form, used in the keyring, in the root event, in rotation wraps and in the pairing transfer:

- epoch ≥ 1: `v2:<epoch in decimal>:<64 lower-case hex characters>`
- epoch 0: the 64 hex characters alone.

A parser MUST reject a `v2:` form whose epoch is below 1.

### 2.4 Derivation (epoch ≥ 1)

```
expand(S, label, epoch, out_len) =
    HKDF-SHA256( salt = "peridot/v2",
                 ikm  = S.bytes,
                 info = label ‖ 0x00 ‖ u64_be(epoch) )
```

| Derived value | Computation | Length | Use |
|---|---|---|---|
| naming key | `expand(S, "names", e, 32)` | 32 bytes | HMAC key for `d` tags (3.2) |
| content key | `expand(S, "content", e, 32)` | 32 bytes | used directly as a NIP-44 v2 conversation key (3.3) |
| signing key `K_e` | `scalar(S, "signer", e)` | secp256k1 key pair | signs every item and deletion of epoch `e` |
| rekey address `R_{e+1}` | `scalar(S, "rekey", e + 1)` | secp256k1 key pair | author of, and NIP-44 self key for, the rotation event to `e + 1` (6.2). It is derived from epoch `e`'s secret with the **next** epoch number in the info. |

`scalar(S, label, e)`: take `expand(S, label, e, 32)`; if the 32 bytes are not a valid secp256k1 secret key, retry with the label `"<label>/1"`, then `"<label>/2"`, and so on (the counter as decimal text) until they are.

### 2.5 Commitment

```
commitment(S) = hex( SHA256( "peridot/epoch-commit" ‖ u64_be(S.epoch) ‖ S.bytes ) )
```

64 hex characters. It proves which secret a rotation continues from without revealing it. It travels in the clear on the root event (section 5) and inside the rotation event (6.2).

### 2.6 Who signs what

| Event | Signed by |
|---|---|
| Items (files, chunks, device entries, state), epoch ≥ 1 | `K_e` of that epoch |
| Items, epoch 0 | the identity key |
| NIP-09 deletions of an epoch's items | `K_e` of that epoch; the identity key for epoch 0 |
| Rotation event to `e + 1` | `R_{e+1}` |
| Root event | the identity key |
| NIP-42 AUTH | `K_e` of the current epoch; the identity key in epoch 0 (9.2) |
| Pairing messages | one-time pairing keys (section 11) |

## 3. Item events

### 3.1 Shape

Every synced thing is an *item* sealed into one kind `30078` event:

```json
{
  "kind": 30078,
  "pubkey": "<K_e public key, hex>",
  "created_at": "<see 3.5>",
  "tags": [ ["d", "<name, 32 hex characters>"] ],
  "content": "<base64 of the NIP-44 v2 payload, see 3.3>"
}
```

No other tags are set: no `alt`, `t`, `p`, `k` or `expiration`. A reader MUST verify the signature, MUST ignore events of other kinds, and MUST ignore an event whose `created_at` is more than 600 seconds ahead of its own clock.

### 3.2 Names (the `d` tag)

```
name(label) = hex( HMAC-SHA256( key = naming key, msg = label )[0..16] )
```

32 hex characters. The labels are:

| Item | Label |
|---|---|
| file entry | `f:` ‖ path relative to the home directory, forward slashes (for example `f:.config/hypr/bindings.lua`) |
| chunk | `c:` ‖ sha256 hex of the chunk |
| device entry | `dev:` ‖ device id |
| state: current theme | `s:theme` |
| state: installed themes | `s:themes` |
| state: installed plugins | `s:plugins` |

A name is stable within an epoch and opaque to relays. It changes with every epoch because the naming key does.

### 3.3 Content

The plaintext is the UTF-8 JSON

```json
{"v": 1, "item": <Item, see 3.4>}
```

A reader MUST accept `v == 1` and MUST reject anything else.

For epoch ≥ 1 the plaintext is padded before encryption:

```
padded = u32_be(len(plaintext)) ‖ plaintext ‖ 0x00 …
```

filled to the smallest of the size classes `1024`, `4096`, `16384`, `32768` bytes that holds `4 + len`; if none does, exactly `4 + len` bytes with no fill. Epoch 0 items are not padded.

The padded bytes are encrypted with NIP-44 v2 and a fresh random 32-byte nonce under the **content key used directly as the conversation key**, not the ECDH conversation key of two Nostr keys. The NIP-44 payload (version byte, nonce, ciphertext, MAC) is base64 encoded (standard alphabet, padded) and is the event `content`. NIP-44's own padding applies on top of the size-class padding.

Reading: base64-decode, NIP-44 v2 decrypt with the content key, strip the padding (fail if shorter than 4 bytes or if the length prefix overruns), parse the JSON, check `v == 1`, then **recompute the name from the item's label and require it to equal the event's `d` tag**. An entry replayed under another path's name is refused. A payload that fails to decrypt is not ours and MUST be ignored silently.

### 3.4 Item payloads

`Item` is a JSON object with a discriminator `"t"`:

| `t` | Payload |
|---|---|
| `"file"` | a file entry, or a tombstone |
| `"chunk"` | one piece of a large file |
| `"device"` | a computer's entry |
| `"state"` | a piece of desktop state |

**File entry** (`"t": "file"`):

| Field | Type | Meaning |
|---|---|---|
| `path` | string | relative to home, forward slashes |
| `sha256` | string | hex sha256 of the contents; empty when `deleted` |
| `size` | integer | byte length; `0` when deleted |
| `base` | string, optional | sha256 of the version this change was made on top of (4.3); absent when unknown |
| `device` | string | id of the device that made the change |
| `deleted` | boolean | present and `true` only on a tombstone; omitted otherwise |
| `data` | string, optional | base64 (standard) of the contents; present when `size ≤ 16384` and not deleted |
| `chunks` | array of strings | sha256 hex of each chunk in order; present when the file is larger than 16384 bytes |

Exactly one of `data` and `chunks` is present on a live file; neither on a tombstone. `INLINE_MAX` is 16384 bytes.

**Chunk** (`"t": "chunk"`): `{"sha256": "<hex of this piece>", "data": "<base64 of the piece>"}`. Files over `INLINE_MAX` are split into consecutive pieces of `CHUNK_SIZE` = 20480 bytes, the last one shorter. A publisher MUST publish every chunk before, or together with, the entry that references it. A reader reassembles by concatenating the chunks in `chunks` order, checking each piece's sha256, then the whole's `size` and `sha256`. A missing or damaged piece is an error, never an empty file. Chunks are content-addressed by their label `c:<sha256>`, so identical pieces of different files share one event.

**Device entry** (`"t": "device"`):

| Field | Type | Meaning |
|---|---|---|
| `id` | string | device id: 8 random bytes, hex, created once per installation; the value used in file entries' `device` |
| `name` | string | human name, for example "Desk" |
| `version` | string | client version |
| `last_seen` | integer | Unix seconds when this entry was made |
| `removed` | boolean | present and `true` when the device was removed from the set |
| `pubkey` | string, optional | hex public key of the device key (2.2); absent on entries written by epoch 0 clients |

A computer whose entry lacks `pubkey` cannot receive a rotation and is left behind by the next one.

**State entry** (`"t": "state"`), discriminated by `"state"`:

| `state` | Fields |
|---|---|
| `"theme"` | `name` (string), `device` (string) |
| `"themes"` | `themes`: array of `{"name", "url"}`, `device` |
| `"plugins"` | `plugins`: array of `{"name", "url"}`, `device` |

A theme name MUST match `[a-z0-9-]{1,64}`. A plugin id MUST match `[A-Za-z0-9._-]{1,100}` and not start with `.` or `-`. A source `url` MUST start with `https://`, be shorter than 300 characters, contain no `@`, and contain only `A-Za-z0-9-._~/:%+`. A receiving client only *offers* to act on a state entry from another device; it MUST NOT run anything on receipt.

### 3.5 `created_at`

Items are addressable events; relays keep the newest per `(pubkey, kind, d)`. A publisher MUST date a replacement strictly after the version it replaces, whatever its clock says:

```
base = floor(now / 3600) * 3600      // epoch ≥ 1: dated to the hour
       now                            // epoch 0
t    = max(base, after + 1, last_issued + 1)
```

where `after` is the `created_at` of the newest known event under that name (0 if none) and `last_issued` is the last timestamp this process issued, so timestamps are strictly increasing per process. The hour floor hides when exactly a person works; the `+1` steps leak at most the number of edits within an hour.

Readers resolve ties deterministically: an incoming event replaces the stored one if its `created_at` is greater, or equal with a lexicographically smaller event id.

Because of the hour buckets, a catch-up query MUST look back at least `CATCH_UP_MARGIN` = 2 hours 10 minutes (7800 s) before the newest `created_at` it has seen.

### 3.6 Tombstones and stale chunks

A deletion is a file entry with `deleted: true`, an empty `sha256`, `size: 0` and neither `data` nor `chunks`, published under the same name, so it replaces the live entry. Chunk events that no live entry references are collected by the audit (section 10) once they are older than `CHUNK_GRACE` = 7 days and removed with a NIP-09 request.

## 4. Reading

### 4.1 Subscriptions

A client at epoch `e` subscribes for `kinds: [30078]` from the authors below, `since` a point in time, with **one request per author**: a relay that serves private data only to its logged-in author (9.2) answers a request naming several authors with a refusal, and asked apart it serves the epoch's own items and refuses the rest, which other relays carry.

1. `K_e`, its own epoch's signing key (the identity key when `e = 0`);
2. `K_{e-1}` while the previous epoch's window is open (6.3); the identity key when the previous epoch is 0;
3. `R_{e+1}`, the rekey address derived from the current secret, except when `e = 0`.

A catch-up fetches from `since = last_seen − 7800`. The live subscription opens from `now − 7800`. `last_seen` is the largest `created_at` of any item successfully opened.

The root event is fetched separately: `authors: [identity]`, `kinds: [30078]`, `#d: [root_name]` (section 5).

### 4.2 Ingest

For each received event a client MUST:

1. discard it unless the kind is 30078, the signature verifies, it has a `d` tag, and `created_at ≤ now + 600`;
2. if the author is `R_{e+1}`, treat it as a rotation announcement (6.2) and stop;
3. if the author is `K_e`, open it with the current keys; if the author is `K_{e-1}` during the window, open it with the previous keys **and** re-seal and re-queue it under the current keys so it survives the cleanup; otherwise discard it;
4. record its `created_at` as `last_seen` if larger;
5. for a file entry, discard it unless the path is one the client's own manifest could ever sync (4.4). A path in the never or local tier is refused even inside a validly signed event;
6. store it with the newest-wins rule of 3.5: a file entry replaces the stored one for its path only when newer, or equal with a smaller event id; chunks are stored by hash; device and state entries by id and variant.

### 4.3 File status

Each computer remembers, per path, the hash it last agreed on with the others, `synced` (absent if never). Given `local` (the hash of the file here, absent if missing), `synced`, and the newest remote entry (`theirs` = its `sha256`, absent for a tombstone; `device` = who published it), the first matching row decides:

| Condition | Status |
|---|---|
| no remote entry, local file exists | outgoing |
| no remote entry, no local file | in sync |
| `local == theirs` | in sync |
| `remote.device == this device` | outgoing: our own last publish, changed since |
| `synced` present and `synced == theirs` | outgoing: they have not moved since we agreed, we have |
| `synced` absent, or `local == synced` | incoming: nothing changed here since we agreed, or we never did; take theirs, after asking |
| otherwise | conflict |

A conflict MUST be left alone until the user resolves it. "Keep mine" records `theirs` as the agreed hash and republishes the local version, so it becomes the newer change everywhere; "use theirs" applies the remote version explicitly. After an undo, a remote version the user chose not to take is remembered as *kept* and shown as such instead of incoming, until either side changes the file.

When publishing, `base` is set to the remote entry's hash if one exists, else to `synced`.

### 4.4 Tiers

The wire carries no tier; the receiving client decides from its own manifest. The reference tiers are *shared* (synced by default), *ask* (off by default, can be turned on; these files can run commands), *local* (never synced) and *never* (secrets and other programs' private data; cannot be turned on). A path that is not clean and relative, that is `..`, absolute or otherwise escaping home, is *never*. Never takes precedence over local, local over ask, ask over shared. On the wire only shared and ask paths are accepted (4.2 step 5). The reference patterns:

| Tier | Patterns |
|---|---|
| shared | `.config/hypr/*.lua`, `.config/hypr/*.conf`, `.config/omarchy/shell.json`, `.config/omarchy/extensions/**`, `.config/omarchy/branding/**`, `.config/omarchy/themed/**`, `.config/alacritty/**`, `.config/foot/**`, `.config/ghostty/**`, `.config/kitty/**`, `.config/btop/btop.conf`, `.config/starship.toml`, `.config/tmux/**`, `.config/lazygit/config.yml`, `.config/mpv/*.conf`, `.XCompose` |
| ask | `.config/hypr/autostart.lua`, `.bashrc`, `.config/omarchy/hooks/**`, `.config/nvim/**`, `.config/git/config`, `.config/mimeapps.list` |
| local | `.config/hypr/monitors.lua`, `.config/hypr/input.lua`, `**/*.bak`, `**/*.bak.*`, `**/*.orig`, `**/*~`, `**/.luarc.json` |
| never | `.ssh/**`, `.gnupg/**`, `.netrc`, `.npmrc`, `.pypirc`, `.docker/config.json`, `.aws/**`, `.kube/**`, `.config/sops/**`, `.password-store/**`, `.local/share/keyrings/**`, `.config/gh/**`, `.config/rclone/**`, `.config/vdirsyncer/**`, `.config/khal/**`, `.config/khard/**`, `.config/opencode/**`, `.config/opal/**`, `.config/peridot/**`, `.config/Signal/**`, `.config/BraveSoftware/**`, `.config/chromium/**`, `.config/google-chrome*/**`, `.config/microsoft-edge*/**`, `.config/omarchy/plugins/**`, `.config/omarchy/themes/**`, `**/*.env`, `**/.env*`, `**/*token*`, `**/*secret*`, `**/*password*`, `**/*credential*`, `**/*.pem`, `**/*.key` |

A content scanner additionally refuses to publish files whose contents look like a secret (private key blocks, `nsec1`, well-known API token shapes).

### 4.5 Files that run commands

Paths in the *runs-commands* set (`.config/hypr/*.lua`, `.config/hypr/*.conf`, `.config/hypr/autostart.lua`, `.config/omarchy/extensions/**`, `.config/omarchy/hooks/**`, `.bashrc`, `.config/kitty/**`, `.config/ghostty/**`, `.config/alacritty/**`, `.config/tmux/**`, `.config/starship.toml`, `.config/lazygit/config.yml`, `.config/mpv/**`, `.config/nvim/**`) are files something on the receiving computer executes, sources or evaluates. A client MUST NOT apply them without the user seeing them first. The reference's optional auto-apply excludes them.

### 4.6 Applying

To apply a remote version: back up the current file (an owner-only folder per apply, the 20 most recent kept), write the assembled content beneath home with symlinks refused, atomically, at mode 0644 with no exec bit, and record the remote hash as the agreed hash. A tombstone removes the file and records "agreed: absent". Every apply is recorded in a history the user can undo; undo restores the backups and marks the restored versions as kept (4.3).

## 5. The root event

The one event under the identity key that says "this key uses Peridot". It lets a computer that has only the identity key find the current sync secret.

```
{
  "kind": 30078,
  "pubkey": <identity public key>,
  "tags": [ ["d", root_name], ["c", commitment(S_e)] ],   // "c" is absent for epoch 0
  "content": NIP-44 v2( identity → identity, body )
}
root_name = hex( SHA256( "peridot/root" ‖ identity pubkey )[0..16] )
body      = {"v": 2, "sync_secret": "v2:<e>:<hex>"}        // epoch ≥ 1
            {"v": 1, "sync_secret": "<hex>"}                // epoch 0
```

The content is standard NIP-44 v2 from the identity key to itself (the ECDH self conversation key), produced by whoever holds the identity key, the client or the external signer. It is not size-class padded. Like items, a root MUST be dated strictly after the newest root the publisher knows the relays to hold: `created_at = max(now, previous + 1)`. The root is addressable, and a replacement landing in the same second as the root it replaces could otherwise lose the relay's tie-break, leaving a restore with a stale secret.

Reading: the reader MUST check that `pubkey` is the expected identity, that the `d` tag equals `root_name(identity)` and that the signature verifies; decrypt; require `v ≤ 2`; parse the secret text; and require `(v == 1) ⇔ (epoch == 0)`. Among several roots the newest `created_at` wins.

The root is published when an identity is created, after every rotation (it names the new epoch), and by the audit when no relay shows it or it is older than 30 days (section 10). A restored computer takes the secret from the root, generates a new device key, and MUST publish its own device entry so that it receives the next rotation.

## 6. Epochs

### 6.1 When an epoch changes

A rotation happens when a computer is removed, on request, and once at migration from epoch 0 (section 7). Before rotating, the rotating computer MUST hold every chunk that current entries reference, because it is about to re-say everything under the new keys, and MUST refuse to remove itself.

### 6.2 The rotation event

```
{
  "kind": 30078,
  "pubkey": R_{e+1} public key,
  "tags": [ ["d", "0"] ],
  "content": NIP-44 v2( R_{e+1} → R_{e+1}, body )     // ECDH self key of the rekey pair
}
body = {
  "v": 2,
  "epoch": e + 1,
  "prev": e,
  "prevcommit": commitment(S_e),
  "rotator": <hex pubkey of the rotating computer's device key D_rot>,
  "wraps": [ { "loc": <32 hex>, "w": <NIP-44 payload> }, … ]
}
loc = hex( SHA256( "peridot/locator" ‖ D_rot pubkey ‖ D_i pubkey ‖ u64_be(e + 1) )[0..16] )
w   = NIP-44 v2( D_rot → D_i, "v2:<e+1>:<hex of S_{e+1}>" )
```

There is one wrap per **remaining** device: every device entry that is not `removed`, not in the removal list, that has a `pubkey`, and is not the rotator itself. The event has `d = "0"`, so there is exactly one rotation event per rekey address and relays replace it. Every current device already listens for `R_{e+1}` (4.1), because it is derivable from `S_e`. The rotation event is published before the rotator switches its own keys, so a crash before the switch leaves everything as it was.

Adopting. A device receiving an event from `R_{e+1}` MUST: check that the author equals its own derived `R_{e+1}` and that the signature verifies; decrypt with `R_{e+1}`'s self key; require `v == 2`, `prev == e`, `epoch == e + 1` and `prevcommit == commitment(S_e)`, else discard (it continues from a secret this device does not hold: a forgery or a fork); compute `loc` for `(rotator, own device pubkey, e + 1)`; if a wrap with that `loc` exists, decrypt `w` with the device key against the rotator's pubkey, parse the secret and require its epoch to be `e + 1`. Then **adopt**: keep `S_e` as the previous secret, store `S_{e+1}` as current, open the window (6.3), and restart with the new keys. If no wrap matches, the device has been **removed**: it MUST stop syncing and tell the user to pair again. Its files stay as they are. A removed device can open the envelope, since it holds `S_e`, and count the wraps, but none opens for it.

A client whose stored epoch is already at or past the announced one ignores the announcement. Two rotations racing from the same epoch: a client that has seen one and receives another keeps the one whose secret bytes compare lower, and a "removed" verdict is replaced by any "adopt". A computer that missed several rotations walks forward one epoch at a time: adopting epoch `n` derives `R_{n+1}`, on which the next announcement is found. This relies on relays still holding each intermediate rotation event; they have `d = "0"` under distinct addresses and are never deleted by the client.

### 6.3 After the switch

The rotator, and for step 3 each adopting device:

1. publishes the root event with the new secret (section 5);
2. re-publishes every item it knows (all file entries, their chunks, the three state entries, every device entry, including entries marked `removed: true` so the directory shows the removal) sealed and named under the new keys, each dated after the newest `created_at` it knew for that item;
3. keeps the previous secret for a **window** of 7 days, during which it still subscribes to `K_e` and re-seals anything that arrives under it (4.2 step 3); when the window closes, it fetches every kind 30078 event authored by `K_e` and sends NIP-09 kind 5 events signed by `K_e` (the identity key when `e = 0`) with the tag `["k", "30078"]` and one `["a", "30078:<K_e hex>:<d>"]` per event, at most 100 coordinates per deletion event; then it forgets `S_e`.

A device that adopted does step 3 only; its items were re-said by the rotator.

The rotator persists the new secret, the previous secret, the window's end and "republish pending" and "root pending" flags before restarting, so a crash at any point restarts into the new epoch and finishes the rest.

### 6.4 Device removal

Removing a computer is a rotation whose removal list names it. Its device entry is republished with `removed: true`; it receives no wrap, sees the announcement and stops. The previous epoch's copies, which it could still read, are deleted after the window. A client MUST tell the user the honest limit: what the removed computer already decrypted stays on it, and nothing can wipe it from afar. "Stop syncing here" on a computer forgets the local identity and state instead of going through this path.

## 7. The legacy epoch and migration

Epoch 0 is the first protocol version. Version 2 clients read it and never write it. Its differences:

| | Epoch 0 | Epoch ≥ 1 |
|---|---|---|
| KDF | `HKDF-SHA256(salt = "peridot/v1", ikm = S)`, info `"names"` or `"content"`, no epoch, no NUL | 2.4 |
| signing key | none: items, deletions and relay logins are signed by the identity key | `K_e` |
| padding | none | size classes |
| `created_at` | `max(now, after + 1, last_issued + 1)` | hour floor |
| rekey address | none; an epoch 0 client does not listen for rotations | `R_{e+1}` |
| root | `{"v": 1, "sync_secret": "<hex>"}`, no `c` tag | section 5 |
| device `pubkey` | absent | present |

`R_1 = scalar(S_0, "rekey", 1)` is derived with the version 2 salt.

**Migration.** A version 2 client that starts at epoch 0 first catches up, then fetches the root:

- the root has a `c` tag: another computer already migrated, and this one never received epoch 1, because epoch 0 device entries carry no `pubkey`, so no wrap could be made for it. It MUST stop and ask the user to pair again. Its local settings are untouched.
- the root has no `c` tag, or no root exists: this computer rotates `0 → 1` with an empty removal list (section 6): it announces under `R_1` with wraps only for devices that have a `pubkey` (typically none), switches to `S_1`, publishes the version 2 root, re-says everything under `K_1`, and after the 7-day window deletes the epoch 0 events with kind 5 requests signed by the identity key.

## 8. Publishing

### 8.1 Outbox and retry

Every event a client produces is written to a durable outbox before it is sent, so nothing is lost across a restart. The outbox is keyed by event id and, for addressable events, by coordinate `kind:pubkey:d`; queueing a newer event for the same coordinate drops the older queued one. A flush sends up to 50 due events, oldest first, to every configured relay. A send counts as successful when at least one relay accepts the event. An event no relay accepted is retried after `30 · 2^min(attempts, 6)` seconds, at most 30 minutes.

### 8.2 Never before catching up

A client MUST NOT publish any item until it has completed at least one successful fetch under the current identity. A brand-new identity has nothing to catch up on and may publish at once. This is what stops a freshly paired computer from pushing its default files over the user's real settings. State entries follow the same rule; the device entry does not.

### 8.3 What a change publishes

For each path the manifest covers, the client compares the local hash, `synced` and the newest remote entry (4.3) and publishes only paths whose status is *outgoing*. The entry carries `base` and is dated after the remote entry's `created_at` (3.5). A missing local file whose path has a remote entry publishes a tombstone. After queueing, the client records the local hash as `synced`, so the change does not look outgoing again when it echoes back from the relay.

State entries are published when the local value differs from the last agreed value, or when no computer has published that entry yet. Each computer publishes its device entry at setup, after every debounced publish, and at least daily as a heartbeat (`last_seen`).

### 8.4 Cadence (implementation note)

The reference daemon's timers; none is required for interoperability:

| What | When |
|---|---|
| Publish local changes | 3 s after the last file-system change in a burst; also on every catch-up and on "sync now" |
| Flush the outbox | every 60 s, plus after every publish |
| Catch up | every 15 min, and once before anything is published |
| Device entry heartbeat | every 24 h |
| Audit (section 10) and window cleanup (6.3) | every 24 h; the audit also at startup when the last one is older than a day |

## 9. Relays

### 9.1 Kinds

A relay used for sync MUST accept:

| Kind | Used for | Signed by |
|---|---|---|
| 30078 | every item, and the root event; addressable, one `d` tag | `K_e`; the root by the identity key; everything by the identity key in epoch 0 |
| 21078 | pairing messages (section 11); ephemeral, relays SHOULD NOT store them | one-time pairing keys |
| 5 | NIP-09 deletions: a `k` tag of `30078` and up to 100 `a` tags of the form `30078:<pubkey>:<d>` | `K_e` of the epoch that published the items; the identity key for epoch 0 |
| 22242 | NIP-42 authentication, when a relay asks | `K_e`; the identity key in epoch 0 (9.2) |

A full Peridot installation also uses kinds outside this protocol (24242 Blossom authorizations for private links; 0, 3, 7, 13, 14, 17, 1059, 1111, 1985, 10050 and 30490 for the profile, follows, Gallery and private messages). A client that only syncs needs only the four above.

### 9.2 Authentication

A client MUST support NIP-42. When a relay sends an `AUTH` challenge, the client answers with a kind 22242 event carrying the standard `challenge` and `relay` tags, signed by the current epoch's signing key `K_e`, so a relay that requires logins learns no more about the identity than one that does not. In epoch 0 there is no `K_e` and the identity key logs in, as it signs everything else in that epoch. The root event, signed by the identity, is published over the same session. A relay that refuses events whose author differs from the logged-in key, beyond NIP-70 protected events, would refuse the root event and the previous epoch's deletions after a rotation; such a relay is not supported.

A relay MAY serve kind 30078 only to a session logged in as the author (relay.ditto.pub does). Such a relay serves the epoch's items and the previous epoch's re-sealed items, but not the rotation announcement (authored by the rekey address) nor, to the epoch key's session, the identity's root; a client MUST therefore keep at least one relay that serves rotation events, and the audit does not hold the root's absence against any one relay (section 10).

One login is made by the identity: the lookup of the root event on a computer that does not yet hold a sync secret (setup with an existing key, restore, 12.2), since no other key exists there yet. That lookup asks each relay separately and treats a relay that refused or did not answer as unknown, never as empty: a new sync secret is minted only when at least one relay answered and none had a root.

### 9.3 Sizes, retention, defaults

Items are padded to at most 32 KiB before base64 and chunks are 20 KiB, so every sync event is well under 64 KiB and a relay with the common 64 KiB message limit suffices. A relay SHOULD retain kind 30078 events indefinitely; the protocol tolerates relays that drop them (section 10), at the cost of a republish. NIP-11 is not consulted.

The reference default relay list:

```
wss://relay.ditto.pub
wss://auth.nostr1.com
wss://relay.primal.net
wss://relay.damus.io
```

Every event goes to every relay. Per-relay gaps are found and filled by the audit.

## 10. The daily audit

Once a day, and at startup when the last audit is older than a day, a client SHOULD check every relay for everything that should be there. The expected set is every current file entry, every chunk a live entry references, the three state entries, every device entry, and the root event.

1. For each relay separately, fetch the full item filter (`authors: [K_e]`, `since: 0`) and the root filter. A relay that does not answer within 15 s is unreachable. If no relay answers, the audit fails.
2. For each event with a `d` tag by `K_e`, or by the identity for the root, record the newest `created_at` per `d` and which relays hold it. Events under a `d` that is not expected are candidates for removal. Every event is also ingested, since a relay that was down during a catch-up may hold news.
3. Re-send every expected item that at least one reachable relay lacks, and re-publish every expected item whose newest copy anywhere is older than `REFRESH_AFTER` = 30 days, dated after the newest copy so it replaces rather than resurrects. Other computers' device entries are exempt from the age rule.
4. Re-publish the root when no relay that could be asked shows it, or when it is older than 30 days. A relay that serves only its logged-in author (9.2) cannot show the root to the epoch key's session, and an empty answer is indistinguishable from a refusal, so the root is not counted against any one relay. Re-publishing needs the identity signer; if it is unavailable, the root is left as is.
5. Among the unexpected names older than `CHUNK_GRACE` = 7 days, those that decrypt to a chunk no live entry references are stale. Delete them with kind 5 events (9.1) in batches of 100, signed by `K_e`.

Implementation note: the reference sends the deletions on its own when the identity key is on the computer, and waits for the user when Opal holds the key, because Opal treats deletions as sensitive.

## 11. Pairing

Pairing hands the current sync secret, and optionally the identity's key, from a computer that already syncs (the **sponsor**) to a new one (the **joiner**) over relays, using one-time keys and a short code the person carries between the two screens. Both people confirm a six-digit number before anything secret moves. The construction is commit-then-reveal over a Diffie-Hellman exchange bound to the code, so someone who glimpsed the code and interposes gets one guess in a million and nothing to grind.

### 11.1 Roles and keys

| Name | Held by | Lifetime | Purpose |
|---|---|---|---|
| `code` | typed by the person | 5 minutes, one use | 10 random bytes (80 bits); derives the meeting point and authenticates the first message |
| `M` | derived from `code` | the pairing | the **meeting point**, a Nostr key anyone with the code can derive; the sponsor addresses its hello to it |
| `S` | sponsor | one-time | the sponsor's ephemeral Nostr key; signs and encrypts every sponsor message |
| `E` | joiner | one-time | the joiner's ephemeral Nostr key; signs and encrypts every joiner message |
| `D` | joiner | permanent | the joiner's device key (2.2); the sync secret is encrypted to it |
| `n_s`, `n_e` | sponsor, joiner | the pairing | 32-byte nonces, each fixed before its side sees the other's |
| `K0` | both | the pairing | session key from the code and ECDH(S, E); authenticates every message after the hello |

A joiner MUST create a fresh `E` and a fresh code for every pairing. A sponsor MUST create a fresh `S` for every pairing. `D` is generated once per computer and reused.

### 11.2 The code

The code is 10 random bytes shown as 16 characters of Crockford base32 (alphabet `0123456789ABCDEFGHJKMNPQRSTVWXYZ`, no I, L, O or U), most significant bits first, in four groups of four with a `PDT-` prefix:

```
PDT-XXXX-XXXX-XXXX-XXXX
```

Treat the 10 bytes as an 80-bit big-endian integer; character `i`, for `i` from 15 down to 0, is `ALPHABET[(bits >> (5·i)) & 31]`. There is no checksum and no version field; the code carries nothing but the secret. The QR code on the joiner is the display string itself.

Parsing MUST drop whitespace and `-`, upper-case, strip a leading `PDT`, require exactly 16 characters, and map `O` to `0` and `I` and `L` to `1` before looking characters up. Anything else is a bad code.

### 11.3 Derivations

**Meeting point `M`.**

```
prk  = HKDF-Extract(salt = "peridot/pair", ikm = code)
sk_M = HKDF-Expand(prk, info = "meeting" ‖ counter, 32 bytes)
```

`counter` is one byte starting at 0, incremented until the output is a valid secp256k1 secret key. `M` is the key pair for `sk_M`.

**Hello MAC**, the only thing the code itself authenticates, before there is a shared key:

```
hello_mac = hex( HMAC(key = code, msg = "hello" ‖ S_pub ‖ commit) )
```

**Commitment**, made by the sponsor before it sees `E`:

```
commit = SHA256( "peridot/pair/commit" ‖ S_pub ‖ n_s )
```

**Session key `K0`.**

```
shared = NIP-44 v2 conversation key of (S, E)
prk    = HKDF-Extract(salt = "peridot/pair/v2", ikm = shared ‖ code)
K0     = HKDF-Expand(prk, info = S_pub ‖ E_pub, 32 bytes)
```

Both sides compute the same `K0`. Without the code, or without one of the two ephemeral private keys, nothing after the hello can be read, forged or checked.

**Transcript `T`**, everything both sides said, in order:

```
T = SHA256( "peridot/pair/transcript"
          ‖ commit ‖ S_pub ‖ E_pub ‖ D_pub ‖ n_s ‖ n_e
          ‖ u32_be(len(name_s)) ‖ name_s
          ‖ u32_be(len(name_j)) ‖ name_j )
```

`name_s` and `name_j` are the device names as sent on the wire (UTF-8, not the cleaned display form).

**Message MAC**, on every message after the hello:

```
mac(label, parts…) = hex( HMAC(key = K0, msg = label ‖ T ‖ parts[0] ‖ parts[1] ‖ …) )
```

For the `reply`, `T` is not complete yet, so 32 zero bytes stand in for it.

**The six-digit number.**

```
d = HMAC(key = K0, msg = "sas" ‖ T)
n = u32_be(d[0..4]) mod 1 000 000
```

shown as two groups of three digits. MAC comparisons MUST be constant time.

### 11.4 Transport

Every message is one Nostr event:

| Field | Value |
|---|---|
| `kind` | `21078` (ephemeral range) |
| `pubkey` | the sender's one-time key, `S` or `E` |
| `tags` | exactly one `["p", "<recipient hex>"]` |
| `content` | NIP-44 v2 encryption of the JSON message from the sender's one-time key to the recipient key |
| `sig` | by the sender's one-time key |

The hello is addressed to `M`; every later message is addressed to the other side's one-time key. A receiver MUST verify the event signature, decrypt with its own key against the event's `pubkey`, parse the JSON, and reject any message whose `v` is not `2`.

Both sides use the same relay list the sync engine is configured with. Each opens its own short-lived client, subscribes with `since = now − 30 s`, and counts a send as successful when at least one relay accepts it. The sponsor MUST subscribe before sending its hello, because the joiner answers within a second. Filters: the joiner subscribes to `kind 21078`, `#p` in `[M_pub, E_pub]`; the sponsor to `kind 21078`, `#p = S_pub`.

### 11.5 Messages

JSON objects discriminated by `t`. All hex is lower-case. Every message carries `"v": 2`.

**hello** (sponsor → M)

| Field | Type | Meaning |
|---|---|---|
| `t` | `"hello"` | |
| `v` | `2` | |
| `name` | string | sponsor's device name |
| `commit` | hex(32) | `SHA256("peridot/pair/commit" ‖ S_pub ‖ n_s)` |
| `mac` | hex(32) | `HMAC(code, "hello" ‖ S_pub ‖ commit)` |

The sponsor generates `S` and `n_s`, computes `commit` and sends. `S_pub` is the event's `pubkey`.

**reply** (joiner → S)

| Field | Type | Meaning |
|---|---|---|
| `t` | `"reply"` | |
| `v` | `2` | |
| `name` | string | joiner's device name |
| `device` | hex(32) | `D_pub`, the joiner's permanent device key |
| `nonce` | hex(32) | `n_e` |
| `mac` | hex(32) | `mac("reply", commit, D_pub, n_e, name)` with `T` = 32 zero bytes |

Before replying the joiner MUST check the hello's `mac` against the code, then generate `n_e` and derive `K0`. `E_pub` is the event's `pubkey`.

**reveal** (S → E)

| Field | Type | Meaning |
|---|---|---|
| `t` | `"reveal"` | |
| `v` | `2` | |
| `nonce` | hex(32) | `n_s` |
| `mac` | hex(32) | `mac("reveal", n_s)` over the full transcript |

On receipt the joiner MUST check that `SHA256("peridot/pair/commit" ‖ S_pub ‖ n_s)` equals the hello's `commit`, so the sponsor could not change its nonce after seeing `E`, then compute `T` and check the MAC. Both sides now show the number.

**confirm** (E → S)

| Field | Type | Meaning |
|---|---|---|
| `t` | `"confirm"` | |
| `v` | `2` | |
| `mac` | hex(32) | `mac("confirm-j")` |

Sent only after the person at the joiner answered yes to "do the numbers match?".

**transfer** (S → E)

| Field | Type | Meaning |
|---|---|---|
| `t` | `"transfer"` | |
| `v` | `2` | |
| `pubkey` | hex(32) | the identity's public key |
| `wrap` | string | NIP-44 v2 ciphertext from `S` to `D` of the sync secret's text form (2.3) |
| `key_wrap` | string, optional | NIP-44 v2 ciphertext from `S` to `D` of the identity's secret key (64 hex); present only when the sponsor holds the key and the person chose "also keep the key on the new computer" |
| `mac` | hex(32) | `mac("transfer", pubkey, wrap, key_wrap-or-empty)`, each part as its UTF-8 bytes |

The sponsor MUST NOT send `transfer` until both its own person's yes and the joiner's `confirm` are in, in either order. On receipt the joiner MUST check the MAC, decrypt `wrap` with `D` against `S_pub`, parse the secret, and if `key_wrap` is present decrypt it and check that its public key equals `pubkey`. The resulting identity is `{pubkey, key if key_wrap, secret, device D}`.

**done** (E → S)

| Field | Type | Meaning |
|---|---|---|
| `t` | `"done"` | |
| `v` | `2` | |
| `mac` | hex(32) | `mac("done")` |

Sent by the joiner once it has stored the identity. The sponsor checks the MAC and ends.

### 11.6 State machines

**Joiner:** `Waiting → Replied → ShowNumber → Confirmed → Ended`.

- `Waiting` accepts only a hello addressed to `M` whose `mac` verifies; it replies and records `S_pub`, the sponsor's name, `commit`, `n_e` and the time.
- `Replied` accepts only a reveal from the recorded `S_pub`, within `REVEAL_WINDOW` = 60 s of the reply. A commitment mismatch or a bad MAC ends the pairing.
- `ShowNumber` accepts nothing until the person answers. No ends it, and the code is burnt. Yes sends `confirm`.
- `Confirmed` accepts only a transfer from `S_pub`; a bad MAC ends it. It sends `done`, ends, and yields the identity.
- In any state after `Waiting`, a second valid hello (addressed to `M`, `mac` verifies, from a key other than the recorded sponsor) ends the pairing with "another computer answered this code". The pairing stops visibly instead of picking a winner.

**Sponsor:** `Started → Revealed → Sent → Ended`.

- `Started` accepts only a reply within `REPLY_WINDOW` = 120 s of its hello; derives `K0`, checks the reply MAC, builds `T`, sends `reveal`, shows the number and the joiner's name.
- `Revealed` waits for the person's answer and the joiner's `confirm`, in any order. No ends it. A reply from a different `E` ends it with "another computer answered this code". Once both are in, it sends `transfer`.
- `Sent` accepts only `done` from the joiner, and ends.

Terminal states MUST zeroize the code and the one-time key. The whole pairing expires `CODE_LIFETIME` = 300 s after it started, the joiner counting from code generation and the sponsor from its hello, checked on every message and by a deadline. Twenty messages that fail to verify abort the side that received them.

Device names received from the other computer are cleaned for display: control characters become spaces, whitespace collapses, at most 40 characters, empty becomes "a computer". The transcript uses the uncleaned wire form.

### 11.7 Limits and failure modes

Enforced by the reference daemon around the protocol; other implementations SHOULD match them.

| Rule | Value |
|---|---|
| Code lifetime | 5 minutes, one use |
| Reply window (sponsor waits for the reply) | 120 s |
| Reveal window (joiner waits for the reveal) | 60 s |
| Bad messages before aborting | 20 |
| Sponsor starts per computer | at most 5 per 15 minutes |
| Cooldown after a no or an abort | 30 s |
| Code reuse | never: the sponsor records `sha256(uppercased typed code)` and refuses a repeat |
| Concurrent pairings | one per computer; a computer that is already set up does not join |

Replay: every message after the hello is bound to `K0`, which needs both one-time private keys and the code, and to the transcript; each side accepts each message type only in the state that expects it, from the recorded peer key. `E` and `S` are discarded at the end, so a captured transfer cannot be opened later; the sync secret is encrypted to `D`, never to `E`.

**Sponsor's identity held by an external signer.** The sponsor can offer `key_wrap` only when it holds the identity key itself. With Opal holding the key, the transfer carries `wrap` only, and the joiner receives an identity without a key. Such a joiner MUST have Opal installed with an account for `pubkey`; it pairs with that Opal before saving, and fails otherwise. A computer that is to hold the key itself can instead restore from the recovery kit (section 12).

**Hold key.** Off by default. When on, and the sponsor holds the key, the identity's secret key travels in `key_wrap` and the joiner holds it from then on.

The joiner announces its own device entry, with `D_pub`, when its engine starts after pairing; the sponsor picks it up on its next sync.

### 11.8 Sequence

```
 person            joiner (new, keys E, D)         relays          sponsor (existing, key S)         person
   |   start pairing   |                              |                    |                            |
   |------------------>| code = 10 random bytes       |                    |                            |
   |  shows PDT-code   | M = meeting(code)            |                    |                            |
   |<------------------| subscribe #p in [M, E]       |                    |                            |
   |  carries the code to the other screen ..........................................................> |
   |                   |                              |                    |  type code                 |
   |                   |                              |                    |<---------------------------|
   |                   |                              |                    | S, n_s; commit             |
   |                   |                              |                    | subscribe #p = S           |
   |                   |                              |   hello  (S -> M)  |  {name, commit, mac(code)} |
   |                   |<-----------------------------|<-------------------|                            |
   |                   | check hello mac              |                    |                            |
   |                   | n_e; K0                      |                    |                            |
   |                   |   reply (E -> S) {name, D, n_e, mac(K0)}          |                            |
   |                   |----------------------------->|------------------->| check mac; K0; T           |
   |                   |                              |   reveal (S -> E)  |  {n_s, mac(K0,T)}          |
   |                   |<-----------------------------|<-------------------|                            |
   |                   | commit == H(S, n_s)? T; mac  |                    |                            |
   |  shows 6 digits   |                              |                    |  shows 6 digits            |
   |<------------------|                              |                    |--------------------------->|
   |  yes              |                              |                    |  yes (+ hold key?)         |
   |------------------>|                              |                    |<---------------------------|
   |                   |   confirm (E -> S) {mac}     |                    |                            |
   |                   |----------------------------->|------------------->| both yes?                  |
   |                   |                              | transfer (S -> E)  |  {pubkey, wrap=NIP44(S->D, |
   |                   |                              |                    |   secret), key_wrap?, mac} |
   |                   |<-----------------------------|<-------------------|                            |
   |                   | check mac; open wrap with D  |                    |                            |
   |                   | save identity; start engine  |                    |                            |
   |                   |   done (E -> S) {mac}        |                    |                            |
   |                   |----------------------------->|------------------->| end; nudge sync            |
   |  "paired"         | zeroize code, E              |                    |  "done"                    |
```

## 12. The recovery kit and restore

### 12.1 The kit

A recovery kit is the identity's secret key encrypted with NIP-49 under a password of six words:

| Field | Value |
|---|---|
| Encoding | NIP-49 `ncryptsec1…` (bech32) |
| KDF | scrypt, `log_n = 18` |
| Key security byte | `0x01` |
| Password | the six words, lower-case, joined with `-`, for example `canopy-glider-sulfur-mammal-dizzy-overlap` |
| Word list | the EFF long word list without its four hyphenated entries (`drop-down`, `felt-tip`, `t-shirt`, `yo-yo`): 7,772 words, lower-case ASCII |
| Word choice | uniform, by rejection sampling on 32 random bits per word; about 77 bits of entropy in six words |

When the words are typed back, a client MUST accept any case and any of space, `-` or `,` as separators, MUST require exactly six words, and MUST reject a word not in the list. The kit is a printable page holding the `ncryptsec` as text and as a QR code, and a blank box for the words; the words MUST NOT be written into the file. Anyone with the page and the words can read every synced setting.

A kit can only be made where the identity key is present. When an external signer holds the key, that signer's own backup is the kit.

### 12.2 Restore

On a computer that is not yet set up:

1. Open the kit: decode the `ncryptsec` and decrypt it with the normalized words, yielding the identity key.
2. Fetch the root event from each configured relay separately (section 5), logging in as the identity where a relay asks (9.2); take the newest by `created_at`. If no relay answered, fail without concluding anything. If relays answered but none has a root, fail with "settings not found"; a restore never starts a fresh identity.
3. Open the root (section 5) and take the sync secret. `v == 1` means the legacy epoch.
4. Generate a new device key: a restored computer is a new device, not the one that made the kit.
5. Save the identity (key, sync secret, device key) and start syncing normally: catch up first, then announce the device entry with its device pubkey, so it receives the next rotation. On epoch 0 the migration of section 7 follows.

The same lookup is used when a user imports a key by hand (`nsec`, hex or `ncryptsec`) or chooses an identity held by an external signer: a key with a root on the relays joins its existing settings; only a definite "no root anywhere" starts a new sync secret.

### 12.3 Moving the key into an external signer

Handing the identity key to Opal changes nothing on the wire: the identity pubkey, sync secret, epoch and device key stay, the local copy of the key is replaced by a "held by Opal" marker, and the identity signer becomes Opal. Other computers are unaffected. The handoff itself reuses the kit format.

## 13. Security considerations

**Threat model.** Relays are untrusted storage: they may read everything they store, drop it, delay it or serve stale copies, and they may be publicly readable. Anyone who can read the network may see pairing traffic. Code running as the same user on the same computer is out of scope: it can read the keyring, the socket and the files.

**What the design provides.**

- *Confidentiality of settings.* Every item is encrypted under a key derived from the sync secret before it leaves the computer; names are keyed hashes; sizes are padded to four classes and timestamps rounded to the hour. A relay sees that some Peridot syncs a few blobs an hour, not whose, what or how big.
- *Unlinkability of sync traffic and identity.* Items and deletions are signed by the epoch signing key, not by the identity. Relay logins are signed by the epoch key too. Only the root event is signed by the identity, and it reveals nothing but that the key uses Peridot.
- *Integrity.* Every event is signature-checked, decrypts only under the right keys, and a file entry names its path inside the ciphertext, so a relay cannot forge, splice or re-path an item. Replays are bounded by newest-wins on `created_at` with a deterministic tie-break, and by the 600 s future limit.
- *Forward removal.* A removed computer cannot read anything published after the rotation, since it never receives the new secret, and the old copies are deleted after the window.
- *Pairing.* A glimpsed code is not enough: it authenticates only the hello, and everything after needs `K0`, which needs ECDH(S, E) as well. An interposer who takes the joiner's `E` cannot grind an `S'` for the sponsor: `n_s` is committed before the sponsor sees `E`, `n_e` is fixed before the joiner sees `n_s`, and the number covers both nonces and every key. One guess in a million per code, and a wrong guess ends the pairing. A second computer answering the same code is visible to both sides. Nothing secret moves before two people say yes. The sync secret is encrypted to the joiner's permanent device key, not to `E`, and the identity's key travels only on explicit request. Ephemeral events are not stored, and the one-time keys are discarded, so there is nothing to replay or to decrypt later.
- *Recovery without a server.* The root event plus the kit rebuild everything; the kit's words are the only secret to keep offline.

**What it does not provide.**

- *No forward secrecy within an epoch.* Whoever obtains the sync secret can read every item of that epoch still on the relays, and the previous epoch's items during its window.
- *A removed computer keeps its copy.* Nothing can wipe it remotely.
- *Whoever holds the sync secret can push files that run commands* on every other computer; that is the product. The mitigations are that such files are never applied unseen (4.5), never gain an exec bit, never land outside the sync list (4.4), and that the user is shown which computer sent them.
- *Relay availability.* Relays may drop events; the audit and the multi-relay set limit the damage but cannot prevent a total loss if every relay drops everything while no computer is online to republish.
- *Local compromise.* A key kept in the login keyring is exactly as safe as the login session and the disk encryption. Moving the key into an external signer takes it out of the sync daemon's reach, but the sync secret itself must stay with the daemon.

## 14. Reference implementation

| Topic | Where |
|---|---|
| Secret, derivations, commitment, padding, sealing | `crates/peridot-sync/src/crypto.rs` |
| Item payloads, names, chunking | `crates/peridot-sync/src/envelope.rs` |
| Identity, device key, the root event | `crates/peridot-sync/src/identity.rs` |
| The rotation event | `crates/peridot-sync/src/rotation.rs` |
| Subscriptions, ingest, publishing, rotation, audit, cleanup | `crates/peridot-sync/src/sync.rs` |
| File status and the local store | `crates/peridot-sync/src/store.rs` |
| Tiers and the runs-commands set | `crates/peridot-sync/src/manifest.rs` |
| The secret scanner | `crates/peridot-sync/src/scan.rs` |
| Pairing | `crates/peridot-sync/src/pairing.rs`, `crates/peridotd/src/pair.rs` |
| The recovery kit | `crates/peridot-sync/src/recovery.rs` |
| Epoch window, removal, migration, timers | `crates/peridotd/src/app.rs`, `crates/peridotd/src/runner.rs` |
| Restore and import | `crates/peridotd/src/api.rs` |
| Relay client, outbox, NIP-42 | the `opal-kit` crate in the Opal repository |

Two computers end to end, with a mock relay, are exercised in `crates/peridotd/tests/e2e.rs`: pairing, a change propagating, conflicts, rotation, removal, recovery, migration from epoch 0.
