use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_channel::Receiver;
use cosmic::app::{ContextDrawer, Core, Task, context_drawer};
use cosmic::iced::{
    Alignment, Border, Event, Length, Subscription,
    event::listen_with,
    keyboard::{self, Key, Modifiers, key::Named},
};
use cosmic::widget::{button, column, container, row, scrollable, text, text_input};
use cosmic::{Application, ApplicationExt, Element};
use serde::Deserialize;

use crate::{
    Args,
    api::{ApiConfig, ApiHandle, Command, UiEvent},
    credentials::{self, CloudflareAccessCredentials, PasswordTarget, SystemKeyring},
    icons,
    jobs::{self, JobKind},
    markdown,
    model::{self, Conversation, ModelCatalog, Role, RunStatus, Session, TrayItem},
    palette,
    persist::{PersistedState, default_path},
    preview, protocol,
    tray::{RowAction, SendMode, enter_mode},
};

/// A session-row drag: `to` follows the row under the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TabDrag {
    from: usize,
    to: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawerPage {
    /// GTK's new-session palette.
    NewSession,
    /// GTK's rename dialog (title + session ID).
    Rename,
    Jobs,
    Sessions,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerPicker {
    Model,
    Level,
}

pub struct OpenCodeCosmic {
    core: Core,
    args: Args,
    api: Option<ApiHandle>,
    receiver: Option<Receiver<UiEvent>>,
    mock_server: Option<preview::State>,
    state: PersistedState,
    sessions: HashMap<String, Session>,
    tabs: Vec<String>,
    active_session_id: Option<String>,
    conversations: HashMap<String, Conversation>,
    catalogs: HashMap<String, ModelCatalog>,
    statuses: HashMap<String, RunStatus>,
    jobs: jobs::Jobs,
    composer_text: String,
    /// GTK's composer was a multi-line text view, not a one-line entry.
    composer_editor: cosmic::widget::text_editor::Content,
    search_query: String,
    model_search: String,
    level_search: String,
    composer_picker: Option<ComposerPicker>,
    /// The row an open picker's arrow keys are on.
    picker_highlight: usize,
    active_drawer: Option<DrawerPage>,
    sidebar_open: bool,
    connection_status: String,
    /// The status carries GTK's `.error` class: the text turns
    /// `@oc_fg_connection_status_error`.
    connection_status_error: bool,
    error_banner: Option<String>,
    server_url_input: String,
    username_input: String,
    password_input: String,
    current_password: Option<String>,
    password_stored: bool,
    remember_password: bool,
    cloudflare_client_id_input: String,
    cloudflare_client_secret_input: String,
    cloudflare_access: Option<CloudflareAccessCredentials>,
    settings_page: SettingsPage,
    settings_validation: String,
    next_req_id: u64,
    /// Set when the active session changes: the next tick focuses the composer.
    focus_composer: bool,
    /// Focus a newly opened palette's entry on the next tick, after it exists.
    focus_modal: Option<DrawerPage>,
    /// UI zoom, mirroring the GTK client's `zoom_level` (0.7 … 1.75).
    zoom: f32,
    /// Locations the server knows (`project.list`), for GTK's new-session
    /// palette.
    projects: Vec<model::Project>,
    /// The rename palette's title entry.
    rename_input: String,
    /// A tray request (switch/cancel/resume) awaits its answer, so Resume
    /// stays disabled the way GTK's did.
    tray_in_flight: bool,
    /// Open permission requests (bootstrap, reconciliation and live events).
    permissions: Vec<crate::pending::PendingRequest>,
    /// Open forms, with `pending`'s visibility and notice rules.
    forms: crate::pending::Forms,
    /// How far the transcript is scrolled from its start (loading older
    /// history) and from its end (following the run, GTK's sticky prompt).
    /// Scroll position: pixels scrolled past the transcript's start.
    transcript_offset: f32,
    /// Pixels still left below the viewport (0 at the very end).
    transcript_remaining: f32,
    /// The transcript is following the end of the run (the user has not
    /// scrolled up).
    transcript_follow: bool,
    /// A session switch asks for the transcript to be shown from the top.
    transcript_scroll_top: bool,
    /// The modifiers of the latest key event: the composer's editor reports an
    /// Enter action without them.
    modifiers: cosmic::iced::keyboard::Modifiers,
    /// An older-history request is in flight.
    history_loading: bool,
    /// The session row a drag started on, and the row it would land on:
    /// GTK's drag-to-reorder.
    tab_drag: Option<TabDrag>,
    /// The session row under the cursor, for GTK's row hover (and its tab
    /// actions, which only show for the active or hovered row).
    hovered_tab: Option<String>,
    /// Sessions whose run finished while they were not active: GTK's unread
    /// marker (a blue dot and title until the session is opened).
    unread: std::collections::HashSet<String>,
    /// The session the rename palette edits (the active one unless a row's
    /// rename action set it).
    rename_target: Option<String>,
    /// Alt is held: GTK swapped every row's status marker for its shortcut
    /// number.
    shortcut_hint: bool,
    /// Files picked with the paperclip for the next prompt.
    pending_attachments: Vec<PathBuf>,
    /// Set while the file dialog runs on its own thread.
    attachment_picker: Option<Receiver<Vec<PathBuf>>>,
}

#[derive(Clone, Debug)]
pub enum Message {
    Tick,
    ToggleSidebar,
    SelectTab(String),
    CloseTab(String),
    NewSession,
    CloseActiveTab,
    CycleTab(i32),
    SelectTabIndex(usize),
    /// A change in the multi-line composer.
    ComposerEdit(cosmic::widget::text_editor::Action),
    SendPrompt(SendMode),
    /// Enter in the composer; the run status decides send/steer/queue.
    ComposerEnter {
        ctrl: bool,
    },
    /// Puts the caret back in the composer (Ctrl+G).
    FocusComposer,
    /// Creates a session in a palette-chosen location.
    CreateSessionIn(String),
    /// Resumes a parked tray (GTK's `queue-tray-resume`).
    ResumeTray,
    /// Answers a permission prompt.
    ReplyPermission {
        request_id: String,
        session_id: String,
        decision: protocol::PermissionDecision,
    },
    /// Opens the server's web UI in the desktop's browser.
    OpenWebUi,
    /// Cancels the form the notice points at (GTK's `Ctrl+Shift+X`).
    CancelVisibleForm,
    /// Alt was pressed or released: GTK's tab shortcut hint.
    AltHint(bool),
    ModifiersChanged(cosmic::iced::keyboard::Modifiers),
    /// GTK's "Load earlier messages" button.
    LoadOlderHistory,
    /// The cursor entered or left a session row.
    TabHover {
        tab: String,
        hovered: bool,
    },
    /// The left button was released: the dragged row lands, or the row under
    /// the cursor is selected.
    PointerRelease,
    /// A session row started being dragged (GTK reorders tabs by drag).
    TabDragStart(usize),
    /// The cursor is over this session row while dragging.
    TabDragOver(usize),
    /// The transcript scrolled: GTK pinned the current request at its top and
    /// loaded older history when it reached the beginning.
    TranscriptScrolled(cosmic::iced::widget::scrollable::Viewport),
    /// Opens the rename palette for the active session.
    OpenRename,
    /// A row's rename action, for that row's session.
    OpenRenameFor(String),
    RenameInput(String),
    ApplyRename,
    /// Opens the file dialog for the composer's attachments.
    PickAttachments,
    RemoveAttachment(usize),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    StopSession,
    TrayAction(String, RowAction),
    TrayClear,
    CopyText(String),
    ToggleDrawer(DrawerPage),
    CloseDrawer,
    SearchInput(String),
    SelectSession(String),
    SelectModel(String),
    SelectVariant(String),
    ToggleComposerPicker(ComposerPicker),
    CloseComposerPicker,
    PickerMove(i32),
    PickerAccept,
    ModelSearchInput(String),
    LevelSearchInput(String),
    SettingsUrlInput(String),
    SettingsUsernameInput(String),
    SettingsPasswordInput(String),
    SettingsRememberPassword(bool),
    SettingsCloudflareClientIdInput(String),
    SettingsCloudflareClientSecretInput(String),
    SettingsPage(SettingsPage),
    ApplySettings,
    DismissError,
    /// The headerbar was dragged (GTK's headerbar moved the window).
    HeaderDrag,
    /// The headerbar was double-clicked: toggle maximize like GTK's titlebar.
    HeaderMaximize,
    /// The headerbar's minimize button.
    HeaderMinimize,
    /// The headerbar's close button.
    HeaderClose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPage {
    Connection,
    Sessions,
}

impl Application for OpenCodeCosmic {
    type Executor = cosmic::iced::executor::Default;
    type Flags = Args;
    type Message = Message;
    const APP_ID: &'static str = "ai.opencode.Cosmic";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Self::Message>) {
        // GTK's own `GtkHeaderBar` carried the toggle, the title and the window
        // buttons in the app's colours; libcosmic's headerbar is themed by the
        // COSMIC theme, so the port draws its own (see `header_bar`).
        let mut core = core;
        core.window.show_headerbar = false;
        // libcosmic insets the content by `border_padding` (7px) on each side;
        // GTK's content ran to the window edges.
        core.window.border_padding = Some(0);
        let (state, _) = PersistedState::load(&default_path()).unwrap_or_default();
        let zoom = if (0.5..=3.0).contains(&state.zoom_level) {
            state.zoom_level as f32
        } else {
            1.0
        };
        let server_url = flags
            .server
            .clone()
            .unwrap_or_else(|| state.connection.server.clone());
        let username = flags
            .username
            .clone()
            .unwrap_or_else(|| state.connection.username.clone());
        let password_load = credentials::initial_password(
            &SystemKeyring,
            &server_url,
            &username,
            flags.password.clone(),
            state.connection.basic_auth_in_keyring,
            state.connection.basic_auth_in_keyring,
        );
        let (cloudflare_access, cloudflare_warning) = if let (Some(id), Some(secret)) = (
            flags.cf_access_client_id.as_ref(),
            flags.cf_access_client_secret.as_ref(),
        ) {
            (
                CloudflareAccessCredentials::new(id.clone(), secret.clone()).ok(),
                None,
            )
        } else if state.connection.cloudflare_access {
            match credentials::load(&server_url) {
                Ok(access) => (access, None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let cloudflare_client_id_input = cloudflare_access
            .as_ref()
            .map(|access| access.client_id.clone())
            .unwrap_or_default();

        let mut app = Self {
            core,
            args: flags.clone(),
            api: None,
            receiver: None,
            mock_server: None,
            state,
            sessions: HashMap::new(),
            tabs: Vec::new(),
            active_session_id: None,
            conversations: HashMap::new(),
            catalogs: HashMap::new(),
            statuses: HashMap::new(),
            jobs: jobs::Jobs::default(),
            composer_text: String::new(),
            composer_editor: cosmic::widget::text_editor::Content::new(),
            search_query: String::new(),
            model_search: String::new(),
            level_search: String::new(),
            composer_picker: None,
            picker_highlight: 0,
            active_drawer: None,
            sidebar_open: true,
            connection_status: "Connecting".to_string(),
            connection_status_error: false,
            error_banner: password_load.warning.or(cloudflare_warning),
            server_url_input: server_url,
            username_input: username,
            password_input: String::new(),
            current_password: password_load.password,
            password_stored: password_load.stored,
            remember_password: true,
            cloudflare_client_id_input,
            cloudflare_client_secret_input: String::new(),
            cloudflare_access,
            settings_page: SettingsPage::Connection,
            settings_validation: String::new(),
            next_req_id: 1,
            focus_composer: false,
            focus_modal: None,
            zoom,
            projects: Vec::new(),
            rename_input: String::new(),
            tray_in_flight: false,
            permissions: Vec::new(),
            forms: crate::pending::Forms::default(),
            transcript_offset: 0.0,
            transcript_remaining: 0.0,
            transcript_follow: true,
            transcript_scroll_top: false,
            modifiers: cosmic::iced::keyboard::Modifiers::default(),
            history_loading: false,
            tab_drag: None,
            hovered_tab: None,
            unread: std::collections::HashSet::new(),
            rename_target: None,
            shortcut_hint: false,
            pending_attachments: Vec::new(),
            attachment_picker: None,
        };

        if flags.preview {
            let mock = preview::State::new();
            app.connection_status_error = false;
            // Hand the mock to the app first: handling an event can send a
            // follow-up command (the jobs list asks for child sessions), and
            // in preview mode that has to reach the mock.
            app.mock_server = Some(mock);

            let b_event =
                app.mock_server
                    .as_mut()
                    .expect("preview mock")
                    .handle(Command::Bootstrap {
                        sessions: Vec::new(),
                        directories: Vec::new(),
                    });
            app.handle_ui_event(b_event);

            let s_state = preview::server_state();
            app.tabs.clear();
            for tab in &s_state.tabs {
                app.tabs.push(tab.id.clone());
            }
            app.active_session_id = s_state.active;
            // GTK's preview handed this state to the app as its persisted
            // state, so the unread set reached the session rows.
            app.unread = s_state.unread.clone();
            app.focus_composer = app.active_session_id.is_some();

            for tab in &s_state.tabs {
                let m_event =
                    app.mock_server
                        .as_mut()
                        .expect("preview mock")
                        .handle(Command::LoadMessages {
                            session_id: tab.id.clone(),
                            cursor: None,
                        });
                app.handle_ui_event(m_event);
            }

            let mod_event =
                app.mock_server
                    .as_mut()
                    .expect("preview mock")
                    .handle(Command::LoadModels {
                        directory: "/repo".to_string(),
                    });
            app.handle_ui_event(mod_event);

            for event in app
                .mock_server
                .as_mut()
                .expect("preview mock")
                .take_server_events()
            {
                app.handle_ui_event(event);
            }
            // GTK's preview seeds no projects: its new-session picker lists
            // `project_paths(state.projects, state.sessions)`, which for the
            // fixture is the sessions' own `/repo` alone. Seeding extra
            // locations here made the picker show rows GTK's never has.

            // Preview mode is the screenshot/demo surface: show the paperclip
            // chips without a real dialog. Off by default so the preview's
            // fixture matches the GTK client's (no pending attachments);
            // `OPENCODE_GTK_PREVIEW_ATTACHMENTS=1` turns the demo on.
            if std::env::var("OPENCODE_GTK_PREVIEW_ATTACHMENTS").is_ok() {
                app.pending_attachments = vec![
                    PathBuf::from("/state/home/paperclip-22px.png"),
                    PathBuf::from("/state/home/composer-actions-34x32.png"),
                ];
            }
        } else {
            app.connect_api();
        }

        if let Some(drawer_name) = &flags.drawer {
            match drawer_name.to_lowercase().as_str() {
                "jobs" => app.active_drawer = Some(DrawerPage::Jobs),
                "sessions" => app.active_drawer = Some(DrawerPage::Sessions),
                "settings" => app.active_drawer = Some(DrawerPage::Settings),
                _ => {}
            }
        }

        // Without this the compositor shows an empty window title.
        let title = if flags.preview {
            "OpenCode Preview".to_string()
        } else {
            "OpenCode".to_string()
        };
        let title_task = match app.core().main_window_id() {
            Some(id) => app.set_window_title(title, id),
            None => Task::none(),
        };

        (app, title_task)
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::Tick => {
                // Focus the composer one tick after the active session changed,
                // so the widget exists by the time the focus operation runs.
                let focus = std::mem::take(&mut self.focus_composer);
                let focus_modal = self.focus_modal.take();
                let picked = self.take_picked_attachments();
                if let Some(paths) = picked {
                    match crate::api::check_attachments(&paths) {
                        Ok(()) => {
                            for path in paths {
                                if !self.pending_attachments.contains(&path) {
                                    self.pending_attachments.push(path);
                                }
                            }
                        }
                        Err(error) => self.error_banner = Some(error.to_string()),
                    }
                }
                self.drain_events();
                let mut tasks = Vec::new();
                if let Some(page) = focus_modal.filter(|page| Some(*page) == self.active_drawer) {
                    let id = match page {
                        DrawerPage::NewSession => Some(new_session_search_id()),
                        DrawerPage::Rename => Some(rename_title_id()),
                        _ => None,
                    };
                    if let Some(id) = id {
                        tasks.push(focus_widget(id));
                    }
                } else if focus && self.active_drawer.is_none() {
                    tasks.push(focus_composer());
                }
                // A session switch lands at the top of its transcript.
                if std::mem::take(&mut self.transcript_scroll_top) {
                    tasks.push(cosmic::iced::widget::scrollable::scroll_to(
                        transcript_id(),
                        cosmic::iced::widget::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(0.0),
                        },
                    ));
                }
                // GTK's transcript followed the run; the scroll itself reports
                // back through `TranscriptScrolled`.
                if self.transcript_follow {
                    tasks.push(cosmic::iced::widget::scrollable::scroll_to(
                        transcript_id(),
                        cosmic::iced::widget::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(f32::MAX),
                        },
                    ));
                }
                Task::batch(tasks)
            }
            Message::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
                Task::none()
            }
            Message::SelectTab(id) => {
                self.set_active_session(&id);
                Task::none()
            }
            Message::CloseTab(id) => {
                self.close_tab(&id);
                Task::none()
            }
            Message::NewSession => {
                self.active_drawer = Some(DrawerPage::NewSession);
                self.focus_modal = Some(DrawerPage::NewSession);
                Task::none()
            }
            Message::CreateSessionIn(directory) => {
                self.active_drawer = None;
                self.create_session(&directory);
                Task::none()
            }
            Message::ResumeTray => {
                self.resume_tray();
                Task::none()
            }
            Message::ReplyPermission {
                request_id,
                session_id,
                decision,
            } => {
                if let Some(api) = &self.api {
                    api.send(Command::ReplyPermission {
                        request_id,
                        session_id,
                        decision,
                    });
                }
                Task::none()
            }
            Message::TabHover { tab, hovered } => {
                if hovered {
                    self.hovered_tab = Some(tab);
                } else if self.hovered_tab.as_deref() == Some(tab.as_str()) {
                    self.hovered_tab = None;
                }
                Task::none()
            }
            Message::TabDragStart(position) => {
                self.tab_drag = Some(TabDrag {
                    from: position,
                    to: position,
                });
                Task::none()
            }
            Message::TabDragOver(position) => {
                if let Some(drag) = &mut self.tab_drag {
                    drag.to = position;
                }
                Task::none()
            }
            Message::PointerRelease => {
                if let Some(drag) = self.tab_drag.take() {
                    reorder_tabs(&mut self.tabs, drag.from, drag.to);
                    self.persist_tabs();
                    return Task::none();
                }
                if let Some(tab) = self.hovered_tab.clone()
                    && self.tabs.contains(&tab)
                {
                    self.set_active_session(&tab);
                }
                Task::none()
            }
            Message::TranscriptScrolled(viewport) => {
                // The transcript is anchored to its end, so the absolute offset
                // is the distance from the end and the reversed one from the
                // start (where older history is).
                self.transcript_offset = viewport.absolute_offset().y;
                self.transcript_remaining = viewport.absolute_offset_reversed().y;
                // Follow the run until the user scrolls up; scrolling back to
                // the end resumes it, the way GTK's transcript behaved.
                // GTK followed the run while the viewport sat at the bottom.
                self.transcript_follow = self.transcript_remaining < 24.0;
                self.load_older_history();
                Task::none()
            }
            Message::OpenWebUi => {
                self.open_web_ui();
                Task::none()
            }
            Message::CancelVisibleForm => {
                if let Some(target) = self.form_notice().and_then(|notice| notice.cancel)
                    && let Some(api) = &self.api
                {
                    api.send(Command::CancelForm {
                        form_id: target.form_id,
                        session_id: target.session_id,
                        directory: target.directory,
                    });
                }
                Task::none()
            }
            Message::OpenRename => {
                self.rename_target = self.active_session_id.clone();
                self.rename_input = self.active_session_title();
                self.active_drawer = Some(DrawerPage::Rename);
                self.focus_modal = Some(DrawerPage::Rename);
                Task::none()
            }
            Message::OpenRenameFor(id) => {
                self.rename_target = Some(id.clone());
                self.rename_input = self
                    .sessions
                    .get(&id)
                    .map(|session| session.title.clone())
                    .unwrap_or_default();
                self.active_drawer = Some(DrawerPage::Rename);
                self.focus_modal = Some(DrawerPage::Rename);
                Task::none()
            }
            Message::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                self.shortcut_hint = modifiers.alt();
                Task::none()
            }
            Message::AltHint(alt) => {
                self.shortcut_hint = alt;
                Task::none()
            }
            Message::LoadOlderHistory => {
                self.load_older_history();
                Task::none()
            }
            Message::RenameInput(value) => {
                self.rename_input = value;
                Task::none()
            }
            Message::ApplyRename => {
                self.apply_rename();
                Task::none()
            }
            Message::CloseActiveTab => {
                if let Some(id) = self.active_session_id.clone() {
                    self.close_tab(&id);
                }
                Task::none()
            }
            Message::CycleTab(delta) => {
                self.cycle_tab(delta);
                Task::none()
            }
            Message::SelectTabIndex(index) => {
                if let Some(id) = self.tabs.get(index).cloned() {
                    self.set_active_session(&id);
                }
                Task::none()
            }
            Message::ComposerEdit(action) => {
                // Enter (and Ctrl+Enter) are captured by the key subscription;
                // this is the fallback when the editor sees them first, and
                // the path for every other edit, Shift+Enter's newline
                // included.
                if let cosmic::widget::text_editor::Action::Edit(
                    cosmic::widget::text_editor::Edit::Enter,
                ) = &action
                {
                    let busy = self
                        .active_session_id
                        .as_deref()
                        .is_some_and(|id| self.is_session_busy(id));
                    self.send_composer_prompt(enter_mode(busy, self.modifiers.control()));
                    return Task::none();
                }
                self.composer_editor.perform(action);
                self.composer_text = self.composer_editor.text();
                Task::none()
            }
            Message::SendPrompt(mode) => {
                self.send_composer_prompt(mode);
                // A prompt the reader just sent stays in view, as GTK's does.
                self.transcript_follow = true;
                Task::none()
            }
            Message::ComposerEnter { ctrl } => {
                // With a picker open, Enter takes its highlighted row rather
                // than sending the prompt.
                if let Some(picker) = self.composer_picker {
                    if let Some((_, _, _, message)) = self
                        .picker_candidates(picker)
                        .into_iter()
                        .nth(self.picker_highlight)
                    {
                        self.composer_picker = None;
                        return self.update(message);
                    }
                    return Task::none();
                }
                if self.active_drawer == Some(DrawerPage::NewSession) {
                    self.confirm_new_session();
                    return Task::none();
                }
                // Enter steers while a run is active, Ctrl+Enter queues a new turn.
                let busy = self
                    .active_session_id
                    .as_deref()
                    .is_some_and(|id| self.is_session_busy(id));
                self.send_composer_prompt(enter_mode(busy, ctrl));
                Task::none()
            }
            Message::FocusComposer => focus_composer(),
            Message::PickAttachments => {
                self.pick_attachments();
                Task::none()
            }
            Message::RemoveAttachment(index) => {
                if index < self.pending_attachments.len() {
                    self.pending_attachments.remove(index);
                }
                Task::none()
            }
            Message::ZoomIn => {
                self.zoom_step(1);
                Task::none()
            }
            Message::ZoomOut => {
                self.zoom_step(-1);
                Task::none()
            }
            Message::ZoomReset => {
                self.set_zoom(1.0);
                Task::none()
            }
            Message::StopSession => {
                self.stop_active_session();
                Task::none()
            }
            Message::TrayAction(id, action) => {
                self.handle_tray_action(&id, action);
                Task::none()
            }
            Message::TrayClear => {
                self.clear_tray();
                Task::none()
            }
            Message::CopyText(txt) => cosmic::iced::clipboard::write(txt),
            Message::ToggleDrawer(page) => {
                if self.active_drawer == Some(page) {
                    self.active_drawer = None;
                } else {
                    self.active_drawer = Some(page);
                    if page == DrawerPage::Settings {
                        self.settings_page = SettingsPage::Connection;
                        self.settings_validation.clear();
                        self.password_input.clear();
                        self.cloudflare_client_secret_input.clear();
                        self.remember_password = true;
                    }
                }
                Task::none()
            }
            Message::CloseDrawer => {
                self.active_drawer = None;
                self.composer_picker = None;
                Task::none()
            }
            Message::SearchInput(q) => {
                self.search_query = q;
                Task::none()
            }
            Message::SelectSession(id) => {
                self.open_session(&id);
                self.active_drawer = None;
                Task::none()
            }
            Message::SelectModel(model_id) => {
                self.switch_model(&model_id);
                self.composer_picker = None;
                focus_composer()
            }
            Message::SelectVariant(variant) => {
                self.switch_variant(&variant);
                self.composer_picker = None;
                focus_composer()
            }
            Message::ToggleComposerPicker(picker) => {
                if self.composer_picker == Some(picker) {
                    self.composer_picker = None;
                    return Task::none();
                }
                self.composer_picker = Some(picker);
                self.picker_highlight = 0;
                match picker {
                    ComposerPicker::Model => {
                        self.model_search.clear();
                        focus_widget(model_search_id())
                    }
                    ComposerPicker::Level => {
                        self.level_search.clear();
                        focus_widget(level_search_id())
                    }
                }
            }
            Message::CloseComposerPicker => {
                self.composer_picker = None;
                Task::none()
            }
            Message::PickerMove(delta) => {
                if let Some(picker) = self.composer_picker {
                    let rows = self.picker_candidates(picker).len().max(1) as i32;
                    self.picker_highlight =
                        (self.picker_highlight as i32 + delta).rem_euclid(rows) as usize;
                }
                Task::none()
            }
            Message::PickerAccept => {
                if let Some(picker) = self.composer_picker
                    && let Some((_, _, _, message)) = self
                        .picker_candidates(picker)
                        .into_iter()
                        .nth(self.picker_highlight)
                {
                    self.composer_picker = None;
                    return self.update(message);
                }
                Task::none()
            }
            Message::ModelSearchInput(query) => {
                self.model_search = query;
                Task::none()
            }
            Message::LevelSearchInput(query) => {
                self.level_search = query;
                Task::none()
            }
            Message::SettingsUrlInput(url) => {
                self.server_url_input = url;
                Task::none()
            }
            Message::SettingsUsernameInput(user) => {
                self.username_input = user;
                Task::none()
            }
            Message::SettingsPasswordInput(pwd) => {
                self.password_input = pwd;
                Task::none()
            }
            Message::SettingsRememberPassword(remember) => {
                self.remember_password = remember;
                Task::none()
            }
            Message::SettingsCloudflareClientIdInput(id) => {
                self.cloudflare_client_id_input = id;
                Task::none()
            }
            Message::SettingsCloudflareClientSecretInput(secret) => {
                self.cloudflare_client_secret_input = secret;
                Task::none()
            }
            Message::SettingsPage(page) => {
                self.settings_page = page;
                Task::none()
            }
            Message::ApplySettings => {
                self.reconnect_with_settings();
                Task::none()
            }
            Message::DismissError => {
                self.error_banner = None;
                Task::none()
            }
            // GTK's headerbar: dragging it moved the window, a double click
            // toggled maximize, and the three window controls did their thing.
            Message::HeaderDrag => self.drag(),
            Message::HeaderMaximize => {
                let maximized = self.core.window.is_maximized;
                self.core().maximize(None, !maximized)
            }
            Message::HeaderMinimize => self.minimize(),
            Message::HeaderClose => match self.core().main_window_id() {
                Some(id) => cosmic::iced::window::close::<Message>(id).discard(),
                None => Task::none(),
            },
        }
    }

    fn view(&self) -> Element<'_, Self::Message> {
        // 1. Build Left Sidebar (GTK style)
        let mut sidebar_items = Vec::new();

        let new_session_btn = button::custom(
            row::with_children(vec![
                inline_icon(icons::add(), self.zoom).into(),
                text("New session")
                    .size(self.em(crate::metrics::px(13.0)))
                    .into(),
            ])
            .spacing(self.space(crate::metrics::px(6.0)))
            .align_y(Alignment::Center),
        )
        .on_press(Message::NewSession)
        .class(sidebar_row_class(self.zoom))
        .width(Length::Fill)
        .padding([self.pad_px(8.0), self.pad_px(12.0)]);
        sidebar_items.push(
            container(new_session_btn)
                .padding([
                    self.pad_px(8.0),
                    self.pad_px(8.0),
                    self.pad_px(4.0),
                    self.pad_px(8.0),
                ])
                .into(),
        );

        let jobs_sessions = self.jobs.sessions_with_jobs();
        let mut tab_rows = Vec::new();
        let mut first_row = true;
        let mut previous_active = true;
        for (position, tab_id) in self.tabs.iter().enumerate() {
            let title = self
                .sessions
                .get(tab_id)
                .map(|s| s.title.as_str())
                .unwrap_or(tab_id.as_str());

            let is_busy = self.is_session_busy(tab_id);
            let is_active = self.active_session_id.as_deref() == Some(tab_id.as_str());
            let is_unread = self.unread.contains(tab_id);
            // GTK showed the Settings gear while the turn or a job ran, and a
            // coloured dot otherwise; the colour carries the attention.
            let has_jobs = jobs_sessions.contains(tab_id);
            let attention = if is_busy {
                palette::current().status_busy
            } else if is_unread {
                palette::current().status_unread
            } else {
                palette::current().status_idle
            };

            let mut marker_items: Vec<Element<'_, Message>> = Vec::new();
            if is_busy || has_jobs {
                marker_items.push(
                    inline_icon(icons::settings(), self.zoom)
                        .size(self.em(1.04) as u16)
                        .class(cosmic::theme::Svg::custom(move |_theme: &cosmic::Theme| {
                            cosmic::iced::widget::svg::Style {
                                color: Some(attention),
                            }
                        }))
                        .into(),
                );
            } else {
                marker_items.push(status_dot(attention, true));
            }
            // GTK swapped the marker for the row's number while Alt was held.
            let status_marker: Element<'_, Message> = if self.shortcut_hint && position < 9 {
                text((position + 1).to_string())
                    .size(self.em(0.78))
                    .font(cosmic::iced::Font {
                        weight: cosmic::iced::font::Weight::Bold,
                        ..cosmic::iced::Font::DEFAULT
                    })
                    .class(cosmic::theme::Text::Color(
                        palette::current().tab_index_text,
                    ))
                    .into()
            } else {
                row::with_children(marker_items).into()
            };
            // GTK's `.session-tab-hint` carries `margin-right: 0.22em` and
            // `.session-tab-title` a `padding-left: 0.22em`; without the pair
            // the dot sits against the title (and the title fits an extra
            // character before its ellipsis).
            let status_marker: Element<'_, Message> = container(status_marker)
                .padding([0.0_f32, self.space(0.44), 0.0, 0.0])
                .into();

            // GTK: a finished run or unread output recolours the title (both
            // bold), the active row uses the active-title colour, and the rest
            // the sidebar's own foreground (`@oc_fg_sidebar_new_session`).
            let title_class = if is_busy {
                cosmic::theme::Text::Color(palette::current().status_busy)
            } else if is_unread {
                cosmic::theme::Text::Color(palette::current().tab_unread_text)
            } else if is_active {
                cosmic::theme::Text::Color(palette::current().tab_active_text)
            } else {
                cosmic::theme::Text::Color(palette::current().sidebar_label)
            };
            let title_weight = if is_busy || is_unread {
                cosmic::iced::font::Weight::Bold
            } else {
                cosmic::iced::font::Weight::Normal
            };

            let close_id = tab_id.clone();
            let row_hint = self
                .sessions
                .get(tab_id)
                .map(|session| format!("{}\nOpen session", session.directory))
                .unwrap_or_else(|| "Open session".to_string());
            let hovered = self.hovered_tab.as_deref() == Some(tab_id.as_str());
            let show_actions = is_active || hovered || self.tab_drag.is_some();

            let tab_btn = container(
                row::with_children(vec![
                    status_marker,
                    text(title)
                        .size(self.em(1.0))
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                            cosmic::iced::core::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .font(cosmic::iced::Font {
                            weight: title_weight,
                            ..cosmic::iced::Font::DEFAULT
                        })
                        .class(title_class)
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(self.space(0.22))
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([0, self.space(0.59) as u16]);

            // GTK kept rename and close on every row, dimmed until the row is
            // active or hovered.
            let close_btn = button::icon(icons::close())
                .on_press(Message::CloseTab(close_id))
                .padding([self.space(0.2) as u16, self.space(0.4) as u16])
                .class(close_button_class(show_actions, self.space(0.81)));
            let rename_btn = button::icon(icons::edit())
                .padding([self.space(0.2) as u16, self.space(0.2) as u16])
                .on_press(Message::OpenRenameFor(tab_id.clone()))
                .class(tab_action_class(show_actions, self.space(0.81)));

            let close_btn = hinted(close_btn, "Close tab");
            let rename_btn = hinted(rename_btn, "Rename session (F2)");

            let mut tab_row_items = vec![tab_btn.into()];
            tab_row_items.push(rename_btn);
            tab_row_items.push(close_btn);
            let tab_row = row::with_children(tab_row_items)
                .align_y(Alignment::Center)
                .spacing(self.space(0.15));

            let radius = self.space(0.5);
            let dragging = self.tab_drag.is_some_and(|drag| drag.from == position);
            let drag_target = self
                .tab_drag
                .is_some_and(|drag| drag.to == position && drag.from != position);
            let tab_card = container(tab_row)
                .width(Length::Fill)
                // GTK's `.session-tab { min-height: 2.6em }` resolves against the
                // sidebar's 0.96em font (33.3px), not the base 13.33px one.
                .height(Length::Fixed(self.space(2.6 * 0.96)))
                // A fixed-height container places its child at the top unless
                // told otherwise, which left the row's content riding high in
                // GTK's 2.6em box instead of centred in it.
                .align_y(Alignment::Center)
                .padding([0, self.space(0.3) as u16])
                .style(move |_theme: &cosmic::Theme| {
                    // GTK marked the dragged row and the drop position.
                    if drag_target {
                        return container::Style {
                            border: Border {
                                color: palette::current().accent_bg,
                                width: 1.0,
                                radius: radius.into(),
                            },
                            ..Default::default()
                        };
                    }
                    if dragging {
                        return container::Style {
                            background: Some(palette::current().sidebar_row_active_bg.into()),
                            border: Border {
                                color: palette::current().accent_bg,
                                width: 1.0,
                                radius: radius.into(),
                            },
                            ..Default::default()
                        };
                    }
                    if is_active || hovered {
                        // GTK rounded every session row (`border-radius: 0.5em`).
                        container::Style {
                            background: Some(
                                if is_active {
                                    palette::current().sidebar_row_active_bg
                                } else {
                                    palette::current().sidebar_hover_bg
                                }
                                .into(),
                            ),
                            border: Border {
                                radius: radius.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    } else {
                        container::Style::default()
                    }
                });

            // GTK's drag-to-reorder: each row reports the drag and the rows the
            // cursor crosses, so a drop lands where the cursor is.
            let hover_id = tab_id.clone();
            let leave_id = tab_id.clone();
            let tab_card: Element<'_, Message> = tab_card.into();
            let tab_card = cosmic::iced::widget::mouse_area(tab_card)
                .on_enter(Message::TabHover {
                    tab: hover_id,
                    hovered: true,
                })
                .on_exit(Message::TabHover {
                    tab: leave_id,
                    hovered: false,
                })
                .on_drag(Message::TabDragStart(position))
                .on_move(move |_point| Message::TabDragOver(position));
            let tab_card = hinted(tab_card, row_hint);

            // GTK separates inactive rows with a hairline.
            if !first_row && !is_active && !previous_active {
                tab_rows.push(hairline(palette::current().nav_separator));
            }
            tab_rows.push(tab_card);
            previous_active = is_active;
            first_row = false;
        }

        let tab_list_col =
            column::with_children(tab_rows).spacing(self.space(crate::metrics::px(2.0)));
        let tab_scroll = scrollable(tab_list_col)
            .direction(scrollbar_direction())
            .height(Length::Fill)
            .width(Length::Fill);
        sidebar_items.push(
            container(tab_scroll)
                .padding([self.pad_px(4.0), self.pad_px(4.0)])
                .height(Length::Fill)
                .into(),
        );

        let job_rows = self.jobs.rows(self.active_session_id.as_deref());
        if !job_rows.is_empty() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            // GTK's `.background-jobs`: a separator, `BACKGROUND` with a count
            // badge, then one plain row per job — a square kind badge, the
            // title and `<kind> · [owner ·] <elapsed>`.
            let mut jobs_col_items: Vec<Element<'_, Message>> = Vec::new();
            jobs_col_items.push(
                container(hairline(palette::current().nav_separator))
                    .padding([
                        self.space(0.3) as u16,
                        self.space(0.3) as u16,
                        self.space(0.44) as u16,
                        self.space(0.3) as u16,
                    ])
                    .into(),
            );

            let count_radius = self.space(999.0);
            let count = container(
                text(job_rows.len().to_string())
                    .size(self.em(0.76))
                    .class(cosmic::theme::Text::Color(palette::current().jobs_count_fg)),
            )
            .padding([0.0, self.space(0.52)])
            .style(move |_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().jobs_count_bg.into()),
                border: Border {
                    radius: count_radius.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
            jobs_col_items.push(
                container(
                    row::with_children(vec![
                        text("BACKGROUND")
                            .size(self.em(0.8))
                            .font(cosmic::iced::Font {
                                weight: cosmic::iced::font::Weight::Bold,
                                ..cosmic::iced::Font::DEFAULT
                            })
                            .class(cosmic::theme::Text::Color(palette::current().jobs_heading))
                            .into(),
                        container(count)
                            .padding([0.0, 0.0, 0.0, self.space(0.44)])
                            .into(),
                    ])
                    .align_y(Alignment::Center),
                )
                .padding([
                    self.space(0.44) as u16,
                    self.space(0.74) as u16,
                    self.space(0.3) as u16,
                    self.space(0.74) as u16,
                ])
                .into(),
            );

            for row in job_rows {
                let (glyph, icon_bg, icon_fg) = match row.kind {
                    JobKind::Subagent => (
                        "\u{25c6}",
                        palette::current().job_subagent_bg,
                        palette::current().job_subagent_fg,
                    ),
                    JobKind::Shell => (
                        "$",
                        palette::current().job_shell_bg,
                        palette::current().job_shell_fg,
                    ),
                };
                let icon_radius = self.space(0.37);
                let mut glyph_text = text(glyph)
                    .size(self.em(0.8))
                    .font(cosmic::iced::Font {
                        weight: cosmic::iced::font::Weight::Bold,
                        ..cosmic::iced::Font::DEFAULT
                    })
                    .class(cosmic::theme::Text::Color(icon_fg));
                if row.kind == JobKind::Shell {
                    glyph_text = glyph_text.font(cosmic::iced::Font::MONOSPACE);
                }
                let icon = container(glyph_text)
                    .width(Length::Fixed(self.space(1.33)))
                    .height(Length::Fixed(self.space(1.33)))
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center)
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(icon_bg.into()),
                        border: Border {
                            radius: icon_radius.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    });

                let (label, elapsed) = row.subtitle_parts(now);
                let meta_class = cosmic::theme::Text::Color(palette::current().job_meta);
                let mut meta_items: Vec<Element<'_, Message>> =
                    vec![text(label).size(self.em(0.85)).class(meta_class).into()];
                if let Some(elapsed) = elapsed {
                    meta_items.push(
                        text(format!(" · {elapsed}"))
                            .size(self.em(0.85))
                            .class(meta_class)
                            .into(),
                    );
                }

                let job_text = column::with_children(vec![
                    text(row.title.clone())
                        .size(self.em(0.96))
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                            cosmic::iced::core::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .class(cosmic::theme::Text::Color(palette::current().job_title))
                        .width(Length::Fill)
                        .into(),
                    row::with_children(meta_items).into(),
                ]);

                jobs_col_items.push(
                    container(row::with_children(vec![
                        container(icon)
                            .padding([0.0, self.space(0.67), 0.0, 0.0])
                            .into(),
                        job_text.width(Length::Fill).into(),
                    ]))
                    .padding([self.space(0.44) as u16, self.space(0.74) as u16])
                    .into(),
                );
            }

            sidebar_items.push(
                container(column::with_children(jobs_col_items).spacing(2))
                    .padding([0.0, self.space(0.59), self.space(0.3), self.space(0.59)])
                    .into(),
            );
        }

        let footer_buttons = column::with_children(vec![
            button::custom(hinted(
                row::with_children(vec![
                    inline_icon(icons::sessions(), self.zoom).into(),
                    text("Tabs").size(self.em(crate::metrics::px(13.0))).into(),
                ])
                .spacing(self.space(crate::metrics::px(6.0)))
                .align_y(Alignment::Center),
                "Search tabs (Ctrl+P)",
            ))
            .on_press(Message::ToggleDrawer(DrawerPage::Sessions))
            .class(sidebar_row_class(self.zoom))
            .width(Length::Fill)
            .padding([
                self.pad_px(11.0),
                self.pad_px(9.0),
                self.pad_px(11.0),
                self.pad_px(8.0),
            ])
            .into(),
            button::custom(hinted(
                row::with_children(vec![
                    inline_icon(icons::settings(), self.zoom).into(),
                    text("Settings")
                        .size(self.em(crate::metrics::px(13.0)))
                        .into(),
                ])
                .spacing(self.space(crate::metrics::px(6.0)))
                .align_y(Alignment::Center),
                "Server connection (Ctrl+,)",
            ))
            .on_press(Message::ToggleDrawer(DrawerPage::Settings))
            .class(sidebar_row_class(self.zoom))
            .width(Length::Fill)
            .padding([
                self.pad_px(11.0),
                self.pad_px(9.0),
                self.pad_px(11.0),
                self.pad_px(8.0),
            ])
            .into(),
        ])
        .spacing(self.space(crate::metrics::px(4.0)));

        let footer_container = container(footer_buttons).padding([
            self.pad_px(8.0),
            self.pad_px(8.0),
            self.pad_px(8.0),
            self.pad_px(8.0),
        ]);
        sidebar_items.push(footer_container.into());

        // GTK's paned position: 270px (plus the 1px right border).
        // GTK's paned position is 270: a 269px strip plus the tab strip's
        // own 1px border, with Adwaita's separator just outside it.
        let sidebar_column = column::with_children(sidebar_items)
            .width(Length::Fixed(269.0))
            .height(Length::Fill);

        // GTK's `.tab-strip`: the sidebar background with a 1px right border
        // only (the header's hairline closes the top).
        let sidebar = container(sidebar_column)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(palette::current().sidebar_bg.into()),
                ..Default::default()
            });
        // GTK's tab strip carries a 1px right border and Adwaita's paned
        // separator adds a second pixel beside it: x269 `@oc_border_tab_strip`,
        // x270 the separator colour.
        let sidebar_divider = row::with_children(vec![
            container(row::with_children(Vec::<Element<'_, Message>>::new()))
                .width(Length::Fixed(1.0))
                .height(Length::Fill)
                .style(|_theme| container::Style {
                    background: Some(palette::current().sidebar_border.into()),
                    ..Default::default()
                })
                .into(),
            container(row::with_children(Vec::<Element<'_, Message>>::new()))
                .width(Length::Fixed(1.0))
                .height(Length::Fill)
                .style(|_theme| container::Style {
                    background: Some(palette::current().window_separator.into()),
                    ..Default::default()
                })
                .into(),
        ])
        .spacing(0.0)
        .height(Length::Fill);

        // 2. Build Main Content Pane
        let mut main_items = Vec::new();

        // The banner and tray nodes stay in the tree even when they are empty:
        // a sibling appearing above the composer would otherwise be matched
        // against the composer's own widget state, dropping its focus while a
        // run starts.
        let banner_node = if let Some(err) = &self.error_banner {
            let banner = row::with_children(vec![
                text(format!("⚠ {err}"))
                    .size(self.em(crate::metrics::px(13.0)))
                    .width(Length::Fill)
                    .into(),
                button::text("Dismiss")
                    .on_press(Message::DismissError)
                    .into(),
            ])
            .padding(8)
            .spacing(self.space(crate::metrics::px(8.0)));

            container(banner).padding(4)
        } else {
            container(column::with_children(Vec::<Element<'_, Message>>::new()))
        };

        main_items.push(banner_node.into());

        if let Some(active_id) = &self.active_session_id {
            let active_title = self
                .sessions
                .get(active_id)
                .map(|s| s.title.as_str())
                .unwrap_or(active_id.as_str());
            // GTK's strip: the session's marker, its title, and the tab hint.
            let strip_busy = self.is_session_busy(active_id);
            let strip_jobs = self.jobs.sessions_with_jobs().contains(active_id);
            let strip_attention = if strip_busy {
                palette::current().status_busy
            } else if self.unread.contains(active_id) {
                palette::current().status_unread
            } else {
                palette::current().status_idle
            };
            let strip_marker: Element<'_, Message> = if strip_busy || strip_jobs {
                inline_icon(icons::settings(), self.zoom)
                    .size(self.em(1.04) as u16)
                    .class(cosmic::theme::Svg::custom(move |_theme: &cosmic::Theme| {
                        cosmic::iced::widget::svg::Style {
                            color: Some(strip_attention),
                        }
                    }))
                    .into()
            } else {
                status_dot(strip_attention, true)
            };

            let title_max = if self.active_drawer.is_some() { 20 } else { 38 };
            let display_title = if active_title.chars().count() > title_max {
                let s: String = active_title.chars().take(title_max - 1).collect();
                format!("{s}…")
            } else {
                active_title.to_string()
            };

            let session_header = container(
                row::with_children(vec![
                    strip_marker,
                    text(display_title)
                        .size(self.em(0.9))
                        .font(cosmic::iced::Font {
                            weight: cosmic::iced::font::Weight::Bold,
                            ..cosmic::iced::Font::DEFAULT
                        })
                        .width(Length::Fill)
                        .into(),
                    text("Ctrl+P to switch")
                        .size(self.em(0.8))
                        .class(cosmic::theme::Text::Color(palette::current().time_text))
                        .into(),
                ])
                .align_y(Alignment::Center)
                .padding([self.space(0.35) as u16, self.space(2.0) as u16]),
            )
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(palette::current().inset_bg.into()),
                border: Border {
                    color: palette::current().inset_border,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            });

            // GTK's strip is only visible with the sidebar folded (the
            // highlighted row carries the session otherwise).
            if !self.sidebar_open {
                main_items.push(session_header.into());
            }
            let conversation = self.conversations.get(active_id);
            let tray_items: Vec<TrayItem> =
                conversation.map(|c| c.tray_items()).unwrap_or_default();
            let is_busy = self.is_session_busy(active_id);

            let mut message_elements = Vec::new();

            if let Some(conv) = conversation {
                // GTK kept a flat "Load earlier messages" above the transcript
                // whenever the server had another page.
                if conv.next_cursor.is_some() {
                    message_elements.push(
                        container(
                            button::text("Load earlier messages")
                                .on_press(Message::LoadOlderHistory)
                                .class(flat_button_class(self.zoom)),
                        )
                        .width(Length::Fill)
                        .center_x(Length::Fill)
                        .padding([self.pad_px(6.0), 0])
                        .into(),
                    );
                }
                for message in &conv.messages {
                    if message.role == Role::User && tray_items.iter().any(|t| t.id == message.id) {
                        continue;
                    }
                    // GTK rendered one widget per transcript row: a message is
                    // split at every reasoning and tool segment, and each row
                    // carries its own header, timestamp and styling.
                    for row in message.rows() {
                        if row.body.trim().is_empty() && row.images.is_empty() {
                            // Keep a zero-height node: dropping a sibling above
                            // a stateful widget makes iced re-match the tree and
                            // drop the following widgets' state.
                            message_elements.push(
                                container(
                                    column::with_children(Vec::<Element<'_, Message>>::new()),
                                )
                                .height(Length::Fixed(0.0))
                                .into(),
                            );
                            continue;
                        }
                        let is_user = row.role == Role::User;
                        let reasoning = row.kind == model::TranscriptRowKind::Reasoning;
                        // GTK's `.message-reasoning { opacity: 0.42 }` dims the
                        // whole row, header included.
                        let dim = move |color: cosmic::iced::Color| {
                            if reasoning {
                                cosmic::iced::Color::from_rgba(color.r, color.g, color.b, 0.42)
                            } else {
                                color
                            }
                        };
                        let role_color = dim(if is_user {
                            palette::current().user_role_text
                        } else {
                            palette::current().muted_text
                        });
                        let header_row = row::with_children(vec![
                            text(if is_user { "YOU" } else { "AGENT" })
                                .size(self.em(0.76))
                                .line_height(line_height(1.35))
                                .font(cosmic::iced::Font {
                                    weight: cosmic::iced::font::Weight::Bold,
                                    ..cosmic::iced::Font::DEFAULT
                                })
                                .class(cosmic::theme::Text::Color(role_color))
                                .width(Length::Fill)
                                .into(),
                            text(clock_time(row.time))
                                .size(self.em(0.76))
                                .line_height(line_height(1.35))
                                .class(cosmic::theme::Text::Color(
                                    dim(palette::current().time_text),
                                ))
                                .into(),
                        ])
                        .align_y(Alignment::Center);

                        let mut body_items: Vec<Element<'_, Message>> = Vec::new();
                        match row.kind {
                            model::TranscriptRowKind::Error => {
                                // GTK `.message-error-card`.
                                let accent = container(row::with_children(Vec::<
                                    Element<'_, Message>,
                                >::new(
                                )))
                                .width(Length::Fixed(4.0))
                                .height(Length::Fill)
                                .style(
                                    |_theme: &cosmic::Theme| container::Style {
                                        background: Some(palette::current().error_text.into()),
                                        ..Default::default()
                                    },
                                );
                                let card_body = column::with_children(vec![
                                    row::with_children(vec![
                                        text("⚠")
                                            .size(self.em(0.88))
                                            .class(cosmic::theme::Text::Color(
                                                palette::current().error_text,
                                            ))
                                            .into(),
                                        text("Error")
                                            .size(self.em(0.88))
                                            .font(cosmic::iced::Font {
                                                weight: cosmic::iced::font::Weight::Bold,
                                                ..cosmic::iced::Font::DEFAULT
                                            })
                                            .class(cosmic::theme::Text::Color(
                                                palette::current().error_text,
                                            ))
                                            .into(),
                                    ])
                                    .spacing(self.space(0.44))
                                    .align_y(Alignment::Center)
                                    .into(),
                                    text(row.body.clone())
                                        .size(self.em(0.92))
                                        .line_height(line_height(1.4))
                                        .class(cosmic::theme::Text::Color(
                                            palette::current().error_body_text,
                                        ))
                                        .width(Length::Fill)
                                        .into(),
                                ])
                                .spacing(self.space(0.3))
                                .width(Length::Fill);
                                let card_radius = self.space(0.44);
                                // GTK's `.message-error-card` carries a 4px
                                // `border-left` plus `0.89em 1.04em` of padding,
                                // so the stripe hugs the card's edge and runs
                                // its full height while the body alone is
                                // padded. Padding the card instead pushed the
                                // body ~28px in and left the stripe short.
                                body_items.push(
                                    container(
                                        row::with_children(vec![
                                            accent.into(),
                                            container(card_body)
                                                .width(Length::Fill)
                                                .padding([
                                                    self.space(0.89) as u16,
                                                    self.space(1.04) as u16,
                                                    self.space(0.89) as u16,
                                                    self.space(1.04) as u16,
                                                ])
                                                .into(),
                                        ])
                                        .align_y(Alignment::Start),
                                    )
                                    .width(Length::Fill)
                                    .style(move |_theme: &cosmic::Theme| container::Style {
                                        background: Some(palette::current().error_card_bg.into()),
                                        border: Border {
                                            color: palette::current().error_card_border,
                                            width: 1.0,
                                            radius: card_radius.into(),
                                        },
                                        ..Default::default()
                                    })
                                    .into(),
                                );
                            }
                            _ if is_user => {
                                // GTK rendered a user body as one plain-text
                                // label, so the blank line in
                                // "…as 22px:\n\nAttached: …" is a blank line on
                                // screen. Splitting the body into blocks gave a
                                // 10px gap instead, which left every user row
                                // shorter than GTK's.
                                body_items.push(
                                    text(blank_lines(&row.body))
                                        .size(self.em(0.96))
                                        .line_height(line_height(1.45))
                                        .class(cosmic::theme::Text::Color(dim(
                                            palette::current().content_text
                                        )))
                                        .width(Length::Fill)
                                        .into(),
                                );
                            }
                            _ => {
                                body_items.push(markdown::render_markdown(
                                    &row.body,
                                    Message::CopyText,
                                    self.zoom,
                                ));
                            }
                        }

                        for image in &row.images {
                            if let Some(bytes) = inline_image_bytes(image) {
                                let radius = self.space(0.59);
                                let handle = cosmic::iced::widget::image::Handle::from_bytes(bytes);
                                body_items.push(
                                    container(
                                        cosmic::iced::widget::image(handle).border_radius(radius),
                                    )
                                    .style(move |_theme: &cosmic::Theme| container::Style {
                                        border: Border {
                                            color: palette::current().message_image_border,
                                            width: 1.0,
                                            radius: radius.into(),
                                        },
                                        ..Default::default()
                                    })
                                    .into(),
                                );
                            } else {
                                body_items.push(
                                    text(image.clone())
                                        .size(self.em(0.92))
                                        .class(cosmic::theme::Text::Color(dim(
                                            palette::current().content_text
                                        )))
                                        .into(),
                                );
                            }
                        }

                        // GTK's row: the header, 6px, then the body's blocks
                        // 10px apart (`.message-content` spacing).
                        let body = column::with_children(body_items)
                            .spacing(self.space(crate::metrics::px(10.0)));
                        let turn_col = column::with_children(vec![header_row.into(), body.into()])
                            .spacing(self.space(crate::metrics::px(6.0)));

                        // `.message-row { padding: 1.33em 2.07em 1.48em }`; a
                        // user row is a full-width band with a
                        // `@oc_border_user_message` hairline above and below,
                        // an assistant row has no bottom border at all.
                        let row_bg = if is_user {
                            palette::current().user_message_bg
                        } else {
                            palette::current().window_bg
                        };
                        let row = container(turn_col)
                            .padding([
                                self.space(1.33) as u16,
                                self.space(2.07) as u16,
                                self.space(1.48) as u16,
                                self.space(2.07) as u16,
                            ])
                            .width(Length::Fill)
                            .style(move |_theme: &cosmic::Theme| container::Style {
                                background: Some(row_bg.into()),
                                border: if is_user {
                                    Border {
                                        color: palette::current().user_message_border,
                                        width: 1.0,
                                        radius: 0.0.into(),
                                    }
                                } else {
                                    Border::default()
                                },
                                ..Default::default()
                            });

                        message_elements.push(row.into());
                        if is_user {
                            message_elements.push(hairline(palette::current().user_message_border));
                        }
                    }
                }
            }

            let message_list = column::with_children(message_elements)
                .spacing(self.space(crate::metrics::px(0.0)))
                // GTK's scrollbar occupies a 16px gutter; iced reserves
                // ~9px and overlays the rest, so the content carries the
                // remaining ~7px as a right inset. Without it the band ends
                // at 1161 (GTK 1154) and every full-width markdown block
                // overflows its row's content box by ~15px.
                .padding([0.0, self.space(0.53), 0.0, 0.0]);

            // Anchored to the end: the run stays in view and `snap_to` keeps
            // it there, while an empty transcript has nothing to anchor.
            // No `anchor_bottom`: iced's anchored offset swallows the
            // programmatic `snap_to` that follows a running transcript.
            let transcript_scroll = scrollable(message_list)
                .direction(scrollbar_direction())
                .id(transcript_id())
                .width(Length::Fill)
                .height(Length::Fill)
                .on_scroll(Message::TranscriptScrolled);

            // GTK's `.sticky-message`: a copy of the user row — role and
            // time, then the wrapped body — pinned over the transcript's top
            // with a `@oc_border_sticky_message` hairline and a 0/4/12 shadow.
            //
            // The node stays in the tree even when there is nothing to pin:
            // swapping the whole scrollable for a stack (and back) would make
            // iced re-match the tree and drop the scrollable's state.
            let sticky_node: Element<'_, Message> = match self.sticky_prompt() {
                None => container(column::with_children(Vec::<Element<'_, Message>>::new()))
                    .height(Length::Fixed(0.0))
                    .into(),
                Some((_id, prompt_text, created)) => {
                    // GTK's `.sticky-message` carries its padding inside the
                    // box, closes it with `border-bottom` alone and casts its
                    // shadow from the box's own edge - so the hairline is drawn
                    // after the padded, shadowed box rather than as a border on
                    // it, which would leave the shadow tinting the hairline.
                    let sticky = container(
                        column::with_children(vec![
                            container(
                                column::with_children(vec![
                                    row::with_children(vec![
                                        text("YOU")
                                            .size(self.em(0.76))
                                            .font(cosmic::iced::Font {
                                                weight: cosmic::iced::font::Weight::Bold,
                                                ..cosmic::iced::Font::DEFAULT
                                            })
                                            .class(cosmic::theme::Text::Color(
                                                palette::current().user_role_text,
                                            ))
                                            .width(Length::Fill)
                                            .into(),
                                        text(clock_time(created))
                                            .size(self.em(0.76))
                                            .class(cosmic::theme::Text::Color(
                                                palette::current().time_text,
                                            ))
                                            .into(),
                                    ])
                                    .align_y(Alignment::Center)
                                    .into(),
                                    text(blank_lines(&prompt_text))
                                        .size(self.em(0.96))
                                        .class(cosmic::theme::Text::Color(
                                            palette::current().content_text,
                                        ))
                                        .width(Length::Fill)
                                        .into(),
                                ])
                                .spacing(self.space(crate::metrics::px(6.0))),
                            )
                            .padding([
                                self.space(1.33) as u16,
                                self.space(2.07) as u16,
                                self.space(1.48) as u16,
                                self.space(2.07) as u16,
                            ])
                            .width(Length::Fill)
                            .into(),
                            // Inside the box, so the shadow starts below the
                            // hairline instead of tinting it.
                            hairline(palette::current().sticky_border),
                        ])
                        .width(Length::Fill),
                    )
                    .width(Length::Fill)
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(palette::current().window_bg.into()),
                        shadow: cosmic::iced::Shadow {
                            color: palette::current().sticky_shadow,
                            offset: cosmic::iced::Vector::new(0.0, 4.0),
                            blur_radius: 12.0,
                        },
                        ..Default::default()
                    });

                    container(sticky).align_top(Length::Fill).into()
                }
            };

            let transcript_area: Element<'_, Message> = cosmic::iced::widget::Stack::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .push(transcript_scroll)
                .push(sticky_node)
                .into();

            main_items.push(transcript_area);

            // GTK put the working/retry state in a compact pill *below* the
            // transcript (`.transcript-status-compact`), not inside it.
            if let Some(pill) = self.status_pill(active_id) {
                main_items.push(pill);
            }

            // Steer/Queue Tray. Pushed even when it has no rows, so the
            // composer keeps its widget state (and the caret) when a run
            // starts and the tray fills up.
            // GTK's `.form-notice`: one line, the label bold, Cancel (or the
            // hint while every waiting form belongs to another session).
            let notice = self.form_notice();
            let notice_outer: Element<'_, Message> = match notice {
                None => container(column::with_children(Vec::<Element<'_, Message>>::new()))
                    .padding([0, self.pad_px(18.0)])
                    .into(),
                Some(notice) => {
                    let radius = self.space(0.67);
                    let mut items: Vec<Element<'_, Message>> = vec![
                        // GTK's `.form-notice-label` sets no size, so it renders
                        // at the inherited 1em (13.33px), not a reduced one.
                        text(notice.text)
                            .size(self.em(1.0))
                            // `.form-notice-label` sets no line-height, so GTK
                            // renders it at the font's natural height.
                            .line_height(line_height(1.0))
                            .font(cosmic::iced::Font {
                                weight: cosmic::iced::font::Weight::Bold,
                                ..cosmic::iced::Font::DEFAULT
                            })
                            .class(cosmic::theme::Text::Color(
                                palette::current().prompt_subheading,
                            ))
                            .width(Length::Fill)
                            .into(),
                    ];
                    // GTK: a flat "Open web UI" beside a plain bordered
                    // "Cancel" (the shortcut lives in its tooltip, not in the
                    // row).
                    items.push(
                        button::custom(hinted(
                            text("Open web UI").size(self.em(0.96)),
                            "Answer it in the server's web UI",
                        ))
                        .class(flat_button_class(self.zoom))
                        .on_press(Message::OpenWebUi)
                        .padding([self.space(0.67) as u16, self.space(1.5) as u16])
                        .into(),
                    );
                    if notice.cancel.is_some() {
                        items.push(
                            button::custom(hinted(
                                text("Cancel").size(self.em(0.96)),
                                format!(
                                    "Cancel this form ({})",
                                    crate::pending::CANCEL_FORM_SHORTCUT
                                ),
                            ))
                            .class(plain_button_class(self.zoom))
                            .on_press(Message::CancelVisibleForm)
                            .padding([self.space(0.67) as u16, self.space(1.5) as u16])
                            .into(),
                        );
                    }
                    let padded = container(
                        row::with_children(items)
                            .spacing(self.space(0.59))
                            .align_y(Alignment::Center),
                    )
                    // GTK's `.form-notice`: `padding: 0.3em 0.3em 0.3em 0.89em`
                    // inside a box with start/end margins of 18px.
                    .padding([
                        self.space(0.3) as u16,
                        self.space(0.3) as u16,
                        self.space(0.3) as u16,
                        self.space(0.89) as u16,
                    ])
                    .width(Length::Fill)
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(palette::current().form_notice_bg.into()),
                        border: Border {
                            color: palette::current().form_notice_border,
                            width: 1.0,
                            radius: radius.into(),
                        },
                        ..Default::default()
                    });
                    // GTK's notice box carried `margin-start/end: 18` and
                    // `margin-bottom: 8`. The margin is the notice's own: with
                    // a tray below it the 8px sits between them (the composer's
                    // 8px top padding is gone).
                    container(padded)
                        .padding([
                            0.0_f32,
                            self.space(1.35),
                            self.space(0.59),
                            self.space(1.35),
                        ])
                        .into()
                }
            };

            let tray_outer = if tray_items.is_empty() {
                container(column::with_children(Vec::<Element<'_, Message>>::new())).padding([
                    self.pad_px(0.0),
                    self.pad_px(16.0),
                    self.pad_px(0.0),
                    self.pad_px(16.0),
                ])
            } else {
                // GTK: `.queue-tray-header` with a bold, padded title, then
                // hairline-separated rows.
                // GTK: "3 waiting" while the run is active, "Paused · 3 waiting"
                // with a dot and Resume once it is idle with items left.
                let paused = !is_busy;
                let mut header_items: Vec<Element<'_, Message>> = Vec::new();
                if paused {
                    let dot_radius = self.space(0.12);
                    header_items.push(
                        container(row::with_children(Vec::<Element<'_, Message>>::new()))
                            .width(Length::Fixed(self.space(0.55)))
                            .height(Length::Fixed(self.space(0.55)))
                            .style(move |_theme: &cosmic::Theme| container::Style {
                                background: Some(palette::current().tray_paused.into()),
                                border: Border {
                                    radius: dot_radius.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            })
                            .into(),
                    );
                }
                header_items.push(
                    text(crate::tray::header_text(tray_items.len(), paused))
                        .size(self.em(0.96))
                        .font(cosmic::iced::Font {
                            weight: cosmic::iced::font::Weight::Bold,
                            ..cosmic::iced::Font::DEFAULT
                        })
                        .class(cosmic::theme::Text::Color(
                            palette::current().tray_title_text,
                        ))
                        .width(Length::Fill)
                        .into(),
                );
                if paused {
                    let resume_radius = self.space(0.59);
                    header_items.push(
                        button::custom(
                            text("Resume")
                                .size(self.em(0.96))
                                .class(cosmic::theme::Text::Color(palette::current().accent_fg)),
                        )
                        .padding([self.space(0.3) as u16, self.space(0.85) as u16])
                        .class(resume_button_class(resume_radius))
                        .on_press_maybe((!self.tray_in_flight).then_some(Message::ResumeTray))
                        .into(),
                    );
                }

                let mut tray_rows = Vec::new();
                tray_rows.push(
                    container(
                        row::with_children(header_items)
                            .spacing(self.space(0.44))
                            .align_y(Alignment::Center),
                    )
                    .padding([self.space(0.1) as u16, self.space(0.6) as u16])
                    .into(),
                );

                // GTK groups the tray's rows by when they run
                // (`tray::tray_groups`) and heads each group with its label,
                // styled by `.queue-tray-group`.
                let grouped_rows = self.tray_rows();
                let mut ordered: Vec<(Option<String>, crate::tray::TrayRow)> = Vec::new();
                for group in crate::tray::tray_groups(&grouped_rows, is_busy) {
                    for (index, row) in group.rows.iter().enumerate() {
                        ordered.push((
                            if index == 0 {
                                Some(group.label.clone())
                            } else {
                                None
                            },
                            row.clone(),
                        ));
                    }
                }
                for (group_label, item) in ordered {
                    if let Some(label) = group_label {
                        tray_rows.push(hairline(palette::current().tray_row_divider));
                        tray_rows.push(
                            container(
                                text(label)
                                    .size(self.em(0.72))
                                    .line_height(line_height(1.0))
                                    .font(cosmic::iced::Font {
                                        weight: cosmic::iced::font::Weight::Bold,
                                        ..cosmic::iced::Font::DEFAULT
                                    })
                                    .class(cosmic::theme::Text::Color(
                                        palette::current().tray_group_fg,
                                    )),
                            )
                            // GTK's `.queue-tray-group` padding, and 0.09em
                            // letter-spacing.
                            .padding([self.space(0.45), 0.0, self.space(0.05), self.space(0.75)])
                            .width(Length::Fill)
                            .into(),
                        );
                    }
                    // GTK's `tray::badge_text`: "↪ STEER" / "⏸ QUEUE".
                    let delivery_label = crate::tray::badge_text(item.delivery);

                    let preview_text = item.summary.clone();

                    // GTK queue badges: tinted pills, amber for steers and
                    // grey for queued turns.
                    let is_steer = item.delivery != protocol::Delivery::Queue;
                    let (badge_bg, badge_border, badge_fg) = if is_steer {
                        (
                            palette::current().badge_steer_bg,
                            palette::current().badge_steer_border,
                            palette::current().badge_steer_text,
                        )
                    } else {
                        (
                            palette::current().badge_queue_bg,
                            palette::current().badge_queue_border,
                            palette::current().badge_queue_text,
                        )
                    };

                    // GTK's `.session-badge`: 0.74em/700, padding
                    // `0.15em 0.52em` and radius 0.3em. Its 0.04em
                    // letter-spacing has no iced counterpart on text.
                    let badge_radius = self.space(0.3);
                    let badge = container(
                        text(delivery_label)
                            .size(self.em(0.74))
                            .line_height(line_height(1.0))
                            .font(cosmic::iced::Font {
                                weight: cosmic::iced::font::Weight::Bold,
                                ..cosmic::iced::Font::DEFAULT
                            }),
                    )
                    .padding([self.space(0.15) as u16, self.space(0.52) as u16])
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(badge_bg.into()),
                        border: Border {
                            color: badge_border,
                            width: 1.0,
                            radius: badge_radius.into(),
                        },
                        text_color: Some(badge_fg),
                        ..Default::default()
                    });

                    let mut row_items: Vec<Element<'_, Message>> = vec![
                        badge.into(),
                        text(preview_text)
                            .size(self.em(0.96))
                            .class(cosmic::theme::Text::Color(palette::current().tray_text))
                            .width(Length::Fill)
                            .into(),
                    ];
                    // GTK hides a queued row's switch while the tray is paused.
                    if crate::tray::shows_switch(item.delivery, paused) {
                        row_items.push(tray_text_button(
                            crate::tray::switch_label(item.delivery),
                            self.zoom,
                            crate::tray::row_request(&item, RowAction::Switch, paused)
                                .map(|_| Message::TrayAction(item.id.clone(), RowAction::Switch)),
                        ));
                    }
                    row_items.push(tray_icon_button(
                        icons::close(),
                        self.zoom,
                        crate::tray::row_request(&item, RowAction::Cancel, paused)
                            .map(|_| Message::TrayAction(item.id.clone(), RowAction::Cancel)),
                    ));
                    let item_row = row::with_children(row_items)
                        .spacing(self.space(0.59))
                        .align_y(Alignment::Center);

                    tray_rows.push(
                        container(item_row)
                            .padding([self.space(0.3) as u16, self.space(0.6) as u16])
                            .into(),
                    );
                    tray_rows.push(hairline(palette::current().tray_row_divider));
                }

                let tray_radius = self.space(0.67);
                let tray_col =
                    column::with_children(tray_rows).spacing(self.space(crate::metrics::px(0.0)));
                let tray_container = container(tray_col)
                    .padding([
                        self.space(0.25) as u16,
                        self.space(0.3) as u16,
                        self.space(0.3) as u16,
                        self.space(0.3) as u16,
                    ])
                    .width(Length::Fill)
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(palette::current().tray_bg.into()),
                        border: Border {
                            color: palette::current().tray_border,
                            width: 1.0,
                            radius: tray_radius.into(),
                        },
                        ..Default::default()
                    });

                container(tray_container).padding([
                    self.pad_px(0.0),
                    self.pad_px(16.0),
                    self.pad_px(6.0),
                    self.pad_px(16.0),
                ])
            };

            main_items.push(notice_outer);
            main_items.push(tray_outer.into());

            // Composer area
            // GTK's composer: the prompt row, then a footer with the model
            // menu, the token counter, the queue hint while a run is active,
            // and the actions (0.3/0.59/1.19em group spacing).
            let active_dir = self
                .active_session_id
                .as_ref()
                .and_then(|id| self.sessions.get(id))
                .map(|s| s.directory.clone());
            let catalog = active_dir.as_ref().and_then(|d| self.catalogs.get(d));
            let model_id = self.active_session_model_id();

            let supports_attachments = catalog.is_some_and(|catalog| {
                catalog
                    .models
                    .iter()
                    .any(|model| model.supports_attachments)
            });

            let mut footer_items: Vec<Element<'_, Message>> = Vec::new();

            if supports_attachments {
                footer_items.push(
                    // GTK's `.composer-menu`: `padding: 0 0.59em` and a
                    // 2.37em minimum height.
                    button::icon(icons::attach())
                        // GTK's glyph measures 8x12px; 16px rendered 8x10 and
                        // 20px overshot to 10x14.
                        .icon_size(self.em(1.28) as u16)
                        .padding([self.space(0.2) as u16, self.space(0.59) as u16])
                        .on_press(Message::PickAttachments)
                        .into(),
                );
            }

            if let Some(catalog) = catalog
                && !catalog.models.is_empty()
            {
                footer_items.push(
                    self.composer_picker_control(
                        ComposerPicker::Model,
                        model_id
                            .as_ref()
                            .and_then(|id| catalog.models.iter().find(|m| &m.model_id == id))
                            .map(|m| m.label.as_str())
                            .unwrap_or("Select model"),
                        self.model_catalog_menu(),
                    ),
                );

                if let Some(model) = model_id
                    .as_ref()
                    .and_then(|id| catalog.models.iter().find(|model| &model.model_id == id))
                    .filter(|model| !model.variants.is_empty())
                {
                    let selected = self
                        .sessions
                        .get(active_id)
                        .and_then(|session| session.model.as_ref())
                        .and_then(|model| model.variant.as_ref())
                        .and_then(|variant| model.variants.iter().find(|v| *v == variant))
                        .map(String::as_str);
                    footer_items.push(self.composer_picker_control(
                        ComposerPicker::Level,
                        selected.unwrap_or("Default"),
                        self.reasoning_level_menu(),
                    ));
                }
            }

            footer_items.push(
                row::with_children(Vec::<Element<'_, Message>>::new())
                    .width(Length::Fill)
                    .into(),
            );

            let usage = self.active_context_usage();
            let mut status_items: Vec<Element<'_, Message>> = Vec::new();
            if !usage.is_empty() {
                status_items.push(
                    text(usage)
                        .size(self.em(0.82))
                        .class(cosmic::theme::Text::Color(palette::current().muted_text))
                        .into(),
                );
            }
            if is_busy {
                // GTK's `.queue-hint`: 0.82em text with a 16px left margin, so
                // the hint keeps its distance from the counter next to it.
                status_items.push(
                    container(
                        row::with_children(vec![
                            keycap("Ctrl", self.zoom),
                            muted_hint("+", self.zoom),
                            keycap("Enter", self.zoom),
                            muted_hint("to queue", self.zoom),
                        ])
                        .spacing(self.space(0.3))
                        .align_y(Alignment::Center),
                    )
                    .padding([0, self.space(1.19) as u16, 0, self.space(1.19) as u16])
                    .into(),
                );
            }

            let mut action_items: Vec<Element<'_, Message>> = Vec::new();
            // GTK's `.composer-action`: 2.52em (34px) wide/tall.
            let action_pad = self.pad_px(9.0);
            if is_busy {
                action_items.push(
                    button::icon(icons::stop())
                        .padding(action_pad)
                        .on_press(Message::StopSession)
                        .into(),
                );
            }
            action_items.push(
                button::icon(icons::send())
                    .class(accent_button_class(self.zoom))
                    .padding(action_pad)
                    .on_press(Message::SendPrompt(SendMode::Send))
                    .into(),
            );

            let mut composer_items: Vec<Element<'_, Message>> = Vec::new();

            // The chip row stays in the tree even when empty: dropping it
            // would shift the prompt input's widget state (iced matches
            // siblings positionally) and panic on the next frame.
            let chip_radius = self.space(0.44);
            let chips: Vec<Element<'_, Message>> = self
                .pending_attachments
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    container(
                        row::with_children(vec![
                            inline_icon(icons::attach(), self.zoom)
                                .size(self.em(0.76) as u16)
                                .into(),
                            text(attachment_label(path))
                                .size(self.em(0.88))
                                .class(cosmic::theme::Text::Color(palette::current().tray_text))
                                .into(),
                            button::icon(icons::close())
                                .padding([self.pad_px(1.0), self.pad_px(3.0)])
                                .on_press(Message::RemoveAttachment(index))
                                .into(),
                        ])
                        .spacing(self.space(0.3))
                        .align_y(Alignment::Center),
                    )
                    .padding([self.space(0.15) as u16, self.space(0.44) as u16])
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(palette::current().card_bg.into()),
                        border: Border {
                            color: palette::current().panel_border,
                            width: 1.0,
                            radius: chip_radius.into(),
                        },
                        ..Default::default()
                    })
                    .into()
                })
                .collect();
            composer_items.push(
                row::with_children(chips)
                    .spacing(self.space(0.3))
                    .wrap()
                    .into(),
            );

            composer_items.push(
                // GTK's `.composer-input`: a text view with a 72px minimum
                // height and 10/12px margins.
                cosmic::iced::widget::text_editor(&self.composer_editor)
                    .id(composer_id())
                    .on_action(Message::ComposerEdit)
                    .padding([10, 12])
                    .height(Length::Fixed(72.0))
                    // GTK's `.composer-input` is transparent inside the
                    // composer frame, which draws the border itself.
                    .style(|_theme, _status| cosmic::iced::widget::text_editor::Style {
                        background: cosmic::iced::Background::Color(
                            cosmic::iced::Color::TRANSPARENT,
                        ),
                        border: cosmic::iced::Border::default(),
                        placeholder: palette::current().muted_text,
                        value: palette::current().content_text,
                        selection: palette::current().user_message_bg,
                    })
                    .into(),
            );

            // GTK's `.composer-footer-status` keeps 1.19em between the status
            // row and the action group; the actions themselves are one group.
            let action_group = row::with_children(action_items)
                .spacing(self.space(0.59))
                .align_y(Alignment::Center);
            footer_items.push(
                row::with_children(vec![
                    row::with_children(status_items)
                        .spacing(0.0)
                        .align_y(Alignment::Center)
                        .into(),
                    action_group.into(),
                ])
                .spacing(self.space(1.19))
                .align_y(Alignment::Center)
                .into(),
            );

            let footer = row::with_children(footer_items)
                .spacing(self.space(0.59))
                .align_y(Alignment::Center);

            composer_items.push(footer.into());

            let composer_radius = self.space(0.89);
            let composer_frame = container(
                column::with_children(composer_items)
                    .spacing(self.space(0.59))
                    .width(Length::Fill),
            )
            // GTK's composer box margins: 14px sides, 10px top, 12px bottom.
            .padding([
                self.space(0.75) as u16,
                self.space(1.05) as u16,
                self.space(0.9) as u16,
                self.space(1.05) as u16,
            ])
            .width(Length::Fill)
            .style(move |_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().composer_bg.into()),
                border: Border {
                    color: palette::current().composer_border,
                    width: 1.0,
                    radius: composer_radius.into(),
                },
                ..Default::default()
            });

            // GTK's composer stack carries 18px side margins and 16px at the
            // bottom; the top gap below the notice is the composer box's own
            // padding.
            // The gap above the composer comes from the notice's own 8px
            // bottom margin (GTK) rather than from this block.
            let composer_outer = container(composer_frame).padding([
                0,
                self.space(1.35) as u16,
                self.space(1.19) as u16,
                self.space(1.35) as u16,
            ]);
            // GTK's `composer_stack`: an open permission prompt replaces the
            // composer inside the same slot.
            match self.composer_prompt() {
                Some(prompt) => main_items.push(prompt),
                None => main_items.push(composer_outer.into()),
            }
        } else {
            let empty_view = column::with_children(vec![
                text("Welcome to OpenCode COSMIC")
                    .size(self.em(crate::metrics::px(20.0)))
                    .into(),
                text(format!("Status: {}", self.connection_status))
                    .size(self.em(crate::metrics::px(14.0)))
                    .into(),
                button::text("Create New Session")
                    .on_press(Message::NewSession)
                    .padding([self.pad_px(8.0), self.pad_px(16.0)])
                    .into(),
            ])
            .spacing(self.space(crate::metrics::px(16.0)))
            .padding(32)
            .align_x(Alignment::Center);

            main_items.push(
                container(empty_view)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center)
                    .into(),
            );
        }

        let main_pane = column::with_children(main_items)
            .width(Length::Fill)
            .height(Length::Fill);

        let body = if self.sidebar_open {
            row::with_children(vec![
                sidebar.into(),
                sidebar_divider.into(),
                main_pane.into(),
            ])
            .width(Length::Fill)
            .height(Length::Fill)
        } else {
            row::with_children(vec![main_pane.into()])
                .width(Length::Fill)
                .height(Length::Fill)
        };

        container(
            column::with_children(vec![self.header_bar(), body.into()])
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(palette::current().window_bg.into()),
            ..Default::default()
        })
        .into()
    }

    fn context_drawer(&self) -> Option<ContextDrawer<'_, Self::Message>> {
        // GTK had no side panels: sessions and settings were centred modal
        // palettes, and only the background-job list stays a drawer here.
        if self.active_drawer != Some(DrawerPage::Jobs) {
            return None;
        }

        let job_rows = self.jobs.rows(self.active_session_id.as_deref());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut list_items = Vec::new();
        list_items.push(text("Running Background Jobs").size(self.em(1.14)).into());

        if job_rows.is_empty() {
            list_items.push(
                text("No background subagents or shells running.")
                    .size(self.em(0.96))
                    .into(),
            );
        } else {
            for row in job_rows {
                let kind_str = match row.kind {
                    JobKind::Subagent => "◆ Subagent",
                    JobKind::Shell => "$ Shell",
                };

                let item = column::with_children(vec![
                    text(format!("{kind_str}: {}", row.title))
                        .size(self.em(0.96))
                        .into(),
                    text(row.subtitle(now))
                        .size(self.em(0.82))
                        .class(cosmic::theme::Text::Color(palette::current().muted_text))
                        .into(),
                ])
                .spacing(self.space(0.3));

                let radius = self.space(0.44);
                let job_card = container(item)
                    .padding([self.space(0.59) as u16, self.space(0.89) as u16])
                    .width(Length::Fill)
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(palette::current().card_bg.into()),
                        border: Border {
                            color: palette::current().panel_border,
                            width: 1.0,
                            radius: radius.into(),
                        },
                        ..Default::default()
                    });

                list_items.push(job_card.into());
            }
        }

        let list = column::with_children(list_items)
            .spacing(self.space(0.59))
            .padding(self.space(1.19) as u16);
        Some(context_drawer(list, Message::CloseDrawer))
    }

    /// GTK's centred modal palettes (`.app-modal-palette`).
    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        match self.active_drawer? {
            DrawerPage::Jobs => None,
            DrawerPage::Sessions => Some(self.sessions_palette()),
            DrawerPage::Settings => Some(self.settings_palette()),
            DrawerPage::NewSession => Some(self.new_session_palette()),
            DrawerPage::Rename => Some(self.rename_palette()),
        }
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch([
            cosmic::iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick),
            listen_with(|event, _status, _window| match event {
                Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    shortcut(&key, modifiers)
                }
                // GTK showed each row's shortcut number while Alt was held.
                Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                    Some(Message::ModifiersChanged(modifiers))
                }
                // A row's press must not rebuild it (the drag state lives in
                // the widget), so selection and the drop both happen here: the
                // release either lands a dragged row or selects the row under
                // the cursor.
                Event::Mouse(cosmic::iced::mouse::Event::ButtonReleased(
                    cosmic::iced::mouse::Button::Left,
                )) => Some(Message::PointerRelease),
                _ => None,
            }),
        ])
    }
}

