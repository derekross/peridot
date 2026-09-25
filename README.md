# Peridot

**Keep your Omarchy computers matching.** Sign in once, and your look, keyboard shortcuts, terminal, bar and menu follow you to every computer you use. No account to create, no cloud company holding your settings.

> Peridot is in early development. Sync and device pairing are being built now; nothing is ready to install yet.

## What it will do

- **Sync**: change a setting on one computer and the others offer to apply it. Every change can be undone.
- **Pair a new computer**: scan a code or type it in, check that both screens show the same number, done.
- **Recovery kit**: a printable page and six words that bring everything back if you lose all your computers.

Later: private share links for screenshots and files, a gallery of themes and plugins with real reviews, and a community lounge.

## What it never syncs

Passwords, keys, tokens, browser profiles, your monitor layout and input devices. Files that can run commands (autostart, shell startup, hooks) only sync if you turn them on. Anything that looks like a secret is skipped whatever the setting.

## How it works

Your settings are end-to-end encrypted on your computer before they leave it, then stored on several independent servers that can't read them. Only your own paired computers hold the key. Under the hood this uses [Nostr](https://nostr.com), an open protocol, so no single company runs it, and you can take your identity to other apps later (Peridot pairs with [Opal](https://github.com/derekross/opal) for that).

## Development

```sh
cargo test
```

## License

MIT
