//! Plugin panels dialog: the TUI surface for block-content UI contributions
//! (`dashboard-card`, `session-detail-panel`, `session-detail-header-badge`)
//! and the plugin notification ring. Opens from the command palette; content
//! comes from the cached UI state store, so rendering never waits on a
//! plugin worker.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::plugin::ui::{Block as UiBlock, UiEntry, UiPayload};
use crate::tui::home::render::plugin_severity_color;
use crate::tui::styles::Theme;

pub struct PluginPanelsDialog {
    /// Session whose detail panels show alongside the global cards.
    session: Option<(String, String)>,
    scroll: u16,
}

impl PluginPanelsDialog {
    pub fn new(session: Option<(String, String)>) -> Self {
        Self { session, scroll: 0 }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => DialogResult::Cancel,
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.clamp(40, 100);
        let height = area.height.clamp(12, 32);
        let rect = super::centered_rect(area, width, height);
        f.render_widget(Clear, rect);

        let block = Block::default()
            .title(" Plugin panels ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(rect);
        f.render_widget(block, rect);

        let mut lines: Vec<Line> = Vec::new();

        let cards = crate::plugin::ui::entries(aoe_plugin_api::UiSlot::DashboardCard, None);
        for entry in &cards {
            push_blocks_entry(&mut lines, entry, theme);
        }

        if let Some((session_id, session_title)) = &self.session {
            let badges = crate::plugin::ui::entries(
                aoe_plugin_api::UiSlot::SessionDetailHeaderBadge,
                Some(session_id),
            );
            let panels = crate::plugin::ui::entries(
                aoe_plugin_api::UiSlot::SessionDetailPanel,
                Some(session_id),
            );
            if !badges.is_empty() || !panels.is_empty() {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                let mut header = vec![Span::styled(
                    format!("Session: {session_title}"),
                    Style::default().fg(theme.title).bold(),
                )];
                for badge in &badges {
                    if let UiPayload::Badge { text, severity, .. } = &badge.payload {
                        header.push(Span::styled(
                            format!("  [{text}]"),
                            Style::default().fg(plugin_severity_color(*severity, theme)),
                        ));
                    }
                }
                lines.push(Line::from(header));
                for entry in &panels {
                    push_blocks_entry(&mut lines, entry, theme);
                }
            }
        }

        let notifications = crate::plugin::ui::notifications();
        if !notifications.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                "Notifications",
                Style::default().fg(theme.title).bold(),
            )));
            for n in &notifications {
                let mut spans = vec![
                    Span::styled(
                        format!("\u{2691} {}", n.title),
                        Style::default()
                            .fg(plugin_severity_color(n.severity, theme))
                            .bold(),
                    ),
                    Span::styled(
                        format!("  ({})", n.plugin_id),
                        Style::default().fg(theme.dimmed),
                    ),
                ];
                if !n.body.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", n.body),
                        Style::default().fg(theme.text),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "No plugin panels or notifications. Plugins with ui contributions \
                 populate this view as they push state.",
                Style::default().fg(theme.dimmed),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "j/k scroll · esc close",
            Style::default().fg(theme.dimmed),
        )));

        // Clamp the offset to the content so an over-scroll (the Down key has
        // no content-size context to stop at) never renders an empty body.
        let max_scroll = lines
            .len()
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16;
        let body = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll.min(max_scroll), 0));
        f.render_widget(body, inner);
    }
}

fn push_blocks_entry(lines: &mut Vec<Line<'static>>, entry: &UiEntry, theme: &Theme) {
    let UiPayload::Blocks { severity, blocks } = &entry.payload else {
        return;
    };
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled(
            entry.title.clone(),
            Style::default()
                .fg(plugin_severity_color(*severity, theme))
                .bold(),
        ),
        Span::styled(
            format!("  ({})", entry.plugin_id),
            Style::default().fg(theme.dimmed),
        ),
    ]));
    for block in blocks {
        match block {
            UiBlock::Text { text, severity } => {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(plugin_severity_color(*severity, theme)),
                )));
            }
            UiBlock::Kv { items } => {
                for (k, v) in items {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {k}: "), Style::default().fg(theme.dimmed)),
                        Span::styled(v.clone(), Style::default().fg(theme.text)),
                    ]));
                }
            }
            UiBlock::List { items } => {
                for item in items {
                    lines.push(Line::from(Span::styled(
                        format!("  - {item}"),
                        Style::default().fg(theme.text),
                    )));
                }
            }
            UiBlock::Metric { label, value } => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {value} "),
                        Style::default().fg(theme.accent).bold(),
                    ),
                    Span::styled(label.clone(), Style::default().fg(theme.dimmed)),
                ]));
            }
        }
    }
}
