//! The threading model: network I/O runs on a blocking thread, its result is
//! delivered to a closure on the main loop. The job gets owned, `Send` data
//! (a cloned `Account`, a frozen request) -- never a widget or the database.
//! Credentials are resolved *inside* the job, because the keyring and GNOME
//! Online Accounts both block on IPC.

use gtk::gio;
use gtk::glib;

/// Run `job` off the main thread and hand what it returns to `on_done` on
/// the main thread.
pub fn run<T, J, D>(job: J, on_done: D)
where
    T: Send + 'static,
    J: FnOnce() -> T + Send + 'static,
    D: FnOnce(T) + 'static,
{
    glib::spawn_future_local(async move {
        match gio::spawn_blocking(job).await {
            Ok(result) => on_done(result),
            Err(_) => log::error!("a worker thread panicked"),
        }
    });
}
