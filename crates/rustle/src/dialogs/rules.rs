//! Rules: the list, and the editor for one. What they do is in
//! rustle_core::rules and window/rules.rs.

use crate::i18n::{self, gettext};
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::db::Database;
use rustle_core::folders;
use rustle_core::models::{Account, Folder};
use rustle_core::rules::{Field, Rule};
use std::cell::RefCell;
use std::rc::Rc;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RulesDialog {
        pub db: RefCell<Option<Rc<RefCell<Database>>>>,
        pub accounts: RefCell<Vec<Account>>,
        pub group: RefCell<Option<adw::PreferencesGroup>>,
        pub rows: RefCell<Vec<adw::ActionRow>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RulesDialog {
        const NAME: &'static str = "RustleRulesDialog";
        type Type = super::RulesDialog;
        type ParentType = adw::Dialog;
    }

    impl ObjectImpl for RulesDialog {}
    impl WidgetImpl for RulesDialog {}
    impl AdwDialogImpl for RulesDialog {}
}

glib::wrapper! {
    pub struct RulesDialog(ObjectSubclass<imp::RulesDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

fn field_label(field: Field) -> String {
    match field {
        Field::From => gettext("From"),
        Field::To => gettext("To"),
        Field::Subject => gettext("Subject"),
        Field::Body => gettext("Message text"),
        Field::Anywhere => gettext("Anywhere"),
    }
}

impl RulesDialog {
    pub fn new(db: Rc<RefCell<Database>>, accounts: Vec<Account>) -> Self {
        let dialog: Self = glib::Object::builder()
            .property("title", gettext("Rules"))
            .property("content-width", 520)
            .property("content-height", 560)
            .build();
        dialog.imp().db.replace(Some(db));
        dialog.imp().accounts.replace(accounts);
        dialog.build();
        dialog
    }

    fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.borrow().clone().expect("set at construction")
    }

    fn build(&self) {
        let group = adw::PreferencesGroup::builder()
            .title(gettext("New Mail"))
            .description(gettext(
                "Rules act on mail as it arrives in an inbox, while Rustle is running.",
            ))
            .build();
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(gettext("Add Rule"))
            .css_classes(["flat"])
            .build();
        add.connect_clicked(glib::clone!(
            #[weak(rename_to = dialog)]
            self,
            move |_| dialog.edit(None)
        ));
        group.set_header_suffix(Some(&add));
        let page = adw::PreferencesPage::new();
        page.add(&group);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));
        self.set_child(Some(&toolbar));
        self.imp().group.replace(Some(group));
        self.refresh();
    }

    fn refresh(&self) {
        let imp = self.imp();
        let Some(group) = imp.group.borrow().clone() else {
            return;
        };
        for row in imp.rows.borrow_mut().drain(..) {
            group.remove(&row);
        }
        let rules = self.db().borrow().rules(None).unwrap_or_default();
        let many_accounts = imp.accounts.borrow().len() > 1;
        for rule in rules {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&i18n::format(
                    &gettext("{field} contains “{text}”"),
                    &[("field", &field_label(rule.field)), ("text", &rule.pattern)],
                )))
                .subtitle(glib::markup_escape_text(
                    &self.describe_actions(&rule, many_accounts),
                ))
                .activatable(true)
                .build();
            let switch = gtk::Switch::builder()
                .active(rule.enabled)
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("On"))
                .build();
            let toggled = rule.clone();
            switch.connect_active_notify(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |switch| {
                    let rule = Rule {
                        enabled: switch.is_active(),
                        ..toggled.clone()
                    };
                    if let Err(error) = dialog.db().borrow().save_rule(&rule) {
                        log::error!("could not switch rule {}: {error}", rule.id);
                    }
                }
            ));
            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(gettext("Delete Rule"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let rule_id = rule.id;
            delete.connect_clicked(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_| {
                    if let Err(error) = dialog.db().borrow().delete_rule(rule_id) {
                        log::error!("could not delete rule {rule_id}: {error}");
                    }
                    dialog.refresh();
                }
            ));
            row.add_suffix(&switch);
            row.add_suffix(&delete);
            row.connect_activated(glib::clone!(
                #[weak(rename_to = dialog)]
                self,
                move |_| dialog.edit(Some(rule.clone()))
            ));
            group.add(&row);
            imp.rows.borrow_mut().push(row);
        }
        if imp.rows.borrow().is_empty() {
            let row = adw::ActionRow::builder()
                .title(gettext("No rules yet"))
                .subtitle(gettext(
                    "Add one to file newsletters, star mail from someone, and more.",
                ))
                .build();
            group.add(&row);
            imp.rows.borrow_mut().push(row);
        }
    }

    fn describe_actions(&self, rule: &Rule, with_account: bool) -> String {
        let mut parts = Vec::new();
        if rule.mark_read {
            parts.push(gettext("Mark read"));
        }
        if rule.star {
            parts.push(gettext("Star"));
        }
        if let Some(folder) = rule
            .move_to
            .and_then(|id| self.db().borrow().folder(id).ok().flatten())
        {
            parts.push(i18n::format(
                &gettext("Move to {folder}"),
                &[("folder", &folder_name(&folder))],
            ));
        }
        if with_account {
            let account = self
                .imp()
                .accounts
                .borrow()
                .iter()
                .find(|account| account.id == rule.account_id)
                .map(|account| account.email.clone());
            parts.extend(account);
        }
        parts.join(" · ")
    }

    /// The editor for a new rule (None) or an existing one.
    fn edit(&self, rule: Option<Rule>) {
        let accounts = self.imp().accounts.borrow().clone();
        let Some(first) = accounts.first() else {
            return;
        };
        let rule = rule.unwrap_or(Rule {
            id: 0,
            account_id: first.id,
            field: Field::From,
            pattern: String::new(),
            mark_read: false,
            star: false,
            move_to: None,
            enabled: true,
        });

        let emails: Vec<String> = accounts.iter().map(|a| a.email.clone()).collect();
        let account_choice =
            gtk::DropDown::from_strings(&emails.iter().map(String::as_str).collect::<Vec<_>>());
        account_choice.set_selected(
            accounts
                .iter()
                .position(|account| account.id == rule.account_id)
                .unwrap_or(0) as u32,
        );
        account_choice.set_visible(accounts.len() > 1);
        let fields: Vec<String> = Field::ALL.iter().map(|f| field_label(*f)).collect();
        let field_choice =
            gtk::DropDown::from_strings(&fields.iter().map(String::as_str).collect::<Vec<_>>());
        field_choice.set_selected(
            Field::ALL
                .iter()
                .position(|field| *field == rule.field)
                .unwrap_or(0) as u32,
        );
        let pattern = gtk::Entry::builder()
            .text(&rule.pattern)
            .placeholder_text(gettext("contains…"))
            .tooltip_text(gettext(
                "Separate alternatives with commas: any of them matches.",
            ))
            .hexpand(true)
            .activates_default(true)
            .build();
        let mark_read = gtk::CheckButton::with_label(&gettext("Mark as read"));
        mark_read.set_active(rule.mark_read);
        let star = gtk::CheckButton::with_label(&gettext("Star"));
        star.set_active(rule.star);
        let move_choice = gtk::DropDown::from_strings(&[]);
        let folders: Rc<RefCell<Vec<Folder>>> = Rc::default();
        let fill_folders = {
            let (db, folders, move_choice, accounts) = (
                self.db(),
                folders.clone(),
                move_choice.clone(),
                accounts.clone(),
            );
            move |account_index: u32, selected: Option<i64>| {
                let Some(account) = accounts.get(account_index as usize) else {
                    return;
                };
                let list: Vec<Folder> = db
                    .borrow()
                    .folders_for_account(account.id)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|folder| folder.name != folders::OUTBOX_FOLDER)
                    .collect();
                let mut labels = vec![gettext("Don't move")];
                labels.extend(list.iter().map(folder_name));
                move_choice.set_model(Some(&gtk::StringList::new(
                    &labels.iter().map(String::as_str).collect::<Vec<_>>(),
                )));
                let position = selected
                    .and_then(|id| list.iter().position(|folder| folder.id == id))
                    .map_or(0, |index| index + 1);
                move_choice.set_selected(position as u32);
                folders.replace(list);
            }
        };
        fill_folders(account_choice.selected(), rule.move_to);
        let refill = fill_folders.clone();
        account_choice.connect_selected_notify(move |choice| refill(choice.selected(), None));

        let condition = gtk::Box::builder().spacing(6).build();
        condition.append(&field_choice);
        condition.append(&pattern);
        let move_row = gtk::Box::builder().spacing(6).build();
        move_row.append(&gtk::Label::new(Some(&gettext("Move to"))));
        move_row.append(&move_choice);
        let form = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        form.append(&account_choice);
        form.append(&condition);
        form.append(&mark_read);
        form.append(&star);
        form.append(&move_row);

        let dialog = adw::AlertDialog::new(
            Some(&if rule.id == 0 {
                gettext("New Rule")
            } else {
                gettext("Edit Rule")
            }),
            Some(&gettext("When new mail in an inbox matches:")),
        );
        dialog.set_extra_child(Some(&form));
        dialog.add_responses(&[("cancel", &gettext("Cancel")), ("save", &gettext("Save"))]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let update = {
            let (dialog, pattern, mark_read, star, move_choice) = (
                dialog.clone(),
                pattern.clone(),
                mark_read.clone(),
                star.clone(),
                move_choice.clone(),
            );
            move || {
                let has_text = pattern
                    .text()
                    .split(',')
                    .any(|part| !part.trim().is_empty());
                let has_action =
                    mark_read.is_active() || star.is_active() || move_choice.selected() > 0;
                dialog.set_response_enabled("save", has_text && has_action);
            }
        };
        update();
        pattern.connect_changed({
            let update = update.clone();
            move |_| update()
        });
        for check in [&mark_read, &star] {
            let update = update.clone();
            check.connect_toggled(move |_| update());
        }
        move_choice.connect_selected_notify(move |_| update());

        dialog.connect_response(
            None,
            glib::clone!(
                #[weak(rename_to = rules)]
                self,
                move |_, response| {
                    if response != "save" {
                        return;
                    }
                    let account_id = accounts
                        .get(account_choice.selected() as usize)
                        .map_or(rule.account_id, |account| account.id);
                    let move_to = (move_choice.selected() as usize)
                        .checked_sub(1)
                        .and_then(|index| folders.borrow().get(index).map(|folder| folder.id));
                    let saved = Rule {
                        account_id,
                        field: Field::ALL[field_choice.selected() as usize % Field::ALL.len()],
                        pattern: pattern.text().trim().to_string(),
                        mark_read: mark_read.is_active(),
                        star: star.is_active(),
                        move_to,
                        ..rule.clone()
                    };
                    if let Err(error) = rules.db().borrow().save_rule(&saved) {
                        log::error!("could not save a rule: {error}");
                    }
                    rules.refresh();
                }
            ),
        );
        dialog.present(Some(self));
    }
}

fn folder_name(folder: &Folder) -> String {
    folders::decode_mailbox_name(&folder.name)
}
