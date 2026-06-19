//! Plugin manager: list, enable/disable, install, update, and uninstall
//! plugins from the TUI (#268). The TUI twin of `aoe plugin` and the web
//! Plugins tab. Installs are two-phase like every other surface: the first
//! pass captures the declared capability set without writing anything, a
//! confirm screen shows it (with the honest no-sandbox wording), and only an
//! explicit approval re-runs the install with consent.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::{centered_rect, DialogResult};
use crate::plugin::grants::GrantStatus;
use crate::plugin::install::{self, InstallOutcome, InstallPrompt};
use crate::plugin::TrustLevel;
use crate::tui::styles::Theme;

enum Mode {
    Browse,
    /// Typing an install source (`owner/repo` or a local path).
    InstallInput,
    /// GitHub topic-search results; fetched only on the explicit 'd' press.
    Discover,
    /// Showing a captured capability prompt awaiting approval.
    ConfirmCaps {
        action: PendingAction,
        summary: Vec<String>,
    },
}

#[derive(Clone)]
enum PendingAction {
    Install(String),
    Update(String),
}

pub struct PluginManagerDialog {
    /// The shared manager view-model, the same shape the web dashboard renders
    /// from (`crate::plugin::view`). Built straight off the registry, so the
    /// TUI never re-derives plugin fields.
    rows: Vec<crate::plugin::PluginView>,
    load_errors: Vec<String>,
    selected: usize,
    mode: Mode,
    install_input: Input,
    error: Option<String>,
    info: Option<String>,
    /// Filled by the explicit 'c' check; does network IO per GitHub plugin,
    /// so never on open.
    update_status: std::collections::HashMap<String, crate::plugin::update_check::UpdateStatus>,
    /// Filled by the explicit 'd' discover search; never fetched on open.
    discovered: Vec<crate::plugin::discover::DiscoveredPlugin>,
    discover_selected: usize,
    /// Set whenever the on-disk plugin config changed (enable/disable, install,
    /// update, uninstall). An embedding surface drains it via [`take_mutated`]
    /// to re-sync its own config view; the standalone modal ignores it.
    mutated: bool,
    /// True when hosted inside the settings screen (vs the command-palette
    /// modal). Only changes the footer and per-row hints: Enter opens a
    /// plugin's settings and Esc returns to the category list.
    embedded: bool,
}

impl Default for PluginManagerDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginManagerDialog {
    pub fn new() -> Self {
        let mut dialog = Self {
            rows: Vec::new(),
            load_errors: Vec::new(),
            selected: 0,
            mode: Mode::Browse,
            install_input: Input::default(),
            error: None,
            info: None,
            update_status: std::collections::HashMap::new(),
            discovered: Vec::new(),
            discover_selected: 0,
            mutated: false,
            embedded: false,
        };
        dialog.reload();
        dialog
    }

    /// A manager hosted inside the settings screen rather than the command
    /// palette. Only the footer and per-row hints differ: Enter opens a
    /// plugin's settings and Esc returns to the category list.
    pub fn embedded() -> Self {
        let mut dialog = Self::new();
        dialog.embedded = true;
        dialog
    }

    /// Whether the manager is on the plugin list (not typing an install path,
    /// browsing discovery, or approving capabilities). The settings host only
    /// repurposes Enter as "open settings" while browsing.
    pub fn is_browsing(&self) -> bool {
        matches!(self.mode, Mode::Browse)
    }

    /// Whether the manager is capturing free text (the install-source input),
    /// so an embedding surface treats it as an editing mode for paste/focus.
    pub fn is_capturing_input(&self) -> bool {
        matches!(self.mode, Mode::InstallInput)
    }

    /// Take and clear the "config mutated" flag (enable/disable/install/
    /// update/uninstall wrote to disk and reloaded the registry).
    pub fn take_mutated(&mut self) -> bool {
        std::mem::take(&mut self.mutated)
    }