/// Keyboard shortcuts, mirroring the GTK client's key controller.
///
/// Shortcuts advertised in the UI ("Ctrl+P", "Ctrl+,", "Ctrl+Enter") must
/// resolve here. Enter reports whether Ctrl is held; the run status then
/// decides between send, steer and queue (see [`enter_mode`]).
fn shortcut(key: &Key, modifiers: Modifiers) -> Option<Message> {
    if modifiers.control() {
        return match key {
            Key::Character(c) => match c.as_str() {
                // GTK's `crate::pending::CANCEL_FORM_SHORTCUT`.
                "x" | "X" if modifiers.shift() => Some(Message::CancelVisibleForm),
                "t" | "T" => Some(Message::NewSession),
                "b" | "B" => Some(Message::ToggleSidebar),
                "w" | "W" => Some(Message::CloseActiveTab),
                "p" | "P" => Some(Message::ToggleDrawer(DrawerPage::Sessions)),
                "," => Some(Message::ToggleDrawer(DrawerPage::Settings)),
                "g" | "G" => Some(Message::FocusComposer),
                "=" | "+" => Some(Message::ZoomIn),
                "-" => Some(Message::ZoomOut),
                "0" => Some(Message::ZoomReset),
                _ => tab_index(c).map(Message::SelectTabIndex),
            },
            Key::Named(Named::Enter) => Some(Message::ComposerEnter { ctrl: true }),
            Key::Named(Named::Tab) => {
                Some(Message::CycleTab(if modifiers.shift() { -1 } else { 1 }))
            }
            _ => None,
        };
    }

    if modifiers.alt() {
        return match key {
            Key::Character(c) => tab_index(c).map(Message::SelectTabIndex),
            _ => None,
        };
    }

    match key {
        Key::Named(Named::F2) => Some(Message::OpenRename),
        // Shift+Enter is the editor's newline.
        Key::Named(Named::Enter) if !modifiers.shift() => {
            Some(Message::ComposerEnter { ctrl: false })
        }
        Key::Named(Named::Escape) => Some(Message::CloseDrawer),
        // An open picker's highlight; without one these do nothing.
        Key::Named(Named::ArrowUp) => Some(Message::PickerMove(-1)),
        Key::Named(Named::ArrowDown) => Some(Message::PickerMove(1)),
        _ => None,
    }
}

