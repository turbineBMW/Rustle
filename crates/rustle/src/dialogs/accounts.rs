//! The Manage Accounts dialog: the list, with remove, and the one way in to
//! adding an account.

use super::add_account::AddAccountDialog;
use super::signature::SignatureDialog;
use crate::account_colors;
use crate::account_pictures;
use crate::i18n::gettext;
use crate::widgets::sound_row;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use rustle_core::db::Database;
use rustle_core::models::Account;
use rustle_core::sounds::NotificationSound;
use rustle_core::{eds, secrets};
use std::cell::RefCell;
use std::rc::Rc;

const PAGE_LIST: &str = "list";
const PAGE_EMPTY: &str = "empty";

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/accounts-dialog.ui")]
    pub struct AccountsDialog {
        #[template_child]
        pub accounts_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub accounts_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub add_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub empty_add_button: TemplateChild<gtk::Button>,
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub settings: RefCell<Option<gio::Settings>>,
        pub rows: RefCell<Vec<adw::ExpanderRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AccountsDialog {
        const NAME: &'static str = "RustleAccountsDialog";
        type Type = super::AccountsDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AccountsDialog {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
                std::sync::OnceLock::new();
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("account-added").build()])
        }

        fn constructed(&self) {
            self.parent_constructed();
            let dialog = self.obj().clone();
            for button in [&self.add_button, &self.empty_add_button] {
                button.connect_clicked(glib::clone!(
                    #[weak]
                    dialog,
                    move |_| dialog.on_add_clicked()
                ));
            }
        }
    }
    impl WidgetImpl for AccountsDialog {}
    impl AdwDialogImpl for AccountsDialog {}
}

