//! The main window: folders, conversations, reader. One class, ordered by
//! concern across the files of this module -- Rust lets each concern sit in
//! its own `impl` block without redeclaring anything.

mod accounts;
mod actions;
mod folders;
mod list;
mod moves;
mod reader;
mod sync;

use crate::avatar_loader::AvatarLoader;
use crate::objects::{ConversationObject, SidebarItem};
use crate::settings as keys;
use crate::widgets::folder_row::FolderRow;
use crate::widgets::message_view::{self, Handlers, MessageView};
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use rustle_core::db::Database;
use rustle_core::models::{Account, Folder};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

pub use moves::PendingMove;

/// Window action names, grouped by what enables and disables them together.
const MAIL_ACTIONS: [&str; 6] = [
    "toggle-read",
    "toggle-star",
    "toggle-pin",
    "archive",
    "trash",
    "move",
];
const REPLY_FORWARD_ACTIONS: [&str; 3] = ["reply", "reply-all", "forward"];

/// How long an archive/trash/move stays undoable before the real IMAP MOVE
/// runs. The Undo toast is shown for this window, so the two have to agree.
const MOVE_UNDO_MS: u64 = 5000;
/// Wait for typing to settle before running a search over FTS.
const SEARCH_DEBOUNCE_MS: u64 = 200;
/// How long a folder counts as freshly synced. Opening one costs a
/// connection, a login and a header fetch, which clicking between two
/// folders would otherwise pay for on every visit.
const FOLDER_SYNC_COOLDOWN_SECS: u64 = 60;

/// Gtk.Stack child names, matching the ids in main-window.blp.
const PAGE_MAIL: &str = "mail";
const PAGE_NO_ACCOUNT: &str = "no-account";
const PAGE_EMPTY: &str = "empty";
const PAGE_LIST: &str = "list";
const PAGE_LOADING: &str = "loading";
const PAGE_MESSAGE: &str = "message";

/// What the conversation list is showing.
#[derive(Clone, Debug)]
pub enum View {
    /// Every account's inbox at once.
    UnifiedInbox,
    Folder(Folder),
}

/// Source UIDs stay protected while an optimistic move is pending or its
/// worker is in flight. Completed moves remain protected until a newest-page
/// sync confirms that the source UID is gone.
#[derive(Default)]
pub struct Tombstone {
    pub active: i32,
    pub awaiting: i32,
}

/// Everything the window mutates. Kept in one `RefCell` behind short
/// borrows: read what you need, drop the borrow, then act.
#[derive(Default)]
pub struct State {
    /// Every account, by id.
    pub accounts: HashMap<i64, Account>,
    /// Set once `load_mail_view` has run; None on an empty database, so
    /// everything per-account reads it through a guard.
    pub view: Option<View>,
    pub active_view: Option<MessageView>,
    pub thread_views: Vec<MessageView>,
    pub search_timeout: Option<glib::SourceId>,
    pub rendered_id: Option<i64>,
    pub is_folder_refresh_suppressed: bool,
    pub is_selection_update_in_progress: bool,
    pub pending_moves: Vec<PendingMove>,
    pub pending_toast: Option<adw::Toast>,
    pub pending_timeout: Option<glib::SourceId>,
    pub move_tombstones: HashMap<(i64, String), Tombstone>,
    /// Load-on-scroll paging state, keyed by folder id.
    pub loaded_counts: HashMap<i64, u32>,
    pub folders_with_more_mail: HashMap<i64, bool>,
    pub folder_sync_times: HashMap<i64, Instant>,
    /// Unread counts the server last reported for the folders we don't fetch.
    pub remote_unread_counts: HashMap<i64, u32>,
    /// The rows currently on screen, so badges refresh without a rebuild.
    pub folder_rows: HashMap<i64, FolderRow>,
    pub account_rows: HashMap<i64, FolderRow>,
    pub unified_row: Option<FolderRow>,
    /// The account ids and (id, parent_id) folder pairs the tree was last
    /// built from.
    pub folder_shape: (Vec<i64>, Vec<(i64, Option<i64>)>),
    pub account_roots: HashMap<i64, Vec<Folder>>,
    pub folder_children: HashMap<i64, Vec<Folder>>,
    /// Accounts with a sync in flight. A set, not a flag: every account
    /// syncs on the same tick.
    pub syncing_account_ids: HashSet<i64>,
    pub sync_timer: Option<glib::SourceId>,
    pub is_online: bool,
    pub message_handlers: Option<Rc<Handlers>>,
}

