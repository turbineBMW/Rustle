//! Address to texture, fetched in the background and cached in memory. A
//! cached None means nobody had a picture, so a sender without one costs a
//! single request per session.

use crate::settings;
use crate::workers;
use gtk::gdk;
use gtk::gdk_pixbuf::Pixbuf;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;

/// ponytail: clears wholesale rather than evicting; an LRU if churn shows up.
const MAX_CACHED: usize = 500;

type Callback = Box<dyn Fn(&gdk::Texture)>;

#[derive(Default)]
struct State {
    cache: HashMap<String, Option<gdk::Texture>>,
    waiting: HashMap<String, Vec<Callback>>,
}

#[derive(Clone)]
pub struct AvatarLoader {
    settings: gio::Settings,
    state: Rc<RefCell<State>>,
}

impl AvatarLoader {
    pub fn new(settings: gio::Settings) -> Self {
        AvatarLoader {
            settings,
            state: Rc::default(),
        }
    }

    pub fn load(&self, address: &str, on_ready: impl Fn(&gdk::Texture) + 'static) {
        let address = address.trim().to_lowercase();
        if address.is_empty() || !self.settings.boolean(settings::LOAD_SENDER_AVATARS) {
            return;
        }
        let mut state = self.state.borrow_mut();
        if let Some(cached) = state.cache.get(&address) {
            if let Some(texture) = cached {
                on_ready(texture);
            }
            return;
        }
        if let Some(callbacks) = state.waiting.get_mut(&address) {
            callbacks.push(Box::new(on_ready));
            return;
        }
        state
            .waiting
            .insert(address.clone(), vec![Box::new(on_ready)]);
        drop(state);

        let cache_dir = glib::user_cache_dir().join("rustle");
        let loader = self.clone();
        let key = address.clone();
        workers::run(
            move || rustle_core::avatars::fetch(&address, &cache_dir, &decode_width),
            move |bytes| loader.deliver(&key, bytes),
        );
    }

    fn deliver(&self, address: &str, bytes: Option<Vec<u8>>) {
        let texture =
            bytes.and_then(|data| gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)).ok());
        let mut state = self.state.borrow_mut();
        if state.cache.len() >= MAX_CACHED {
            state.cache.clear();
        }
        state.cache.insert(address.to_string(), texture.clone());
        let callbacks = state.waiting.remove(address).unwrap_or_default();
        drop(state);
        if let Some(texture) = texture {
            for callback in callbacks {
                callback(&texture);
            }
        }
    }
}

/// Decode on the worker thread just far enough to know it is an image and
/// how wide; the texture itself is made on the main thread.
fn decode_width(data: &[u8]) -> Option<u32> {
    Pixbuf::from_read(Cursor::new(data.to_vec()))
        .ok()
        .map(|pixbuf| pixbuf.width().max(0) as u32)
}
