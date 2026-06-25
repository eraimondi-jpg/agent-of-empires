//! Viewer for a group's shared `context.md` and `summary.md`. The `context.md`
//! pane is editable (Enter to edit, Ctrl-S to save, Esc to cancel); `summary.md`
//! stays read-only since its curation is the L2 curator's job.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use ratatui_textarea::TextArea;

use super::{centered_rect, DialogResult};
use crate::session::group_context;
use crate::tui::styles::Theme;

#[derive(Debug, PartialEq, Clone, Copy)]
enum Pane {
    Context,
    Summary,
}

pub struct GroupContextDialog {
    profile: String,
    group_path: String,
    context_lines: Vec<String>,
    summary_lines: Vec<String>,
    pane: Pane,
    scroll_offset: usize,
    // Some while editing the context pane; None in view mode.
    editor: Option<TextArea<'static>>,
}

impl GroupContextDialog {
    pub fn new(profile: &str, group_path: &str) -> Self {
        let context = group_context::read_context(profile, group_path).unwrap_or_default();
        let summary = group_context::read_summary(profile, group_path).unwrap_or_default();
        Self {
            profile: profile.to_string(),
            group_path: group_path.to_string(),
            context_lines: context.lines().map(str::to_string).collect(),
            summary_lines: summary.lines().map(str::to_string).collect(),
            pane: Pane::Context,
            scroll_offset: 0,
            editor: None,
        }
    }

    fn lines(&self) -> &[String] {
        match self.pane {
            Pane::Context => &self.context_lines,
            Pane::Summary => &self.summary_lines,
        }
    }

    fn enter_edit_mode(&mut self) {
        let lines: Vec<String> = if self.context_lines.is_empty() {
            vec![String::new()]
        } else {
            self.context_lines.clone()
        };
        let mut editor = TextArea::new(lines);
        editor.set_cursor_line_style(Style::default());
        self.editor = Some(editor);
    }

    fn save_edit(&mut self) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let text = editor.lines().join("\n");
        if group_context::write_context(&self.profile, &self.group_path, &text).is_ok() {
            let saved =
                group_context::read_context(&self.profile, &self.group_path).unwrap_or(text);
            self.context_lines = saved.lines().map(str::to_string).collect();
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        // In edit mode the textarea owns every key except the two control keys.
        if self.editor.is_some() {
            match key.code {
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.save_edit();
                }
                KeyCode::Esc => {
                    self.editor = None;
                }
                _ => {
                    if let Some(editor) = self.editor.as_mut() {
                        editor.input(key);
                    }
                }
            }
            return DialogResult::Continue;
        }

        let max = self.lines().len().saturating_sub(1);
        match key.code {
            KeyCode::Enter if self.pane == Pane::Context => {
                self.enter_edit_mode();
                DialogResult::Continue
            }
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

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(editor) = self.editor.as_mut() {
            editor.insert_str(text);
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width = (area.width * 80 / 100).clamp(60, 100);
        let dialog_height = (area.height * 80 / 100).clamp(16, 40);
        let dialog_area = centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let editing = self.editor.is_some();
        let pane_name = match (self.pane, editing) {
            (Pane::Context, true) => "context.md - EDITING",
            (Pane::Context, false) => "context.md",
            (Pane::Summary, _) => "summary.md",
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

        if editing {
            self.render_editor(frame, content_area, theme);
        } else {
            self.render_view(frame, content_area, theme);
        }

        let hint = self.footer_hint(content_area.height as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.dimmed),
            )))
            .alignment(Alignment::Center),
            chunks[1],
        );
    }

    fn render_view(&self, frame: &mut Frame, content_area: Rect, theme: &Theme) {
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
    }

    fn render_editor(&self, frame: &mut Frame, content_area: Rect, theme: &Theme) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let mut editor = editor.clone();
        editor.set_style(Style::default().fg(theme.text));
        editor.set_cursor_style(Style::default().fg(theme.background).bg(theme.accent));
        frame.render_widget(&editor, content_area);
        if content_area.width > 0 && content_area.height > 0 {
            let cursor = editor.screen_cursor();
            let max_x = content_area
                .x
                .saturating_add(content_area.width.saturating_sub(1));
            let max_y = content_area
                .y
                .saturating_add(content_area.height.saturating_sub(1));
            let cursor_x = content_area.x.saturating_add(cursor.col as u16).min(max_x);
            let cursor_y = content_area.y.saturating_add(cursor.row as u16).min(max_y);
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
    }

    fn footer_hint(&self, visible_height: usize) -> String {
        if self.editor.is_some() {
            return "Ctrl-S save  Esc cancel".to_string();
        }
        let total = self.lines().len();
        let edit_hint = if self.pane == Pane::Context {
            "Enter edit  "
        } else {
            ""
        };
        if total > visible_height {
            format!(
                "{edit_hint}Tab switch  j/k scroll  ({}/{})  Esc close",
                (self.scroll_offset + visible_height).min(total),
                total
            )
        } else {
            format!("{edit_hint}Tab switch  j/k scroll  Esc close")
        }
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
            profile: "default".into(),
            group_path: "g".into(),
            context_lines: (0..50).map(|i| i.to_string()).collect(),
            summary_lines: vec!["s".into()],
            pane: Pane::Context,
            scroll_offset: 0,
            editor: None,
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

    #[test]
    fn enter_on_context_pane_enters_edit_mode() {
        let mut d = dialog();
        let r = d.handle_key(key(KeyCode::Enter));
        assert!(matches!(r, DialogResult::Continue));
        assert!(d.editor.is_some());
    }

    #[test]
    fn enter_on_summary_pane_does_not_edit() {
        let mut d = dialog();
        d.pane = Pane::Summary;
        let r = d.handle_key(key(KeyCode::Enter));
        // Summary pane treats Enter as a close request, never an edit.
        assert!(matches!(r, DialogResult::Cancel));
        assert!(d.editor.is_none());
    }

    #[test]
    fn esc_cancels_edit_mode_without_closing() {
        let mut d = dialog();
        let _ = d.handle_key(key(KeyCode::Enter));
        assert!(d.editor.is_some());
        let r = d.handle_key(key(KeyCode::Esc));
        // Esc leaves edit mode but keeps the dialog open.
        assert!(matches!(r, DialogResult::Continue));
        assert!(d.editor.is_none());
    }
}
