use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    AuthMethod, ContentBlock, PermissionOption, PermissionOptionId, PlanEntryStatus,
    RequestPermissionRequest, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionModeState, SessionUpdate, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui_textarea::TextArea;
use tokio::sync::oneshot;

const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub takes_argument: bool,
    pub requires_argument: bool,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        usage: "/help",
        description: "Show keyboard shortcuts and commands",
        takes_argument: false,
        requires_argument: false,
    },
    SlashCommand {
        name: "login",
        usage: "/login [provider]",
        description: "Authenticate with a provider",
        takes_argument: true,
        requires_argument: false,
    },
    SlashCommand {
        name: "model",
        usage: "/model [id]",
        description: "Select the session model",
        takes_argument: true,
        requires_argument: false,
    },
    SlashCommand {
        name: "mode",
        usage: "/mode [id]",
        description: "Select ask or code mode",
        takes_argument: true,
        requires_argument: false,
    },
    SlashCommand {
        name: "resume",
        usage: "/resume <session-id>",
        description: "Load an existing session",
        takes_argument: true,
        requires_argument: true,
    },
    SlashCommand {
        name: "new",
        usage: "/new",
        description: "Start a new session",
        takes_argument: false,
        requires_argument: false,
    },
    SlashCommand {
        name: "clear",
        usage: "/clear",
        description: "Clear the local transcript",
        takes_argument: false,
        requires_argument: false,
    },
    SlashCommand {
        name: "quit",
        usage: "/quit",
        description: "Exit fxrs",
        takes_argument: false,
        requires_argument: false,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMenu {
    pub matches: Vec<usize>,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Cancelling,
    Working,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    Thought,
    Tool,
    Plan,
    Notice,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: EntryKind,
    pub key: Option<String>,
    pub title: String,
    pub body: String,
    pub collapsed: bool,
    pub tool_status: Option<ToolCallStatus>,
    pub cached_width: u16,
    pub cached_revision: u64,
    pub cached_height: u32,
    revision: u64,
}

impl Entry {
    fn new(kind: EntryKind, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            key: None,
            title: title.into(),
            body: body.into(),
            collapsed: false,
            tool_status: None,
            cached_width: 0,
            cached_revision: u64::MAX,
            cached_height: 0,
            revision: 0,
        }
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.title.len() + self.body.len()
    }
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigChoice {
    pub id: String,
    pub name: String,
    pub current: String,
    pub options: Vec<Choice>,
    pub category: Option<SessionConfigOptionCategory>,
}

#[derive(Debug, Clone)]
pub struct ModeChoice {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerKind {
    Authenticate,
    Config(String),
    Mode,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    Help,
    Picker {
        kind: PickerKind,
        title: String,
        options: Vec<Choice>,
        selected: usize,
    },
}

#[derive(Debug)]
pub enum Action {
    Submit(String),
    Cancel,
    Authenticate(String),
    NewSession,
    ResumeSession(String),
    SetMode(String),
    SetConfig { config_id: String, value: String },
    Quit,
}

pub struct PendingPermission {
    pub title: String,
    pub options: Vec<PermissionOption>,
    pub selected: usize,
    pub parked: bool,
    pub hits: Vec<Rect>,
    responder: Option<oneshot::Sender<Option<PermissionOptionId>>>,
}

impl PendingPermission {
    pub fn from_request(
        request: RequestPermissionRequest,
        responder: oneshot::Sender<Option<PermissionOptionId>>,
    ) -> Self {
        Self {
            title: request
                .tool_call
                .fields
                .title
                .unwrap_or_else(|| "Approve tool action?".into()),
            options: request.options,
            selected: 0,
            parked: false,
            hits: Vec::new(),
            responder: Some(responder),
        }
    }

    fn resolve(&mut self, option: Option<PermissionOptionId>) {
        if let Some(responder) = self.responder.take() {
            let _ = responder.send(option);
        }
    }
}

impl Drop for PendingPermission {
    fn drop(&mut self) {
        self.resolve(None);
    }
}

pub struct App {
    pub cwd: PathBuf,
    pub session_id: String,
    pub session_title: Option<String>,
    pub agent_name: String,
    pub auth_methods: Vec<Choice>,
    pub modes: Vec<ModeChoice>,
    pub current_mode: Option<String>,
    pub configs: Vec<ConfigChoice>,
    pub entries: VecDeque<Entry>,
    pub focus: Focus,
    pub phase: Phase,
    pub overlay: Option<Overlay>,
    pub command_menu: Option<CommandMenu>,
    pub permission: Option<PendingPermission>,
    pub status: String,
    pub scroll_from_bottom: u32,
    pub follow_tail: bool,
    pub selected_entry: Option<usize>,
    pub composer: TextArea<'static>,
    pub queued: VecDeque<String>,
    pub dirty: bool,
    pub spinner_frame: usize,
    pub quit: bool,
    transcript_bytes: usize,
    tools: HashMap<String, usize>,
}

impl App {
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        let mut composer = TextArea::default();
        composer.set_tab_length(4);
        Self {
            cwd,
            session_id: String::new(),
            session_title: None,
            agent_name: "fxrs".into(),
            auth_methods: Vec::new(),
            modes: Vec::new(),
            current_mode: None,
            configs: Vec::new(),
            entries: VecDeque::new(),
            focus: Focus::Composer,
            phase: Phase::Working,
            overlay: None,
            command_menu: None,
            permission: None,
            status: "Connecting to ACP agent…".into(),
            scroll_from_bottom: 0,
            follow_tail: true,
            selected_entry: None,
            composer,
            queued: VecDeque::new(),
            dirty: true,
            spinner_frame: 0,
            quit: false,
            transcript_bytes: 0,
            tools: HashMap::new(),
        }
    }

    pub fn set_initialized(&mut self, agent_name: String, auth_methods: &[AuthMethod]) {
        self.agent_name = agent_name;
        self.auth_methods = auth_methods
            .iter()
            .map(|method| Choice {
                id: method.id().0.to_string(),
                name: method.name().to_owned(),
                description: method.description().map(str::to_owned),
            })
            .collect();
        self.dirty = true;
    }

    pub fn set_session(
        &mut self,
        session_id: impl Into<String>,
        modes: Option<SessionModeState>,
        configs: Option<Vec<SessionConfigOption>>,
    ) {
        self.session_id = session_id.into();
        if let Some(modes) = modes {
            self.set_modes(modes);
        }
        if let Some(configs) = configs {
            self.set_configs(configs);
        }
        self.phase = Phase::Idle;
        self.status = "Ready".into();
        self.dirty = true;
    }

    fn set_modes(&mut self, modes: SessionModeState) {
        self.current_mode = Some(modes.current_mode_id.0.to_string());
        self.modes = modes
            .available_modes
            .into_iter()
            .map(|mode| ModeChoice {
                id: mode.id.0.to_string(),
                name: mode.name,
                description: mode.description,
            })
            .collect();
    }

    pub fn set_configs(&mut self, configs: Vec<SessionConfigOption>) {
        self.configs = configs.into_iter().filter_map(project_config).collect();
        self.dirty = true;
    }

    pub fn add_notice(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.push_entry(Entry::new(EntryKind::Notice, title, body));
    }

    pub fn on_session_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::UserMessageChunk(chunk) => {
                self.append_chunk(
                    EntryKind::User,
                    "You",
                    chunk.message_id.map(|id| id.0.to_string()),
                    chunk.content,
                );
            }
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.append_chunk(
                    EntryKind::Assistant,
                    "Assistant",
                    chunk.message_id.map(|id| id.0.to_string()),
                    chunk.content,
                );
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                self.append_chunk(
                    EntryKind::Thought,
                    "Thinking",
                    chunk.message_id.map(|id| id.0.to_string()),
                    chunk.content,
                );
            }
            SessionUpdate::ToolCall(tool) => self.add_tool(tool),
            SessionUpdate::ToolCallUpdate(update) => self.update_tool(update),
            SessionUpdate::Plan(plan) => {
                let body = plan
                    .entries
                    .into_iter()
                    .map(|entry| {
                        let marker = match entry.status {
                            PlanEntryStatus::Completed => "✓",
                            PlanEntryStatus::InProgress => "◆",
                            PlanEntryStatus::Pending => "○",
                            _ => "·",
                        };
                        format!("{marker} {}", entry.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .find(|entry| entry.kind == EntryKind::Plan)
                {
                    self.transcript_bytes = self.transcript_bytes.saturating_sub(entry.body.len());
                    entry.body = body;
                    entry.changed();
                    self.transcript_bytes += entry.body.len();
                } else {
                    self.push_entry(Entry::new(EntryKind::Plan, "Plan", body));
                }
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                self.current_mode = Some(update.current_mode_id.0.to_string());
            }
            SessionUpdate::ConfigOptionUpdate(update) => self.set_configs(update.config_options),
            SessionUpdate::SessionInfoUpdate(update) => {
                if !update.title.is_undefined() {
                    self.session_title = update.title.take();
                }
            }
            SessionUpdate::AvailableCommandsUpdate(_) | SessionUpdate::UsageUpdate(_) => {}
            _ => {}
        }
        self.trim_transcript();
        if self.follow_tail {
            self.scroll_from_bottom = 0;
        }
        self.dirty = true;
    }

    pub fn on_prompt_finished(&mut self, result: Result<String, String>) -> Option<Action> {
        self.phase = Phase::Idle;
        for entry in &mut self.entries {
            if entry.kind == EntryKind::Thought {
                entry.collapsed = true;
            }
        }
        match result {
            Ok(reason) => self.status = format!("Ready · {reason}"),
            Err(error) => {
                self.status = "Prompt failed".into();
                self.add_notice("Request failed", error);
            }
        }
        self.dirty = true;
        self.queued.pop_front().map(Action::Submit)
    }

    pub fn on_operation_finished(&mut self, label: &str, result: Result<(), String>) {
        self.phase = Phase::Idle;
        match result {
            Ok(()) => self.status = format!("{label} complete"),
            Err(error) => {
                self.status = format!("{label} failed");
                self.add_notice(format!("{label} failed"), error);
            }
        }
        self.dirty = true;
    }

    pub fn set_permission(&mut self, permission: PendingPermission) {
        self.command_menu = None;
        self.permission = Some(permission);
        self.focus = Focus::Composer;
        self.status = "Permission required".into();
        self.dirty = true;
    }

    #[must_use]
    pub fn composer_height(&self) -> u16 {
        let lines = u16::try_from(self.composer.lines().len()).unwrap_or(u16::MAX);
        lines.saturating_add(2).clamp(3, 8)
    }

    #[must_use]
    pub fn composer_text(&self) -> String {
        self.composer.lines().join("\n")
    }

    pub fn insert_composer_text(&mut self, text: &str) {
        self.composer.insert_str(text);
        self.sync_command_menu();
        self.dirty = true;
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> Option<Action> {
        if event.kind != KeyEventKind::Press && event.kind != KeyEventKind::Repeat {
            return None;
        }
        self.dirty = true;

        if self
            .permission
            .as_ref()
            .is_some_and(|permission| !permission.parked)
        {
            return self.handle_permission_key(event);
        }
        if self.overlay.is_some() {
            self.command_menu = None;
            return self.handle_overlay_key(event);
        }

        if self.focus == Focus::Composer
            && self.command_menu.is_some()
            && event.modifiers.is_empty()
        {
            match event.code {
                KeyCode::Up => {
                    self.select_previous_command();
                    return None;
                }
                KeyCode::Down => {
                    self.select_next_command();
                    return None;
                }
                KeyCode::Tab => {
                    self.complete_selected_command();
                    return None;
                }
                KeyCode::Enter => return self.activate_selected_command(),
                KeyCode::Esc => {
                    self.command_menu = None;
                    return None;
                }
                _ => {}
            }
        }

        if event.modifiers.contains(KeyModifiers::CONTROL) {
            match event.code {
                KeyCode::Char('c') => {
                    if self.phase == Phase::Running {
                        self.phase = Phase::Cancelling;
                        self.status = "Cancelling…".into();
                        return Some(Action::Cancel);
                    }
                    self.quit = true;
                    return Some(Action::Quit);
                }
                KeyCode::Char('p') | KeyCode::Char('?') => {
                    self.command_menu = None;
                    self.overlay = Some(Overlay::Help);
                    return None;
                }
                KeyCode::Char('m') => {
                    self.command_menu = None;
                    self.open_model_picker();
                    return None;
                }
                _ => {}
            }
        }

        match event.code {
            KeyCode::BackTab => return self.cycle_mode(),
            KeyCode::Tab => {
                if let Some(permission) = &mut self.permission
                    && permission.parked
                {
                    permission.parked = false;
                    self.status = "Permission required".into();
                } else {
                    self.focus = match self.focus {
                        Focus::Composer => Focus::Transcript,
                        Focus::Transcript => Focus::Composer,
                    };
                    if self.focus == Focus::Composer {
                        self.sync_command_menu();
                    } else {
                        self.command_menu = None;
                    }
                }
                return None;
            }
            KeyCode::Esc => {
                if self.phase == Phase::Running {
                    self.phase = Phase::Cancelling;
                    self.status = "Cancelling…".into();
                    return Some(Action::Cancel);
                }
                self.focus = Focus::Composer;
                return None;
            }
            _ => {}
        }

        match self.focus {
            Focus::Composer => self.handle_composer_key(event),
            Focus::Transcript => {
                self.handle_transcript_key(event);
                None
            }
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<Action> {
        self.dirty = true;
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            MouseEventKind::Down(_) => {
                if let Some(permission) = &mut self.permission
                    && !permission.parked
                    && let Some(index) = permission
                        .hits
                        .iter()
                        .position(|area| area.contains((event.column, event.row).into()))
                {
                    permission.selected = index;
                    let option = permission.options[index].option_id.clone();
                    permission.resolve(Some(option));
                    self.permission = None;
                    self.status = "Permission sent".into();
                }
            }
            _ => {}
        }
        None
    }

    pub fn advance_spinner(&mut self) {
        if self.is_animating() {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
            self.dirty = true;
        }
    }

    #[must_use]
    pub fn is_animating(&self) -> bool {
        matches!(
            self.phase,
            Phase::Running | Phase::Cancelling | Phase::Working
        )
    }

    pub fn mark_prompt_started(&mut self, prompt: &str) {
        self.phase = Phase::Running;
        self.status = "Agent is working…".into();
        self.push_entry(Entry::new(EntryKind::User, "You", prompt));
        self.follow_tail = true;
        self.scroll_from_bottom = 0;
    }

    pub fn mark_working(&mut self, label: &str) {
        self.phase = Phase::Working;
        self.status = label.into();
        self.dirty = true;
    }

    pub fn begin_session_switch(&mut self, label: &str) {
        self.clear_transcript();
        self.session_title = None;
        self.mark_working(label);
    }

    fn handle_composer_key(&mut self, event: KeyEvent) -> Option<Action> {
        if event.code == KeyCode::Enter
            && event
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            self.composer.insert_newline();
            return None;
        }
        if event.code == KeyCode::Enter && event.modifiers.is_empty() {
            let prompt = self.composer_text();
            let prompt = prompt.trim().to_owned();
            if prompt.is_empty() {
                return None;
            }
            self.composer = TextArea::default();
            self.command_menu = None;
            if matches!(self.phase, Phase::Running | Phase::Cancelling) {
                self.queued.push_back(prompt);
                self.status = format!("Queued · {} waiting", self.queued.len());
                return None;
            }
            if let Some(action) = self.command(&prompt) {
                return action;
            }
            self.mark_prompt_started(&prompt);
            return Some(Action::Submit(prompt));
        }
        let _ = self.composer.input(event);
        self.sync_command_menu();
        None
    }

    fn sync_command_menu(&mut self) {
        if self.phase != Phase::Idle || self.focus != Focus::Composer || self.overlay.is_some() {
            self.command_menu = None;
            return;
        }
        let prompt = self.composer_text();
        let Some(query) = prompt.strip_prefix('/') else {
            self.command_menu = None;
            return;
        };
        if query.chars().any(char::is_whitespace) {
            self.command_menu = None;
            return;
        }
        let matches = SLASH_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, command)| command.name.starts_with(query))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            self.command_menu = None;
            return;
        }
        let previous = self.command_menu.as_ref().and_then(|menu| {
            menu.matches
                .get(menu.selected)
                .copied()
                .filter(|index| matches.contains(index))
        });
        let selected = previous
            .and_then(|index| matches.iter().position(|candidate| *candidate == index))
            .unwrap_or_default();
        self.command_menu = Some(CommandMenu { matches, selected });
    }

    fn select_previous_command(&mut self) {
        let Some(menu) = &mut self.command_menu else {
            return;
        };
        menu.selected = menu
            .selected
            .checked_sub(1)
            .unwrap_or_else(|| menu.matches.len().saturating_sub(1));
    }

    fn select_next_command(&mut self) {
        let Some(menu) = &mut self.command_menu else {
            return;
        };
        if !menu.matches.is_empty() {
            menu.selected = (menu.selected + 1) % menu.matches.len();
        }
    }

    fn selected_command(&self) -> Option<SlashCommand> {
        let menu = self.command_menu.as_ref()?;
        let index = *menu.matches.get(menu.selected)?;
        SLASH_COMMANDS.get(index).copied()
    }

    fn replace_composer(&mut self, value: &str) {
        let mut composer = TextArea::default();
        composer.set_tab_length(4);
        composer.insert_str(value);
        self.composer = composer;
    }

    fn complete_selected_command(&mut self) {
        let Some(command) = self.selected_command() else {
            return;
        };
        let suffix = if command.takes_argument { " " } else { "" };
        self.replace_composer(&format!("/{}{suffix}", command.name));
        self.command_menu = None;
    }

    fn activate_selected_command(&mut self) -> Option<Action> {
        let command = self.selected_command()?;
        if command.requires_argument {
            self.replace_composer(&format!("/{} ", command.name));
            self.command_menu = None;
            return None;
        }
        let prompt = format!("/{}", command.name);
        self.replace_composer("");
        self.command_menu = None;
        self.command(&prompt).flatten()
    }

    fn handle_transcript_key(&mut self, event: KeyEvent) {
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::PageUp => self.scroll_up(12),
            KeyCode::PageDown => self.scroll_down(12),
            KeyCode::Home => {
                self.scroll_from_bottom = u32::MAX / 2;
                self.follow_tail = false;
            }
            KeyCode::End => {
                self.scroll_from_bottom = 0;
                self.follow_tail = true;
            }
            KeyCode::Left => self.set_selected_collapsed(true),
            KeyCode::Right | KeyCode::Enter => self.set_selected_collapsed(false),
            _ => {}
        }
    }

    fn handle_permission_key(&mut self, event: KeyEvent) -> Option<Action> {
        let Some(permission) = &mut self.permission else {
            return None;
        };
        match event.code {
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                permission.selected = permission
                    .selected
                    .checked_sub(1)
                    .unwrap_or_else(|| permission.options.len().saturating_sub(1));
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                if !permission.options.is_empty() {
                    permission.selected = (permission.selected + 1) % permission.options.len();
                }
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                if let Some(option) = permission.options.get(index) {
                    permission.resolve(Some(option.option_id.clone()));
                    self.permission = None;
                    self.status = "Permission sent".into();
                }
            }
            KeyCode::Enter => {
                if let Some(option) = permission.options.get(permission.selected) {
                    permission.resolve(Some(option.option_id.clone()));
                    self.permission = None;
                    self.status = "Permission sent".into();
                }
            }
            KeyCode::Esc => {
                permission.parked = true;
                self.focus = Focus::Transcript;
                self.status = "Permission parked · Tab to return".into();
            }
            _ => {}
        }
        None
    }

    fn handle_overlay_key(&mut self, event: KeyEvent) -> Option<Action> {
        match &mut self.overlay {
            Some(Overlay::Help) => {
                if matches!(
                    event.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')
                ) {
                    self.overlay = None;
                }
                None
            }
            Some(Overlay::Picker {
                kind,
                options,
                selected,
                ..
            }) => match event.code {
                KeyCode::Esc => {
                    self.overlay = None;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                    *selected = selected
                        .checked_sub(1)
                        .unwrap_or_else(|| options.len().saturating_sub(1));
                    None
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    if !options.is_empty() {
                        *selected = (*selected + 1) % options.len();
                    }
                    None
                }
                KeyCode::Enter => {
                    let action = options.get(*selected).map(|choice| match kind {
                        PickerKind::Authenticate => Action::Authenticate(choice.id.clone()),
                        PickerKind::Config(config_id) => Action::SetConfig {
                            config_id: config_id.clone(),
                            value: choice.id.clone(),
                        },
                        PickerKind::Mode => Action::SetMode(choice.id.clone()),
                    });
                    self.overlay = None;
                    action
                }
                _ => None,
            },
            None => None,
        }
    }

    fn command(&mut self, prompt: &str) -> Option<Option<Action>> {
        let command = prompt.strip_prefix('/')?;
        let (name, argument) = command.split_once(' ').unwrap_or((command, ""));
        let argument = argument.trim();
        let action = match name {
            "help" => {
                self.overlay = Some(Overlay::Help);
                None
            }
            "model" if argument.is_empty() => {
                self.open_model_picker();
                None
            }
            "model" => Some(Action::SetConfig {
                config_id: self
                    .configs
                    .iter()
                    .find(|config| {
                        config.id == "model"
                            || matches!(config.category, Some(SessionConfigOptionCategory::Model))
                    })
                    .map(|config| config.id.clone())
                    .unwrap_or_else(|| "model".into()),
                value: argument.into(),
            }),
            "mode" if argument.is_empty() => {
                self.open_mode_picker();
                None
            }
            "mode" => Some(Action::SetMode(argument.into())),
            "login" if argument.is_empty() => self.open_auth_picker(),
            "login" => Some(Action::Authenticate(argument.into())),
            "resume" if !argument.is_empty() => Some(Action::ResumeSession(argument.into())),
            "new" => Some(Action::NewSession),
            "clear" => {
                self.clear_transcript();
                None
            }
            "quit" | "exit" => {
                self.quit = true;
                Some(Action::Quit)
            }
            _ => {
                self.add_notice(
                    "Unknown command",
                    format!("/{name} · type /help for available commands"),
                );
                None
            }
        };
        Some(action)
    }

    fn open_model_picker(&mut self) {
        if let Some(config) = self.configs.iter().find(|config| {
            config.id == "model"
                || matches!(config.category, Some(SessionConfigOptionCategory::Model))
        }) {
            let selected = config
                .options
                .iter()
                .position(|option| option.id == config.current)
                .unwrap_or_default();
            self.overlay = Some(Overlay::Picker {
                kind: PickerKind::Config(config.id.clone()),
                title: config.name.clone(),
                options: config.options.clone(),
                selected,
            });
        } else {
            self.add_notice(
                "Model selector unavailable",
                "The ACP agent did not advertise a model configuration.",
            );
        }
    }

    fn open_mode_picker(&mut self) {
        let options = self
            .modes
            .iter()
            .map(|mode| Choice {
                id: mode.id.clone(),
                name: mode.name.clone(),
                description: mode.description.clone(),
            })
            .collect::<Vec<_>>();
        if options.is_empty() {
            self.add_notice(
                "Mode selector unavailable",
                "The ACP agent did not advertise session modes.",
            );
            return;
        }
        let selected = self
            .current_mode
            .as_ref()
            .and_then(|current| options.iter().position(|option| &option.id == current))
            .unwrap_or_default();
        self.overlay = Some(Overlay::Picker {
            kind: PickerKind::Mode,
            title: "Session mode".into(),
            options,
            selected,
        });
    }

    fn open_auth_picker(&mut self) -> Option<Action> {
        match self.auth_methods.as_slice() {
            [] => {
                self.add_notice(
                    "Authentication unavailable",
                    "The ACP agent did not advertise an authentication method.",
                );
                None
            }
            [method] => Some(Action::Authenticate(method.id.clone())),
            methods => {
                self.overlay = Some(Overlay::Picker {
                    kind: PickerKind::Authenticate,
                    title: "Authentication provider".into(),
                    options: methods.to_vec(),
                    selected: 0,
                });
                None
            }
        }
    }

    fn cycle_mode(&mut self) -> Option<Action> {
        if self.modes.is_empty() {
            return None;
        }
        let current = self
            .current_mode
            .as_ref()
            .and_then(|id| self.modes.iter().position(|mode| &mode.id == id))
            .unwrap_or_default();
        let next = (current + 1) % self.modes.len();
        Some(Action::SetMode(self.modes[next].id.clone()))
    }

    fn append_chunk(
        &mut self,
        kind: EntryKind,
        title: &str,
        key: Option<String>,
        content: ContentBlock,
    ) {
        let text = content_text(content);
        if text.is_empty() {
            return;
        }
        let can_merge = self
            .entries
            .back()
            .is_some_and(|entry| entry.kind == kind && (key.is_none() || entry.key == key));
        if can_merge {
            let entry = self.entries.back_mut().expect("entry checked above");
            entry.body.push_str(&text);
            entry.changed();
            self.transcript_bytes += text.len();
        } else {
            let mut entry = Entry::new(kind, title, text);
            entry.key = key;
            self.push_entry(entry);
        }
    }

    fn add_tool(&mut self, tool: ToolCall) {
        let id = tool.tool_call_id.0.to_string();
        let body = tool_body(
            &tool.content,
            tool.raw_input.as_ref(),
            tool.raw_output.as_ref(),
        );
        let mut entry = Entry::new(EntryKind::Tool, tool.title, body);
        entry.key = Some(id.clone());
        entry.tool_status = Some(tool.status);
        entry.collapsed = matches!(tool.status, ToolCallStatus::Completed);
        self.push_entry(entry);
        self.tools.insert(id, self.entries.len().saturating_sub(1));
    }

    fn update_tool(&mut self, update: ToolCallUpdate) {
        let id = update.tool_call_id.0.to_string();
        let index = self
            .tools
            .get(&id)
            .copied()
            .filter(|index| {
                self.entries
                    .get(*index)
                    .is_some_and(|entry| entry.key.as_deref() == Some(&id))
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|entry| entry.key.as_deref() == Some(&id))
            });
        let Some(index) = index else {
            let mut entry = Entry::new(
                EntryKind::Tool,
                update.fields.title.clone().unwrap_or_else(|| "Tool".into()),
                tool_fields_body(&update),
            );
            entry.key = Some(id.clone());
            entry.tool_status = update.fields.status;
            entry.collapsed = matches!(entry.tool_status, Some(ToolCallStatus::Completed));
            self.push_entry(entry);
            self.tools.insert(id, self.entries.len().saturating_sub(1));
            return;
        };
        let entry = &mut self.entries[index];
        self.transcript_bytes = self.transcript_bytes.saturating_sub(entry.bytes());
        if let Some(title) = update.fields.title {
            entry.title = title;
        }
        if let Some(status) = update.fields.status {
            entry.tool_status = Some(status);
            entry.collapsed = matches!(status, ToolCallStatus::Completed);
        }
        let body_changed = update.fields.content.is_some()
            || update.fields.raw_input.is_some()
            || update.fields.raw_output.is_some();
        let content = update.fields.content.unwrap_or_default();
        let body = tool_body(
            &content,
            update.fields.raw_input.as_ref(),
            update.fields.raw_output.as_ref(),
        );
        if body_changed {
            entry.body = body;
        }
        entry.changed();
        self.transcript_bytes += entry.bytes();
    }

    fn push_entry(&mut self, entry: Entry) {
        self.transcript_bytes += entry.bytes();
        self.entries.push_back(entry);
        self.selected_entry = Some(self.entries.len().saturating_sub(1));
        self.trim_transcript();
        self.dirty = true;
    }

    fn trim_transcript(&mut self) {
        let mut removed = false;
        while self.transcript_bytes > MAX_TRANSCRIPT_BYTES && self.entries.len() > 1 {
            if let Some(entry) = self.entries.pop_front() {
                self.transcript_bytes = self.transcript_bytes.saturating_sub(entry.bytes());
                removed = true;
            }
        }
        if removed {
            self.rebuild_tool_indices();
        }
    }

    fn rebuild_tool_indices(&mut self) {
        self.tools.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.kind == EntryKind::Tool
                && let Some(key) = &entry.key
            {
                self.tools.insert(key.clone(), index);
            }
        }
        self.selected_entry = self
            .selected_entry
            .map(|index| index.min(self.entries.len().saturating_sub(1)));
    }

    fn clear_transcript(&mut self) {
        self.entries.clear();
        self.tools.clear();
        self.transcript_bytes = 0;
        self.selected_entry = None;
        self.scroll_from_bottom = 0;
        self.follow_tail = true;
    }

    fn select_previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected_entry = Some(
            self.selected_entry
                .unwrap_or(self.entries.len())
                .saturating_sub(1),
        );
        self.scroll_up(2);
    }

    fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let current = self.selected_entry.unwrap_or_default();
        self.selected_entry = Some((current + 1).min(self.entries.len() - 1));
        self.scroll_down(2);
    }

    fn set_selected_collapsed(&mut self, collapsed: bool) {
        if let Some(index) = self.selected_entry
            && let Some(entry) = self.entries.get_mut(index)
            && matches!(
                entry.kind,
                EntryKind::Tool | EntryKind::Thought | EntryKind::Plan
            )
        {
            entry.collapsed = collapsed;
            entry.changed();
        }
    }

    fn scroll_up(&mut self, rows: u32) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(rows);
        self.follow_tail = false;
    }

    fn scroll_down(&mut self, rows: u32) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(rows);
        if self.scroll_from_bottom == 0 {
            self.follow_tail = true;
        }
    }

    #[must_use]
    pub fn current_model_label(&self) -> Option<&str> {
        self.configs
            .iter()
            .find(|config| {
                config.id == "model"
                    || matches!(config.category, Some(SessionConfigOptionCategory::Model))
            })
            .and_then(|config| {
                config
                    .options
                    .iter()
                    .find(|option| option.id == config.current)
            })
            .map(|option| option.name.as_str())
    }

    #[must_use]
    pub fn current_mode_label(&self) -> Option<&str> {
        let current = self.current_mode.as_deref()?;
        self.modes
            .iter()
            .find(|mode| mode.id == current)
            .map(|mode| mode.name.as_str())
    }
}

