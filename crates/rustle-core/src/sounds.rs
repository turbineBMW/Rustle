//! Which sound plays when new mail arrives, and where the desktop's sound
//! theme keeps it.
//!
//! The choice is stored as one string, in GSettings for the app-wide default
//! and in the accounts table for a per-account override. The playback itself
//! lives in the GTK layer; this module only decides what to play.

use std::path::{Path, PathBuf};

/// The freedesktop sound-naming-spec event for new mail. Themes that don't
/// ship it fall back down the name, so it ends at the generic `message`.
pub const NEW_MAIL_EVENT: &str = "message-new-email";

/// The theme every other theme inherits from, per the spec.
const FALLBACK_THEME: &str = "freedesktop";

/// A stored sound choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationSound {
    /// Use the app-wide default (only meaningful for an account).
    Inherit,
    /// The desktop sound theme's new-mail sound.
    System,
    /// No sound.
    Silent,
    /// An audio file of the user's choosing.
    File(PathBuf),
}

impl NotificationSound {
    /// Parse the stored form: "" inherits, "system", "none", or a path.
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "" => Self::Inherit,
            "system" => Self::System,
            "none" => Self::Silent,
            path => Self::File(PathBuf::from(path)),
        }
    }

    /// The stored form; the inverse of [`NotificationSound::parse`].
    pub fn as_setting(&self) -> String {
        match self {
            Self::Inherit => String::new(),
            Self::System => "system".into(),
            Self::Silent => "none".into(),
            Self::File(path) => path.to_string_lossy().into_owned(),
        }
    }

    /// The account's own choice unless it inherits, in which case the app
    /// default -- and an app default that somehow inherits means the system
    /// sound, so the result is never `Inherit`.
    pub fn resolve(account: &Self, default: &Self) -> Self {
        match account {
            Self::Inherit => match default {
                Self::Inherit => Self::System,
                other => other.clone(),
            },
            other => other.clone(),
        }
    }

    /// The file's name, for showing which one is chosen.
    pub fn file_name(&self) -> Option<String> {
        match self {
            Self::File(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            _ => None,
        }
    }
}

/// The directories the sound theme spec says to search, in order:
/// `$XDG_DATA_HOME/sounds` then each of `$XDG_DATA_DIRS/sounds`.
pub fn theme_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/share")));
    if let Some(data_home) = data_home {
        dirs.push(data_home.join("sounds"));
    }
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(Path::new(dir).join("sounds"));
    }
    dirs
}

/// The event's sound file in `theme`, or in whatever it inherits from,
/// or in the freedesktop theme, walking down the event name at each step
/// (`message-new-email`, `message-new`, `message`). None when no theme has
/// it or the theme marks it `.disabled`.
pub fn theme_sound_path(event: &str, theme: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut visited = Vec::new();
    let mut queue = vec![theme.to_string()];
    while let Some(theme) = queue.pop() {
        if visited.contains(&theme) {
            continue;
        }
        visited.push(theme.clone());
        for dir in search_dirs {
            let theme_dir = dir.join(&theme);
            if !theme_dir.is_dir() {
                continue;
            }
            let index = std::fs::read_to_string(theme_dir.join("index.theme")).ok();
            let (subdirs, inherits) = parse_index(index.as_deref().unwrap_or(""));
            let mut name = event;
            loop {
                for subdir in subdirs.iter().map(String::as_str).chain(["stereo"]) {
                    let stem = theme_dir.join(subdir).join(name);
                    if stem.with_extension("disabled").is_file() {
                        return None;
                    }
                    for extension in ["oga", "ogg", "wav"] {
                        let path = stem.with_extension(extension);
                        if path.is_file() {
                            return Some(path);
                        }
                    }
                }
                match name.rsplit_once('-') {
                    Some((shorter, _)) => name = shorter,
                    None => break,
                }
            }
            // Parents are searched after the theme itself; push in reverse so
            // the first listed parent is popped first.
            for parent in inherits.into_iter().rev() {
                queue.push(parent);
            }
        }
        if theme != FALLBACK_THEME && !queue.iter().any(|t| t == FALLBACK_THEME) {
            queue.insert(0, FALLBACK_THEME.to_string());
        }
    }
    None
}

