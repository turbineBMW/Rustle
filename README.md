# Rustle

A native GTK 4 / libadwaita email client for GNOME, written in Rust.

- Every mail account on the desktop, from Evolution Data Server: GNOME Online Accounts
  (OAuth), graphmail-bridge, Evolution's, or typed in here (iCloud brings its calendars
  and contacts along)
- A unified inbox across every account, plus each account's own folder tree
- Individual emails, full-text search, load-on-scroll history
- Pinned emails stay at the top of the list, in sync with Outlook through graphmail-bridge (the `$Pinned` keyword)
- Rich-text composer with attachments, Outbox with retry, one-click unsubscribe
- Follows the system accent colour and light/dark style
- On [Omarchy](https://omarchy.org/), follows the desktop theme live (below)

## Build

Needs `cargo`, GTK 4 ≥ 4.18, libadwaita ≥ 1.8, WebKitGTK 6.0, `blueprint-compiler`,
`glib-compile-schemas` (glib2), sqlite is bundled. At runtime, Evolution Data Server
(`evolution-data-server`) and a Secret Service keyring; spell checking in the composer
needs a Hunspell dictionary for your language (e.g. `hunspell-en_us`).

```
cargo run -p rustle      # run from the checkout — no install needed
just check               # clippy + fmt + tests
sh install.sh            # install to ~/.local (PREFIX=/usr for system-wide)
```

`RUSTLE_LOG=debug` turns on logging. Data lives in `$XDG_DATA_HOME/rustle/rustle.db`,
settings under the `io.github.turbinebmw.Rustle` schema.

## Accounts

Rustle reads its accounts from Evolution Data Server (EDS), the registry GNOME Online
Accounts, Evolution and graphmail-bridge (`graphmail-bridge eds-setup`) already keep
theirs in, and watches it: an account added there appears in Rustle by itself.
Servers and sign-in come from EDS — OAuth tokens through its registry, passwords from
the keyring entry EDS keeps for the account (in omarchy-mobile's sandbox, which can't
reach the keyring, through the phone's `dev.omarchy.Accounts`, which keeps only those
entries). Rustle keeps only what EDS has no place
for: colour, label, signature, notification sound.

*Add Account → Manual Setup* creates an account in EDS (so Evolution and other apps
see it too); for iCloud (use an app-specific password) it also registers iCloud's
calendars and contacts. Removing an account Rustle created deletes it from EDS;
removing any other only takes it out of Rustle, and *Online Accounts* lists it to show
again. Accounts Rustle kept itself before EDS are moved there on first start, password
and all, keeping their mail.

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
