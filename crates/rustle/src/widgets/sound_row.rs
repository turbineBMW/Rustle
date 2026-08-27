//! A combo row that picks a new-mail sound: the system sound, silence or a
//! file of the user's own, with a button to hear the choice. Used for the
//! app-wide default (Preferences) and per account (Accounts), where it also
//! offers "App Default".

use crate::i18n::gettext;
use crate::sound;
use adw::prelude::*;
use gtk::{gio, glib};
use rustle_core::sounds::NotificationSound;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The fixed entries, in row order after the optional "App Default".
const SYSTEM: u32 = 0;
const SILENT: u32 = 1;
const CHOOSE_FILE: u32 = 2;

struct Picker {
    row: adw::ComboRow,
    /// Present when the row is an account's, letting it inherit this.
    inherit_from: Option<gio::Settings>,
    current: RefCell<NotificationSound>,
    /// Set while the row's selection is being put back by code, so the
    /// notify handler ignores it.
    settling: Cell<bool>,
    on_change: Box<dyn Fn(&NotificationSound)>,
}

/// Turn `row` into a sound picker showing `current`. `inherit_from` is the
/// app settings when the row belongs to an account, which adds the
/// "App Default" entry and resolves previews against it. `on_change` gets
/// every new choice to store.
pub fn setup(
    row: &adw::ComboRow,
    current: NotificationSound,
    inherit_from: Option<gio::Settings>,
    on_change: impl Fn(&NotificationSound) + 'static,
) {
    let mut labels = Vec::new();
    if inherit_from.is_some() {
        labels.push(gettext("App Default"));
    }
    labels.extend([
        gettext("System Sound"),
        gettext("None"),
        gettext("Choose a File…"),
    ]);
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    row.set_model(Some(&gtk::StringList::new(&labels)));

    let picker = Rc::new(Picker {
        row: row.clone(),
        inherit_from,
        current: RefCell::new(current),
        settling: Cell::new(false),
        on_change: Box::new(on_change),
    });
    picker.show_current();

    let preview = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text(gettext("Play"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    preview.connect_clicked(glib::clone!(
        #[strong]
        picker,
        move |_| picker.preview()
    ));
    row.add_suffix(&preview);

    row.connect_selected_notify(glib::clone!(
        #[strong]
        picker,
        move |row| {
            if picker.settling.get() {
                return;
            }
            picker.on_selected(row.selected());
        }
    ));
}

impl Picker {
    /// Rows before the fixed entries: one for "App Default" when offered.
    fn offset(&self) -> u32 {
        u32::from(self.inherit_from.is_some())
    }

    fn index_of(&self, sound: &NotificationSound) -> u32 {
        let fixed = match sound {
            // An account with no default row can't inherit; show it as the
            // system sound, which is what an inherited empty default means.
            NotificationSound::Inherit if self.inherit_from.is_none() => SYSTEM,
            NotificationSound::Inherit => return 0,
            NotificationSound::System => SYSTEM,
            NotificationSound::Silent => SILENT,
            NotificationSound::File(_) => CHOOSE_FILE,
        };
        self.offset() + fixed
    }

    /// Put the row's selection and subtitle where the current choice is.
    fn show_current(&self) {
        let current = self.current.borrow().clone();
        self.settling.set(true);
        self.row.set_selected(self.index_of(&current));
        self.settling.set(false);
        let subtitle = match &current {
            NotificationSound::File(path) if !sound::is_playable(path) => {
                gettext("Won't play: unsupported format. Use OGG, WAV, FLAC or Opus.")
            }
            NotificationSound::File(_) => current.file_name().unwrap_or_default(),
            NotificationSound::Inherit if self.inherit_from.is_some() => {
                gettext("Whatever Preferences says")
            }
            NotificationSound::Inherit | NotificationSound::System => {
                gettext("The desktop's new-mail sound")
            }
            NotificationSound::Silent => gettext("Just the notification"),
        };
        self.row.set_subtitle(&subtitle);
    }

    fn on_selected(self: &Rc<Self>, index: u32) {
        if self.inherit_from.is_some() && index == 0 {
            self.commit(NotificationSound::Inherit);
            return;
        }
        match index - self.offset() {
            SYSTEM => self.commit(NotificationSound::System),
            SILENT => self.commit(NotificationSound::Silent),
            _ => self.choose_file(),
        }
    }

    fn commit(&self, sound: NotificationSound) {
        *self.current.borrow_mut() = sound;
        (self.on_change)(&self.current.borrow());
        self.show_current();
    }

    /// Ask for a file; cancelling puts the row back on the old choice.
    fn choose_file(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(&gettext("Audio Files (OGG, WAV, FLAC, Opus, AAC)")));
        for mime in sound::SUPPORTED_MIME_TYPES {
            filter.add_mime_type(mime);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Choose a Notification Sound"))
            .modal(true)
            .filters(&filters)
            .default_filter(&filter)
            .build();
        if let NotificationSound::File(path) = &*self.current.borrow() {
            dialog.set_initial_file(Some(&gio::File::for_path(path)));
        }
        let root = self.row.root().and_downcast::<gtk::Window>();
        let picker = self.clone();
        dialog.open(
            root.as_ref(),
            None::<&gio::Cancellable>,
            move |result| match result.ok().and_then(|file| file.path()) {
                Some(path) => picker.commit(NotificationSound::File(path)),
                None => picker.show_current(),
            },
        );
    }

    /// Hear what the current choice plays, resolved against the app default
    /// for an account row.
    fn preview(&self) {
        let current = self.current.borrow().clone();
        let resolved = match &self.inherit_from {
            Some(settings) => NotificationSound::resolve(&current, &sound::default_sound(settings)),
            None => NotificationSound::resolve(&current, &NotificationSound::System),
        };
        sound::play(&resolved);
    }
}