/// GTK restored the open tabs and their order from the saved state on start;
/// ids the server no longer reports are dropped.
fn restore_tabs(
    saved: Option<&crate::persist::ServerState>,
    known: &std::collections::HashSet<String>,
) -> Vec<String> {
    let Some(saved) = saved else {
        return Vec::new();
    };
    saved
        .tabs
        .iter()
        .filter(|tab| known.contains(&tab.id))
        .map(|tab| tab.id.clone())
        .collect()
}

/// GTK's drag-to-reorder: the dragged session lands at `to`, the rows between
/// shift by one. Reports whether anything moved.
fn reorder_tabs(tabs: &mut Vec<String>, from: usize, to: usize) -> bool {
    if from == to || from >= tabs.len() || to >= tabs.len() {
        return false;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
    true
}

/// Widget id of the transcript, so a `Task` can keep it at the end of the run.
fn transcript_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("opencode-transcript")
}

/// Puts the caret in the prompt composer (its editor has no `focus` helper).
fn focus_composer() -> Task<Message> {
    focus_widget(composer_id())
}

fn focus_widget(id: cosmic::widget::Id) -> Task<Message> {
    cosmic::iced::advanced::widget::operate(
        cosmic::iced::advanced::widget::operation::focusable::focus(id),
    )
}