/// The `Directories` and `Inherits` lists of an index.theme.
fn parse_index(index: &str) -> (Vec<String>, Vec<String>) {
    let mut subdirs = Vec::new();
    let mut inherits = Vec::new();
    let mut in_theme_group = false;
    for line in index.lines().map(str::trim) {
        if line.starts_with('[') {
            in_theme_group = line == "[Sound Theme]";
            continue;
        }
        if !in_theme_group {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let values = value
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from);
            match key.trim() {
                "Directories" => subdirs.extend(values),
                "Inherits" => inherits.extend(values),
                _ => {}
            }
        }
    }
    (subdirs, inherits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn choices_round_trip_through_their_stored_form() {
        for choice in [
            NotificationSound::Inherit,
            NotificationSound::System,
            NotificationSound::Silent,
            NotificationSound::File("/tmp/mail.wav".into()),
        ] {
            assert_eq!(NotificationSound::parse(&choice.as_setting()), choice);
        }
        assert_eq!(
            NotificationSound::parse("  none "),
            NotificationSound::Silent
        );
    }

    #[test]
    fn an_account_inherits_the_default_and_the_default_never_inherits() {
        let file = NotificationSound::File("/a.ogg".into());
        assert_eq!(
            NotificationSound::resolve(&NotificationSound::Inherit, &file),
            file
        );
        assert_eq!(
            NotificationSound::resolve(&NotificationSound::Silent, &file),
            NotificationSound::Silent
        );
        assert_eq!(
            NotificationSound::resolve(&NotificationSound::Inherit, &NotificationSound::Inherit),
            NotificationSound::System
        );
        assert_eq!(file.file_name().as_deref(), Some("a.ogg"));
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn the_event_name_walks_down_and_themes_fall_back_to_freedesktop() {
        let dir = tempfile::tempdir().unwrap();
        let sounds = dir.path().join("sounds");
        touch(&sounds.join("freedesktop/stereo/message.oga"));
        fs::write(
            sounds.join("freedesktop/index.theme"),
            "[Sound Theme]\nName=Default\nDirectories=stereo\n",
        )
        .unwrap();
        fs::create_dir_all(sounds.join("mine")).unwrap();
        fs::write(
            sounds.join("mine/index.theme"),
            "[Sound Theme]\nName=Mine\nInherits=parent\nDirectories=stereo\n",
        )
        .unwrap();
        let dirs = vec![sounds.clone()];

        // Nothing of its own: down the name in freedesktop.
        assert_eq!(
            theme_sound_path(NEW_MAIL_EVENT, "mine", &dirs),
            Some(sounds.join("freedesktop/stereo/message.oga"))
        );
        // A parent's more specific sound wins over freedesktop's generic one.
        touch(&sounds.join("parent/stereo/message-new.ogg"));
        assert_eq!(
            theme_sound_path(NEW_MAIL_EVENT, "mine", &dirs),
            Some(sounds.join("parent/stereo/message-new.ogg"))
        );
        // The theme's own exact match wins over everything.
        touch(&sounds.join("mine/stereo/message-new-email.wav"));
        assert_eq!(
            theme_sound_path(NEW_MAIL_EVENT, "mine", &dirs),
            Some(sounds.join("mine/stereo/message-new-email.wav"))
        );
        // A theme that isn't installed still finds freedesktop.
        assert!(theme_sound_path(NEW_MAIL_EVENT, "missing", &dirs).is_some());
        assert_eq!(theme_sound_path("nothing-like-this", "mine", &dirs), None);
    }

    #[test]
    fn a_disabled_marker_silences_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let sounds = dir.path().join("sounds");
        touch(&sounds.join("freedesktop/stereo/message.oga"));
        touch(&sounds.join("quiet/stereo/message-new-email.disabled"));
        assert_eq!(theme_sound_path(NEW_MAIL_EVENT, "quiet", &[sounds]), None);
    }
}