mod imp {
    use super::*;

    #[derive(Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/turbinebmw/Rustle/ui/main-window.ui")]
    pub struct MainWindow {
        #[template_child]
        pub folder_list: TemplateChild<gtk::ListView>,
        #[template_child]
        pub conversation_list: TemplateChild<gtk::ListView>,
        #[template_child]
        pub conversation_scroller: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub sticky_day: TemplateChild<gtk::Label>,
        #[template_child]
        pub conversation_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub reader_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub reader_subject: TemplateChild<gtk::Label>,
        #[template_child]
        pub thread_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub main_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub add_account_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub online_accounts_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub refresh_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub search_bar: TemplateChild<gtk::SearchBar>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub unread_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub compose_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub reply_all_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub reply_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub forward_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub mark_read_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub star_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub pin_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub archive_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub archive_button_content: TemplateChild<adw::ButtonContent>,
        #[template_child]
        pub move_button: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub connection_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub outer_split: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub inner_split: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub folder_resize_handle: TemplateChild<gtk::Box>,
        #[template_child]
        pub conversation_resize_handle: TemplateChild<gtk::Box>,

        pub db: OnceCell<Rc<RefCell<Database>>>,
        pub settings: OnceCell<gio::Settings>,
        pub state: RefCell<State>,
        pub folder_root_store: OnceCell<gio::ListStore>,
        pub folder_tree_model: OnceCell<gtk::TreeListModel>,
        pub folder_selection: OnceCell<gtk::SingleSelection>,
        // One persistent outer store of per-day sections, each a ListStore
        // of ConversationObject, spliced in place on every refresh: swapping
        // in a new model makes GtkListView reset its scroll to the top, which
        // fights load-on-scroll. The flattened view is what the selection
        // and the list see; its sections drive the sticky day headers.
        pub conversation_sections: OnceCell<gio::ListStore>,
        pub conversation_model: OnceCell<gtk::FlattenListModel>,
        pub selection: OnceCell<gtk::MultiSelection>,
        pub avatars: OnceCell<AvatarLoader>,
        pub network: OnceCell<gio::NetworkMonitor>,
        pub is_closing: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MainWindow {
        const NAME: &'static str = "RustleMainWindow";
        type Type = super::MainWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for MainWindow {}
    impl WidgetImpl for MainWindow {}
    impl WindowImpl for MainWindow {
        fn close_request(&self) -> glib::Propagation {
            self.obj().on_close_request()
        }
    }
    impl ApplicationWindowImpl for MainWindow {}
    impl AdwApplicationWindowImpl for MainWindow {}
}

glib::wrapper! {
    pub struct MainWindow(ObjectSubclass<imp::MainWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl MainWindow {
    pub fn new(
        app: &impl IsA<gtk::Application>,
        db: Rc<RefCell<Database>>,
        settings: &gio::Settings,
    ) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();
        let imp = window.imp();
        let _ = imp.db.set(db);
        let _ = imp.settings.set(settings.clone());

        window.set_default_size(
            settings.int(keys::WINDOW_WIDTH),
            settings.int(keys::WINDOW_HEIGHT),
        );
        if settings.boolean(keys::WINDOW_MAXIMIZED) {
            window.maximize();
        }
        // Drag limits mirror the <range> in the gschema: the schema stops a
        // bad stored value, these stop a drag from producing one.
        window.setup_sidebar_resize(
            &imp.folder_resize_handle,
            &imp.outer_split,
            keys::FOLDER_WIDTH,
            180,
            500,
        );
        window.setup_sidebar_resize(
            &imp.conversation_resize_handle,
            &imp.inner_split,
            keys::CONVERSATION_WIDTH,
            220,
            600,
        );

        let _ = imp.avatars.set(AvatarLoader::new(settings.clone()));
        settings.connect_changed(
            Some(keys::LOAD_SENDER_AVATARS),
            glib::clone!(
                #[weak]
                window,
                move |_, _| window.refresh_conversations(None)
            ),
        );
        settings.connect_changed(
            Some(keys::SYNC_INTERVAL),
            glib::clone!(
                #[weak]
                window,
                move |_, _| window.reschedule_sync()
            ),
        );

        window.setup_actions();
        window.connect_widgets();
        window.setup_message_handlers();

        let network = gio::NetworkMonitor::default();
        imp.state.borrow_mut().is_online = network.is_network_available();
        network.connect_network_changed(glib::clone!(
            #[weak]
            window,
            move |_, is_available| window.on_network_changed(is_available)
        ));
        let _ = imp.network.set(network);

        window.build_mail_models();

        // The WebKit views carry the accent in their own stylesheet, so a
        // change in Settings re-renders the open thread with the new colour.
        crate::accent::watch(glib::clone!(
            #[weak]
            window,
            move || {
                if window.state_mut().rendered_id.take().is_some() {
                    window.update_reader();
                }
            }
        ));

        // Actions start enabled, so the accelerators would stay live on a
        // window with nothing selected to act on.
        window.set_mail_actions_enabled(false);
        window.set_reply_forward_enabled(false);

        if !window.state().is_online {
            window.show_offline_banner();
        }

        let has_accounts = window
            .db()
            .borrow()
            .accounts()
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !has_accounts {
            imp.main_stack.set_visible_child_name(PAGE_NO_ACCOUNT);
            return window;
        }
        window.load_mail_view();
        window
    }

    // --- shared access ----------------------------------------------------

    pub(super) fn db(&self) -> Rc<RefCell<Database>> {
        self.imp().db.get().expect("set at construction").clone()
    }

    pub(super) fn settings(&self) -> gio::Settings {
        self.imp()
            .settings
            .get()
            .expect("set at construction")
            .clone()
    }

    pub(super) fn state(&self) -> std::cell::Ref<'_, State> {
        self.imp().state.borrow()
    }

    pub(super) fn state_mut(&self) -> std::cell::RefMut<'_, State> {
        self.imp().state.borrow_mut()
    }