fn new_session_search_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("opencode-new-session-search")
}

fn rename_title_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("opencode-rename-title")
}

/// Widget id of the prompt composer, so a `Task` can put the caret in it.
fn composer_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("opencode-composer")
}

fn model_search_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("opencode-model-search")
}

fn level_search_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("opencode-level-search")
}

/// The GTK client's zoom ladder.
const ZOOM_STEPS: [f32; 9] = [0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.5, 1.75];

/// Decodes an inline `data:image/...;base64,...` URI (GTK's message images).
fn inline_image_bytes(uri: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;

    let rest = uri.strip_prefix("data:image/")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.to_lowercase().contains("base64") {
        return None;
    }
    let data = data.trim();
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
        .ok()
}

/// GTK's `.session-tab-action`: dimmed to `opacity: 0.45` until the row is
/// active or hovered.
/// GTK's transcript line heights (`.message-content` 1.35, plain paragraphs
/// 1.45, the error card's body 1.4).
fn line_height(factor: f32) -> cosmic::iced::core::text::LineHeight {
    crate::metrics::line_height(factor)
}

/// GTK dims these glyphs with `opacity: 0.45`. iced's SVG icons honour
/// `icon_color` but not its alpha, so the blend is done here against the
/// surface behind them (the sidebar's own background).
fn dim_over(c: cosmic::iced::Color, bg: cosmic::iced::Color, alpha: f32) -> cosmic::iced::Color {
    cosmic::iced::Color::from_rgba(
        c.r * alpha + bg.r * (1.0 - alpha),
        c.g * alpha + bg.g * (1.0 - alpha),
        c.b * alpha + bg.b * (1.0 - alpha),
        1.0,
    )
}

fn tab_action_class(shown: bool, radius: f32) -> cosmic::theme::Button {
    // GTK: `@oc_fg_sidebar_new_session` dimmed to 45% until the row is active or
    // hovered, then `@oc_fg_session_tab_active_session_tab_title` at full
    // strength. The glyphs are SVG, so `icon_color` is the one that paints them.
    let fg = move || {
        if shown {
            palette::current().tab_active_text
        } else {
            dim_over(
                palette::current().sidebar_label,
                palette::current().sidebar_bg,
                0.45,
            )
        }
    };
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(fg()),
        icon_color: Some(fg()),
        ..Default::default()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| base()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `button.session-tab-close:hover`: a red fill with a white glyph.
fn close_button_class(shown: bool, radius: f32) -> cosmic::theme::Button {
    let fg = move || {
        if shown {
            palette::current().tab_active_text
        } else {
            dim_over(
                palette::current().sidebar_label,
                palette::current().sidebar_bg,
                0.45,
            )
        }
    };
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(fg()),
        icon_color: Some(fg()),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().tab_close_hover_bg.into()),
        text_color: Some(palette::current().tab_close_hover_fg),
        icon_color: Some(palette::current().tab_close_hover_fg),
        ..base()
    };
    let base = move || cosmic::widget::button::Style {
        text_color: if shown {
            None
        } else {
            Some(palette::current().muted_text)
        },
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| hovered()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `button.queue-tray-resume`: the accent fill with a pill radius.
fn resume_button_class(radius: f32) -> cosmic::theme::Button {
    let base = move || cosmic::widget::button::Style {
        background: Some(palette::current().accent_bg.into()),
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(palette::current().accent_fg),
        ..Default::default()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| base()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// A chip label for an attachment: the file name, middle-shortened when long.
fn attachment_label(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 32 {
        name
    } else {
        let head: String = chars[..14].iter().collect();
        let tail: String = chars[chars.len() - 10..].iter().collect();
        format!("{head}…{tail}")
    }
}

/// `13400` -> `13.4k`, `200000` -> `200k`, `950` -> `950`.
fn compact_tokens(value: u64) -> String {
    if value < 1000 {
        return value.to_string();
    }
    let thousands = value as f64 / 1000.0;
    if (thousands.fract() * 10.0).round() < 0.5 {
        format!("{:.0}k", thousands)
    } else {
        format!("{:.1}k", thousands)
    }
}

/// The next step along [`ZOOM_STEPS`] in `direction` (clamped at both ends).
fn next_zoom(current: f32, direction: i32) -> f32 {
    if direction > 0 {
        ZOOM_STEPS
            .iter()
            .copied()
            .find(|step| *step > current + 0.04)
            .unwrap_or_else(|| *ZOOM_STEPS.last().unwrap_or(&1.0))
    } else {
        ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|step| *step < current - 0.04)
            .unwrap_or_else(|| *ZOOM_STEPS.first().unwrap_or(&1.0))
    }
}

/// A 1px full-width rule, for the GTK client's `border-bottom` row separators
/// (iced's `Border` has no per-side control).
fn hairline(color: cosmic::iced::Color) -> Element<'static, Message> {
    container(row::with_children(Vec::<Element<'_, Message>>::new()))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_theme: &cosmic::Theme| container::Style {
            background: Some(color.into()),
            ..Default::default()
        })
        .into()
}

/// GTK drew plain message bodies as one `gtk::Label`, where an empty line in
/// the text is an empty line on screen. iced gives an empty line no height, so
/// each blank line carries a space to hold it open.
fn blank_lines(body: &str) -> String {
    body.replace("\n\n", "\n \n")
}

/// `HH:MM` in local time from a protocol timestamp (milliseconds, or seconds
/// when the value is small enough to be one).
fn clock_time(created: u64) -> String {
    let ms = if created > 10_000_000_000 {
        created
    } else {
        created.saturating_mul(1000)
    };
    let Ok(stamp) = jiff::Timestamp::from_millisecond(ms as i64) else {
        return String::new();
    };
    let zoned = stamp.to_zoned(jiff::tz::TimeZone::system());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        zoned.year(),
        zoned.month(),
        zoned.day(),
        zoned.hour(),
        zoned.minute()
    )
}

/// GTK's `.composer-menu` chevron follows its label by 0.3em.
fn menu_chevron(zoom: f32) -> Element<'static, Message> {
    cosmic::widget::icon::icon(icons::chevron_down())
        .size(crate::metrics::em(0.76, zoom) as u16)
        .class(cosmic::theme::Svg::custom(|_theme: &cosmic::Theme| {
            cosmic::iced::widget::svg::Style {
                color: Some(palette::current().muted_text),
            }
        }))
        .into()
}

