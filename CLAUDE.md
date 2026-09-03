# CLAUDE.md

Rustle is a native GTK 4 / libadwaita email client written in Rust. It began as a rewrite
of Postcard (`~/Projects/postcard`, Python) and is now its own project: its own app id
(`io.github.turbinebmw.Rustle`), GSettings schema, data dir (`$XDG_DATA_HOME/rustle`),
keyring schema and D-Bus name. Nothing is shared with Postcard at runtime.

## Build & run

No Flatpak, no meson. `cargo run -p rustle` works from a checkout: `build.rs` compiles the
Blueprints into a GResource and the GSettings schema into `OUT_DIR`, and `settings::load`
falls back to that compiled schema when the app isn't installed.

```
just run           # cargo run -p rustle
just check         # clippy -D warnings + rustfmt --check + cargo test  (keep it clean)
just install       # sh install.sh -> ~/.local (PREFIX=/usr for system-wide)
```

Host deps: gtk4 ≥ 4.18, libadwaita ≥ 1.8, webkitgtk-6.0, blueprint-compiler, glib2
(glib-compile-schemas), openssl (native-tls). sqlite is bundled by rusqlite.

`RUSTLE_LOG=debug` (any `env_logger` filter) turns logging up. Dev hooks read at launch:
`RUSTLE_DEBUG_OPEN=<folder id>:<uid>` opens that message, `RUSTLE_DEBUG_COMPOSE=1` opens the
composer. To exercise the app without touching real data: `XDG_DATA_HOME=<dir> cargo run`.
Prefer these over re-activating a running window over D-Bus — `activate` presents the
window and its search bar captures whatever the user is typing elsewhere.

## Layout

```
Cargo.toml                 workspace (edition 2021, thin LTO in release)
crates/rustle-core/src/    no widgets; everything here is unit-tested (`cargo test`)
  models.rs    Account/Folder/Email/Conversation/Attachment/MessageHeader, Security enum
  db.rs        rusqlite; schema + append-only MIGRATIONS tracked by PRAGMA user_version
  net/         imap.rs (imap crate; socket built by hand so it has a timeout, STARTTLS
               spoken on the raw socket), smtp.rs (lettre), auth.rs (Credential:
               login | xoauth2), errors.rs (NetError -> Failure, the enum the UI translates)
  sync.rs      fetch_mailbox / fetch_full_message / set_flag / move_messages / send_message
  mime.rs      mail-parser -> ParsedMessage; sandbox_html builds the CSP'd reader document
  compose.rs   reply/forward bodies, MIME building (lettre), mailto:, address helpers
  folders.rs   FolderRole classification by name, display names, modified UTF-7
  threader.rs  union-find threading; address.rs, dates.rs, providers.rs, html.rs
  watch.rs     IMAP IDLE: one cancellable long-lived session per account on its inbox
  secrets.rs   secret-service keyring + credential_for; goa.rs GNOME Online Accounts (D-Bus)
  avatars.rs   sender pictures: local graphmail-bridge photo endpoint (loopback+plain IMAP
               accounts, port from `bridge-photo-port`), then Gravatar/favicon; on-disk cache
crates/rustle/             the GTK layer
  build.rs     blueprint-compiler ui/*.blp -> gresource; glib-compile-schemas -> OUT_DIR
  ui/*.blp     Blueprint templates; the Rust attribute names must match the ids
  src/window/  one MainWindow, one impl block per concern: accounts, actions, folders,
               list, moves, reader, sync, watch (the IDLE threads)
  src/widgets/ FolderRow, ConversationRow (gtk::Box subclasses), MessageView (plain struct)
  src/dialogs/ account, accounts, online_accounts, preferences;  src/composer.rs
  src/workers.rs  the threading model (below);  src/accent.rs  accent colour helpers
data/                      gschema, desktop file, metainfo, D-Bus service, icons
```

## Rules

- **Threading:** all network I/O goes through `workers::run(job, on_done)`. `job` runs on
  a blocking thread with owned `Send` data (a cloned `Account`, a frozen request) and
  resolves credentials itself (keyring/GOA block on IPC); `on_done` runs on the main loop
  and is the only place that touches the database or widgets. Never log a `Credential`'s
  secret — its `Debug` hides it; don't `{:?}` a struct that embeds the raw token.
  The one long-lived thread is the inbox watcher (`window/watch.rs`): it reports back
  through a `SendWeakRef` on the window via `MainContext::invoke`, and is cancelled through
  its `InboxWatch` handle, which shuts the socket so a blocked read returns at once.
  `sync_inbox_watchers` reconciles the threads against `State::accounts` and is idempotent.
- **Window state** is one `RefCell<State>` behind `state()`/`state_mut()`. Never hold a
  `Ref` across a call that may need `state_mut()`. Bind to a local before an
  `if let`/`match`: a temporary in the scrutinee lives for the whole block. This crashed
  the first run; grep for `if let .* self.state()` before adding code.
- **Unified inbox:** `View::UnifiedInbox` shows every account's inbox. Per-account
  actions resolve the account from the email's `folder_id` via `account_for_folder`;
  nothing assumes one "current account". Moves are grouped by source folder (one
  `PendingMove` per account under a single Undo toast).
- **Accent colour:** CSS uses `var(--accent-color)`; the WebKit views (reader, composer)
  get it injected via `accent::accent_hex()` and re-render on `accent::watch`.
- **Reader security:** message HTML renders with JavaScript off, a CSP that blocks every
  remote subresource except opted-in images, and a `decide-policy` handler that only
  hands http/https/mailto link clicks to the browser. Keep it that way.
- **Errors surfaced to the user are also logged** at the worker boundary, naming the
  resource (`could not move 3 message(s) from INBOX to Trash (account x)`).
- No account is a real state: `State::view` is `None` on an empty database; anything
  per-account reads it through a guard.
- Commits: Conventional Commits, terse, no AI/co-author trailers.
