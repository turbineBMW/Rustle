//! Searching the server. Typed searches send their words to a full-text
//! SEARCH of the view's folders as typing settles, and the matches join the
//! list: the database holds few bodies, the server holds them all.
//!
//! Smart Search: with the sparkle toggled in the search bar, Enter hands
//! what was typed to the assistant's tool (`rustle_core::assistant`), which
//! answers with a `SearchFilter`. The list then shows the view's emails that
//! pass it, from the database at once and, for the words, from a full-text
//! SEARCH of the view's folders on the server as it comes back. The tool
//! sees the request only, never mail.

use super::MainWindow;
use crate::i18n::{self, gettext};
use crate::settings as keys;
use crate::workers;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use rustle_core::assistant::{self, Harness, SearchFilter};
use rustle_core::models::{Account, Folder};
use rustle_core::{secrets, sync};
use std::collections::{HashMap, HashSet};

/// The search the list is showing, once the tool has answered.
#[derive(Clone)]
pub struct SmartSearch {
    pub filter: SearchFilter,
    /// Emails the server found the words in.
    pub server_ids: HashSet<i64>,
}

impl MainWindow {
    pub(super) fn setup_smart_search(&self) {
        let imp = self.imp();
        self.settings().connect_changed(
            Some(keys::ASSISTANT),
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_, _| window.update_ask_button()
            ),
        );
        self.update_ask_button();
        imp.ask_button.connect_toggled(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |button| window.on_ask_toggled(button.is_active())
        ));
        imp.search_entry.connect_activate(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                if window.imp().ask_button.is_active() {
                    window.run_smart_search();
                }
            }
        ));
    }

    fn harness(&self) -> Option<Harness> {
        Harness::parse(&self.settings().string(keys::ASSISTANT))
    }

    /// The toggle is there only while a tool is picked.
    fn update_ask_button(&self) {
        let button = &self.imp().ask_button;
        let harness = self.harness();
        button.set_visible(harness.is_some());
        if let Some(harness) = harness {
            button.set_tooltip_text(Some(&i18n::format(
                &gettext("Smart Search with {tool}"),
                &[("tool", harness.label())],
            )));
        } else if button.is_active() {
            button.set_active(false);
        }
    }

    fn on_ask_toggled(&self, is_active: bool) {
        let entry = &self.imp().search_entry;
        entry.set_placeholder_text(Some(&if is_active {
            gettext("Describe it, then press Enter")
        } else {
            gettext("Search mail")
        }));
        if is_active {
            entry.grab_focus();
        }
        self.clear_smart_search();
        // Back to plain search over whatever is typed, or the other way:
        // nothing runs until Enter.
        if !is_active {
            self.refresh_emails(self.selected_email().map(|email| email.id()));
        }
    }

    /// Typing while the toggle is on doesn't search; emptying the field
    /// (closing the search bar does) drops the smart search.
    pub(super) fn is_smart_typing(&self) -> bool {
        let imp = self.imp();
        if !imp.ask_button.is_active() {
            return false;
        }
        if imp.search_entry.text().trim().is_empty() && self.state().smart_search.is_some() {
            self.clear_smart_search();
            self.refresh_emails(None);
        }
        true
    }

    fn clear_smart_search(&self) {
        {
            let mut state = self.state_mut();
            state.smart_search = None;
            state.smart_generation += 1;
            state.smart_pending = 0;
        }
        let imp = self.imp();
        imp.smart_summary.set_visible(false);
        self.update_ask_spinner();
    }

    fn update_ask_spinner(&self) {
        let is_busy = {
            let state = self.state();
            state.smart_pending > 0 || state.typed_pending > 0
        };
        let spinner = &self.imp().ask_spinner;
        spinner.set_visible(is_busy);
        spinner.set_spinning(is_busy);
    }

    fn show_summary(&self, text: &str) {
        let label = &self.imp().smart_summary;
        label.set_label(text);
        label.set_visible(!text.is_empty());
    }

    fn run_smart_search(&self) {
        let Some(harness) = self.harness() else {
            return;
        };
        let request = self.imp().search_entry.text().trim().to_string();
        if request.is_empty() {
            return;
        }
        self.clear_smart_search();
        let generation = {
            let mut state = self.state_mut();
            state.smart_pending = 1;
            state.smart_generation
        };
        self.update_ask_spinner();
        self.show_summary(&i18n::format(
            &gettext("Asking {tool}…"),
            &[("tool", harness.label())],
        ));
        let model = self.settings().string(keys::ASSISTANT_MODEL).to_string();
        let prompt = assistant::search_prompt(&request, chrono::Local::now().date_naive());
        workers::run(
            move || {
                let answer = assistant::ask(harness, &model, &prompt)?;
                assistant::parse_filter(&answer).ok_or_else(|| {
                    log::debug!("unusable Smart Search answer: {answer}");
                    "the answer wasn't a search".to_string()
                })
            },
            glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |result: Result<SearchFilter, String>| {
                    if window.state().smart_generation != generation {
                        return; // asked again, or switched off, meanwhile
                    }
                    window.state_mut().smart_pending = 0;
                    window.update_ask_spinner();
                    match result {
                        Ok(filter) => window.on_filter_ready(filter, generation),
                        Err(message) => {
                            log::warn!("Smart Search with {} failed: {message}", harness.id());
                            window.show_summary("");
                            window.toast(&i18n::format(
                                &gettext("Couldn't ask {tool}: {msg}"),
                                &[("tool", harness.label()), ("msg", &message)],
                            ));
                        }
                    }
                }
            ),
        );
    }

    fn on_filter_ready(&self, filter: SearchFilter, generation: u64) {
        if filter.is_empty() {
            self.show_summary(&gettext("Couldn't make a search out of that."));
            return;
        }
        self.show_summary(&describe(&filter));
        self.state_mut().smart_search = Some(SmartSearch {
            filter: filter.clone(),
            server_ids: HashSet::new(),
        });
        self.refresh_emails(None);
        if let Some(criteria) = filter.imap_criteria() {
            self.search_server(&criteria, generation);
        }
    }

    /// The words, in the full text of the view's folders on the server:
    /// one connection per account.
    fn search_server(&self, criteria: &str, generation: u64) {
        if !self.state().is_online {
            return;
        }
        let mut by_account: HashMap<i64, Vec<Folder>> = HashMap::new();
        for folder in self.current_folders() {
            by_account
                .entry(folder.account_id)
                .or_default()
                .push(folder);
        }
        for (account_id, folders) in by_account {
            let account = self.state().accounts.get(&account_id).cloned();
            let Some(account) = account else { continue };
            self.state_mut().smart_pending += 1;
            let names: Vec<String> = folders.iter().map(|folder| folder.name.clone()).collect();
            let criteria = criteria.to_string();
            let job_account = account.clone();
            workers::run(
                move || search_job(&job_account, &names, &criteria),
                glib::clone!(
                    #[weak(rename_to = window)]
                    self,
                    move |found: Vec<(String, Vec<String>)>| {
                        window.on_server_found(&account, &folders, found, generation)
                    }
                ),
            );
        }
        self.update_ask_spinner();
    }

    /// The typed search's words, in the full text of the view's folders on
    /// the server: the database has few bodies, the server has them all.
    /// Matches join the list as they come back.
    pub(super) fn search_server_for_typed(&self) {
        let query = self.imp().search_entry.text().trim().to_string();
        let generation = {
            let mut state = self.state_mut();
            state.typed_generation += 1;
            state.typed_pending = 0;
            if state.typed_matches.0 != query {
                state.typed_matches = (query.clone(), HashSet::new());
            }
            state.typed_generation
        };
        self.update_ask_spinner();
        if self.imp().ask_button.is_active() || !self.state().is_online {
            return;
        }
        let Some(criteria) = rustle_core::query::parse(&query).imap_criteria() else {
            return;
        };
        let mut by_account: HashMap<i64, Vec<Folder>> = HashMap::new();
        for folder in self.current_folders() {
            if folder.name == rustle_core::folders::OUTBOX_FOLDER {
                continue;
            }
            by_account
                .entry(folder.account_id)
                .or_default()
                .push(folder);
        }
        for (account_id, folders) in by_account {
            let account = self.state().accounts.get(&account_id).cloned();
            let Some(account) = account else { continue };
            self.state_mut().typed_pending += 1;
            let names: Vec<String> = folders.iter().map(|folder| folder.name.clone()).collect();
            let criteria = criteria.clone();
            let job_account = account.clone();
            let query = query.clone();
            workers::run(
                move || search_job(&job_account, &names, &criteria),
                glib::clone!(
                    #[weak(rename_to = window)]
                    self,
                    move |found: Vec<(String, Vec<String>)>| {
                        window.on_typed_found(&account, &folders, &query, found, generation)
                    }
                ),
            );
        }
        self.update_ask_spinner();
    }

    fn on_typed_found(
        &self,
        account: &Account,
        folders: &[Folder],
        query: &str,
        found: Vec<(String, Vec<String>)>,
        generation: u64,
    ) {
        if self.state().typed_generation != generation {
            return;
        }
        let ids = self.local_ids(account, folders, &found);
        let is_new = {
            let mut state = self.state_mut();
            state.typed_pending = state.typed_pending.saturating_sub(1);
            if state.typed_matches.0 != query {
                false
            } else {
                let before = state.typed_matches.1.len();
                state.typed_matches.1.extend(ids);
                state.typed_matches.1.len() > before
            }
        };
        self.update_ask_spinner();
        if is_new {
            self.refresh_emails(self.selected_email().map(|email| email.id()));
        }
    }

    /// The local ids of the messages a server search found, by mailbox.
    fn local_ids(
        &self,
        account: &Account,
        folders: &[Folder],
        found: &[(String, Vec<String>)],
    ) -> Vec<i64> {
        let mut ids = Vec::new();
        let db = self.db();
        let db = db.borrow();
        for (mailbox, uids) in found {
            let Some(folder) = folders.iter().find(|folder| folder.name == *mailbox) else {
                continue;
            };
            match db.email_ids_for_uids(folder.id, uids) {
                Ok(found) => ids.extend(found),
                Err(error) => log::error!(
                    "could not look up search matches in {mailbox} (account {}): {error}",
                    account.email
                ),
            }
        }
        ids
    }

    fn on_server_found(
        &self,
        account: &Account,
        folders: &[Folder],
        found: Vec<(String, Vec<String>)>,
        generation: u64,
    ) {
        if self.state().smart_generation != generation {
            return;
        }
        let ids = self.local_ids(account, folders, &found);
        let is_new = {
            let mut state = self.state_mut();
            state.smart_pending = state.smart_pending.saturating_sub(1);
            match state.smart_search.as_mut() {
                Some(search) => {
                    let before = search.server_ids.len();
                    search.server_ids.extend(ids);
                    search.server_ids.len() > before
                }
                None => false,
            }
        };
        self.update_ask_spinner();
        if is_new {
            self.refresh_emails(self.selected_email().map(|email| email.id()));
        }
    }
}

