//! Tracks whether an MPRIS media player is actively playing.
//!
//! MPRIS players each own a session-bus name below
//! `org.mpris.MediaPlayer2`.  One proxy per player keeps PlaybackStatus
//! cached, while the bus proxy tells us when players appear and disappear.

use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Clone, Default)]
pub struct MediaActivity {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// Retained because it owns the NameOwnerChanged signal handler.
    bus: Option<gio::DBusProxy>,
    players: HashMap<String, Player>,
    pending: HashSet<String>,
}

struct Player {
    /// Retained because it owns the PropertiesChanged signal handler.
    _proxy: gio::DBusProxy,
    is_playing: bool,
}

impl MediaActivity {
    /// Start monitoring without delaying application startup. Until the
    /// initial bus query finishes, the conservative answer is not playing.
    pub fn new() -> Self {
        let activity = Self::default();
        let start = activity.clone();
        glib::MainContext::default().spawn_local(async move {
            start.connect().await;
        });
        activity
    }

    pub fn is_playing(&self) -> bool {
        self.inner
            .lock()
            .expect("media activity lock poisoned")
            .players
            .values()
            .any(|player| player.is_playing)
    }

    async fn connect(&self) {
        let bus = match gio::DBusProxy::for_bus_future(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START,
            None,
            DBUS_NAME,
            DBUS_PATH,
            DBUS_NAME,
        )
        .await
        {
            Ok(bus) => bus,
            Err(error) => {
                log::debug!("could not monitor MPRIS players: {error}");
                return;
            }
        };

        let weak = Arc::downgrade(&self.inner);
        bus.connect_g_signal(Some("NameOwnerChanged"), move |_, _, _, parameters| {
            let Some((name, old_owner, new_owner)) = parameters.get::<(String, String, String)>()
            else {
                return;
            };
            if !name.starts_with(MPRIS_PREFIX) {
                return;
            }
            if !old_owner.is_empty() {
                Self::remove_player(&weak, &name);
            }
            if !new_owner.is_empty() {
                Self::add_player(&weak, name);
            }
        });
        self.inner.lock().expect("media activity lock poisoned").bus = Some(bus.clone());

        match bus
            .call_future("ListNames", None, gio::DBusCallFlags::NONE, -1)
            .await
        {
            Ok(reply) => {
                let Some((names,)) = reply.get::<(Vec<String>,)>() else {
                    log::debug!("D-Bus returned an unexpected ListNames response");
                    return;
                };
                for name in names {
                    if name.starts_with(MPRIS_PREFIX) {
                        Self::add_player(&Arc::downgrade(&self.inner), name);
                    }
                }
            }
            Err(error) => log::debug!("could not list MPRIS players: {error}"),
        }
    }

    fn add_player(inner: &Weak<Mutex<Inner>>, name: String) {
        let Some(shared) = inner.upgrade() else {
            return;
        };
        {
            let mut state = shared.lock().expect("media activity lock poisoned");
            if state.players.contains_key(&name) || !state.pending.insert(name.clone()) {
                return;
            }
        }

        let weak = Arc::downgrade(&shared);
        glib::MainContext::default().spawn_local(async move {
            let flags = gio::DBusProxyFlags::DO_NOT_AUTO_START
                | gio::DBusProxyFlags::DO_NOT_AUTO_START_AT_CONSTRUCTION
                | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES;
            let result = gio::DBusProxy::for_bus_future(
                gio::BusType::Session,
                flags,
                None,
                &name,
                MPRIS_PATH,
                MPRIS_PLAYER_INTERFACE,
            )
            .await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            shared
                .lock()
                .expect("media activity lock poisoned")
                .pending
                .remove(&name);

            let Ok(proxy) = result else {
                return;
            };
            // The player may have vanished while its proxy was being made.
            if proxy.name_owner().is_none() {
                return;
            }

            let changed_name = name.clone();
            let changed_inner = Arc::downgrade(&shared);
            proxy.connect_g_properties_changed(move |proxy, _, _| {
                let Some(shared) = changed_inner.upgrade() else {
                    return;
                };
                let is_playing = playback_status(proxy);
                let mut state = shared.lock().expect("media activity lock poisoned");
                if let Some(player) = state.players.get_mut(&changed_name) {
                    player.is_playing = is_playing;
                }
            });
            let is_playing = playback_status(&proxy);
            shared
                .lock()
                .expect("media activity lock poisoned")
                .players
                .insert(
                    name,
                    Player {
                        _proxy: proxy,
                        is_playing,
                    },
                );
        });
    }

    fn remove_player(inner: &Weak<Mutex<Inner>>, name: &str) {
        let Some(shared) = inner.upgrade() else {
            return;
        };
        let mut state = shared.lock().expect("media activity lock poisoned");
        state.pending.remove(name);
        state.players.remove(name);
    }
}

fn playback_status(proxy: &gio::DBusProxy) -> bool {
    proxy
        .cached_property("PlaybackStatus")
        .and_then(|status| status.get::<String>())
        .is_some_and(|status| status == "Playing")
}
