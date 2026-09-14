//! Plays the new-mail sound through GTK's media backend. Which file that is
//! comes from `rustle_core::sounds`; the desktop's own sound settings
//! (theme name, the event-sounds switch) are read here.

use crate::settings as keys;
use gtk::gio;
use gtk::prelude::*;
use rustle_core::sounds::{self, NotificationSound};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

const DESKTOP_SOUND_SCHEMA: &str = "org.gnome.desktop.sound";
const DESKTOP_NOTIFICATIONS_SCHEMA: &str = "org.gnome.desktop.notifications";
const QUIET_NOTIFICATION_VOLUME: f64 = 0.2;

thread_local! {
    /// Every sound still playing, held so none is finalised mid-note:
    /// dropping a GtkMediaFile while GStreamer is still starting it has
    /// crashed the app.
    static PLAYING: RefCell<Vec<gtk::MediaFile>> = const { RefCell::new(Vec::new()) };
}

/// MIME types GTK's media backend is known to play safely. MP3 is left out
/// on purpose: GtkMediaFile aborts on it inside GStreamer's decodebin3
/// (`assertion failed: (collection)`, gst 1.28 / gtk 4.20) -- every MP3, even
/// ones gst-play handles -- and an abort can't be caught, so it's refused
/// up front.
pub const SUPPORTED_MIME_TYPES: &[&str] = &[
    "audio/ogg",
    "audio/x-vorbis+ogg",
    "audio/x-opus+ogg",
    "audio/opus",
    "audio/flac",
    "audio/x-flac",
    "audio/x-wav",
    "audio/wav",
    "audio/vnd.wave",
    "audio/mp4",
    "audio/aac",
];

/// Whether a file may be handed to the media backend at all, judged by
/// content sniffing with the name as a fallback.
pub fn is_playable(path: &Path) -> bool {
    let mut header = [0u8; 512];
    let read = std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut header))
        .unwrap_or(0);
    let header = &header[..read];
    // The extension outranks content in gio's guess, so a renamed MP3 is
    // caught by hand: an ID3 tag or an MPEG audio frame sync.
    if header.starts_with(b"ID3")
        || (header.len() >= 2 && header[0] == 0xff && header[1] & 0xe6 == 0xe2)
    {
        return false;
    }
    let name = path.file_name().map(|name| name.to_string_lossy());
    let (guess, _) = gio::content_type_guess(name.as_deref(), header);
    let mime = gio::content_type_get_mime_type(&guess).unwrap_or_default();
    SUPPORTED_MIME_TYPES.contains(&mime.as_str())
}

/// The desktop's sound settings, when the desktop has them (the schema is
/// gsettings-desktop-schemas', so it's present under GNOME and absent
/// elsewhere).
fn desktop_sound_settings() -> Option<gio::Settings> {
    gio::SettingsSchemaSource::default()?.lookup(DESKTOP_SOUND_SCHEMA, true)?;
    Some(gio::Settings::new(DESKTOP_SOUND_SCHEMA))
}

/// The file the desktop's sound theme plays for new mail, or None when the
/// desktop has event sounds switched off or the theme has no such sound.
pub fn system_sound_path() -> Option<PathBuf> {
    let settings = desktop_sound_settings();
    if settings
        .as_ref()
        .is_some_and(|settings| !settings.boolean("event-sounds"))
    {
        return None;
    }
    let theme = settings
        .map(|settings| settings.string("theme-name").to_string())
        .filter(|theme| !theme.is_empty())
        .unwrap_or_else(|| "freedesktop".into());
    sounds::theme_sound_path(sounds::NEW_MAIL_EVENT, &theme, &sounds::theme_search_dirs())
}

/// The app-wide default from GSettings.
pub fn default_sound(settings: &gio::Settings) -> NotificationSound {
    NotificationSound::parse(&settings.string(keys::NOTIFICATION_SOUND))
}

/// The file a resolved choice plays, or None for silence.
pub fn path_for(sound: &NotificationSound) -> Option<PathBuf> {
    match sound {
        NotificationSound::Inherit | NotificationSound::System => system_sound_path(),
        NotificationSound::Silent => None,
        NotificationSound::File(path) => Some(path.clone()),
    }
}

/// Play the sound for a choice, if it has one.
pub fn play(sound: &NotificationSound) {
    if let Some(path) = path_for(sound) {
        play_file(&path);
    }
}

/// Play an arriving-mail sound, respecting GNOME's Do Not Disturb switch
/// and reducing its volume while an MPRIS player is active. This is separate
/// from `play` so a sound explicitly previewed in Preferences remains audible.
pub fn play_notification(sound: &NotificationSound, media_is_playing: bool) {
    if do_not_disturb() {
        return;
    }
    if let Some(path) = path_for(sound) {
        let volume = if media_is_playing {
            QUIET_NOTIFICATION_VOLUME
        } else {
            1.0
        };
        play_file_at_volume(&path, volume);
    }
}

/// GNOME implements Do Not Disturb by disabling notification banners. The
/// freedesktop notification protocol has no portable DND state, so desktops
/// without this schema retain Rustle's normal sound behaviour.
fn do_not_disturb() -> bool {
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return false;
    };
    if source.lookup(DESKTOP_NOTIFICATIONS_SCHEMA, true).is_none() {
        return false;
    }
    !gio::Settings::new(DESKTOP_NOTIFICATIONS_SCHEMA).boolean("show-banners")
}

/// Play one file from the start, alongside whatever is still playing --
/// unless that same file already is, so two accounts arriving on the same
/// tick make one sound. Files the backend can't handle are refused (logged),
/// never handed over.
pub fn play_file(path: &Path) {
    play_file_at_volume(path, 1.0);
}

fn play_file_at_volume(path: &Path, volume: f64) {
    let shown = path.display().to_string();
    if !is_playable(path) {
        log::warn!("refusing to play the notification sound {shown}: not a supported audio format");
        return;
    }
    let already = PLAYING.with(|playing| {
        playing.borrow().iter().any(|media| {
            media.file().and_then(|file| file.path()).as_deref() == Some(path) && !media.is_ended()
        })
    });
    if already {
        return;
    }
    let media = gtk::MediaFile::for_filename(path);
    media.set_volume(volume);
    media.connect_error_notify(move |media| {
        if let Some(error) = media.error() {
            log::warn!("could not play the notification sound {shown}: {error}");
        }
        forget(media);
    });
    media.connect_ended_notify(forget);
    media.play();
    PLAYING.with(|playing| playing.borrow_mut().push(media));
}

/// Let a finished (or failed) stream go.
fn forget(media: &gtk::MediaFile) {
    PLAYING.with(|playing| playing.borrow_mut().retain(|kept| kept != media));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn mp3_is_refused_and_ogg_wav_are_accepted() {
        let mut id3 = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        id3.extend([0xff, 0xfb, 0x90, 0x00]);
        let (_d, mp3) = file_with("a.mp3", &id3);
        assert!(!is_playable(&mp3));
        // Even one hiding behind a friendlier name.
        let (_d, disguised) = file_with("a.ogg", &id3);
        assert!(!is_playable(&disguised));
        let (_d, ogg) = file_with("a.ogg", b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00");
        assert!(is_playable(&ogg));
        let mut wav = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
        wav.resize(64, 0);
        let (_d, wav) = file_with("a.wav", &wav);
        assert!(is_playable(&wav));
    }
}