    pub(super) fn avatars(&self) -> AvatarLoader {
        self.imp()
            .avatars
            .get()
            .expect("set at construction")
            .clone()
    }

    pub(super) fn conversation_sections(&self) -> gio::ListStore {
        self.imp()
            .conversation_sections
            .get()
            .expect("built at construction")
            .clone()
    }

    /// Every listed conversation in display order, sections flattened.
    pub(super) fn conversation_model(&self) -> gtk::FlattenListModel {
        self.imp()
            .conversation_model
            .get()
            .expect("built at construction")
            .clone()
    }

    pub(super) fn selection(&self) -> gtk::MultiSelection {
        self.imp()
            .selection
            .get()
            .expect("built at construction")
            .clone()
    }

    pub(super) fn toast(&self, text: &str) {
        self.imp().toast_overlay.add_toast(adw::Toast::new(text));
    }

    /// The account owning a folder, by folder id. Every per-account action
    /// resolves its account this way, so the unified inbox -- where one
    /// selection can span accounts -- needs no special case.
    pub(super) fn account_for_folder(&self, folder_id: i64) -> Option<(Account, Folder)> {
        let folder = self.db().borrow().folder(folder_id).ok().flatten()?;
        let account = self.state().accounts.get(&folder.account_id).cloned()?;
        Some((account, folder))
    }

    /// The folders the list is showing: one, or every account's inbox.
    pub(super) fn current_folders(&self) -> Vec<Folder> {
        let view = self.state().view.clone();
        match view {
            None => Vec::new(),
            Some(View::Folder(folder)) => vec![folder],
            Some(View::UnifiedInbox) => self.inbox_folders(),
        }
    }

    pub(super) fn current_folder_ids(&self) -> Vec<i64> {
        self.current_folders()
            .into_iter()
            .map(|folder| folder.id)
            .collect()
    }

    /// The single open folder, or None in the unified inbox.
    pub(super) fn current_folder(&self) -> Option<Folder> {
        let view = self.state().view.clone();
        match view {
            Some(View::Folder(folder)) => Some(folder),
            _ => None,
        }
    }

    pub(super) fn is_unified_view(&self) -> bool {
        matches!(self.state().view, Some(View::UnifiedInbox))
    }

