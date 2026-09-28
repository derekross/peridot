# Security

## Reporting

If you find a vulnerability in Peridot (the `peridotd` service, the `peridot`
command, the shell plugin, or the install scripts), please report it privately
rather than in a public issue:

- GitHub: [open a private security advisory](https://github.com/derekross/peridot/security/advisories/new)
- Nostr: a direct message to Derek, `npub18ams6ewn5aj2n3wt2qawzglx9mr4nzksxhvrdc4gzrecw7n5tvjqctp424`

Say what you found, how to reproduce it, and what version or commit you
looked at. You will get an acknowledgement within a few days, and a fix or a
clear answer as soon as one exists, normally within a couple of weeks for
anything that puts data at risk. Credit in the release notes is yours if you
want it.

## What counts

Peridot keeps a user's settings matching across their computers, end-to-end
encrypted, through relays and Blossom servers that never see plaintext. In
scope is anything that breaks that promise or lets someone act as the user:

- reading, forging or replaying synced settings, private links or pairing
  from the network, a relay or a Blossom server;
- pairing a computer, or a link, without the owner's code;
- the daemon writing outside the files the user chose to sync, or following a
  link or path it should not;
- the installer or uninstaller replacing or removing something it did not
  write, or fetching a binary that does not match the pinned checksum;
- a leak of the identity key, the sync secret, or a share key.

## Out of scope

- Code that already runs as the same user on the same machine. Peridot's
  socket, keyring items and files are that user's; another process of theirs
  can read them, and Peridot does not try to defend against that.
- Relays or Blossom servers refusing service, dropping or delaying data
  (Peridot treats them as untrusted storage, not as available storage).
- Settings the user chose to sync that are themselves sensitive: Peridot
  syncs what it is told to.

## Versions

Only the latest release and `main` are supported. Reports against older
versions are welcome, but the fix lands in the next release.
