//! Read-only viewer for a group's shared `context.md` and `summary.md`.
//! Curation is the L2 curator's job; this dialog only displays.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::{centered_rect, DialogResult};
use crate::session::group_context;
use crate::tui::styles::Theme;

#[derive(Debug, PartialEq, Clone, Copy)]
enum Pane {
    Context,
    Summary,
}

pub struct GroupContextDialog {
    group_path: String,
    context_lines: Vec<String>,
    summary_lines: Vec<String>,
    pane: Pane,
    scroll_offset: usize,
}

impl GroupContextDialog {
    pub fn new(profile: &str, group_path: &str) -> Self {
        let context = group_context::read_context(profile, group_path).unwrap_or_default();
        let summary = group_context::read_summary(profile, group_path).unwrap_or_default();
        Self {
            group_path: group_path.to_string(),
            context_lines: context.lines().map(str::to_string).collect(),
            summary_lines: summary.lines().map(str::to_string).collect(),
            pane: Pane::Context,
            scroll_offset: 0,
        }
    }

    fn lines(&self) -> &[String] {
        match self.pane {
            Pane::Context => &self.context_lines,
            Pane::Summary => &self.summary_lines,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        let max = self.lines().len().saturating_sub(1);
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane = match self.pane {
                    Pane::Context => Pane::Summary,
                    Pane::Summary => Pane::Context,
                };
                self.scroll_offset = 0;
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.scroll_offset < max {
                    self.scroll_offset += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                DialogResult::Continue
            }
            KeyCode::PageDown => {
                self.scroll_offset = (self.scroll_offset + 10).min(max);
                DialogResult::Continue
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
                DialogResult::Continue
            }
            KeyCode::Home => {
                self.scroll_offset = 0;
                DialogResult::Continue
            }
            KeyCode::End => {
                self.scroll_offset = max;
                DialogResult::Continue
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => DialogResult::Cancel,
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width = (area.width * 80 / 100).clamp(60, 100);
        let dialog_height = (area.height * 80 / 100).clamp(16, 40);
        let dialog_area = centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let pane_name = match self.pane {
            Pane::Context => "context.md",
            Pane::Summary => "summary.md",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(format!(" group: {}  [{}] ", self.group_path, pane_name))
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let content_area = chunks[0];
        let visible_height = content_area.height as usize;

        let lines = self.lines();
        let styled: Vec<Line> = if lines.is_empty() {
            vec![Line::from(Span::styled(
                "(empty)",
                Style::default().fg(theme.dimmed),
            ))]
        } else {
            lines
                .iter()
                .skip(self.scroll_offset)
                .take(visible_height)
                .map(|l| style_line(l, theme))
                .collect()
        };
        frame.render_widget(
            Paragraph::new(styled).wrap(Wrap { trim: false }),
            content_area,
        );

        let total = lines.len();
        let hint = if total > visible_height {
            format!(
                "Tab switch  j/k scroll  ({}/{})  Esc close",
                (self.scroll_offset + visible_height).min(total),
                total
            )
        } else {
            "Tab switch  j/k scroll  Esc close".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.dimmed),
            )))
            .alignment(Alignment::Center),
            chunks[1],
        );
    }
}

fn style_line(line: &str, theme: &Theme) -> Line<'static> {
    if let Some(h) = line.strip_prefix("## ") {
        Line::from(Span::styled(
            h.to_string(),
            Style::default().fg(theme.accent).bold(),
        ))
    } else if let Some(b) = line.strip_prefix("- ") {
        Line::from(vec![
            Span::styled("  • ".to_string(), Style::default().fg(theme.dimmed)),
            Span::raw(b.to_string()),
        ])
    } else {
        Line::from(line.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn dialog() -> GroupContextDialog {
        GroupContextDialog {
            group_path: "g".into(),
            context_lines: (0..50).map(|i| i.to_string()).collect(),
            summary_lines: vec!["s".into()],
            pane: Pane::Context,
            scroll_offset: 0,
        }
    }

    #[test]
    fn tab_toggles_pane_and_resets_scroll() {
        let mut d = dialog();
        d.scroll_offset = 5;
        let _ = d.handle_key(key(KeyCode::Tab));
        assert_eq!(d.pane, Pane::Summary);
        assert_eq!(d.scroll_offset, 0);
    }

    #[test]
    fn scroll_clamps_at_top_and_bottom() {
        let mut d = dialog();
        let _ = d.handle_key(key(KeyCode::Up));
        assert_eq!(d.scroll_offset, 0);
        let _ = d.handle_key(key(KeyCode::End));
        assert_eq!(d.scroll_offset, 49);
    }

    #[test]
    fn esc_cancels() {
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }
}