    /// Each account's inbox, as far as the database knows them.
    pub(super) fn inbox_folders(&self) -> Vec<Folder> {
        let account_ids: Vec<i64> = {
            let state = self.state();
            let mut ids: Vec<i64> = state.accounts.keys().copied().collect();
            ids.sort();
            ids
        };
        let db = self.db();
        let db = db.borrow();
        account_ids
            .into_iter()
            .filter_map(|account_id| {
                let folders = db.folders_for_account(account_id).ok()?;
                let name = rustle_core::folders::mailbox_with_role(
                    folders.iter().map(|f| f.name.as_str()),
                    rustle_core::folders::FolderRole::Inbox,
                )?;
                folders.iter().find(|f| f.name == name).cloned()
            })
            .collect()
    }

    // --- lifecycle --------------------------------------------------------

    fn connect_widgets(&self) {
        let imp = self.imp();
        imp.add_account_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_add_account_clicked()
        ));
        imp.online_accounts_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_online_accounts_clicked()
        ));
        imp.refresh_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_refresh_clicked()
        ));
        imp.compose_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_compose_clicked()
        ));
        imp.reply_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.open_reply(false)
        ));
        imp.reply_all_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.open_reply(true)
        ));
        imp.forward_button.connect_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.open_forward()
        ));
        imp.connection_banner.connect_button_clicked(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_banner_retry()
        ));

        imp.search_bar.set_key_capture_widget(Some(self));
        imp.search_entry.connect_search_changed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.on_search_changed()
        ));
        imp.search_bar
            .connect_search_mode_enabled_notify(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |bar| {
                    // Closing the search bar clears the query so the full list comes back.
                    if !bar.is_search_mode() {
                        window.imp().search_entry.set_text("");
                    }
                }
            ));
        imp.unread_button.connect_toggled(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.refresh_conversations(None)
        ));
        // Load older mail when the list is scrolled to the bottom.
        imp.conversation_scroller.connect_edge_reached(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, position| window.on_list_edge_reached(position)
        ));
        // The sticky day label tracks whichever row is at the top edge.
        imp.conversation_scroller
            .vadjustment()
            .connect_value_changed(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |_| window.update_sticky_day()
            ));
        // Presenting the window again after close released the reading pane
        // re-renders whatever is still selected.
        self.connect_map(|window| window.update_reader());
    }

    /// Show the mail view over every account at once. Runs once per window,
    /// or again when the first account is added to an empty database.
    pub(super) fn load_mail_view(&self) {
        let imp = self.imp();
        imp.reader_stack.set_visible_child_name(PAGE_EMPTY);
        imp.main_stack.set_visible_child_name(PAGE_MAIL);
        // reload_folders selects the inbox, which fetches it via the
        // selection handler; the sync below covers the other accounts, and
        // bootstraps a fresh one whose folders aren't in the database yet.
        self.reload_folders();
        self.reschedule_sync();
        if self.state().is_online {
            self.sync_all(true);
        }
        self.apply_debug_hooks();
    }

    /// Development aids, read from the environment so a checkout can be
    /// exercised without clicking: `RUSTLE_DEBUG_OPEN=<folder id>:<uid>`
    /// opens that message, `RUSTLE_DEBUG_COMPOSE=1` opens the composer.
    fn apply_debug_hooks(&self) {
        if let Ok(target) = std::env::var("RUSTLE_DEBUG_OPEN") {
            if let Some((folder_id, uid)) = target.split_once(':') {
                if let Ok(folder_id) = folder_id.parse::<i64>() {
                    let uid = uid.to_string();
                    glib::idle_add_local_once(glib::clone!(
                        #[weak(rename_to = window)]
                        self,
                        move || window.open_email(folder_id, &uid)
                    ));
                }
            }
        }
        if std::env::var_os("RUSTLE_DEBUG_COMPOSE").is_some() {
            glib::idle_add_local_once(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move || window.on_compose_clicked()
            ));
        }
    }

    /// Let `handle` drag `split`'s sidebar, remembering the width under `key`.
    /// libadwaita has no resizable split view, so the width is pinned by
    /// setting the sidebar's minimum and maximum to the same value.
    fn setup_sidebar_resize(
        &self,
        handle: &gtk::Box,
        split: &adw::NavigationSplitView,
        key: &'static str,
        lower: i32,
        upper: i32,
    ) {
        let settings = self.settings();
        pin_sidebar_width(split, settings.int(key).clamp(lower, upper));
        handle.set_cursor(gdk::Cursor::from_name("col-resize", None).as_ref());
        // A collapsed sidebar fills the window, which would leave the handle
        // stranded over the middle of the content.
        split
            .bind_property("collapsed", handle, "visible")
            .flags(glib::BindingFlags::SYNC_CREATE | glib::BindingFlags::INVERT_BOOLEAN)
            .build();

        let gesture = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new((0.0f64, 0.0f64)));
        gesture.connect_drag_begin(glib::clone!(
            #[weak]
            split,
            #[strong]
            start,
            move |gesture, _, _| start.set((split.min_sidebar_width(), pointer_x(gesture)))
        ));
        gesture.connect_drag_update(glib::clone!(
            #[weak]
            split,
            #[strong]
            start,
            move |gesture, _, _| {
                let (start_width, start_x) = start.get();
                let width = start_width + pointer_x(gesture) - start_x;
                pin_sidebar_width(&split, (width as i32).clamp(lower, upper));
            }
        ));
        handle.add_controller(gesture);
    }

    fn on_close_request(&self) -> glib::Propagation {
        let imp = self.imp();
        let settings = self.settings();
        let (width, height) = self.default_size();
        let _ = settings.set_int(keys::WINDOW_WIDTH, width);
        let _ = settings.set_int(keys::WINDOW_HEIGHT, height);
        let _ = settings.set_boolean(keys::WINDOW_MAXIMIZED, self.is_maximized());
        let _ = settings.set_int(
            keys::FOLDER_WIDTH,
            imp.outer_split.min_sidebar_width() as i32,
        );
        let _ = settings.set_int(
            keys::CONVERSATION_WIDTH,
            imp.inner_split.min_sidebar_width() as i32,
        );
        // Nothing on screen to render, so give the web process back.
        message_view::release_anchor();

        if settings.boolean(keys::RUN_IN_BACKGROUND) {
            // connect_map renders the reading pane again when the window returns.
            {
                let mut state = self.state_mut();
                state.rendered_id = None;
                state.active_view = None;
            }
            self.clear_thread();
            imp.reader_stack.set_visible_child_name(PAGE_EMPTY);
            self.set_visible(false);
            self.notify_background();
            // Keep the app alive so the sync timer keeps running.
            return glib::Propagation::Stop;
        }

        imp.is_closing.set(true);
        if let Some(timer) = self.state_mut().sync_timer.take() {
            timer.remove();
        }
        if let Some(timeout) = self.state_mut().search_timeout.take() {
            timeout.remove();
        }
        glib::Propagation::Proceed
    }

    /// Open one message by IMAP UID (from a notification). Clearing
    /// `rendered_id` makes the reader rebuild even if the thread is already
    /// shown, so the usual open path marks it read.
    pub fn open_email(&self, folder_id: i64, uid: &str) {
        if self.state().view.is_none() {
            return;
        }
        self.select_folder_by_id(folder_id);
        let model = self.conversation_model();
        for index in 0..model.n_items() {
            let Some(conversation) = model.item(index).and_downcast::<ConversationObject>() else {
                continue;
            };
            let holds_uid = conversation.with(|c| {
                c.emails
                    .iter()
                    .any(|mail| mail.server_id.as_deref() == Some(uid))
            });
            if holds_uid {
                self.state_mut().rendered_id = None;
                let selection = self.selection();
                selection.unselect_all();
                selection.select_item(index, true);
                self.update_reader();
                return;
            }
        }
    }

    pub(super) fn sidebar_item_at(&self, position: u32) -> Option<SidebarItem> {
        let model = self.imp().folder_tree_model.get()?;
        let tree_row = model.item(position).and_downcast::<gtk::TreeListRow>()?;
        tree_row.item().and_downcast::<SidebarItem>()
    }
}

fn pin_sidebar_width(split: &adw::NavigationSplitView, width: i32) {
    split.set_max_sidebar_width(width as f64);
    split.set_min_sidebar_width(width as f64);
}

/// The pointer's x within the window, not within the handle: the handle
/// rides the trailing edge of the sidebar it resizes, so its own coordinates
/// move underneath the drag.
fn pointer_x(gesture: &gtk::GestureDrag) -> f64 {
    gesture
        .current_sequence()
        .and_then(|sequence| gesture.last_event(Some(&sequence)))
        .and_then(|event| event.position())
        .map(|(x, _)| x)
        .unwrap_or(0.0)
}
