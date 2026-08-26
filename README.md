# Rustle

A native GTK 4 / libadwaita email client for GNOME, written in Rust.

- Multiple IMAP/SMTP accounts, or accounts imported from GNOME Online Accounts (OAuth)
- A unified inbox across every account, plus each account's own folder tree
- Threaded conversations, full-text search, load-on-scroll history
- Rich-text composer with attachments, Outbox with retry, one-click unsubscribe
- Follows the system accent colour and light/dark style

## Build

Needs `cargo`, GTK 4 ≥ 4.18, libadwaita ≥ 1.8, WebKitGTK 6.0, `blueprint-compiler`,
`glib-compile-schemas` (glib2), sqlite is bundled.

```
cargo run -p rustle      # run from the checkout — no install needed
just check               # clippy + fmt + tests
sh install.sh            # install to ~/.local (PREFIX=/usr for system-wide)
```

`RUSTLE_LOG=debug` turns on logging. Data lives in `$XDG_DATA_HOME/rustle/rustle.db`,
passwords in the system keyring, settings under the `io.github.turbinebmw.Rustle` schema.
