# Rustle

A native GTK 4 / libadwaita email client for GNOME, written in Rust.

- Multiple IMAP/SMTP accounts, or accounts imported from GNOME Online Accounts (OAuth)
- A unified inbox across every account, plus each account's own folder tree
- Individual emails, full-text search, load-on-scroll history
- Pinned emails stay at the top of the list, in sync with Outlook through graphmail-bridge (the `$Pinned` keyword)
- Rich-text composer with attachments, Outbox with retry, one-click unsubscribe
- Follows the system accent colour and light/dark style
- On [Omarchy](https://omarchy.org/), follows the desktop theme live (below)

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

## Omarchy themes

Where an Omarchy theme is present, *Preferences → Follow Omarchy Theme* (on by
default) takes the window colours, accent and light/dark style from the active
desktop theme and follows every `omarchy theme set` live, links in the reader
and composer included. The palette is read straight from
`~/.local/state/omarchy/current/theme/colors.toml` with the same fallbacks
`omarchy-theme-color` applies, legacy `color0`..`color15` themes included.
Turn it off and Rustle is back on the system accent and style.

A theme can take full control by shipping a `rustle.css` (plain GTK CSS setting
libadwaita's variables), either in the theme directory or generated from a
`~/.config/omarchy/themed/rustle.css.tpl` template; it is used verbatim in
place of the derived palette.