/// GTK's suggested action (`.composer-action.suggested-action`): the client's
/// amber, not the COSMIC theme accent.
fn accent_button_class(zoom: f32) -> cosmic::theme::Button {
    // GTK's `.composer-action.suggested-action`: a circle on a 2.52em button.
    let radius = crate::metrics::space(999.0, zoom);
    let base = move || cosmic::widget::button::Style {
        background: Some(palette::current().accent_bg.into()),
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(palette::current().accent_fg),
        ..Default::default()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| base()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `.new-session-row`: the first row is selected, others highlight on hover.
fn modal_row_class(selected: bool, radius: f32) -> cosmic::theme::Button {
    let base = move || cosmic::widget::button::Style {
        background: selected.then(|| palette::current().new_session_row_hover_bg.into()),
        border_radius: radius.into(),
        border_width: 0.0,
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().new_session_row_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `.composer-menu`: transparent at rest, with the composer-action hover fill.
fn composer_menu_class(radius: f32) -> cosmic::theme::Button {
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(palette::current().window_fg),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().action_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| hovered()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `.model-picker-row` is a flat button; selection and hover share a fill.
fn model_row_class(
    selected: bool,
    level: bool,
    highlighted: bool,
    radius: f32,
) -> cosmic::theme::Button {
    let base = move || cosmic::widget::button::Style {
        background: if selected && level {
            Some(palette::current().level_selected_bg.into())
        } else if selected || highlighted {
            Some(palette::current().model_row_hover_bg.into())
        } else {
            None
        },
        border_radius: radius.into(),
        border_width: 0.0,
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().model_row_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| hovered()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `.model-picker-search` uses the session-id entry tokens, not the
/// generic field's colours, and focuses with the picker-specific blue border.
fn model_search_class(radius: f32) -> cosmic::theme::TextInput {
    let appearance = move |focused: bool| {
        let p = palette::current();
        cosmic::widget::text_input::Appearance {
            background: p.model_search_bg.into(),
            border_radius: radius.into(),
            border_offset: None,
            border_width: 1.0,
            border_color: if focused {
                p.model_search_focus_border
            } else {
                p.model_search_border
            },
            label_color: p.model_search_text,
            placeholder_color: p.model_subtext,
            selected_text_color: p.model_search_text,
            icon_color: Some(p.model_subtext),
            text_color: Some(p.model_search_text),
            selected_fill: p.model_active_title,
        }
    };
    cosmic::theme::TextInput::Custom {
        active: Box::new(move |_theme| appearance(false)),
        error: Box::new(move |_theme| appearance(true)),
        hovered: Box::new(move |_theme| appearance(false)),
        focused: Box::new(move |_theme| appearance(true)),
        disabled: Box::new(move |_theme| appearance(false)),
    }
}

/// GTK's `.entry`: white fill, 1px border, the accent while focused. Every
/// `text_input` in the app uses it (the composer's editor is not a text_input).
fn field_input_class() -> cosmic::theme::TextInput {
    let appearance = move |focused: bool| {
        let p = palette::current();
        cosmic::widget::text_input::Appearance {
            background: p.composer_bg.into(),
            border_radius: 4.0.into(),
            border_offset: None,
            border_width: 1.0,
            border_color: if focused {
                p.field_focus_border
            } else {
                p.modal_border
            },
            label_color: p.muted_text,
            placeholder_color: p.prompt_metadata,
            selected_text_color: p.content_text,
            icon_color: Some(p.muted_text),
            text_color: Some(p.content_text),
            selected_fill: p.accent_bg,
        }
    };
    cosmic::theme::TextInput::Custom {
        active: Box::new(move |_theme| appearance(false)),
        error: Box::new(move |_theme| appearance(true)),
        hovered: Box::new(move |_theme| appearance(false)),
        focused: Box::new(move |_theme| appearance(true)),
        disabled: Box::new(move |_theme| appearance(false)),
    }
}

/// GTK's `.new-session-search` paints the whole band, not an inset entry.
fn new_session_search_input_class() -> cosmic::theme::TextInput {
    let appearance = move || {
        let p = palette::current();
        cosmic::widget::text_input::Appearance {
            background: cosmic::iced::Color::TRANSPARENT.into(),
            border_radius: 0.0.into(),
            border_offset: None,
            border_width: 0.0,
            border_color: p.new_session_search_border,
            label_color: p.header_title_text,
            placeholder_color: p.prompt_metadata,
            selected_text_color: p.header_title_text,
            icon_color: None,
            text_color: Some(p.header_title_text),
            selected_fill: p.accent_bg,
        }
    };
    cosmic::theme::TextInput::Custom {
        active: Box::new(move |_theme| appearance()),
        error: Box::new(move |_theme| appearance()),
        hovered: Box::new(move |_theme| appearance()),
        focused: Box::new(move |_theme| appearance()),
        disabled: Box::new(move |_theme| appearance()),
    }
}

fn field_input<'a>(
    placeholder: &'a str,
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
) -> cosmic::widget::TextInput<'a, Message> {
    text_input(placeholder, value)
        .on_input(on_input)
        .style(field_input_class())
}

/// A key cap for GTK's modal footer hints (`.keycap`-style chip).
fn keycap<'a>(label: &'a str, zoom: f32) -> Element<'a, Message> {
    container(
        text(label)
            .size(crate::metrics::em(0.72, zoom))
            .class(cosmic::theme::Text::Color(palette::current().muted_text)),
    )
    .padding([
        crate::metrics::px(1.0) as u16,
        crate::metrics::px(4.0) as u16,
    ])
    .style(|_theme: &cosmic::Theme| container::Style {
        background: Some(palette::current().composer_bg.into()),
        border: Border {
            color: palette::current().modal_border,
            width: 1.0,
            radius: 3.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn fill_spacer<'a>() -> Element<'a, Message> {
    container(row::with_children(Vec::<Element<'a, Message>>::new()))
        .width(Length::Fill)
        .into()
}

fn muted_hint<'a>(label: &'a str, zoom: f32) -> Element<'a, Message> {
    text(label)
        .size(crate::metrics::em(0.82, zoom))
        .class(cosmic::theme::Text::Color(palette::current().muted_text))
        .into()
}

/// GTK's `fuzzy_score` (ui.rs:939): subsequence match with bonuses for
/// adjacency, word starts and a contiguous run, used by every picker.
fn fuzzy_score(query: &str, target: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q_lower: Vec<char> = query.to_lowercase().chars().collect();
    let t_lower: Vec<char> = target.to_lowercase().chars().collect();
    let original: Vec<char> = target.chars().collect();
    let mut q_idx = 0;
    let mut score: i64 = 0;
    let mut prev_match_idx: Option<usize> = None;
    let mut first_match_idx: Option<usize> = None;
    for (t_idx, &t_ch) in t_lower.iter().enumerate() {
        if q_idx < q_lower.len() && t_ch == q_lower[q_idx] {
            if first_match_idx.is_none() {
                first_match_idx = Some(t_idx);
            }
            score += 10;
            if let Some(prev) = prev_match_idx
                && prev + 1 == t_idx
            {
                score += 15;
            }
            if t_idx == 0 {
                score += 30;
            } else {
                let prev_ch = original[t_idx - 1];
                if matches!(prev_ch, ' ' | '-' | '_' | '/' | '.' | ':') {
                    score += 25;
                } else if prev_ch.is_lowercase() && original[t_idx].is_uppercase() {
                    score += 20;
                }
            }
            prev_match_idx = Some(t_idx);
            q_idx += 1;
        }
    }
    if q_idx < q_lower.len() {
        return None;
    }
    let t_str = target.to_lowercase();
    let q_str = query.to_lowercase();
    if let Some(idx) = t_str.find(&q_str) {
        score += 50;
        if idx == 0 {
            score += 25;
        }
    }
    if let (Some(first), Some(last)) = (first_match_idx, prev_match_idx) {
        score -= last.saturating_sub(first) as i64;
    }
    Some(score - (t_lower.len() as i64) / 4)
}

/// GTK's `filter_tab_sessions` (ui.rs:8504) and its `SESSION_PICKER_LIMIT`:
/// fuzzy over the title and the directory, the tabs' own order for an empty
/// query, otherwise by score and then by that order, capped at 200 rows.
fn filter_tab_sessions<'a>(
    tabs: &'a [String],
    sessions: &'a std::collections::HashMap<String, model::Session>,
    query: &str,
) -> Vec<&'a model::Session> {
    let query = query.trim();
    let mut scored: Vec<(i64, usize, &'a model::Session)> = tabs
        .iter()
        .enumerate()
        .filter_map(|(idx, id)| {
            let session = sessions.get(id)?;
            if session.parent_id.is_some() {
                return None;
            }
            if query.is_empty() {
                return Some((0, idx, session));
            }
            let title_score = fuzzy_score(query, &session.title);
            let dir_score = fuzzy_score(query, &session.directory);
            match (title_score, dir_score) {
                (Some(a), Some(b)) => Some((a.max(b), idx, session)),
                (a, b) => a.or(b).map(|score| (score, idx, session)),
            }
        })
        .collect();
    if !query.is_empty() {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    }
    scored
        .into_iter()
        .take(200)
        .map(|(_, _, session)| session)
        .collect()
}

/// GTK's `project_paths` (ui.rs:9115): project worktrees then session
/// directories, deduplicated in that order.
fn project_paths(projects: &[model::Project], sessions: &[&model::Session]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for project in projects {
        if !paths.contains(&project.worktree) {
            paths.push(project.worktree.clone());
        }
    }
    for session in sessions {
        if !paths.contains(&session.directory) {
            paths.push(session.directory.clone());
        }
    }
    paths
}

/// GTK's `button.session-picker-row`: white fill, 1px border and a
/// `0 1px 3px` shadow, with hover and active token pairs.
fn session_picker_row_class(active: bool, radius: f32) -> cosmic::theme::Button {
    let style = move |hovered: bool| {
        let p = palette::current();
        // GTK's picker rows are plain ListBox rows: no fill and no border,
        // the ListBox's own hover/selected fill otherwise.
        let background = if active {
            Some(p.picker_row_selected_bg.into())
        } else if hovered {
            Some(p.picker_row_hover_bg.into())
        } else {
            None
        };
        cosmic::widget::button::Style {
            background,
            border_radius: radius.into(),
            ..Default::default()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| style(false)),
        hovered: Box::new(move |_focused, _theme| style(true)),
        pressed: Box::new(move |_focused, _theme| style(false)),
        disabled: Box::new(move |_theme| style(false)),
    }
}

/// GTK's `button.settings-rail-item`: normal, hover and active token pairs.
fn settings_rail_item_class(active: bool, radius: f32) -> cosmic::theme::Button {
    let style = move |hovered: bool| {
        let p = palette::current();
        let (background, foreground) = if active {
            (
                Some(p.settings_rail_item_active_bg.into()),
                p.settings_rail_item_active_fg,
            )
        } else if hovered {
            (
                Some(p.settings_rail_item_hover_bg.into()),
                p.settings_rail_item_hover_fg,
            )
        } else {
            (None, p.settings_rail_item_fg)
        };
        cosmic::widget::button::Style {
            background,
            text_color: Some(foreground),
            icon_color: Some(foreground),
            border_radius: radius.into(),
            border_width: 0.0,
            ..Default::default()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| style(false)),
        hovered: Box::new(move |_focused, _theme| style(true)),
        pressed: Box::new(move |_focused, _theme| style(false)),
        disabled: Box::new(move |_theme| style(false)),
    }
}

/// GTK's `.sidebar-new-session` / `.sidebar-nav`: flat, the label and its icon
/// in `@oc_fg_sidebar_new_session`, with the sidebar's hover fill.
fn sidebar_row_class(zoom: f32) -> cosmic::theme::Button {
    let radius = crate::metrics::space(0.52, zoom);
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(palette::current().sidebar_label),
        icon_color: Some(palette::current().sidebar_label),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().sidebar_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's flat buttons (`.sidebar-nav`, `headerbar button`): no background
/// until hover. `cosmic::theme::Button::Transparent` hides the label, so the
/// class is built here with an explicit text colour.
fn flat_button_class(zoom: f32) -> cosmic::theme::Button {
    let radius = crate::metrics::space(0.5, zoom);
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(palette::current().header_title_text),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().action_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's scrollbars: Adwaita's trough is 15px wide with an 8px rounded thumb
/// centred in it. libcosmic's theme class for scrollables is an enum with no
/// custom variant (its `Scrollable::style` needs a class, not a closure), so
/// only the metrics are settable — the default rail was 10px wide with a 10px
/// thumb, which hid the trough completely.
fn scrollbar_direction() -> cosmic::iced::widget::scrollable::Direction {
    cosmic::iced::widget::scrollable::Direction::Vertical(
        cosmic::iced::widget::scrollable::Scrollbar::new()
            .width(15.0)
            .scroller_width(8.0),
    )
}

/// GTK's plain (default) button — Adwaita's `button.bg` with its 1px border,
/// not one of the client's `@oc_*` styles. Used by the form notice's Cancel and
/// the modals' default actions, which GTK left unstyled.
fn plain_button_class(zoom: f32) -> cosmic::theme::Button {
    let radius = crate::metrics::space(0.5, zoom);
    let base = move || cosmic::widget::button::Style {
        background: Some(palette::current().plain_button_bg.into()),
        border_radius: radius.into(),
        border_width: 1.0,
        border_color: palette::current().plain_button_border,
        text_color: Some(palette::current().plain_button_text),
        icon_color: Some(palette::current().plain_button_text),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().action_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `headerbar button.sidebar-toggle`: flat, `border-radius: 0.44em`,
/// hover fill `@oc_bg_headerbar_button_sidebar_toggle_hover`, drawn in Adwaita's
/// icon colour rather than the theme accent.
fn header_toggle_class() -> cosmic::theme::Button {
    let radius = crate::metrics::space(0.44, 1.0);
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_radius: radius.into(),
        border_width: 0.0,
        text_color: Some(palette::current().header_icon),
        icon_color: Some(palette::current().header_icon),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().headerbar_toggle_hover.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| hovered()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `windowcontrols button`: flat, the glyph in Adwaita's icon colour.
fn header_control_class() -> cosmic::theme::Button {
    let base = move || cosmic::widget::button::Style {
        background: None,
        border_width: 0.0,
        text_color: Some(palette::current().header_icon),
        icon_color: Some(palette::current().header_icon),
        ..Default::default()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| base()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `.queue-tray-button`: 1.85em minimum height, 1px border, its own
/// background and text colour.
fn tray_button_class(radius: f32) -> cosmic::theme::Button {
    let base = move || cosmic::widget::button::Style {
        background: Some(palette::current().tray_button_bg.into()),
        border_radius: radius.into(),
        border_width: 1.0,
        border_color: palette::current().tray_button_border,
        text_color: Some(palette::current().tray_button_text),
        ..Default::default()
    };
    let hovered = move || cosmic::widget::button::Style {
        background: Some(palette::current().action_hover_bg.into()),
        ..base()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |_focused, _theme| base()),
        hovered: Box::new(move |_focused, _theme| hovered()),
        pressed: Box::new(move |_focused, _theme| base()),
        disabled: Box::new(move |_theme| base()),
    }
}

/// GTK's `.prompt-detail`: a monospace band with its own surface, used for a
/// prompt's command, its metadata and its always-allow patterns.
fn prompt_band(content: String, zoom: f32) -> Element<'static, Message> {
    let radius = crate::metrics::space(0.52, zoom);
    container(
        text(content)
            .size(crate::metrics::em(0.9, zoom))
            .font(cosmic::iced::Font::MONOSPACE)
            .class(cosmic::theme::Text::Color(
                palette::current().prompt_detail_fg,
            )),
    )
    .padding([
        crate::metrics::space(0.74, zoom) as u16,
        crate::metrics::space(0.89, zoom) as u16,
    ])
    .width(Length::Fill)
    .style(move |_theme: &cosmic::Theme| container::Style {
        background: Some(palette::current().prompt_detail_bg.into()),
        border: Border {
            radius: radius.into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .into()
}

/// A disabled control's content: GTK dims it toward the surface it sits on
/// (`set_sensitive(false)` renders the icon at roughly a quarter strength).
fn dimmed_content(fg: cosmic::iced::Color, bg: cosmic::iced::Color) -> cosmic::iced::Color {
    let mix = |a: f32, b: f32| a * 0.26 + b * 0.74;
    cosmic::iced::Color::from_rgb(mix(fg.r, bg.r), mix(fg.g, bg.g), mix(fg.b, bg.b))
}

fn tray_text_button(
    label: &'static str,
    zoom: f32,
    action: Option<Message>,
) -> Element<'static, Message> {
    let radius = crate::metrics::space(0.37, zoom);
    let palette = palette::current();
    // GTK: `button.set_sensitive(tray::row_request(row, action, paused).is_some())`.
    let ink = if action.is_some() {
        palette.tray_button_text
    } else {
        dimmed_content(palette.tray_button_text, palette.tray_button_bg)
    };
    button::custom(
        text(label)
            .size(crate::metrics::em(0.96, zoom))
            .line_height(crate::metrics::line_height(1.0))
            .class(cosmic::theme::Text::Color(ink)),
    )
    // GTK's `button.queue-tray-cancel`: `min-height: 1.85em`,
    // `padding: 0 0.7em`.
    .height(Length::Fixed(crate::metrics::space(1.85, zoom)))
    .padding([0, crate::metrics::space(0.7, zoom) as u16])
    .class(tray_button_class(radius))
    .on_press_maybe(action)
    .into()
}

/// The square icon twin of [`tray_text_button`].
fn tray_icon_button(
    handle: cosmic::widget::icon::Handle,
    zoom: f32,
    action: Option<Message>,
) -> Element<'static, Message> {
    let radius = crate::metrics::space(0.37, zoom);
    let size = crate::metrics::em(0.92, zoom) as u16;
    let palette = palette::current();
    let ink = if action.is_some() {
        palette.tray_button_text
    } else {
        dimmed_content(palette.tray_button_text, palette.tray_button_bg)
    };
    button::custom(
        cosmic::widget::icon::icon(handle)
            .size(size)
            .class(cosmic::theme::Svg::custom(move |_theme: &cosmic::Theme| {
                cosmic::iced::widget::svg::Style { color: Some(ink) }
            })),
    )
    // GTK's square twin: `min-width: 1.85em; padding: 0` with a 1em glyph.
    .width(Length::Fixed(crate::metrics::space(1.85, zoom)))
    .height(Length::Fixed(crate::metrics::space(1.85, zoom)))
    .padding(0)
    .class(tray_button_class(radius))
    .on_press_maybe(action)
    .into()
}

/// A bundled 16px icon painted in the theme's icon colour, for inline use.
fn inline_icon(handle: cosmic::widget::icon::Handle, zoom: f32) -> cosmic::widget::icon::Icon {
    cosmic::widget::icon::icon(handle).size(crate::metrics::em(1.23, zoom) as u16)
}

/// A small drawn status dot. The GTK client drew these with CSS; before this,
/// the port used the text glyphs `●` / `○`, which depend on the font.
/// GTK labelled every row, action and footer entry with a tooltip; libcosmic's
/// wrapper carries them again.
fn hinted<'a>(
    content: impl Into<Element<'a, Message>>,
    hint: impl Into<String>,
) -> Element<'a, Message> {
    cosmic::widget::tooltip::tooltip(
        content,
        text(hint.into()).size(13.0),
        cosmic::widget::tooltip::Position::Bottom,
    )
    .into()
}

/// GTK ellipsized a row's title at the row's width; iced clips instead, so the
/// port shortens it to roughly what fits a 272px sidebar next to its actions.
fn truncate_title(title: &str, limit: usize) -> String {
    if title.chars().count() > limit {
        let clipped: String = title.chars().take(limit - 1).collect();
        format!("{clipped}…")
    } else {
        title.to_string()
    }
}

fn status_dot(color: cosmic::iced::Color, filled: bool) -> Element<'static, Message> {
    let style = move |_theme: &cosmic::Theme| container::Style {
        background: filled.then(|| color.into()),
        border: Border {
            color,
            width: if filled { 0.0 } else { 1.0 },
            radius: 5.0.into(),
        },
        ..Default::default()
    };

    container(row::with_children(Vec::<Element<'_, Message>>::new()))
        .width(Length::Fixed(9.0))
        .height(Length::Fixed(9.0))
        .style(style)
        .into()
}

/// Maps a character to a zero-based tab index for the `1`..`9` shortcuts.
fn tab_index(c: &str) -> Option<usize> {
    let digit = c.parse::<usize>().ok()?;
    if (1..=9).contains(&digit) {
        Some(digit - 1)
    } else {
        None
    }
}

/// The first effort-menu entry is the unqualified model selection.
fn variant_selection(selection: &str) -> Option<String> {
    (!selection.is_empty()).then(|| selection.to_string())
}

impl OpenCodeCosmic {
    /// GTK's `GtkHeaderBar`: the sidebar toggle at the start, `OpenCode` and the
    /// connection status centred on the window, and the window buttons at the
    /// end. libcosmic's own headerbar is COSMIC-themed (its background comes
    /// from the theme's base colour, not this client's tokens), so with
    /// `show_headerbar = false` the port draws the whole bar itself.
    fn header_bar(&self) -> Element<'_, Message> {
        // GTK drew the toggle's panel glyph and the window buttons at a fixed
        // 16px, inside boxes sized by `em` (the toggle) and by Adwaita's
        // window controls (the buttons).
        // GTK draws 7px window-control glyphs; the port's measured 10px, so the
        // icons are 14px with the SVGs' glyphs normalised to 8 of a 16-unit viewBox.
        let glyph: u16 = 14;
        let toggle_size = self.space(2.07);
        let toggle = button::icon(icons::panel())
            .icon_size(glyph)
            .on_press(Message::ToggleSidebar)
            .padding((toggle_size - f32::from(glyph)) / 2.0)
            .class(header_toggle_class());

        // GTK's `windowcontrols`: minimize, maximize/restore and close, each a
        // 39px box flush with the window's right edge.
        const CONTROL_SIZE: f32 = 39.0;
        let control_pad = (CONTROL_SIZE - f32::from(glyph)) / 2.0;
        // GTK centred the title widget on the *window*, so the toggle's side
        // gets a box as wide as the window controls'.
        let group_width = CONTROL_SIZE * 3.0;
        let mut controls: Vec<Element<'_, Message>> = Vec::new();
        if cosmic::config::show_minimize() {
            controls.push(
                button::icon(icons::window_minimize())
                    .icon_size(glyph)
                    .on_press(Message::HeaderMinimize)
                    .padding(control_pad)
                    .class(header_control_class())
                    .into(),
            );
        }
        if cosmic::config::show_maximize() {
            let icon = if self.core.window.is_maximized {
                icons::window_restore()
            } else {
                icons::window_maximize()
            };
            controls.push(
                button::icon(icon)
                    .icon_size(glyph)
                    .on_press(Message::HeaderMaximize)
                    .padding(control_pad)
                    .class(header_control_class())
                    .into(),
            );
        }
        controls.push(
            button::icon(icons::window_close())
                .icon_size(glyph)
                .on_press(Message::HeaderClose)
                .padding(control_pad)
                .class(header_control_class())
                .into(),
        );

        // GTK packed the title widget in the headerbar's centre: `OpenCode`
        // (bold, `@oc_fg_window`) then the connection status
        // (`@oc_fg_connection_status`, 0.82em) with a 10px gap.
        let status_color = if self.connection_status_error {
            palette::current().connection_status_error
        } else {
            palette::current().connection_status
        };
        let status = row::with_children(vec![
            text("OpenCode")
                .size(self.em(1.0))
                .font(cosmic::iced::Font {
                    weight: cosmic::iced::font::Weight::Bold,
                    ..cosmic::iced::Font::DEFAULT
                })
                .class(cosmic::theme::Text::Color(palette::current().window_fg))
                .into(),
            text(&self.connection_status)
                .size(self.em(0.82))
                .class(cosmic::theme::Text::Color(status_color))
                .into(),
        ])
        .spacing(10.0)
        .align_y(Alignment::Center);

        // Adwaita's titlebar is 46px high; the em-sized toggle can exceed it at
        // high zoom, exactly as GTK's min-height let it.
        let bar_height = 46.0_f32.max(toggle_size);

        let bar = row::with_children(vec![
            // The toggle's box matches the window controls' width so the
            // centred group lands on the window's centre like GTK's.
            container(row::with_children(vec![toggle.into()]).align_y(Alignment::Center))
                .width(Length::Fixed(group_width))
                .align_x(Alignment::Start)
                .into(),
            // GTK's headerbar was one big drag region; only its buttons did
            // not move the window.
            cosmic::iced::widget::mouse_area(
                container(status)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center),
            )
            .on_drag(Message::HeaderDrag)
            .on_double_press(Message::HeaderMaximize)
            .into(),
            row::with_children(controls)
                .align_y(Alignment::Center)
                .into(),
        ])
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .height(Length::Fixed(bar_height))
        .padding([0.0, 1.0, 0.0, self.space(0.71)]);

        let bar = container(bar)
            .width(Length::Fill)
            .height(Length::Fixed(bar_height))
            .style(|_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().headerbar_bg.into()),
                ..Default::default()
            });

        column::with_children(vec![
            bar.into(),
            hairline(palette::current().headerbar_border),
        ])
        .width(Length::Fill)
        .into()
    }

    /// Runs `change` on the jobs list with the root sessions and the locations
    /// a refresh lists shells in (GTK's `with_jobs`).
    fn with_jobs<R>(
        &mut self,
        change: impl FnOnce(&mut crate::jobs::Jobs, &crate::jobs::Context) -> R,
    ) -> R {
        let directories = self.pending_directories();
        let roots: Vec<Session> = self.sessions.values().cloned().collect();
        let context = crate::jobs::Context {
            roots: &roots,
            directories: &directories,
        };
        change(&mut self.jobs, &context)
    }

    /// Sends a command to the live API, or drives the canned preview server
    /// with it (the preview has no API handle).
    fn send_command(&mut self, command: Command) {
        if let Some(api) = &self.api {
            api.send(command);
        } else if let Some(mock) = &mut self.mock_server {
            let event = mock.handle(command);
            self.handle_ui_event(event);
        }
    }

    /// Fetches the sessions the jobs list is missing (child sessions and their
    /// parents), each once (GTK's `job_info_command`).
    fn job_info_command(&mut self) -> Option<Command> {
        let session_ids = self.with_jobs(|jobs, context| jobs.take_wanted(context));
        (!session_ids.is_empty()).then_some(Command::LoadSessionInfo { session_ids })
    }

    /// The locations of the open prompts and forms.
    fn open_request_directories(&self) -> Vec<String> {
        let directories: std::collections::BTreeSet<&str> = self
            .forms
            .directories()
            .filter(|directory| !directory.is_empty())
            .collect();
        directories.into_iter().map(str::to_owned).collect()
    }

    /// The locations whose pending lists a reconciliation fetches: open tabs,
    /// running sessions and open forms
    /// ([`crate::pending::pending_directories`]).
    fn pending_directories(&self) -> Vec<String> {
        let open = self.open_request_directories();
        let roots: Vec<Session> = self.sessions.values().cloned().collect();
        crate::pending::pending_directories(
            &roots,
            self.tabs.iter().map(String::as_str).chain(
                self.statuses
                    .iter()
                    .filter(|(_, status)| status.is_busy())
                    .map(|(id, _)| id.as_str()),
            ),
            open.iter().map(String::as_str),
        )
    }

    /// `factor` em in the current zoom, as whole pixels.
    fn em(&self, factor: f32) -> u32 {
        crate::metrics::em(factor, self.zoom)
    }

    /// `factor` em in the current zoom, as a logical pixel count for paddings.
    /// A GTK pixel value as an em factor at this zoom.
    fn pad_px(&self, px: f32) -> u16 {
        self.space(crate::metrics::px(px)) as u16
    }

    fn space(&self, factor: f32) -> f32 {
        crate::metrics::space(factor, self.zoom)
    }

    /// GTK anchored both pickers above their `.composer-menu` buttons. The
    /// COSMIC dropdown accepts only strings, so its popover is used directly
    /// to host the GTK search entry and two-line rows.
    fn composer_picker_control<'a>(
        &self,
        picker: ComposerPicker,
        label: &str,
        menu: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let title = text(label.to_owned())
            .size(self.em(0.9))
            .font(cosmic::iced::Font {
                weight: cosmic::iced::font::Weight::Medium,
                ..cosmic::iced::Font::DEFAULT
            })
            .class(cosmic::theme::Text::Color(palette::current().window_fg));
        let button = button::custom(
            row::with_children(vec![title.into(), menu_chevron(self.zoom)])
                .spacing(self.space(0.3))
                .align_y(Alignment::Center),
        )
        // GTK's `.composer-menu`: minimum 2.37em high, 0.59em side padding.
        .height(Length::Fixed(self.space(2.37)))
        .padding([0.0, self.space(0.59)])
        .class(composer_menu_class(self.space(0.59)))
        .on_press(Message::ToggleComposerPicker(picker));
        let mut popover = cosmic::widget::popover(button)
            .position(cosmic::widget::popover::Position::Top)
            .on_close(Message::CloseComposerPicker);
        if self.composer_picker == Some(picker) {
            popover = popover.popup(menu);
        }
        popover.into()
    }

    fn model_catalog_menu<'a>(&'a self) -> Element<'a, Message> {
        let rows: Vec<Element<'a, Message>> = self
            .picker_candidates(ComposerPicker::Model)
            .into_iter()
            .enumerate()
            .map(|(index, (label, subtext, selected, action))| {
                self.model_picker_row(
                    &label,
                    subtext,
                    selected,
                    false,
                    index == self.picker_highlight,
                    action,
                )
            })
            .collect();
        self.composer_picker_menu(
            ComposerPicker::Model,
            rows,
            "Search models (fuzzy)...",
            "No matching models",
            340.0,
            280.0,
            100.0,
            16.0,
        )
    }

    fn reasoning_level_menu<'a>(&'a self) -> Element<'a, Message> {
        let rows: Vec<Element<'a, Message>> = self
            .picker_candidates(ComposerPicker::Level)
            .into_iter()
            .enumerate()
            .map(|(index, (label, subtext, selected, action))| {
                self.model_picker_row(
                    &label,
                    subtext,
                    selected,
                    true,
                    index == self.picker_highlight,
                    action,
                )
            })
            .collect();
        self.composer_picker_menu(
            ComposerPicker::Level,
            rows,
            "Search levels (fuzzy)...",
            "No matching levels",
            240.0,
            240.0,
            80.0,
            14.0,
        )
    }

    /// The active session's model catalog, when its directory's is loaded.
    fn active_catalog(&self) -> Option<&ModelCatalog> {
        let active = self.active_session_id.as_ref()?;
        let directory = self.sessions.get(active)?.directory.clone();
        self.catalogs.get(&directory)
    }

    /// The variants the active session's model offers.
    fn active_model_variants(&self) -> Vec<String> {
        let (Some(catalog), Some(model_id)) =
            (self.active_catalog(), self.active_session_model_id())
        else {
            return Vec::new();
        };
        catalog
            .models
            .iter()
            .find(|model| model.model_id == model_id)
            .map(|model| model.variants.clone())
            .unwrap_or_default()
    }

    /// The active session's chosen reasoning level.
    fn active_variant(&self) -> Option<String> {
        let active = self.active_session_id.as_ref()?;
        self.sessions.get(active)?.model.as_ref()?.variant.clone()
    }

    /// The rows an open picker lists, in display order: the same filter and
    /// sort its menu renders, so the arrow keys move over exactly those rows.
    /// Each entry is (label, subtext, selected, action).
    fn picker_candidates(
        &self,
        picker: ComposerPicker,
    ) -> Vec<(String, Option<String>, bool, Message)> {
        match picker {
            ComposerPicker::Model => {
                let Some(catalog) = self.active_catalog() else {
                    return Vec::new();
                };
                let selected = self.active_session_model_id();
                let query = self.model_search.trim();
                let mut candidates: Vec<(i64, &model::ModelOption)> = catalog
                    .models
                    .iter()
                    .filter_map(|model| {
                        if query.is_empty() {
                            return Some((0, model));
                        }
                        let label_score = fuzzy_score(query, &model.label);
                        let id_score = fuzzy_score(
                            query,
                            &format!("{} / {}", model.provider_id, model.model_id),
                        );
                        label_score
                            .into_iter()
                            .chain(id_score)
                            .max()
                            .map(|score| (score, model))
                    })
                    .collect();
                if !query.is_empty() {
                    candidates
                        .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
                }
                candidates
                    .into_iter()
                    .map(|(_, model)| {
                        (
                            model.label.clone(),
                            Some(format!("{}/{}", model.provider_id, model.model_id)),
                            selected.as_deref() == Some(model.model_id.as_str()),
                            Message::SelectModel(model.model_id.clone()),
                        )
                    })
                    .collect()
            }
            ComposerPicker::Level => {
                let variants = self.active_model_variants();
                let selected = self.active_variant();
                let query = self.level_search.trim();
                let mut candidates: Vec<(i64, &str)> = std::iter::once("Default")
                    .chain(variants.iter().map(String::as_str))
                    .filter_map(|label| {
                        if query.is_empty() {
                            Some((0, label))
                        } else {
                            fuzzy_score(query, label).map(|score| (score, label))
                        }
                    })
                    .collect();
                if !query.is_empty() {
                    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                }
                candidates
                    .into_iter()
                    .map(|(_, label)| {
                        (
                            label.to_owned(),
                            None,
                            selected.as_deref().unwrap_or("Default") == label,
                            Message::SelectVariant(if label == "Default" {
                                String::new()
                            } else {
                                label.to_owned()
                            }),
                        )
                    })
                    .collect()
            }
        }
    }

    fn model_picker_row<'a>(
        &self,
        label: &str,
        subtext: Option<String>,
        selected: bool,
        level: bool,
        highlighted: bool,
        selection: Message,
    ) -> Element<'a, Message> {
        let mut labels: Vec<Element<'a, Message>> = vec![
            text(label.to_owned())
                .size(self.em(0.92))
                .font(cosmic::iced::Font {
                    weight: cosmic::iced::font::Weight::Medium,
                    ..cosmic::iced::Font::DEFAULT
                })
                .class(cosmic::theme::Text::Color(if selected && level {
                    palette::current().level_selected_fg
                } else if selected {
                    palette::current().model_active_title
                } else {
                    palette::current().window_fg
                }))
                .into(),
        ];
        if let Some(subtext) = subtext {
            labels.push(
                text(subtext)
                    .size(self.em(0.76))
                    .class(cosmic::theme::Text::Color(palette::current().model_subtext))
                    .into(),
            );
        }
        let mut content: Vec<Element<'a, Message>> = vec![
            column::with_children(labels)
                .spacing(self.pad_px(2.0))
                .width(Length::Fill)
                .into(),
        ];
        if selected {
            content.push(
                text("✓")
                    .size(self.em(0.9))
                    .font(cosmic::iced::Font {
                        weight: cosmic::iced::font::Weight::Bold,
                        ..cosmic::iced::Font::DEFAULT
                    })
                    .class(cosmic::theme::Text::Color(if level {
                        palette::current().level_selected_fg
                    } else {
                        palette::current().model_active_title
                    }))
                    .into(),
            );
        }
        // GTK's `.model-picker-row`: 0.15em/0.3em button padding; the
        // row's child has an 8px gap and 10px/6px margins.
        button::custom(
            row::with_children(content)
                .spacing(self.pad_px(8.0))
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .padding([self.pad_px(6.0), self.pad_px(10.0)]),
        )
        .padding([self.space(0.15), self.space(0.3)])
        .width(Length::Fill)
        .class(model_row_class(
            selected,
            level,
            highlighted,
            self.space(0.44),
        ))
        .on_press(selection)
        .into()
    }

    #[allow(clippy::too_many_arguments)]
    fn composer_picker_menu<'a>(
        &'a self,
        picker: ComposerPicker,
        mut rows: Vec<Element<'a, Message>>,
        placeholder: &'static str,
        empty: &'static str,
        width: f32,
        max_height: f32,
        min_height: f32,
        empty_margin: f32,
    ) -> Element<'a, Message> {
        // GTK's min applies only while the rows are shorter than it; past that
        // the scrolled window takes the list's natural height up to the max.
        let rows_height = rows.len() as f32
            * if picker == ComposerPicker::Model {
                50.0
            } else {
                34.0
            };
        if rows.is_empty() {
            rows.push(
                container(
                    text(empty)
                        .size(self.em(0.88))
                        .class(cosmic::theme::Text::Color(palette::current().model_subtext)),
                )
                .padding([self.pad_px(empty_margin), 0])
                .width(Length::Fill)
                .align_x(Alignment::Center)
                .into(),
            );
        }
        let (query, id, on_input): (&str, _, fn(String) -> Message) = match picker {
            ComposerPicker::Model => (
                &self.model_search,
                model_search_id(),
                Message::ModelSearchInput,
            ),
            ComposerPicker::Level => (
                &self.level_search,
                level_search_id(),
                Message::LevelSearchInput,
            ),
        };
        let search = field_input(placeholder, query, on_input)
            .id(id)
            .leading_icon(inline_icon(icons::search(), self.zoom).into())
            .style(model_search_class(self.space(0.44)))
            .width(Length::Fill);
        // GTK's `model_scroll`/`variant_scroll` propagate the list's natural
        // height (`propagate_natural_height(true)`) between their
        // `min_content_height` and `max_content_height`, so the list is sized
        // by its rows rather than by a per-row constant.
        let list = container(
            scrollable(column::with_children(rows).width(Length::Fill))
                .direction(scrollbar_direction())
                .height(if rows_height < min_height {
                    Length::Fixed(self.space(crate::metrics::px(min_height)))
                } else {
                    Length::Shrink
                })
                .width(Length::Fill),
        )
        .max_height(self.space(crate::metrics::px(max_height)))
        .width(Length::Fill);
        let body = column::with_children(vec![search.into(), list.into()])
            .spacing(self.pad_px(6.0))
            .width(Length::Fixed(self.space(crate::metrics::px(width))));
        let radius = self.space(0.74);
        let shadow_offset = self.space(crate::metrics::px(8.0));
        let shadow_blur = self.space(crate::metrics::px(24.0));
        let frame =
            container(body)
                .padding(self.space(0.44))
                .style(move |_theme: &cosmic::Theme| container::Style {
                    background: Some(palette::current().code_block_bg.into()),
                    border: Border {
                        radius: radius.into(),
                        ..Default::default()
                    },
                    shadow: cosmic::iced::Shadow {
                        color: palette::current().model_popover_shadow,
                        offset: cosmic::iced::Vector::new(0.0, shadow_offset),
                        blur_radius: shadow_blur,
                    },
                    ..Default::default()
                });
        // GTK's top-positioned popover has a -6px anchor offset. The outer
        // transparent bottom margin puts the painted frame that far above it.
        container(frame)
            .padding([0.0, 0.0, self.space(crate::metrics::px(6.0)), 0.0])
            .into()
    }

    /// Opens GTK's file dialog (paperclip) on its own thread; `Tick` collects
    /// the paths, so the UI thread never blocks on the portal.
    fn pick_attachments(&mut self) {
        if self.attachment_picker.is_some() {
            return;
        }
        let (sender, receiver) = async_channel::bounded(1);
        self.attachment_picker = Some(receiver);
        std::thread::spawn(move || {
            let picked = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime.block_on(
                        rfd::AsyncFileDialog::new()
                            .set_title("Attach files")
                            .pick_files(),
                    )
                })
                .map(|files| {
                    files
                        .into_iter()
                        .map(|file| file.path().to_path_buf())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let _ = sender.send_blocking(picked);
        });
    }

    /// The paths the dialog thread handed back, if it finished.
    fn take_picked_attachments(&mut self) -> Option<Vec<PathBuf>> {
        let receiver = self.attachment_picker.as_ref()?;
        match receiver.try_recv() {
            Ok(paths) => {
                self.attachment_picker = None;
                Some(paths)
            }
            Err(async_channel::TryRecvError::Empty) => None,
            Err(async_channel::TryRecvError::Closed) => {
                self.attachment_picker = None;
                None
            }
        }
    }

    /// The modal frame both palettes share.
    /// GTK's `.modal-backdrop`: every app modal sits over a full-window scrim
    /// (rgba(0,0,0,0.4) light / 0.65 dark).
    fn modal_backdrop<'a>(&'a self, content: Element<'a, Message>) -> Element<'a, Message> {
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .style(|_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().modal_backdrop.into()),
                ..Default::default()
            })
            .into()
    }

    /// GTK's `open_app_modal(kind, width, height)` sizes each modal explicitly:
    /// sessions 520, new session and rename 350 x 310.
    fn modal_frame<'a>(
        &'a self,
        width: f32,
        height: f32,
        title: Option<&str>,
        // GTK's `.app-modal-palette.sessions` has its own background and border.
        sessions_card: bool,
        // GTK's `.new-session-palette` has no inset card padding.
        new_session_card: bool,
        body: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let radius = self.space(0.89);
        // GTK's sessions, new-session and rename cards carry no header; only a
        // caller that passes a title gets the heading row and its close button.
        let mut column_items: Vec<Element<'a, Message>> = Vec::new();
        if let Some(title) = title {
            column_items.push(
                row::with_children(vec![
                    text(title.to_string())
                        .size(self.em(1.14))
                        .font(cosmic::iced::Font {
                            weight: cosmic::iced::font::Weight::Bold,
                            ..cosmic::iced::Font::DEFAULT
                        })
                        .width(Length::Fill)
                        .into(),
                    button::icon(icons::close())
                        .on_press(Message::CloseDrawer)
                        .into(),
                ])
                .align_y(Alignment::Center)
                .into(),
            );
        }
        column_items.push(body);
        let padding = if new_session_card {
            0.0
        } else {
            self.space(1.04)
        };
        let shadow_offset = self.space(crate::metrics::px(20.0));
        let shadow_blur = self.space(crate::metrics::px(48.0));
        let frame = container(
            column::with_children(column_items)
                .spacing(self.space(0.89))
                // GTK's `set_size_request` is the card's outer size and iced adds
                // padding outside a fixed width, so the padding comes off here.
                .width(Length::Fixed(width - 2.0 * padding))
                .height(Length::Fixed(height - 2.0 * padding)),
        )
        .padding(padding)
        .style(move |_theme: &cosmic::Theme| container::Style {
            background: Some(if sessions_card {
                palette::current().sessions_bg.into()
            } else if new_session_card {
                palette::current().new_session_bg.into()
            } else {
                palette::current().modal_bg.into()
            }),
            border: Border {
                color: if sessions_card {
                    palette::current().sessions_border
                } else if new_session_card {
                    palette::current().new_session_border
                } else {
                    palette::current().modal_border
                },
                width: 1.0,
                radius: radius.into(),
            },
            shadow: if new_session_card {
                cosmic::iced::Shadow {
                    color: palette::current().new_session_shadow,
                    offset: cosmic::iced::Vector::new(0.0, shadow_offset),
                    blur_radius: shadow_blur,
                }
            } else {
                cosmic::iced::Shadow::default()
            },
            ..Default::default()
        });

        self.modal_backdrop(frame.into())
    }

    /// GTK's session picker: a search field over the session list.
    fn sessions_palette(&self) -> Element<'_, Message> {
        // GTK sizes this modal's height to its content: 410px rendered.
        self.modal_frame(
            self.space(39.0),
            self.space(30.75),
            None,
            true,
            false,
            self.sessions_palette_body(),
        )
    }

    fn sessions_palette_body(&self) -> Element<'_, Message> {
        let mut body_items: Vec<Element<'_, Message>> = Vec::new();

        body_items.push(
            row::with_children(vec![
                inline_icon(icons::search(), self.zoom).into(),
                field_input("Search tabs...", &self.search_query, Message::SearchInput)
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(self.space(0.44))
            .align_y(Alignment::Center)
            .into(),
        );

        // GTK's picker keeps the tabs' order and matches fuzzily; the port
        // sorted by update time and matched substrings.
        let filtered_sessions = filter_tab_sessions(&self.tabs, &self.sessions, &self.search_query);

        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        for session in filtered_sessions {
            let radius = self.space(0.44);
            rows.push(
                container(
                    button::custom(
                        row::with_children(vec![
                            text(session.title.clone())
                                .size(self.em(0.96))
                                .font(cosmic::iced::Font {
                                    weight: cosmic::iced::font::Weight::Bold,
                                    ..cosmic::iced::Font::DEFAULT
                                })
                                .class(cosmic::theme::Text::Color(
                                    palette::current().picker_title_fg,
                                ))
                                .width(Length::Fill)
                                .into(),
                            text(session.directory.clone())
                                .size(self.em(0.84))
                                .class(cosmic::theme::Text::Color(
                                    palette::current().picker_path_fg,
                                ))
                                .into(),
                        ])
                        .align_y(Alignment::Center)
                        .width(Length::Fill),
                    )
                    // GTK's `.session-picker-row`: `0.74em 0.89em`.
                    .padding([self.space(0.74) as u16, self.space(0.89) as u16])
                    .width(Length::Fill)
                    .class(session_picker_row_class(
                        self.active_session_id.as_deref() == Some(session.id.as_str()),
                        radius,
                    ))
                    .on_press(Message::SelectSession(session.id.clone())),
                )
                // GTK: `margin-bottom: 0.44em`.
                .padding([0, 0, self.space(0.44) as u16, 0])
                .into(),
            );
        }

        body_items.push(
            scrollable(
                column::with_children(rows)
                    .spacing(self.space(0.15))
                    .width(Length::Fill),
            )
            .height(Length::Fill)
            .into(),
        );

        // GTK's picker footer: `↑↓ navigate`, `⏎ switch`, `esc close` with
        // key caps, the last one pushed to the right edge.
        body_items.push(
            container(
                row::with_children(vec![
                    keycap("↑↓", self.zoom),
                    muted_hint("navigate", self.zoom),
                    keycap("⏎", self.zoom),
                    muted_hint("switch", self.zoom),
                    fill_spacer(),
                    keycap("esc", self.zoom),
                    muted_hint("close", self.zoom),
                ])
                .spacing(self.space(0.3))
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .into(),
        );

        column::with_children(body_items)
            .spacing(self.space(0.44))
            .height(Length::Fill)
            .into()
    }

    /// GTK's new-session palette: search over the known locations.
    fn new_session_palette(&self) -> Element<'_, Message> {
        let query = self.search_query.clone();
        let mut rows: Vec<Element<'_, Message>> = Vec::new();

        // GTK's picker lists `project_paths`, names each by its last path
        // segment, and orders the active directory first and then by name.
        let active_dir = self
            .active_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .map(|session| session.directory.clone());
        let session_refs: Vec<&model::Session> = self.sessions.values().collect();
        let mut candidates: Vec<(i64, String, String)> = Vec::new();
        for path in project_paths(&self.projects, &session_refs) {
            let name = path
                .rsplit('/')
                .next()
                .filter(|segment| !segment.is_empty())
                .unwrap_or(&path)
                .to_owned();
            if query.trim().is_empty() {
                let priority = if active_dir.as_deref() == Some(path.as_str()) {
                    1000
                } else {
                    0
                };
                candidates.push((priority, name, path));
            } else {
                let name_score = fuzzy_score(&query, &name);
                let path_score = fuzzy_score(&query, &path);
                if let Some(score) = match (name_score, path_score) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                } {
                    candidates.push((score, name, path));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        for (index, (_, name, path)) in candidates.into_iter().enumerate() {
            let radius = self.space(0.44);
            let selected = active_dir.as_deref() == Some(path.as_str())
                || (index == 0 && active_dir.is_none());
            rows.push(
                button::custom(
                    row::with_children(vec![
                        text(name)
                            .size(self.em(0.93))
                            .font(cosmic::iced::Font {
                                weight: cosmic::iced::font::Weight::Bold,
                                ..cosmic::iced::Font::DEFAULT
                            })
                            .class(cosmic::theme::Text::Color(if selected {
                                palette::current().new_session_selected_name_fg
                            } else {
                                palette::current().header_title_text
                            }))
                            .width(Length::Fill)
                            .into(),
                        text(path.clone())
                            .size(self.em(0.81))
                            .class(cosmic::theme::Text::Color(if selected {
                                palette::current().new_session_selected_path_fg
                            } else {
                                palette::current().new_session_path_fg
                            }))
                            .into(),
                    ])
                    .align_y(Alignment::Center)
                    .width(Length::Fill),
                )
                .padding([self.space(0.52) as u16, self.space(0.74) as u16])
                .width(Length::Fill)
                .class(modal_row_class(selected, radius))
                .on_press(Message::CreateSessionIn(path.clone()))
                .into(),
            );
        }

        if rows.is_empty() {
            rows.push(
                // GTK's empty state for the picker.
                text("No matching projects")
                    .size(self.em(0.92))
                    .class(cosmic::theme::Text::Color(palette::current().muted_text))
                    .into(),
            );
        }

        // GTK's `.new-session-search`: full-width top band, rounded only at
        // the top, with its bottom border supplied by a separate 1px hairline.
        let radius = self.space(0.81);
        let search_band = container(
            field_input(
                "Search projects...",
                &self.search_query,
                Message::SearchInput,
            )
            .id(new_session_search_id())
            .style(new_session_search_input_class())
            .size(self.em(0.96))
            .font(cosmic::iced::Font {
                weight: cosmic::iced::font::Weight::Medium,
                ..cosmic::iced::Font::DEFAULT
            })
            .padding(0)
            .width(Length::Fill),
        )
        .padding([self.space(0.89), self.space(1.04)])
        .width(Length::Fill)
        .style(move |_theme: &cosmic::Theme| container::Style {
            background: Some(palette::current().new_session_search_bg.into()),
            border: Border {
                radius: cosmic::iced::border::Radius {
                    top_left: radius,
                    top_right: radius,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        let search = column::with_children(vec![
            search_band.into(),
            hairline(palette::current().new_session_search_border),
        ]);

        let list: Element<'_, Message> = scrollable(
            container(
                column::with_children(
                    rows.into_iter()
                        .map(|row| container(row).padding([self.space(0.07), 0.0]).into())
                        .collect::<Vec<Element<'_, Message>>>(),
                )
                .width(Length::Fill),
            )
            // GTK's `.new-session-list`: 0.3em 0.44em 0.44em.
            .padding([
                self.space(0.3),
                self.space(0.44),
                self.space(0.44),
                self.space(0.44),
            ])
            .width(Length::Fill),
        )
        // GTK's ScrolledWindow has `min-content-height: 264px`; the
        // 310px card leaves at least that much after the search band.
        .height(Length::Fill)
        .width(Length::Fill)
        .into();

        let body = column::with_children(vec![search.into(), list]).height(Length::Fill);

        self.modal_frame(
            self.space(26.25),
            self.space(23.25),
            None,
            false,
            true,
            body.into(),
        )
    }

    /// GTK's rename dialog: the title entry plus the session ID with a copy
    /// button (`.session-id-field`).
    fn rename_palette(&self) -> Element<'_, Message> {
        // GTK reads the row the rename was opened on, not the active one.
        let session_id = self
            .rename_target
            .clone()
            .or_else(|| self.active_session_id.clone())
            .unwrap_or_default();
        let id_radius = self.space(0.44);

        let id_field = container(
            row::with_children(vec![
                text(session_id.clone())
                    .font(cosmic::iced::Font::MONOSPACE)
                    .size(self.em(0.85))
                    .class(cosmic::theme::Text::Color(palette::current().tray_text))
                    .width(Length::Fill)
                    .into(),
                button::icon(icons::copy())
                    .padding([self.pad_px(2.0), self.pad_px(4.0)])
                    .on_press(Message::CopyText(session_id.clone()))
                    .into(),
            ])
            .spacing(self.space(0.3))
            .align_y(Alignment::Center),
        )
        .padding([self.space(0.15) as u16, self.space(0.44) as u16])
        .width(Length::Fill)
        .style(move |_theme: &cosmic::Theme| container::Style {
            background: Some(palette::current().composer_bg.into()),
            border: Border {
                color: palette::current().panel_border,
                width: 1.0,
                radius: id_radius.into(),
            },
            ..Default::default()
        });

        let body_items: Vec<Element<'_, Message>> = vec![
            text("Session title").into(),
            field_input("Session title", &self.rename_input, Message::RenameInput)
                .id(rename_title_id())
                .into(),
            text("Session ID").into(),
            id_field.into(),
            row::with_children(vec![
                fill_spacer(),
                button::text("Cancel")
                    .class(plain_button_class(self.zoom))
                    .on_press(Message::CloseDrawer)
                    .into(),
                button::text("Save")
                    .class(accent_button_class(self.zoom))
                    .on_press(Message::ApplyRename)
                    .into(),
            ])
            .spacing(self.space(0.59))
            .into(),
        ];

        // GTK's rename root: 18px margins and 10px spacing; the actions
        // follow the ID field rather than sitting at the card's bottom.
        let body = container(column::with_children(body_items).spacing(self.pad_px(10.0)))
            // `modal_frame` already supplies 1.04em of the 18px margin.
            .padding((self.space(crate::metrics::px(18.0)) - self.space(1.04)).max(0.0));

        self.modal_frame(
            self.space(26.25),
            self.space(23.25),
            None,
            false,
            false,
            body.into(),
        )
    }

    fn settings_palette(&self) -> Element<'_, Message> {
        let active = self.settings_page;
        let rail_items = [
            (SettingsPage::Connection, icons::connection(), "Connection"),
            (SettingsPage::Sessions, icons::sessions(), "Sessions"),
        ];
        let mut rail_content: Vec<Element<'_, Message>> = vec![
            container(
                text("Settings")
                    .size(self.em(1.15))
                    .font(cosmic::iced::Font {
                        weight: cosmic::iced::font::Weight::Bold,
                        ..cosmic::iced::Font::DEFAULT
                    }),
            )
            .padding([self.pad_px(4.0), 0, self.pad_px(12.0), self.pad_px(8.0)])
            .into(),
        ];
        for (page, icon, label) in rail_items {
            rail_content.push(
                button::custom(
                    row::with_children(vec![
                        inline_icon(icon, self.zoom).into(),
                        text(label)
                            .size(self.em(0.9))
                            .font(cosmic::iced::Font {
                                weight: if page == active {
                                    cosmic::iced::font::Weight::Bold
                                } else {
                                    cosmic::iced::font::Weight::Medium
                                },
                                ..cosmic::iced::Font::DEFAULT
                            })
                            .into(),
                    ])
                    .spacing(self.pad_px(8.0))
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .height(Length::Fixed(self.space(2.0)))
                .padding([self.space(0.25) as u16, self.space(0.6) as u16])
                .class(settings_rail_item_class(page == active, self.space(0.35)))
                .on_press(Message::SettingsPage(page))
                .into(),
            );
        }
        // GTK's `.settings-rail-footer` expands downwards; its host is elided
        // in the middle rather than allowing a long URL to widen the rail.
        let hostname = url::Url::parse(&self.server_url_input)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "remote".to_owned());
        rail_content.push(
            container(
                column::with_children(vec![
                    text(hostname)
                        .size(self.em(0.74))
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::Middle(
                            cosmic::iced::core::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                    text(format!("opencode-cosmic v{}", env!("CARGO_PKG_VERSION")))
                        .size(self.em(0.74))
                        .into(),
                ])
                .spacing(self.pad_px(2.0)),
            )
            .height(Length::Fill)
            .align_y(Alignment::End)
            .padding([
                self.space(0.59),
                self.space(0.59),
                self.space(0.3),
                self.space(0.59),
            ])
            .style(|_theme: &cosmic::Theme| container::Style {
                text_color: Some(palette::current().rail_badge_fg),
                ..Default::default()
            })
            .into(),
        );
        let rail_rule = container(row::with_children(Vec::<Element<'_, Message>>::new()))
            .width(Length::Fixed(1.0))
            .height(Length::Fill)
            .style(|_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().settings_rail_border.into()),
                ..Default::default()
            });
        let rail_body = container(column::with_children(rail_content).spacing(self.space(0.15)))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([self.space(1.0), self.space(0.6)]);
        let rail_radius = self.space(0.81);
        let rail = container(row::with_children(vec![rail_body.into(), rail_rule.into()]))
            // GTK's `.settings-rail`: 12.5em, right border, left rounded corners.
            .width(Length::Fixed(self.space(21.0)))
            .height(Length::Fill)
            .style(move |_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().settings_rail_bg.into()),
                border: Border {
                    radius: cosmic::iced::border::Radius {
                        top_left: rail_radius,
                        bottom_left: rail_radius,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            });

        let page = match active {
            SettingsPage::Connection => self.settings_connection_page(),
            SettingsPage::Sessions => self.sessions_palette_body(),
        };
        let radius = self.space(0.81);
        let frame = container(row::with_children(vec![rail.into(), page]))
            // GTK `show_settings`: 820 × 640 logical pixels at the UI zoom.
            .width(Length::Fixed(self.space(crate::metrics::px(820.0))))
            .height(Length::Fixed(self.space(crate::metrics::px(640.0))))
            .style(move |_theme: &cosmic::Theme| container::Style {
                background: Some(palette::current().modal_bg.into()),
                border: Border {
                    color: palette::current().modal_border,
                    width: 1.0,
                    radius: radius.into(),
                },
                ..Default::default()
            });
        // The dialog fills its overlay; the frame remains centred above GTK's
        // `.modal-backdrop` tint instead of tinting the modal itself.
        self.modal_backdrop(frame.into())
    }

    fn settings_connection_page(&self) -> Element<'_, Message> {
        let heading = text("Server Connection")
            .size(self.em(1.15))
            .font(cosmic::iced::Font {
                weight: cosmic::iced::font::Weight::Bold,
                ..cosmic::iced::Font::DEFAULT
            });
        let muted = |label: &'static str| -> Element<'static, Message> {
            text(label)
                .size(self.em(0.82))
                .class(cosmic::theme::Text::Color(palette::current().muted_text))
                .into()
        };
        let topbar = container(
            row::with_children(vec![
                column::with_children(vec![
                    heading.into(),
                    muted("Configure server endpoint, credentials, and Cloudflare tokens"),
                ])
                .spacing(self.pad_px(2.0))
                .width(Length::Fill)
                .into(),
                muted("Esc to cancel"),
            ])
            .align_y(Alignment::Center),
        )
        // GTK's `.settings-topbar`: 1.33em 1.78em 1.04em.
        .padding([
            self.space(1.33),
            self.space(1.78),
            self.space(1.04),
            self.space(1.78),
        ]);
        let topbar = column::with_children(vec![
            topbar.into(),
            hairline(palette::current().settings_topbar_border),
        ]);

        let password_placeholder = if self.password_stored {
            "Stored in the system keyring"
        } else if self.current_password.is_some() {
            "Leave blank to keep the current password"
        } else {
            "Required by OpenCode 2.x"
        };
        let cloudflare_placeholder = if self.cloudflare_access.is_some() {
            "Stored in the system keyring"
        } else {
            "Optional"
        };
        let body = column::with_children(vec![
            text("OpenCode server URL").into(),
            field_input("https://opencode.example.com", &self.server_url_input, Message::SettingsUrlInput)
                .width(Length::Fill)
                .into(),
            text("Username").into(),
            field_input("", &self.username_input, Message::SettingsUsernameInput)
                .width(Length::Fill)
                .into(),
            text("Password").into(),
            field_input(password_placeholder, &self.password_input, Message::SettingsPasswordInput)
                .password()
                .width(Length::Fill)
                .into(),
            cosmic::widget::checkbox(self.remember_password)
            .label("Remember the password in the system keyring")
            .on_toggle(Message::SettingsRememberPassword)
            .into(),
            text("Remote servers require HTTPS. Loopback HTTP is supported for SSH tunnels. A remembered password is used only for this server URL and username; uncheck Remember to remove it.")
                .size(self.em(0.82))
                .class(cosmic::theme::Text::Color(palette::current().muted_text))
                .width(Length::Fill)
                .into(),
            hairline(palette::current().settings_topbar_border),
            text("Cloudflare Access service token").into(),
            text("Client ID").into(),
            field_input("Optional", &self.cloudflare_client_id_input, Message::SettingsCloudflareClientIdInput)
                .width(Length::Fill)
                .into(),
            text("Client secret").into(),
            field_input(cloudflare_placeholder, &self.cloudflare_client_secret_input, Message::SettingsCloudflareClientSecretInput)
                .password()
                .width(Length::Fill)
                .into(),
            text("The token is sent only to HTTPS servers and stored in the Linux system keyring. Clear the client ID to remove it.")
                .size(self.em(0.82))
                .class(cosmic::theme::Text::Color(palette::current().muted_text))
                .width(Length::Fill)
                .into(),
            text(&self.settings_validation)
                .size(self.em(0.82))
                .class(cosmic::theme::Text::Color(palette::current().connection_status_error))
                .into(),
        ])
        .spacing(self.space(crate::metrics::px(10.0)))
        .width(Length::Fill);
        let body = scrollable(
            container(body)
                .padding([self.pad_px(16.0), self.pad_px(24.0)])
                .width(Length::Fill),
        )
        .height(Length::Fill)
        .width(Length::Fill);
        let bottom = row::with_children(vec![
            text("Secrets stay out of the state file")
                .size(self.em(0.82))
                .class(cosmic::theme::Text::Color(palette::current().muted_text))
                .width(Length::Fill)
                .into(),
            button::text("Cancel").on_press(Message::CloseDrawer).into(),
            button::text("Apply")
                .class(accent_button_class(self.zoom))
                .on_press(Message::ApplySettings)
                .into(),
        ])
        .spacing(self.space(0.74))
        .align_y(Alignment::Center);
        let bottom = column::with_children(vec![
            hairline(palette::current().settings_topbar_border),
            container(bottom)
                .padding([self.space(0.89), self.space(1.78)])
                .into(),
        ]);
        column::with_children(vec![topbar.into(), body.into(), bottom.into()])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// GTK's compact transcript status pill (`.transcript-status-compact`):
    /// the working or retry state below the transcript, not a card inside it.
    fn status_pill(&self, active_id: &str) -> Option<Element<'_, Message>> {
        let status = self.statuses.get(active_id);
        if !status.is_some_and(RunStatus::is_busy) {
            return None;
        }
        let retry = match status {
            Some(RunStatus::Retry { message, .. }) => Some(message.clone()),
            _ => None,
        };
        let color = if retry.is_some() {
            palette::current().status_busy
        } else {
            palette::current().status_pill_text
        };
        // GTK's `TranscriptIndicator::Working => "OpenCode is working"`
        // (ui.rs:6112); the port said "Working…".
        let label = retry.unwrap_or_else(|| "OpenCode is working".to_string());

        let pill = container(
            row::with_children(vec![
                inline_icon(icons::settings(), self.zoom)
                    .size(self.em(0.92) as u16)
                    .into(),
                text(label)
                    .size(self.em(0.96))
                    .class(cosmic::theme::Text::Color(color))
                    .into(),
                // GTK's transcript indicator is spinner plus label only: the
                // stop control lives in the composer.
            ])
            .spacing(self.space(0.59))
            .align_y(Alignment::Center),
        )
        .padding([self.space(0.52) as u16, self.space(0.89) as u16])
        .style(|_theme: &cosmic::Theme| container::Style {
            background: Some(palette::current().status_pill_bg.into()),
            border: Border {
                color: palette::current().status_pill_border,
                width: 1.0,
                radius: 999.0.into(),
            },
            ..Default::default()
        });

        Some(
            container(pill)
                .padding([
                    self.space(0.59) as u16,
                    self.space(2.07) as u16,
                    self.space(0.74) as u16,
                    self.space(2.07) as u16,
                ])
                .into(),
        )
    }

    /// Moves one step along [`ZOOM_STEPS`] and persists the result.
    fn zoom_step(&mut self, direction: i32) {
        self.set_zoom(next_zoom(self.zoom, direction));
    }

    fn set_zoom(&mut self, zoom: f32) {
        if (zoom - self.zoom).abs() < 0.001 {
            return;
        }
        self.zoom = zoom;
        self.state.zoom_level = f64::from(zoom);
        let _ = self.state.save(&default_path());
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
    }

    fn connect_api(&mut self) {
        let config = ApiConfig {
            base_url: self.server_url_input.clone(),
            username: self.username_input.clone(),
            password: self.current_password.clone(),
            cloudflare_access: self.cloudflare_access.clone(),
        };

        match ApiHandle::start(config) {
            Ok((handle, receiver, _server_key)) => {
                self.api = Some(handle.clone());
                self.receiver = Some(receiver);
                self.connection_status = "Connecting".to_string();
                self.connection_status_error = false;

                handle.send(Command::Bootstrap {
                    sessions: self.tabs.clone(),
                    directories: self
                        .projects
                        .iter()
                        .map(|project| project.worktree.clone())
                        .collect(),
                });
            }
            Err(e) => {
                self.error_banner = Some(format!("Failed to connect: {e}"));
                self.connection_status = format!("Failed to connect: {e}");
                self.connection_status_error = true;
            }
        }
    }

    fn drain_events(&mut self) {
        // Any answer settles the previous tray request; GTK re-enabled Resume
        // when the row's request came back.
        self.tray_in_flight = false;
        let mut events = Vec::new();
        if let Some(mock) = &mut self.mock_server {
            events = mock.take_server_events();
        } else if let Some(receiver) = &self.receiver {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            self.handle_ui_event(event);
        }
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::Connection { connected, error } => {
                self.connection_status_error = !connected;
                if connected {
                    self.connection_status = "Connected".to_string();
                } else {
                    self.connection_status = error.unwrap_or_else(|| {
                        "Disconnected; reconnecting in the background".to_string()
                    });
                }
            }
            UiEvent::Bootstrap(Ok(bootstrap)) => {
                // GTK's connection-status label: `Connected · <version>`, with
                // the reason appended (and the error colour) when the refresh
                // came back partial.
                if bootstrap.warnings.is_empty() {
                    self.connection_status = format!("Connected · {}", bootstrap.version);
                    self.connection_status_error = false;
                } else {
                    self.connection_status =
                        format!("Connected · {} · Partial refresh", bootstrap.version);
                    self.connection_status_error = true;
                }
                for session in &bootstrap.sessions {
                    self.sessions.insert(session.id.clone(), session.clone());
                }

                self.projects = bootstrap.projects.clone();
                self.replace_pending(&bootstrap.pending);

                for (id, st) in &bootstrap.statuses {
                    if st.is_busy() {
                        self.statuses.insert(id.clone(), RunStatus::Busy);
                    } else {
                        self.statuses.insert(id.clone(), RunStatus::Idle);
                    }
                }

                let active_statuses: HashSet<String> = bootstrap
                    .statuses
                    .iter()
                    .filter(|(_, st)| st.is_busy())
                    .map(|(id, _)| id.clone())
                    .collect();
                self.with_jobs(|jobs, ctx| {
                    jobs.apply_snapshot(Some(&active_statuses), bootstrap.shells, ctx);
                });
                // GTK fetched the child sessions the job rows name.
                if let Some(command) = self.job_info_command() {
                    self.send_command(command);
                }

                let saved = self.state.servers.get(&self.state.connection.server);
                if self.tabs.is_empty() {
                    let known: std::collections::HashSet<String> =
                        self.sessions.keys().cloned().collect();
                    self.tabs = restore_tabs(saved, &known);
                    if let Some(unread) = saved.map(|state| state.unread.clone()) {
                        self.unread = unread;
                        self.unread.retain(|id| self.tabs.contains(id));
                    }
                }
                if self.tabs.is_empty() {
                    let mut roots: Vec<_> = self
                        .sessions
                        .values()
                        .filter(|s| s.parent_id.is_none())
                        .collect();
                    roots.sort_by_key(|s| std::cmp::Reverse(s.time.updated));
                    for r in roots.iter().take(5) {
                        self.tabs.push(r.id.clone());
                    }
                }

                if self.active_session_id.is_none() {
                    // GTK reopened the tab that was active, when it still exists.
                    self.active_session_id = saved
                        .and_then(|state| state.active.clone())
                        .filter(|id| self.tabs.contains(id))
                        .or_else(|| self.tabs.first().cloned());
                    self.focus_composer = self.active_session_id.is_some();
                }
                self.persist_tabs();

                if let Some(active_id) = &self.active_session_id {
                    let dir = self
                        .sessions
                        .get(active_id)
                        .map(|s| s.directory.clone())
                        .unwrap_or_else(|| "/repo".to_string());

                    if let Some(api) = &self.api {
                        api.send(Command::LoadMessages {
                            session_id: active_id.clone(),
                            cursor: None,
                        });
                        api.send(Command::LoadModels { directory: dir });
                    }
                }
            }
            UiEvent::SessionInfoLoaded(results) => {
                // GTK's job rows: the sessions the list asked for, then the
                // next batch (a chain of parents still missing).
                self.jobs.apply_session_info(results);
                if let Some(command) = self.job_info_command() {
                    self.send_command(command);
                }
            }
            UiEvent::Bootstrap(Err(err)) => {
                self.error_banner = Some(format!("Bootstrap failed: {err}"));
            }
            UiEvent::MessagesLoaded {
                session_id,
                cursor,
                result: Ok(page),
            } => {
                self.history_loading = false;
                let conv = self.conversations.entry(session_id).or_default();
                if cursor.is_none() {
                    conv.replace_from_api(&page.messages, page.next_cursor);
                    // The page carries the session's inbox: without it a
                    // parked session (queued before this client looked) shows
                    // an empty tray until some live event happens.
                    if let Some(inbox) = &page.queued {
                        conv.sync_queued(inbox);
                    }
                } else {
                    conv.prepend_from_api(&page.messages, page.next_cursor);
                }
            }
            UiEvent::PendingLoaded(snapshot) => {
                self.replace_pending(&snapshot.requests);
                if let Some(warning) = snapshot.warnings.first() {
                    self.error_banner = Some(warning.clone());
                }
            }
            UiEvent::ModelsLoaded {
                directory,
                result: Ok(catalog),
            } => {
                self.catalogs.insert(directory, catalog);
            }
            UiEvent::ServerEvent(envelope) => {
                if let Some((sid, status)) = model::event_run_status(&envelope.payload) {
                    // GTK marked a tab unread when its run went idle while the
                    // user was looking elsewhere.
                    let ran = self
                        .statuses
                        .get(&sid)
                        .is_some_and(|previous| previous.is_busy());
                    if ran
                        && !status.is_busy()
                        && self.active_session_id.as_deref() != Some(sid.as_str())
                        && self.tabs.iter().any(|tab| tab.as_str() == sid)
                    {
                        self.unread.insert(sid.to_string());
                    }
                    self.statuses.insert(sid.to_string(), status);
                }

                if let Ok(event) = protocol::Event::deserialize(&envelope.payload) {
                    let kind = protocol::decode_event(&event);

                    if let Some(sid) = kind.session_id() {
                        let conv = self.conversations.entry(sid.to_string()).or_default();
                        conv.apply(&event, &kind);
                    }

                    match crate::pending::pending_change(&kind, envelope.directory.as_deref()) {
                        Some(crate::pending::PendingChange::Permission { directory, request }) => {
                            self.absorb_pending(crate::pending::PendingRequest::Permission {
                                directory: directory.unwrap_or_default(),
                                request,
                            });
                        }
                        Some(crate::pending::PendingChange::Form(form)) => {
                            self.absorb_pending(crate::pending::PendingRequest::Form(form));
                        }
                        Some(crate::pending::PendingChange::Resolved(id)) => {
                            self.resolve_pending(&id)
                        }
                        None => {}
                    }

                    if let Some(job_evt) =
                        jobs::job_event(&event, &kind, envelope.directory.as_deref())
                    {
                        let roots: Vec<Session> = self.sessions.values().cloned().collect();
                        let ctx = jobs::Context {
                            roots: &roots,
                            directories: &[],
                        };
                        self.jobs.apply_event(job_evt, &ctx);
                    }
                }
            }
            UiEvent::SessionCreated {
                result: Ok(session),
                ..
            } => {
                let id = session.id.clone();
                let dir = session.directory.clone();
                self.sessions.insert(id.clone(), session);
                self.tabs.push(id.clone());
                self.active_session_id = Some(id.clone());
                self.focus_composer = true;

                if let Some(api) = &self.api {
                    api.send(Command::LoadMessages {
                        session_id: id,
                        cursor: None,
                    });
                    api.send(Command::LoadModels { directory: dir });
                }
            }
            UiEvent::PromptAccepted {
                session_id,
                result: Err(e),
                ..
            } => {
                self.error_banner = Some(format!("Prompt rejected for {session_id}: {e}"));
            }
            UiEvent::Aborted { session_id, .. } => {
                self.statuses.insert(session_id.clone(), RunStatus::Idle);
                if let Some(conv) = self.conversations.get_mut(&session_id) {
                    conv.sync_queued(&[]);
                }
            }
            UiEvent::SessionRenamed {
                session_id,
                result: Ok(s),
                ..
            } => {
                self.sessions.insert(session_id, s);
            }
            _ => {}
        }
    }

    /// GTK saved the open tabs, their order, the active one and the unread
    /// markers per server, so the next start reopens the same sessions.
    fn persist_tabs(&mut self) {
        // The preview drives a mock server: never write its tabs into the real
        // state file.
        if self.mock_server.is_some() {
            return;
        }
        let key = self.state.connection.server.clone();
        if key.is_empty() {
            return;
        }
        let tabs: Vec<crate::persist::PersistedTab> = self
            .tabs
            .iter()
            .map(|id| {
                let session = self.sessions.get(id);
                crate::persist::PersistedTab {
                    id: id.clone(),
                    directory: session
                        .map(|session| session.directory.clone())
                        .unwrap_or_default(),
                    title: session
                        .map(|session| session.title.clone())
                        .unwrap_or_default(),
                }
            })
            .collect();
        let unread: std::collections::HashSet<String> = self.unread.clone();
        let active = self.active_session_id.clone();
        let entry = self.state.servers.entry(key).or_default();
        entry.tabs = tabs;
        entry.active = active;
        entry.unread = unread;
        let _ = self.state.save(&default_path());
    }

    fn set_active_session(&mut self, id: &str) {
        self.active_session_id = Some(id.to_string());
        self.focus_composer = true;
        // GTK builds the transcript for a session it has just shown, so its
        // scrolled window starts with a fresh adjustment at the top - following
        // is for a run in the session the reader is already on.
        self.transcript_follow = false;
        self.transcript_scroll_top = true;
        self.unread.remove(id);
        self.persist_tabs();
        if !self.conversations.contains_key(id) {
            if let Some(api) = &self.api {
                api.send(Command::LoadMessages {
                    session_id: id.to_string(),
                    cursor: None,
                });
            } else if let Some(mock) = &mut self.mock_server {
                let event = mock.handle(Command::LoadMessages {
                    session_id: id.to_string(),
                    cursor: None,
                });
                self.handle_ui_event(event);
            }
        }
    }

    fn close_tab(&mut self, id: &str) {
        self.tabs.retain(|t| t != id);
        if self.active_session_id.as_deref() == Some(id) {
            self.active_session_id = self.tabs.first().cloned();
        }
        self.persist_tabs();
    }

    /// Moves the active tab by `delta` positions, wrapping around.
    fn cycle_tab(&mut self, delta: i32) {
        if self.tabs.len() < 2 {
            return;
        }
        let current = self
            .active_session_id
            .as_ref()
            .and_then(|id| self.tabs.iter().position(|tab| tab == id))
            .unwrap_or(0);
        let len = self.tabs.len() as i32;
        let next = (current as i32 + delta).rem_euclid(len) as usize;
        let id = self.tabs[next].clone();
        self.set_active_session(&id);
    }

    fn open_session(&mut self, id: &str) {
        if !self.tabs.contains(&id.to_string()) {
            self.tabs.push(id.to_string());
        }
        self.set_active_session(id);
    }

    /// GTK's new-session palette: create in the first location that matches
    /// the palette's search, so `Ctrl+T` then `Enter` still makes a session.
    fn confirm_new_session(&mut self) {
        let query = self.search_query.to_lowercase();
        let directory = self
            .filtered_projects(&query)
            .first()
            .map(|project| project.worktree.clone());
        if let Some(directory) = directory {
            self.search_query.clear();
            self.active_drawer = None;
            self.create_session(&directory);
        }
    }

    /// The locations matching a search over name and path, in list order.
    fn filtered_projects(&self, query: &str) -> Vec<&model::Project> {
        self.projects
            .iter()
            .filter(|project| {
                query.is_empty()
                    || project.worktree.to_lowercase().contains(query)
                    || project
                        .name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(query))
            })
            .collect()
    }

    fn active_session_title(&self) -> String {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .map(|session| session.title.clone())
            .unwrap_or_default()
    }

    fn apply_rename(&mut self) {
        let Some(session_id) = self
            .rename_target
            .clone()
            .or_else(|| self.active_session_id.clone())
        else {
            return;
        };
        let title = self.rename_input.trim().to_string();
        if title.is_empty() {
            return;
        }
        let req_id = self.next_request_id();
        if let Some(api) = &self.api {
            api.send(Command::RenameSession {
                request_id: req_id,
                session_id,
                title,
            });
        }
        self.active_drawer = None;
        self.rename_target = None;
    }

    fn create_session(&mut self, directory: &str) {
        let req_id = self.next_request_id();
        if let Some(api) = &self.api {
            api.send(Command::CreateSession {
                request_id: req_id,
                directory: directory.to_string(),
                title: None,
            });
        } else if let Some(mock) = &mut self.mock_server {
            let event = mock.handle(Command::CreateSession {
                request_id: req_id,
                directory: directory.to_string(),
                title: Some("New Preview Session".to_string()),
            });
            self.handle_ui_event(event);
        }
    }

    fn send_composer_prompt(&mut self, mode: SendMode) {
        let text = self.composer_text.trim();
        if text.is_empty() {
            return;
        }

        let Some(active_id) = self.active_session_id.clone() else {
            return;
        };

        let prompt = std::mem::take(&mut self.composer_text);
        self.composer_editor = cosmic::widget::text_editor::Content::new();
        let attachments = std::mem::take(&mut self.pending_attachments);
        let req_id = self.next_request_id();
        let msg_id = format!("msg_{}", req_id);

        if let Some(api) = &self.api {
            api.send(Command::SendPrompt {
                request_id: req_id,
                message_id: msg_id,
                session_id: active_id,
                text: prompt,
                attachments: attachments.clone(),
                delivery: mode.delivery(),
            });
        } else if let Some(mock) = &mut self.mock_server {
            let event = mock.handle(Command::SendPrompt {
                request_id: req_id,
                message_id: msg_id,
                session_id: active_id,
                text: prompt,
                attachments,
                delivery: mode.delivery(),
            });
            self.handle_ui_event(event);
        }
    }

    fn stop_active_session(&mut self) {
        let Some(active_id) = self.active_session_id.clone() else {
            return;
        };

        if let Some(api) = &self.api {
            api.send(Command::Abort {
                session_id: active_id,
            });
        } else if let Some(mock) = &mut self.mock_server {
            let event = mock.handle(Command::Abort {
                session_id: active_id,
            });
            self.handle_ui_event(event);
        }
    }

    /// Loads the next older page when the transcript reaches its top.
    fn load_older_history(&mut self) {
        // A little slack so the next page is there when the user arrives,
        // instead of after a visible pause.
        if self.transcript_offset > 24.0 || self.history_loading {
            return;
        }
        let Some(active_id) = self.active_session_id.clone() else {
            return;
        };
        let Some(cursor) = self
            .conversations
            .get(&active_id)
            .and_then(|conversation| conversation.next_cursor.clone())
        else {
            return;
        };
        if let Some(api) = &self.api {
            api.send(Command::LoadMessages {
                session_id: active_id,
                cursor: Some(cursor),
            });
            self.history_loading = true;
        } else if let Some(mock) = &mut self.mock_server {
            let event = mock.handle(Command::LoadMessages {
                session_id: active_id,
                cursor: Some(cursor),
            });
            self.handle_ui_event(event);
        }
    }

    /// The current turn's request, for GTK's sticky prompt: pinned once its own
    /// row has left the top of the transcript (the answer started), or while
    /// the reader has scrolled back up through a long one.
    fn sticky_prompt(&self) -> Option<(String, String, u64)> {
        let conversation = self
            .active_session_id
            .as_ref()
            .and_then(|id| self.conversations.get(id))?;
        let index = conversation
            .messages
            .iter()
            .rposition(|message| message.role == model::Role::User && !message.in_tray())?;
        let message = &conversation.messages[index];
        let text = message
            .segments()
            .iter()
            .filter(|segment| {
                matches!(
                    segment.kind,
                    model::SegmentKind::Text | model::SegmentKind::File
                )
            })
            .map(|segment| segment.text.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.trim().is_empty() {
            return None;
        }
        // GTK's `sticky_user_index` pins the row only once its realized box has
        // passed *entirely* above the viewport, so a row that is still partly
        // visible keeps its own tinted band. The port does not track realized
        // row boxes, so the height comes from the same metrics the row renders
        // with: 1.33em/1.48em padding, the 0.76em role line, the 6px gap and
        // the body's lines at 0.96em.
        let ratio = crate::metrics::GTK_LINE_HEIGHT_RATIO;
        let body_lines = text.lines().count().max(1) as f32;
        let row_height = self.space(1.33 + 1.48)
            + 6.0 * self.zoom
            + self.em(0.76) as f32 * ratio
            + body_lines * (self.em(0.96) as f32 * ratio);
        if self.transcript_offset <= row_height {
            return None;
        }
        let has_answer = conversation
            .messages
            .iter()
            .skip(index + 1)
            .any(|message| message.role == model::Role::Assistant);
        if !(has_answer && self.transcript_offset > 8.0) && self.transcript_remaining <= 40.0 {
            return None;
        }
        Some((message.id.clone(), text, message.created))
    }

    /// Adds a pending request, replacing a known one of the same id.
    fn absorb_pending(&mut self, request: crate::pending::PendingRequest) {
        let id = request.id().to_string();
        match request {
            crate::pending::PendingRequest::Permission { .. } => {
                if self.permissions.iter().all(|known| known.id() != id) {
                    self.permissions.push(request);
                }
            }
            crate::pending::PendingRequest::Form(form) => self.forms.upsert(form),
        }
    }

    /// Drops a request the server settled (a reply, an answer, a cancel).
    fn resolve_pending(&mut self, id: &str) {
        self.permissions.retain(|known| known.id() != id);
        self.forms.remove(id);
    }

    /// Replaces the whole set (bootstrap and reconciliations).
    fn replace_pending(&mut self, requests: &[crate::pending::PendingRequest]) {
        self.permissions.clear();
        self.forms.clear();
        for request in requests {
            self.absorb_pending(request.clone());
        }
    }

    /// Session -> parent, for the form visibility rules.
    fn session_parents(&self) -> HashMap<String, String> {
        self.sessions
            .iter()
            .filter_map(|(id, session)| {
                session
                    .parent_id
                    .as_ref()
                    .map(|parent| (id.clone(), parent.clone()))
            })
            .collect()
    }

    /// GTK's `open_web_ui`: the server's web UI in the default browser.
    fn open_web_ui(&mut self) {
        match crate::api::web_ui_url(&self.state.connection.server) {
            Ok(uri) => {
                if let Err(error) = std::process::Command::new("xdg-open").arg(&uri).spawn() {
                    self.error_banner = Some(format!("Could not open {uri}: {error}"));
                }
            }
            Err(error) => {
                self.error_banner = Some(format!("Could not open the web UI: {error:#}"));
            }
        }
    }

    /// GTK's form notice (`.form-notice`), which `Ctrl+Shift+X` cancels.
    fn form_notice(&self) -> Option<crate::pending::FormNotice> {
        self.forms
            .notice(self.active_session_id.as_deref(), &self.session_parents())
    }

    /// Permission prompts whose scope is the active session: its own, or a
    /// child's whose parent is one of the root sessions (`pending`'s rule), so
    /// a prompt that blocks a running subagent shows where it belongs.
    fn visible_permissions(&self) -> Vec<&protocol::PermissionRequest> {
        let Some(active) = self.active_session_id.as_deref() else {
            return Vec::new();
        };
        let parents = self.session_parents();
        let is_root = |id: &str| {
            self.sessions
                .get(id)
                .is_none_or(|session| session.parent_id.is_none())
        };
        self.permissions
            .iter()
            .filter_map(|request| match request {
                crate::pending::PendingRequest::Permission { request, .. } => Some(request),
                crate::pending::PendingRequest::Form(_) => None,
            })
            .filter(|request| {
                crate::pending::permission_scope(&request.session_id, is_root, &parents)
                    .is_none_or(|scope| scope == active)
            })
            .collect()
    }

    /// GTK's `permission_context`: who asked, in which directory, and how.
    fn permission_context(&self, request: &protocol::PermissionRequest) -> String {
        let session = self.sessions.get(&request.session_id);
        let requester = match session {
            Some(session) => session.title.clone(),
            None => match self
                .session_parents()
                .get(&request.session_id)
                .and_then(|parent| self.sessions.get(parent))
            {
                Some(parent) => format!("a subagent of {}", parent.title),
                None => format!("session {}", request.session_id),
            },
        };
        let directory = session.map(|s| s.directory.clone()).unwrap_or_default();
        let mut context = format!("Requested by {requester}\n{directory}");
        if let Some(source) = crate::pending::source_text(request) {
            context.push_str(&format!("\n{source}"));
        }
        context
    }

    /// GTK's blocking prompt, which replaces the composer: `composer_stack`
    /// holds `composer_frame` and `prompt_frame`, and the prompt frame is the
    /// visible one while a permission for the active session is open. So this
    /// renders in the composer's slot, not in the transcript.
    fn composer_prompt(&self) -> Option<Element<'_, Message>> {
        let requests = self.visible_permissions();
        if requests.is_empty() {
            return None;
        }
        let palette_now = palette::current();
        let radius = self.space(0.89);
        let mut cards: Vec<Element<'_, Message>> = Vec::new();
        for request in requests {
            let action = if request.action.trim().is_empty() {
                "tool action".to_owned()
            } else {
                request.action.clone()
            };
            let mut items: Vec<Element<'_, Message>> = vec![
                // GTK's `.prompt-heading`: 1.2em, 700, no colour of its own,
                // so it inherits the window foreground.
                text(format!("Allow {action}?"))
                    .size(self.em(1.2))
                    .font(cosmic::iced::Font {
                        weight: cosmic::iced::font::Weight::Bold,
                        ..cosmic::iced::Font::DEFAULT
                    })
                    .class(cosmic::theme::Text::Color(palette_now.window_fg))
                    .into(),
                // GTK's `.session-picker-path`: 0.84em in the picker path tone.
                text(self.permission_context(request))
                    .size(self.em(0.84))
                    .class(cosmic::theme::Text::Color(palette_now.picker_path_fg))
                    .into(),
            ];
            let mut details: Vec<Element<'_, Message>> = Vec::new();
            if let Some(message) = request
                .message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
            {
                details.push(text(message.to_owned()).size(self.em(0.9)).into());
            }
            // GTK's `.prompt-detail`: a monospace band carrying the command,
            // `padding: 0.74em 0.89em`, `border-radius: 0.52em` and its own
            // surface (`@oc_bg_prompt_detail`).
            if !request.resources.is_empty() {
                details.push(prompt_band(request.resources.join("\n"), self.zoom));
            }
            if let Some(metadata) = crate::pending::metadata_text(request.metadata.as_ref()) {
                details.push(prompt_band(metadata, self.zoom));
            }
            if let Some(patterns) = crate::pending::always_patterns(request) {
                details.push(
                    text("Always allow would remember:")
                        .size(self.em(0.9))
                        .font(cosmic::iced::Font::MONOSPACE)
                        .class(cosmic::theme::Text::Color(palette_now.prompt_subheading))
                        .into(),
                );
                details.push(prompt_band(patterns, self.zoom));
            }
            if !details.is_empty() {
                // GTK wraps the details in a `ScrolledWindow` with
                // `min_content_height(80)` and `max_content_height(320)`.
                // Inside the prompt frame - which takes the composer's slot -
                // that renders at the composer's own height (the 72px input
                // minimum plus the footer) and clips the rest, so the details
                // box is bounded the same way here rather than growing with
                // its content.
                items.push(
                    container(
                        scrollable(column::with_children(details).spacing(self.space(0.44)))
                            .height(Length::Shrink),
                    )
                    .height(Length::Fixed(self.space(crate::metrics::px(114.0))))
                    .width(Length::Fill)
                    .into(),
                );
            }
            // GTK: right-aligned Deny / Allow once (`suggested-action`) /
            // Always allow. No reply is ever the default, so a stray Enter or
            // Space while typing can never answer a prompt.
            let mut actions: Vec<Element<'_, Message>> = vec![
                fill_spacer(),
                tray_text_button(
                    "Deny",
                    self.zoom,
                    Some(Message::ReplyPermission {
                        request_id: request.id.clone(),
                        session_id: request.session_id.clone(),
                        decision: protocol::PermissionDecision::Reject,
                    }),
                ),
            ];
            actions.push(
                button::custom(
                    text("Allow once")
                        .size(self.em(0.96))
                        .class(cosmic::theme::Text::Color(palette_now.accent_fg)),
                )
                .padding([self.space(0.3) as u16, self.space(0.85) as u16])
                .class(resume_button_class(self.space(0.59)))
                .on_press(Message::ReplyPermission {
                    request_id: request.id.clone(),
                    session_id: request.session_id.clone(),
                    decision: protocol::PermissionDecision::Once,
                })
                .into(),
            );
            if request.offers_always() {
                actions.push(tray_text_button(
                    "Always allow",
                    self.zoom,
                    Some(Message::ReplyPermission {
                        request_id: request.id.clone(),
                        session_id: request.session_id.clone(),
                        decision: protocol::PermissionDecision::Always,
                    }),
                ));
            }
            items.push(
                row::with_children(actions)
                    .spacing(self.space(0.59))
                    .align_y(Alignment::Center)
                    .into(),
            );
            cards.push(
                container(column::with_children(items).spacing(self.space(0.44)))
                    // GTK's `.composer-prompt`: 1.04em padding, 0.89em radius,
                    // the new-session palette's surface.
                    .padding([self.space(1.04) as u16, self.space(1.04) as u16])
                    .width(Length::Fill)
                    .style(move |_theme: &cosmic::Theme| container::Style {
                        background: Some(palette::current().new_session_bg.into()),
                        border: Border {
                            color: palette::current().new_session_border,
                            width: 1.0,
                            radius: radius.into(),
                        },
                        ..Default::default()
                    })
                    .into(),
            );
        }
        Some(
            column::with_children(cards)
                .spacing(self.space(0.59))
                .into(),
        )
    }

    /// The active session's waiting prompts as the tray engine's rows.
    fn tray_rows(&self) -> Vec<crate::tray::TrayRow> {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.conversations.get(id))
            .map(|conversation| {
                conversation
                    .tray_items()
                    .into_iter()
                    .map(|item| crate::tray::TrayRow {
                        id: item.id,
                        delivery: item.delivery,
                        summary: item.text,
                        sending: false,
                        in_flight: false,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// GTK's Resume: wakes a parked session for every waiting message.
    fn resume_tray(&mut self) {
        if self.tray_in_flight {
            return;
        }
        let Some(active_id) = self.active_session_id.clone() else {
            return;
        };
        let Some((inbox_id, request)) = crate::tray::resume_request(&self.tray_rows()) else {
            return;
        };
        if let Some(api) = &self.api {
            api.send(Command::Inbox {
                session_id: active_id,
                inbox_id,
                request,
            });
            self.tray_in_flight = true;
        }
    }

    fn handle_tray_action(&mut self, item_id: &str, action: RowAction) {
        let Some(active_id) = self.active_session_id.clone() else {
            return;
        };

        let current_delivery = self
            .conversations
            .get(&active_id)
            .and_then(|c| c.tray_items().into_iter().find(|t| t.id == item_id))
            .map(|t| t.delivery)
            .unwrap_or(protocol::Delivery::Steer);

        let req = match action {
            RowAction::Switch => {
                let new_del = match current_delivery {
                    protocol::Delivery::Steer => protocol::Delivery::Queue,
                    protocol::Delivery::Queue => protocol::Delivery::Steer,
                    _ => protocol::Delivery::Steer,
                };
                crate::api::InboxRequest::SetDelivery(new_del)
            }
            RowAction::Cancel => crate::api::InboxRequest::Cancel,
        };

        if let Some(api) = &self.api {
            api.send(Command::Inbox {
                session_id: active_id,
                inbox_id: item_id.to_string(),
                request: req,
            });
        } else if let Some(mock) = &mut self.mock_server {
            let event = mock.handle(Command::Inbox {
                session_id: active_id,
                inbox_id: item_id.to_string(),
                request: req,
            });
            self.handle_ui_event(event);
        }
    }

    fn clear_tray(&mut self) {
        let Some(active_id) = &self.active_session_id else {
            return;
        };
        let Some(conv) = self.conversations.get(active_id) else {
            return;
        };

        let items = conv.tray_items();
        for item in items {
            self.handle_tray_action(&item.id, RowAction::Cancel);
        }
    }

    fn switch_model(&mut self, model_id: &str) {
        self.send_model_selection(model_id, None);
    }

    fn switch_variant(&mut self, variant: &str) {
        let Some(model_id) = self.active_session_model_id() else {
            return;
        };
        self.send_model_selection(&model_id, variant_selection(variant));
    }

    fn send_model_selection(&mut self, model_id: &str, variant: Option<String>) {
        let Some(active_id) = self.active_session_id.clone() else {
            return;
        };

        // The provider ID comes from the catalog; only fall back to the
        // historical default when the model is not in it.
        let directory = self
            .sessions
            .get(&active_id)
            .map(|s| s.directory.clone())
            .unwrap_or_default();
        let provider_id = self
            .catalogs
            .get(&directory)
            .and_then(|catalog| {
                catalog
                    .models
                    .iter()
                    .find(|m| m.model_id == model_id)
                    .map(|m| m.provider_id.clone())
            })
            .unwrap_or_else(|| "anthropic".to_string());

        let req_id = self.next_request_id();
        if let Some(api) = &self.api {
            api.send(Command::SelectModel {
                request_id: req_id,
                session_id: active_id,
                model: protocol::ModelRef {
                    id: model_id.to_string(),
                    provider_id,
                    variant,
                },
            });
        }
    }

    fn reconnect_with_settings(&mut self) {
        let server = self.server_url_input.trim().to_owned();
        let username = self.username_input.trim().to_owned();
        if server.is_empty() || username.is_empty() {
            self.settings_validation = "Server URL and username are required".to_owned();
            return;
        }
        let password_plan = credentials::plan_password(
            &SystemKeyring,
            PasswordTarget {
                server: &self.state.connection.server,
                username: &self.state.connection.username,
            },
            self.current_password.as_deref(),
            self.password_stored,
            PasswordTarget {
                server: &server,
                username: &username,
            },
            &self.password_input,
            self.remember_password,
        );
        let client_id = self.cloudflare_client_id_input.trim();
        let secret = self.cloudflare_client_secret_input.trim();
        let cloudflare_access = if client_id.is_empty() && secret.is_empty() {
            None
        } else if secret.is_empty() {
            match self
                .cloudflare_access
                .as_ref()
                .filter(|access| access.client_id == client_id)
            {
                Some(access) => Some(access.clone()),
                None => {
                    self.settings_validation =
                        "Cloudflare Access client secret is required".to_owned();
                    return;
                }
            }
        } else {
            match CloudflareAccessCredentials::new(client_id.to_owned(), secret.to_owned()) {
                Ok(access) => Some(access),
                Err(error) => {
                    self.settings_validation = error.to_string();
                    return;
                }
            }
        };
        if let Some(access) = &cloudflare_access {
            if let Err(error) = credentials::save(&server, access) {
                self.settings_validation = error.to_string();
                return;
            }
        } else if self.cloudflare_access.is_some()
            && crate::api::mount_root(&self.state.connection.server)
                .ok()
                .is_some_and(|current| crate::api::mount_root(&server).ok() == Some(current))
            && let Err(error) = credentials::remove(&server)
        {
            self.settings_validation = error.to_string();
            return;
        }
        let (stored, warning) =
            credentials::apply_password_change(&SystemKeyring, &server, &username, &password_plan);
        self.password_stored = stored;
        self.current_password = password_plan.password;
        self.cloudflare_access = cloudflare_access;
        self.server_url_input = server.clone();
        self.username_input = username.clone();
        self.state.connection.server = server;
        self.state.connection.username = username;
        self.state.connection.basic_auth_in_keyring = stored;
        self.state.connection.cloudflare_access = self.cloudflare_access.is_some();
        if let Some(warning) = warning {
            self.error_banner = Some(warning);
        }
        let _ = self.state.save(&default_path());
        self.connect_api();
        self.active_drawer = None;
    }

    fn is_session_busy(&self, id: &str) -> bool {
        self.statuses.get(id).is_some_and(|s| s.is_busy())
    }

    fn active_session_model_label(&self) -> String {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .and_then(|s| s.model.as_ref())
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "Default".to_string())
    }

    fn active_session_model_id(&self) -> Option<String> {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .and_then(|s| s.model.as_ref())
            .map(|m| m.id.clone())
    }

    /// GTK's session-header strip shows the raw count (`13400 tokens`).
    fn context_usage_raw(&self) -> String {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.conversations.get(id))
            .and_then(|conversation| conversation.context_tokens())
            .map(|tokens| format!("{tokens} tokens"))
            .unwrap_or_default()
    }

    /// GTK's `.composer-usage`: compact tokens against the model's window,
    /// e.g. `13.4k / 200k`.
    fn active_context_usage(&self) -> String {
        let Some(tokens) = self
            .active_session_id
            .as_ref()
            .and_then(|id| self.conversations.get(id))
            .and_then(|c| c.context_tokens())
        else {
            return String::new();
        };
        let limit = self
            .active_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
            .and_then(|s| s.model.as_ref())
            .map(|m| m.id.clone())
            .and_then(|id| {
                let directory = self
                    .active_session_id
                    .as_ref()
                    .and_then(|sid| self.sessions.get(sid))
                    .map(|s| s.directory.clone())?;
                self.catalogs
                    .get(&directory)
                    .and_then(|catalog| catalog.models.iter().find(|m| m.model_id == id).cloned())
                    .and_then(|model| model.context_limit)
            });
        match limit {
            Some(limit) => format!("{} / {}", compact_tokens(tokens), compact_tokens(limit)),
            None => compact_tokens(tokens),
        }
    }

    fn legacy_active_context_usage(&self) -> String {
        self.active_session_id
            .as_ref()
            .and_then(|id| self.conversations.get(id))
            .and_then(|c| c.context_tokens())
            .map(|tokens| format!("{tokens} tokens"))
            .unwrap_or_default()
    }

    fn active_jobs_count(&self) -> usize {
        self.jobs.rows(self.active_session_id.as_deref()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_default_and_named_selection_map_to_model_variants() {
        assert_eq!(variant_selection(""), None);
        assert_eq!(variant_selection("medium"), Some("medium".to_string()));
    }

    fn ch(value: &str) -> Key {
        Key::Character(value.into())
    }

    fn message(key: &Key, modifiers: Modifiers) -> Option<Message> {
        shortcut(key, modifiers)
    }

    #[test]
    fn advertised_shortcuts_resolve() {
        assert!(matches!(
            message(&ch("p"), Modifiers::CTRL),
            Some(Message::ToggleDrawer(DrawerPage::Sessions))
        ));
        assert!(matches!(
            message(&ch(","), Modifiers::CTRL),
            Some(Message::ToggleDrawer(DrawerPage::Settings))
        ));
        assert!(matches!(
            message(&ch("t"), Modifiers::CTRL),
            Some(Message::NewSession)
        ));
        assert!(matches!(
            message(&ch("w"), Modifiers::CTRL),
            Some(Message::CloseActiveTab)
        ));
        assert!(matches!(
            message(&ch("b"), Modifiers::CTRL),
            Some(Message::ToggleSidebar)
        ));
        assert!(matches!(
            message(&ch("g"), Modifiers::CTRL),
            Some(Message::FocusComposer)
        ));
    }

    #[test]
    fn uppercase_variants_match_too() {
        assert!(matches!(
            message(&ch("P"), Modifiers::CTRL),
            Some(Message::ToggleDrawer(DrawerPage::Sessions))
        ));
        assert!(matches!(
            message(&ch("T"), Modifiers::CTRL),
            Some(Message::NewSession)
        ));
    }

    #[test]
    fn enter_reports_whether_ctrl_is_held() {
        let enter = Key::Named(Named::Enter);
        assert!(matches!(
            message(&enter, Modifiers::NONE),
            Some(Message::ComposerEnter { ctrl: false })
        ));
        assert!(matches!(
            message(&enter, Modifiers::CTRL),
            Some(Message::ComposerEnter { ctrl: true })
        ));
        // The run status turns these into send/steer/queue (tray::enter_mode).
        assert_eq!(enter_mode(false, false), SendMode::Send);
        assert_eq!(enter_mode(true, false), SendMode::Steer);
        assert_eq!(enter_mode(true, true), SendMode::Queue);
    }

    #[test]
    fn tab_cycling_wraps_with_shift() {
        assert!(matches!(
            message(&Key::Named(Named::Tab), Modifiers::CTRL),
            Some(Message::CycleTab(1))
        ));
        assert!(matches!(
            message(&Key::Named(Named::Tab), Modifiers::CTRL | Modifiers::SHIFT),
            Some(Message::CycleTab(-1))
        ));
    }

    #[test]
    fn digits_select_tabs_with_ctrl_or_alt() {
        assert!(matches!(
            message(&ch("3"), Modifiers::CTRL),
            Some(Message::SelectTabIndex(2))
        ));
        assert!(matches!(
            message(&ch("9"), Modifiers::ALT),
            Some(Message::SelectTabIndex(8))
        ));
        // Ctrl+0 resets the zoom (GTK's Ctrl+= / Ctrl+- / Ctrl+0 ladder).
        assert!(matches!(
            message(&ch("0"), Modifiers::CTRL),
            Some(Message::ZoomReset)
        ));
    }

    #[test]
    fn zoom_keys_follow_the_gtk_ladder() {
        for key in ["=", "+"] {
            assert!(matches!(
                message(&ch(key), Modifiers::CTRL),
                Some(Message::ZoomIn)
            ));
        }
        assert!(matches!(
            message(&ch("-"), Modifiers::CTRL),
            Some(Message::ZoomOut)
        ));
        assert!(matches!(
            message(&ch("0"), Modifiers::CTRL),
            Some(Message::ZoomReset)
        ));
        assert_eq!(ZOOM_STEPS.first(), Some(&0.7));
        assert_eq!(ZOOM_STEPS.last(), Some(&1.75));
    }

    #[test]
    fn zoom_ladder_matches_the_gtk_steps() {
        assert_eq!(next_zoom(1.0, 1), 1.1);
        assert_eq!(next_zoom(1.0, -1), 0.9);
        assert_eq!(next_zoom(1.3, 1), 1.5);
        assert_eq!(next_zoom(0.7, -1), 0.7, "clamped at the bottom");
        assert_eq!(next_zoom(1.75, 1), 1.75, "clamped at the top");
        // 1em and the GTK spacing scale grow with the zoom. 1em is GTK's
        // "Noto Sans, 10" (10pt = 13.33px).
        assert_eq!(crate::metrics::em(1.0, 1.0), 13);
        assert_eq!(crate::metrics::em(0.76, 1.75), 18);
        assert!((crate::metrics::space(1.19, 1.2) - 19.036).abs() < 0.01);
    }

    #[test]
    fn inline_images_decode_only_base64_image_uris() {
        assert_eq!(
            inline_image_bytes("data:image/png;base64,aGk="),
            Some(b"hi".to_vec())
        );
        assert_eq!(inline_image_bytes("data:text/plain;base64,aGk="), None);
        assert_eq!(inline_image_bytes("data:image/png;base64,@@ nope @@"), None);
        assert_eq!(inline_image_bytes("https://example.com/x.png"), None);
    }

    #[test]
    fn attachment_labels_use_the_file_name_and_shorten_middle() {
        assert_eq!(
            attachment_label(std::path::Path::new("/state/home/paperclip-22px.png")),
            "paperclip-22px.png"
        );
        let long = attachment_label(std::path::Path::new(
            "/tmp/a-very-long-name-for-a-screenshot-of-the-composer.png",
        ));
        assert!(long.contains('…'), "{long}");
        assert!(long.chars().count() <= 26, "{long}");
    }

    #[test]
    fn compact_tokens_matches_the_gtk_usage_line() {
        assert_eq!(compact_tokens(950), "950");
        assert_eq!(compact_tokens(13_400), "13.4k");
        assert_eq!(compact_tokens(200_000), "200k");
        assert_eq!(compact_tokens(1_000), "1k");
        assert_eq!(compact_tokens(1_050), "1.1k");
    }

    #[test]
    fn clock_formats_milliseconds_and_seconds() {
        // GTK printed `YYYY-MM-DD HH:MM`; both protocol shapes must render.
        for value in [1_790_000_000_000u64, 1_790_000_000] {
            let stamp = clock_time(value);
            assert_eq!(stamp.len(), 16, "{stamp}");
            assert_eq!(&stamp[4..5], "-");
            assert_eq!(&stamp[10..11], " ");
            assert_eq!(&stamp[13..14], ":");
        }
    }

    #[test]
    fn escape_closes_the_drawer() {
        assert!(matches!(
            message(&Key::Named(Named::Escape), Modifiers::NONE),
            Some(Message::CloseDrawer)
        ));
    }

    #[test]
    fn typed_text_is_never_swallowed() {
        // Plain keys must reach the composer instead of triggering a shortcut.
        for value in ["t", "p", "b", "w", ",", "1", "9", "0"] {
            assert!(
                message(&ch(value), Modifiers::NONE).is_none(),
                "plain {value:?} must not trigger a shortcut"
            );
            assert!(
                message(&ch(value), Modifiers::SHIFT).is_none(),
                "shift+{value:?} must not trigger a shortcut"
            );
        }
        assert!(message(&ch("q"), Modifiers::CTRL).is_none());
    }

    #[test]
    fn saved_tabs_are_restored_without_sessions_the_server_lost() {
        let known: std::collections::HashSet<String> =
            ["a".to_string(), "c".to_string()].into_iter().collect();
        let saved = crate::persist::ServerState {
            tabs: ["c", "gone", "a"]
                .iter()
                .map(|id| crate::persist::PersistedTab {
                    id: (*id).to_string(),
                    directory: "/repo".to_string(),
                    title: (*id).to_string(),
                })
                .collect(),
            ..Default::default()
        };

        assert_eq!(restore_tabs(Some(&saved), &known), vec!["c", "a"]);
        assert!(restore_tabs(None, &known).is_empty());
    }

    #[test]
    fn dragging_a_session_moves_it_and_shifts_the_rest() {
        let tabs = |ids: &[&str]| ids.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();

        let mut open = tabs(&["a", "b", "c", "d", "e"]);
        assert!(reorder_tabs(&mut open, 4, 1));
        assert_eq!(open, tabs(&["a", "e", "b", "c", "d"]));

        let mut open = tabs(&["a", "b", "c"]);
        assert!(reorder_tabs(&mut open, 0, 2));
        assert_eq!(open, tabs(&["b", "c", "a"]));

        // Dropping a row on itself, or out of range, leaves the order alone.
        let mut open = tabs(&["a", "b"]);
        assert!(!reorder_tabs(&mut open, 1, 1));
        assert!(!reorder_tabs(&mut open, 0, 5));
        assert_eq!(open, tabs(&["a", "b"]));
    }
}
