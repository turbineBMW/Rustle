//! Pictures the user gives their accounts. A chosen image is cropped square,
//! scaled down and saved as a PNG in the data directory under a fresh name,
//! so a texture cached by file name never goes stale; the account row keeps
//! that name (`accounts.picture`).

use gtk::gdk;
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::glib;
use rustle_core::models::Account;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Big enough for the largest avatar the app draws, at 2x and then some.
const SIZE: i32 = 256;

thread_local! {
    static CACHE: RefCell<HashMap<String, Option<gdk::Texture>>> = RefCell::new(HashMap::new());
}

pub fn dir() -> PathBuf {
    glib::user_data_dir()
        .join("rustle")
        .join("account-pictures")
}

/// The account's picture, or None if it has none or the file won't load.
pub fn texture(account: &Account) -> Option<gdk::Texture> {
    let name = account.picture_file()?;
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| {
                let path = dir().join(name);
                gdk::Texture::from_filename(&path)
                    .inspect_err(|error| {
                        log::warn!(
                            "could not load the picture of account {} from {}: {error}",
                            account.id,
                            path.display()
                        );
                    })
                    .ok()
            })
            .clone()
    })
}

/// Decode `source`, crop it to its centre square, scale it down and save it
/// in `dir` under a new name, which is returned. Blocking: run it on a worker.
pub fn import(source: &Path, dir: &Path, account_id: i64) -> Result<String, String> {
    let pixbuf = Pixbuf::from_file(source).map_err(|error| error.to_string())?;
    // Phone photos are stored sideways with an EXIF note saying so.
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let side = width.min(height);
    let square = pixbuf.new_subpixbuf((width - side) / 2, (height - side) / 2, side, side);
    let size = side.min(SIZE);
    let scaled = square
        .scale_simple(size, size, InterpType::Bilinear)
        .ok_or("could not scale the image")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let stamp = glib::real_time();
    let name = format!("{account_id}-{stamp}.png");
    scaled
        .savev(dir.join(&name), "png", &[])
        .map_err(|error| error.to_string())?;
    Ok(name)
}

/// Delete a picture file no account uses any more. `name` is a bare file
/// name, as [`Account::picture_file`] hands out.
pub fn remove(name: &str) {
    CACHE.with(|cache| cache.borrow_mut().remove(name));
    let path = dir().join(name);
    if let Err(error) = std::fs::remove_file(&path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("could not remove {}: {error}", path.display());
        }
    }
}