/// Runs on a worker. A failure only costs the body matches, so it is
/// logged and reported as nothing found.
fn search_job(
    account: &Account,
    mailboxes: &[String],
    criteria: &str,
) -> Vec<(String, Vec<String>)> {
    let Some(credential) = secrets::credential_for(account) else {
        log::warn!(
            "could not sign in to account {} to search it",
            account.email
        );
        return Vec::new();
    };
    sync::search_text(account, &credential, mailboxes, criteria).unwrap_or_else(|error| {
        log::warn!(
            "could not search {} on {} (account {}): {error}",
            mailboxes.join(", "),
            account.imap_host,
            account.email
        );
        Vec::new()
    })
}

/// What the tool made of the request, so a wrong reading is plain to see:
/// "From “ada” · Words: invoice, pdf · On or after Sep 1, 2026".
fn describe(filter: &SearchFilter) -> String {
    let quoted = |template: &str, value: &Option<String>| {
        value
            .as_ref()
            .map(|value| i18n::format(template, &[("text", value.trim())]))
    };
    let day = |day: chrono::NaiveDate| day.format("%b %-d, %Y").to_string();
    let mut parts: Vec<String> = Vec::new();
    parts.extend(quoted(&gettext("From “{text}”"), &filter.from));
    parts.extend(quoted(&gettext("To “{text}”"), &filter.to));
    parts.extend(quoted(&gettext("Subject “{text}”"), &filter.subject));
    if !filter.words.is_empty() {
        parts.push(i18n::format(
            &gettext("Words: {words}"),
            &[("words", &filter.words.join(", "))],
        ));
    }
    if let Some(after) = filter.after_day() {
        parts.push(i18n::format(
            &gettext("On or after {date}"),
            &[("date", &day(after))],
        ));
    }
    if let Some(before) = filter.before_day() {
        parts.push(i18n::format(
            &gettext("Before {date}"),
            &[("date", &day(before))],
        ));
    }
    match filter.unread {
        Some(true) => parts.push(gettext("Unread")),
        Some(false) => parts.push(gettext("Read")),
        None => {}
    }
    match filter.starred {
        Some(true) => parts.push(gettext("Starred")),
        Some(false) => parts.push(gettext("Not starred")),
        None => {}
    }
    parts.join(" · ")
}