glib::wrapper! {
    pub struct AccountsDialog(ObjectSubclass<imp::AccountsDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl AccountsDialog {
    pub fn new(db: Rc<RefCell<Database>>, settings: &gio::Settings) -> Self {
        let dialog: Self = glib::Object::new();
        dialog.imp().db.replace(Some(db));
        dialog.imp().settings.replace(Some(settings.clone()));
        dialog.reload();
        dialog
    }

    pub fn connect_account_added(&self, callback: impl Fn(&Self) + 'static) {
        self.connect_local("account-added", false, move |values| {
            let dialog = values[0].get::<Self>().expect("the emitter");
            callback(&dialog);
            None
        });
    }

    fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.borrow().clone().expect("set at construction")
    }

    fn reload(&self) {
        let imp = self.imp();
        for row in imp.rows.borrow_mut().drain(..) {
            imp.accounts_group.remove(&row);
        }
        let accounts = self.db().borrow().accounts().unwrap_or_default();
        account_colors::apply(&accounts);
        imp.accounts_stack
            .set_visible_child_name(if accounts.is_empty() {
                PAGE_EMPTY
            } else {
                PAGE_LIST
            });
        for account in accounts {
            let row = adw::ExpanderRow::builder()
                .title(account.name())
                .subtitle(if account.label.trim().is_empty() {
                    &account.display_name
                } else {
                    &account.email
                })
                .build();
            let avatar = adw::Avatar::builder()
                .size(32)
                .text(account.name())
                .show_initials(true)
                .css_classes(["account-avatar"])
                .build();
            account_colors::tag(&avatar, Some(account.id));
            avatar.set_custom_image(account_pictures::texture(&account).as_ref());
            row.add_prefix(&avatar);
            row.add_row(&self.name_row(&account));
            row.add_row(&self.picture_row(&account, &avatar));
            row.add_row(&self.signature_row(&account));
            row.add_row(&self.sound_row(&account));
            let color_button = gtk::ColorDialogButton::builder()
                .dialog(
                    &gtk::ColorDialog::builder()
                        .title(gettext("Account Colour"))
                        .with_alpha(false)
                        .build(),
                )
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Colour used to mark this account's mail"))
                .build();
            if let Some(rgba) = account_colors::parse_hex(account.color_hex()) {
                color_button.set_rgba(&rgba);
            }
            let account_id = account.id;
            color_button.connect_rgba_notify(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |button| dialog.on_color_picked(account_id, &button.rgba())
            ));
            row.add_suffix(&color_button);
            // Only accounts Rustle created leave EDS with it; others (Online
            // Accounts, graphmail-bridge) stay for the apps that use them.
            let remove_button = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(if account.is_own() {
                    gettext("Remove Account")
                } else {
                    gettext("Remove from Rustle")
                })
                .css_classes(["flat"])
                .build();
            let removed = account.clone();
            remove_button.connect_clicked(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_| dialog.on_remove_clicked(&removed)
            ));
            row.add_suffix(&remove_button);
            imp.accounts_group.add(&row);
            imp.rows.borrow_mut().push(row);
        }
    }

    /// What the user calls the account, saved as it is typed. The row's
    /// title follows on the next reload (closing the dialog), like the
    /// sidebar.
    fn name_row(&self, account: &Account) -> gtk::Widget {
        let row = adw::EntryRow::builder()
            .title(gettext("Name"))
            .text(&account.label)
            .build();
        let account_id = account.id;
        row.connect_changed(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |row| dialog.on_label_changed(account_id, &row.text())
        ));
        row.upcast()
    }

    /// The picture that stands for the account in the sidebar and, in the
    /// unified inbox, on its mail; without one its colour does. Changes show
    /// at once in `avatar`, the expander's own.
    fn picture_row(&self, account: &Account, avatar: &adw::Avatar) -> gtk::Widget {
        let row = adw::ActionRow::builder()
            .title(gettext("Picture"))
            .activatable(true)
            .build();
        let remove = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text(gettext("Remove Picture"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .visible(account.picture_file().is_some())
            .build();
        let choose = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text(gettext("Choose Picture…"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&remove);
        row.add_suffix(&choose);
        row.set_activatable_widget(Some(&choose));
        let picture = PictureRow {
            account: Rc::new(RefCell::new(account.clone())),
            row: row.clone(),
            avatar: avatar.clone(),
            remove: remove.clone(),
        };
        choose.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            #[strong]
            picture,
            move |_| dialog.on_choose_picture(&picture)
        ));
        remove.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            #[strong]
            picture,
            move |_| dialog.on_picture_changed(&picture, String::new())
        ));
        row.upcast()
    }

    fn on_choose_picture(&self, picture: &PictureRow) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(&gettext("Images")));
        filter.add_pixbuf_formats();
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let chooser = gtk::FileDialog::builder()
            .title(gettext("Choose Account Picture"))
            .modal(true)
            .filters(&filters)
            .default_filter(&filter)
            .build();
        let parent = self.root().and_downcast::<gtk::Window>();
        let picture = picture.clone();
        chooser.open(
            parent.as_ref(),
            gio::Cancellable::NONE,
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |result| {
                    // An error here is the user cancelling.
                    let Some(path) = result.ok().and_then(|file| file.path()) else {
                        return;
                    };
                    dialog.import_picture(picture, path);
                }
            ),
        );
    }

    fn import_picture(&self, picture: PictureRow, source: std::path::PathBuf) {
        let account_id = picture.account.borrow().id;
        let dir = account_pictures::dir();
        let shown = source.clone();
        workers::run(
            move || account_pictures::import(&source, &dir, account_id),
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |result: Result<String, String>| match result {
                    Ok(name) => dialog.on_picture_changed(&picture, name),
                    Err(error) => {
                        log::error!(
                            "could not use {} as the picture of account {account_id}: {error}",
                            shown.display()
                        );
                        picture
                            .row
                            .set_subtitle(&gettext("Could not open that image"));
                    }
                }
            ),
        );
    }

    /// Store the account's new picture file name ("" for none), drop the old
    /// file and show the change.
    fn on_picture_changed(&self, picture: &PictureRow, name: String) {
        let account_id = picture.account.borrow().id;
        if let Err(error) = self.db().borrow().set_account_picture(account_id, &name) {
            log::error!("could not save the picture of account {account_id}: {error}");
            return;
        }
        let old = {
            let mut account = picture.account.borrow_mut();
            let old = account.picture_file().map(str::to_owned);
            account.picture = name;
            old
        };
        if let Some(old) = old {
            account_pictures::remove(&old);
        }
        let texture = account_pictures::texture(&picture.account.borrow());
        picture.avatar.set_custom_image(texture.as_ref());
        picture.remove.set_visible(texture.is_some());
        picture.row.set_subtitle("");
    }

    /// One line of the signature and a button into the editor.
    fn signature_row(&self, account: &Account) -> gtk::Widget {
        let preview = rustle_core::html::html_to_text(&account.signature_html());
        let first_line = preview.lines().find(|line| !line.trim().is_empty());
        let row = adw::ActionRow::builder()
            .title(gettext("Signature"))
            .subtitle(first_line.unwrap_or(&gettext("None")))
            .subtitle_lines(1)
            .activatable(true)
            .build();
        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text(gettext("Edit Signature"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&edit);
        row.set_activatable_widget(Some(&edit));
        let account = account.clone();
        edit.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.on_edit_signature(&account)
        ));
        row.upcast()
    }

    /// Which sound this account's new mail plays; "App Default" follows
    /// Preferences.
    fn sound_row(&self, account: &Account) -> gtk::Widget {
        let row = adw::ComboRow::builder()
            .title(gettext("Notification Sound"))
            .build();
        let account_id = account.id;
        sound_row::setup(
            &row,
            NotificationSound::parse(&account.notification_sound),
            self.imp().settings.borrow().clone(),
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |choice| dialog.on_sound_changed(account_id, choice)
            ),
        );
        row.upcast()
    }

    fn on_sound_changed(&self, account_id: i64, sound: &NotificationSound) {
        if let Err(error) = self
            .db()
            .borrow()
            .set_account_notification_sound(account_id, &sound.as_setting())
        {
            log::error!("could not save the notification sound of account {account_id}: {error}");
        }
    }

    fn on_edit_signature(&self, account: &Account) {
        let account_id = account.id;
        let editor = SignatureDialog::new(
            account.name(),
            &account.signature_html(),
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |html| {
                    dialog.on_signature_changed(account_id, html);
                    dialog.reload();
                }
            ),
        );
        editor.present(Some(self));
    }

    fn on_label_changed(&self, account_id: i64, label: &str) {
        if let Err(error) = self.db().borrow().set_account_label(account_id, label) {
            log::error!("could not rename account {account_id}: {error}");
        }
    }

    fn on_signature_changed(&self, account_id: i64, signature: &str) {
        if let Err(error) = self
            .db()
            .borrow()
            .set_account_signature(account_id, signature)
        {
            log::error!("could not save the signature of account {account_id}: {error}");
        }
    }

    /// Persist a picked colour and recolour every tagged widget at once.
    fn on_color_picked(&self, account_id: i64, rgba: &gdk::RGBA) {
        let hex = crate::accent::rgba_hex(rgba);
        let db = self.db();
        if let Err(error) = db.borrow().set_account_color(account_id, &hex) {
            log::error!("could not save the colour of account {account_id}: {error}");
            return;
        }
        let accounts = db.borrow().accounts().unwrap_or_default();
        account_colors::apply(&accounts);
    }

    fn on_remove_clicked(&self, account: &Account) {
        let account_id = account.id;
        if !account.is_own() {
            if let Err(error) = self.db().borrow_mut().hide_account(account_id) {
                log::error!("could not remove account {account_id}: {error}");
                return;
            }
            self.reload();
            return;
        }
        let root = account.eds_root_uid.clone();
        let uids = [
            account.eds_root_uid.clone(),
            account.eds_uid.clone(),
            account.eds_smtp_uid.clone(),
        ];
        workers::run(
            move || {
                eds::remove_source(&root).map_err(|error| error.to_string())?;
                for uid in uids {
                    if let Err(error) = secrets::clear_source_password(&uid) {
                        log::warn!("could not clear the keyring entry of {uid}: {error}");
                    }
                }
                Ok::<_, String>(())
            },
            glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |result: Result<(), String>| {
                    if let Err(error) = result {
                        log::error!(
                            "could not remove account {account_id} from Evolution Data Server: {error}"
                        );
                        return;
                    }
                    // The picture may have changed since `account` was read.
                    let stored = dialog.db().borrow().account(account_id).ok().flatten();
                    if let Err(error) = dialog.db().borrow_mut().delete_account(account_id) {
                        log::error!("could not delete account {account_id}: {error}");
                    } else if let Some(name) = stored.as_ref().and_then(Account::picture_file) {
                        account_pictures::remove(name);
                    }
                    dialog.reload();
                }
            ),
        );
    }

    fn on_add_clicked(&self) {
        let dialog = AddAccountDialog::new(self.db());
        dialog.connect_account_added(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                this.reload();
                this.emit_by_name::<()>("account-added", &[]);
            }
        ));
        dialog.present(Some(self));
    }
}

/// What the picture row's buttons share: the account as last saved, and the
/// widgets that show its picture.
#[derive(Clone)]
struct PictureRow {
    account: Rc<RefCell<Account>>,
    row: adw::ActionRow,
    avatar: adw::Avatar,
    remove: gtk::Button,
}