    fn reload(&mut self) {
        // reload() runs only after a config-mutating action (and once at
        // construction), so it is the single place to flag a mutation.
        self.mutated = true;
        let registry = crate::plugin::reload_registry();
        self.rows = registry.all().iter().map(|p| p.view()).collect();
        self.load_errors = registry.load_errors().to_vec();
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// Run an install/update, capturing the capability prompt on the first
    /// (unconfirmed) pass. `approved` re-runs with consent.
    fn run_pending(&mut self, action: PendingAction, approved: bool) {
        let mut captured: Option<Vec<String>> = None;
        let mut confirm = |prompt: &InstallPrompt| {
            if approved {
                true
            } else {
                captured = Some(prompt_summary(prompt));
                false
            }
        };
        let outcome = match &action {
            PendingAction::Install(source) => install::parse_source(source)
                .and_then(|source| install::install(source, &mut confirm)),
            PendingAction::Update(id) => install::update(id, &mut confirm),
        };
        match outcome {
            Ok(InstallOutcome::Declined) => match captured {
                Some(summary) => {
                    self.mode = Mode::ConfirmCaps { action, summary };
                }
                None => {
                    self.mode = Mode::Browse;
                }
            },
            Ok(InstallOutcome::Installed { id, version }) => {
                self.info = Some(format!("Installed {id} {version}"));
                self.mode = Mode::Browse;
                self.reload();
            }
            Ok(InstallOutcome::Updated { id, version }) => {
                self.info = Some(format!("Updated {id} to {version}"));
                self.mode = Mode::Browse;
                self.reload();
            }
            Ok(InstallOutcome::UpToDate { id, version }) => {
                self.info = Some(format!("{id} is already up to date ({version})"));
                self.mode = Mode::Browse;
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.mode = Mode::Browse;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        self.info = None;
        match &self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::InstallInput => self.handle_install_input_key(key),
            Mode::Discover => self.handle_discover_key(key),
            Mode::ConfirmCaps { .. } => self.handle_confirm_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected + 1).min(self.rows.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(row) = self.rows.get(self.selected) {
                    let (id, enabled) = (row.id.clone(), row.enabled);
                    match install::set_enabled(&id, !enabled) {
                        Ok(()) => {
                            self.info = Some(format!(
                                "{} {id}",
                                if enabled { "Disabled" } else { "Enabled" }
                            ));
                            self.error = None;
                            self.reload();
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
                DialogResult::Continue
            }
            KeyCode::Char('i') => {
                self.mode = Mode::InstallInput;
                self.install_input = Input::default();
                self.error = None;
                DialogResult::Continue
            }
            KeyCode::Char('u') => {
                if let Some(row) = self.rows.get(self.selected) {
                    if row.builtin {
                        self.error = Some("Builtin plugins update with the aoe binary.".into());
                    } else {
                        let action = PendingAction::Update(row.id.clone());
                        self.error = None;
                        self.run_pending(action, false);
                    }
                }
                DialogResult::Continue
            }
            KeyCode::Char('c') => {
                // Blocks like install: one git ls-remote per GitHub plugin.
                match crate::plugin::update_check::check_all() {
                    Ok(statuses) => {
                        let available = statuses
                            .iter()
                            .filter(|(_, s)| {
                                *s == crate::plugin::update_check::UpdateStatus::Available
                            })
                            .count();
                        self.update_status = statuses.into_iter().collect();
                        self.info = Some(match available {
                            0 => "All community plugins are up to date.".into(),
                            n => format!("{n} plugin(s) can be updated; press u to update."),
                        });
                        self.error = None;
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
                DialogResult::Continue
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(row) = self.rows.get(self.selected) {
                    if row.builtin {
                        self.error =
                            Some("Builtin plugins cannot be uninstalled; disable instead.".into());
                    } else {
                        match install::uninstall(&row.id) {
                            Ok(()) => {
                                self.info = Some(format!("Uninstalled {}", row.id));
                                self.error = None;
                                self.reload();
                            }
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                }
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                // Blocks like 'c': one GitHub search request, explicit
                // user action only; discovery never runs on open.
                match crate::plugin::discover::discover_blocking() {
                    Ok(found) => {
                        self.info = Some(format!("{} plugin repositories found.", found.len()));
                        self.discovered = found;
                        self.discover_selected = 0;
                        self.error = None;
                        self.mode = Mode::Discover;
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_discover_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.discovered.is_empty() {
                    self.discover_selected =
                        (self.discover_selected + 1).min(self.discovered.len() - 1);
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.discover_selected = self.discover_selected.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                if let Some(plugin) = self.discovered.get(self.discover_selected) {
                    if plugin.installed {
                        self.info = Some(format!("{} is already installed.", plugin.slug));
                    } else {
                        // Same two-phase flow as a typed install: the
                        // capability prompt still gates anything unvetted.
                        let action = PendingAction::Install(plugin.slug.clone());
                        self.error = None;
                        self.run_pending(action, false);
                    }
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_install_input_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                DialogResult::Continue
            }
            KeyCode::Enter => {
                let source = self.install_input.value().trim().to_string();
                if source.is_empty() {
                    self.mode = Mode::Browse;
                } else {
                    // Blocks on git clone for slug installs; small repos, a
                    // few seconds at worst, and the dialog reports failure.
                    self.run_pending(PendingAction::Install(source), false);
                }
                DialogResult::Continue
            }
            _ => {
                self.install_input
                    .handle_event(&crossterm::event::Event::Key(key));
                DialogResult::Continue
            }
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Mode::ConfirmCaps { action, .. } =
                    std::mem::replace(&mut self.mode, Mode::Browse)
                {
                    self.run_pending(action, true);
                }
                DialogResult::Continue
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                self.info = Some("Install cancelled; nothing was written.".into());
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// The currently selected plugin row, if any. Lets an embedding surface
    /// (the settings Plugins tab) read the selection, e.g. to decide whether
    /// to drill into the plugin's settings.
    pub fn selected(&self) -> Option<&crate::plugin::PluginView> {
        self.rows.get(self.selected)
    }

    /// Reflect a staged enable/disable in the displayed list without touching
    /// disk or the registry. The settings host stages the change in its own
    /// config and persists it on save, so the row shows the pending state
    /// immediately while still following the normal save flow.
    pub fn set_row_enabled(&mut self, id: &str, enabled: bool) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
            row.enabled = enabled;
            row.active = enabled && row.grant == GrantStatus::Granted;
        }
    }

    /// Render as a centered modal (the command-palette surface): clears a
    /// clamped sub-rect and draws into it.
    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.clamp(40, 100);
        let height = area.height.clamp(12, 28);
        let rect = centered_rect(area, width, height);
        f.render_widget(Clear, rect);
        self.render_into(f, rect, theme);
    }

    /// Render directly into the given rect, no centering or clearing, for
    /// embedding in the settings screen's Plugins category. Same manager, same
    /// state, same key handler; only the framing differs.
    pub fn render_inline(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        self.render_into(f, area, theme);
    }

    fn render_into(&self, f: &mut Frame, rect: Rect, theme: &Theme) {
        let title = match self.mode {
            Mode::Browse => " Plugins ",
            Mode::InstallInput => " Install plugin ",
            Mode::Discover => " Discover plugins ",
            Mode::ConfirmCaps { .. } => " Approve capabilities ",
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(rect);
        f.render_widget(block, rect);

        match &self.mode {
            Mode::Browse => self.render_browse(f, inner, theme),
            Mode::InstallInput => self.render_install_input(f, inner, theme),
            Mode::Discover => self.render_discover(f, inner, theme),
            Mode::ConfirmCaps { summary, .. } => self.render_confirm(f, inner, theme, summary),
        }
    }

    fn render_browse(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(if self.load_errors.is_empty() { 0 } else { 2 }),
                Constraint::Length(2),
            ])
            .split(area);

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| {
                let state = if !row.enabled {
                    ("disabled", theme.dimmed)
                } else if row.grant != GrantStatus::Granted {
                    ("needs grant", theme.error)
                } else {
                    ("enabled", theme.running)
                };
                let trust = match row.trust {
                    TrustLevel::Builtin => "builtin",
                    TrustLevel::Community => "community",
                };
                let mut spans = Vec::new();
                // In the settings host, a leading mark flags plugins whose
                // Enter opens settings (the footer ties "*" to it). A fixed
                // 2-col slot keeps names aligned with non-configurable rows,
                // and being leading it never clips in the narrow settings pane.
                if self.embedded {
                    let mark = if row.setting_count > 0 { "* " } else { "  " };
                    spans.push(Span::styled(mark, Style::default().fg(theme.accent)));
                }
                spans.push(Span::styled(
                    format!("{:<28}", format!("{} v{}", row.name, row.version)),
                    Style::default().fg(theme.text),
                ));
                spans.push(Span::styled(
                    format!("{trust:<10}"),
                    Style::default().fg(theme.dimmed),
                ));
                spans.push(Span::styled(
                    format!("{:<12}", state.0),
                    Style::default().fg(state.1),
                ));
                spans.push(Span::styled(
                    row.source.describe(),
                    Style::default().fg(theme.dimmed),
                ));
                if self.update_status.get(&row.id)
                    == Some(&crate::plugin::update_check::UpdateStatus::Available)
                {
                    spans.push(Span::styled(
                        "  update available",
                        Style::default().fg(theme.accent),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(if self.rows.is_empty() {
            None
        } else {
            Some(self.selected)
        });
        f.render_stateful_widget(list, chunks[0], &mut state);

        if !self.load_errors.is_empty() {
            let errors = Paragraph::new(self.load_errors.join("; "))
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            f.render_widget(errors, chunks[1]);
        }

        let status = self
            .error
            .as_deref()
            .map(|e| (e, theme.error))
            .or(self.info.as_deref().map(|i| (i, theme.running)));
        let footer = match status {
            Some((message, color)) => Paragraph::new(message.to_string())
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: true }),
            None => Paragraph::new(if self.embedded {
                "space toggle · enter opens settings (*) · i install · d discover · u update · c check updates · x uninstall · esc back"
            } else {
                "space/enter toggle · i install · d discover · u update · c check updates · x uninstall · esc close"
            })
            .style(Style::default().fg(theme.dimmed)),
        };
        f.render_widget(footer, chunks[2]);
    }

    fn render_discover(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);

        let items: Vec<ListItem> = self
            .discovered
            .iter()
            .map(|plugin| {
                let (badge, color) = if plugin.installed {
                    ("installed", theme.dimmed)
                } else if plugin.featured {
                    ("featured", theme.running)
                } else {
                    ("unvetted", theme.error)
                };
                let spans = vec![
                    Span::styled(
                        format!("{:<32}", plugin.slug),
                        Style::default().fg(theme.text),
                    ),
                    Span::styled(format!("{badge:<11}"), Style::default().fg(color)),
                    Span::styled(
                        format!("{:>5}* ", plugin.stars),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled(
                        plugin.description.clone().unwrap_or_default(),
                        Style::default().fg(theme.dimmed),
                    ),
                ];
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(if self.discovered.is_empty() {
            None
        } else {
            Some(self.discover_selected)
        });
        f.render_stateful_widget(list, chunks[0], &mut state);

        let status = self
            .error
            .as_deref()
            .map(|e| (e, theme.error))
            .or(self.info.as_deref().map(|i| (i, theme.running)));
        let footer = match status {
            Some((message, color)) => Paragraph::new(message.to_string())
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: true }),
            None => Paragraph::new(
                "Plugins not marked featured are unvetted community code. \
                 enter/i install · j/k move · esc back",
            )
            .style(Style::default().fg(theme.dimmed)),
        };
        f.render_widget(footer, chunks[1]);
    }

    fn render_install_input(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);
        let help = Paragraph::new(
            "GitHub slug (owner/repo) or a local directory containing aoe-plugin.toml. \
             Capabilities are shown for approval before anything is written.",
        )
        .style(Style::default().fg(theme.dimmed))
        .wrap(Wrap { trim: true });
        f.render_widget(help, chunks[0]);
        let input = Paragraph::new(format!("> {}", self.install_input.value()))
            .style(Style::default().fg(theme.text));
        f.render_widget(input, chunks[1]);
        if let Some(error) = &self.error {
            let err = Paragraph::new(error.clone())
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            f.render_widget(err, chunks[2]);
        }
    }

    fn render_confirm(&self, f: &mut Frame, area: Rect, theme: &Theme, summary: &[String]) {
        let mut lines: Vec<Line> = summary
            .iter()
            .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.text))))
            .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "y/enter approve and continue · n/esc cancel",
            Style::default().fg(theme.dimmed),
        )));
        let body = Paragraph::new(lines).wrap(Wrap { trim: false });
        f.render_widget(body, area);
    }
}

fn prompt_summary(prompt: &InstallPrompt) -> Vec<String> {
    let mut lines = vec![format!(
        "{} v{} ({})",
        prompt.name,
        prompt.version,
        prompt.source.describe()
    )];
    if !prompt.description.is_empty() {
        lines.push(prompt.description.clone());
    }
    match prompt.featured {
        crate::plugin::featured::FeaturedValidation::Verified => {
            lines.push("Featured: release matches its maintainer-validated hash.".into());
        }
        crate::plugin::featured::FeaturedValidation::UnknownVersion => {
            lines.push(format!(
                "Featured, but v{} has no validated hash yet (unvalidated).",
                prompt.version
            ));
        }
        crate::plugin::featured::FeaturedValidation::NotFeatured => {}
    }
    lines.push(String::new());
    if let Some(previous) = &prompt.previous_capabilities {
        lines.push(format!(
            "Capability change. Previously granted: {}",
            if previous.is_empty() {
                "none".to_string()
            } else {
                previous
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    if prompt.capabilities.is_empty() {
        lines.push("Requests no runtime capabilities (declarative contributions only).".into());
    } else {
        lines.push("Requests capabilities:".into());
        for cap in &prompt.capabilities {
            lines.push(format!("  - {}", cap.as_str()));
        }
    }
    if !prompt.core_default_overrides.is_empty() {
        lines.push(String::new());
        lines.push("Changes the default of these core settings:".into());
        for ov in &prompt.core_default_overrides {
            lines.push(format!("  - {ov}"));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "Capability gating limits what the plugin can ask aoe to do; it is not an OS \
         sandbox. This plugin {}.",
        crate::plugin::sandbox::backend().isolation_summary()
    ));
    lines
}