fn project_config(config: SessionConfigOption) -> Option<ConfigChoice> {
    let SessionConfigKind::Select(select) = config.kind else {
        return None;
    };
    let options = match select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options,
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.into_iter().flat_map(|group| group.options).collect()
        }
        _ => Vec::new(),
    };
    Some(ConfigChoice {
        id: config.id.0.to_string(),
        name: config.name,
        current: select.current_value.0.to_string(),
        options: options
            .into_iter()
            .map(|option| Choice {
                id: option.value.0.to_string(),
                name: option.name,
                description: option.description,
            })
            .collect(),
        category: config.category,
    })
}

fn content_text(content: ContentBlock) -> String {
    match content {
        ContentBlock::Text(text) => text.text,
        other => serde_json::to_string(&other).unwrap_or_else(|_| "[rich content]".into()),
    }
}

fn tool_body(
    content: &[ToolCallContent],
    raw_input: Option<&serde_json::Value>,
    raw_output: Option<&serde_json::Value>,
) -> String {
    let mut sections = Vec::new();
    if let Some(value) = raw_input {
        sections.push(format_json("Input", value));
    }
    if !content.is_empty() {
        sections.push(
            content
                .iter()
                .map(|item| match item {
                    ToolCallContent::Content(content) => content_text(content.content.clone()),
                    other => serde_json::to_string_pretty(other)
                        .unwrap_or_else(|_| "[tool output]".into()),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if let Some(value) = raw_output {
        sections.push(format_json("Output", value));
    }
    sections
        .into_iter()
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn tool_fields_body(update: &ToolCallUpdate) -> String {
    tool_body(
        update.fields.content.as_deref().unwrap_or_default(),
        update.fields.raw_input.as_ref(),
        update.fields.raw_output.as_ref(),
    )
}

fn format_json(label: &str, value: &serde_json::Value) -> String {
    let value = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    format!("{label}\n{value}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        AuthMethodAgent, ContentChunk, PermissionOptionKind, TextContent, ToolCallUpdateFields,
    };

    #[test]
    fn streaming_chunks_coalesce_by_message() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.on_session_update(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("hello "))).message_id("a"),
        ));
        app.on_session_update(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("world"))).message_id("a"),
        ));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].body, "hello world");
    }

    #[test]
    fn tool_updates_reuse_the_existing_card() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.on_session_update(SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "Read file").status(ToolCallStatus::InProgress),
        ));
        app.on_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tool-1",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .raw_output(serde_json::json!({"ok": true})),
        )));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].tool_status, Some(ToolCallStatus::Completed));
        assert!(app.entries[0].collapsed);
        assert!(app.entries[0].body.contains("true"));
    }

    #[tokio::test]
    async fn permission_keyboard_selection_resolves_request() {
        let mut app = App::new(PathBuf::from("/tmp"));
        let request = RequestPermissionRequest::new(
            "session",
            ToolCallUpdate::new("tool", ToolCallUpdateFields::new().title("Run tests")),
            vec![
                PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
            ],
        );
        let (tx, rx) = oneshot::channel();
        app.set_permission(PendingPermission::from_request(request, tx));
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(rx.await.unwrap().unwrap().0.as_ref(), "deny");
        assert!(app.permission.is_none());
    }

    #[test]
    fn enter_queues_follow_up_while_running() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.phase = Phase::Running;
        app.composer.insert_str("next task");
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.queued.front().map(String::as_str), Some("next task"));
    }

    #[test]
    fn slash_menu_filters_navigates_and_completes() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.phase = Phase::Idle;
        app.insert_composer_text("/");
        assert_eq!(app.command_menu.as_ref().unwrap().matches.len(), 8);

        app.insert_composer_text("mo");
        let menu = app.command_menu.as_ref().unwrap();
        assert_eq!(
            menu.matches
                .iter()
                .map(|index| SLASH_COMMANDS[*index].name)
                .collect::<Vec<_>>(),
            ["model", "mode"]
        );
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.composer_text(), "/mode ");
        assert!(app.command_menu.is_none());
    }

    #[test]
    fn slash_menu_runs_complete_commands_and_prompts_for_required_arguments() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.phase = Phase::Idle;
        app.insert_composer_text("/new");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::NewSession)
        ));

        app.insert_composer_text("/resume");
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert_eq!(app.composer_text(), "/resume ");
        assert!(app.command_menu.is_none());
    }

    #[test]
    fn quit_command_ends_the_app() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.phase = Phase::Idle;
        app.composer.insert_str("/quit");
        let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(Action::Quit)));
        assert!(app.quit);
    }

    #[test]
    fn login_picker_preserves_multiple_provider_auth_methods() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.phase = Phase::Idle;
        app.set_initialized(
            "agent".into(),
            &[
                AuthMethod::Agent(AuthMethodAgent::new("codex:chatgpt", "Codex")),
                AuthMethod::Agent(AuthMethodAgent::new("anthropic:oauth", "Anthropic")),
            ],
        );
        app.composer.insert_str("/login");
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert!(matches!(
            app.overlay,
            Some(Overlay::Picker {
                kind: PickerKind::Authenticate,
                ref options,
                ..
            }) if options.len() == 2
        ));
    }
}
