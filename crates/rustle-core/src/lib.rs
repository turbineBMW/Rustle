//! UI-agnostic mail logic for Rustle.
//!
//! Nothing in this crate touches a widget. The GTK layer (the `rustle`
//! binary) drives it from worker threads and marshals results back to the main
//! loop. Everything here is unit-testable on a headless machine, with the
//! exception of the keyring and GNOME Online Accounts halves of `secrets` and
//! `goa`, which need a session bus.

pub mod address;
pub mod avatars;
pub mod compose;
pub mod darkmode;
pub mod dates;
pub mod db;
pub mod folders;
pub mod goa;
pub mod html;
pub mod mime;
pub mod models;
pub mod net;
pub mod providers;
pub mod secrets;
pub mod sounds;
pub mod sync;
pub mod watch;
