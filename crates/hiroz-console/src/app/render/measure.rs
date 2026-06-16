//! Measurement panel rendering

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Sparkline, Wrap,
    },
};

use crate::app::App;
use crate::app::state::*;
use crate::core::echo::EchoStatus;

use super::common::*;

impl App {
    /// Get sorted list of measuring topics (ensures consistent ordering)
    pub fn get_sorted_measuring_topics(&self) -> Vec<String> {
        let mut topics: Vec<_> = self.measuring_topics.iter().cloned().collect();
        topics.sort();
        topics
    }

    /// Render list items for measuring topics
    pub fn render_measure_list_items(&self, list_width: usize) -> Vec<ListItem<'static>> {
        if self.measuring_topics.is_empty() {
            return vec![];
        }

        let metrics = self.topic_metrics.lock();
        let topics = self.get_sorted_measuring_topics();

        topics
            .iter()
            .enumerate()
            .map(|(i, topic)| {
                let is_selected = i == self.measure_selected_index;
                let style = list_item_style(is_selected);

                // Get rate info
                let rate_str = if let Some(tm) = metrics.get(topic) {
                    format!(" {:.1} Hz", tm.current_rate)
                } else {
                    String::new()
                };

                // Calculate available space for topic name
                let icon_width = 2; // "# "
                let rate_width = rate_str.len();
                let topic_max_width = list_width.saturating_sub(icon_width + rate_width);
                let display_topic = truncate_with_ellipsis(topic, topic_max_width);

                let mut spans = vec![Span::styled("# ".to_string(), style)];
                spans.push(Span::styled(display_topic, style));
                spans.push(Span::styled(rate_str, Style::default().fg(Color::Cyan)));

                ListItem::new(Line::from(spans))
            })
            .collect()
    }

    /// Render the measurement detail panel (sparklines for selected topic)
    pub fn render_measurement_panel(&self, f: &mut Frame, area: Rect) {
        let measuring_count = self.measuring_topics.len();

        if measuring_count == 0 {
            // No topics being monitored
            let info = Paragraph::new(
                "No topics being monitored.\n\n\
                 Go to Topics tab (press 1) and press 'm' on topics to add them.\n\n\
                 Keybindings:\n\
                   m - Toggle topic in measurement list (in Topics tab)\n\
                   r - Clear all measurements (in Measure tab)\n\
                   4 - Jump to this Measure tab",
            )
            .block(Block::default().title(" Sparklines ").borders(Borders::ALL));
            f.render_widget(info, area);
            return;
        }

        let topics = self.get_sorted_measuring_topics();
        let selected_topic = topics.get(self.measure_selected_index);

        let Some(topic) = selected_topic else {
            return;
        };

        let metrics = self.topic_metrics.lock();
        let Some(tm) = metrics.get(topic) else {
            let info = Paragraph::new("Waiting for data...").block(
                Block::default()
                    .title(format!(" {} ", topic))
                    .borders(Borders::ALL),
            );
            f.render_widget(info, area);
            return;
        };

        // Layout for sparklines
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Summary stats
                Constraint::Min(5),    // Rate sparkline
                Constraint::Min(5),    // Bandwidth sparkline
            ])
            .split(area);

        // Summary stats
        let summary = Paragraph::new(format!(
            "Rate: {:.1} Hz  |  Bandwidth: {:.2} KB/s",
            tm.current_rate, tm.current_bandwidth
        ))
        .block(
            Block::default()
                .title(format!(" {} ", topic))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .style(Style::default().fg(Color::White));
        f.render_widget(summary, chunks[0]);

        // Rate sparkline
        self.render_sparkline_with_axes(
            f,
            chunks[1],
            &tm.rate_history.iter().copied().collect::<Vec<_>>(),
            tm.current_rate,
            "Rate (msg/s)",
            Color::Cyan,
        );

        // Bandwidth sparkline
        self.render_sparkline_with_axes(
            f,
            chunks[2],
            &tm.bandwidth_history.iter().copied().collect::<Vec<_>>(),
            tm.current_bandwidth * 10.0,
            "Bandwidth (KB/s)",
            Color::Yellow,
        );
    }

    /// Render the live message echo column for the selected Measure topic,
    /// similar to `ros2 topic echo <topic>`. Messages are produced off-thread by
    /// the echo worker into a capped scrollback buffer; here we render a window
    /// of it. Focus this pane (`l`) and scroll with j/k, arrows, or Ctrl-U/D.
    pub fn render_echo_panel(&mut self, f: &mut Frame, area: Rect) {
        // Snapshot the shared state, then release the lock before rendering.
        let (topic, type_name, status, msg_count, lines) = {
            let st = self.echo_state.lock();
            (
                st.topic.clone(),
                st.type_name.clone(),
                st.status.clone(),
                st.msg_count,
                st.lines.iter().cloned().collect::<Vec<_>>(),
            )
        };

        let inner_height = area.height.saturating_sub(2) as usize; // minus borders
        self.echo_viewport = inner_height;

        let is_focused = self.focus_pane == FocusPane::Detail;
        let border_color = if matches!(status, EchoStatus::Error(_)) {
            Color::Red
        } else if is_focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        // Status-only states have no buffer to scroll yet.
        let status_body = match &status {
            EchoStatus::Idle => Some(
                "Select a topic (j/k) to echo its messages.\n\nPress 'l' to focus this pane, \
                 then j/k or ↑/↓ to scroll, Ctrl-U/Ctrl-D to page."
                    .to_string(),
            ),
            EchoStatus::Discovering => Some(format!(
                "{}\n\nDiscovering message schema...",
                topic.as_deref().unwrap_or("")
            )),
            EchoStatus::NoData => Some(format!(
                "{}\n\nWaiting for messages...",
                type_name.as_deref().unwrap_or("")
            )),
            EchoStatus::Error(e) => Some(format!(
                "Unable to echo this topic:\n  {e}\n\n\
                 No type description service and no local .msg definition \
                 ($AMENT_PREFIX_PATH) for this type."
            )),
            EchoStatus::Active => None,
        };

        if let Some(body) = status_body {
            let para = Paragraph::new(body)
                .block(
                    Block::default()
                        .title(" Echo ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(border_color)),
                )
                .wrap(Wrap { trim: false });
            f.render_widget(para, area);
            return;
        }

        // Wrap each buffered line to the column width so long values are fully
        // visible on narrow windows. Wrapping into concrete display rows keeps
        // scrolling exact (one display row == one screen row).
        let inner_width = area.width.saturating_sub(2).max(1) as usize;
        let display: Vec<String> = lines
            .iter()
            .flat_map(|l| wrap_to_width(l, inner_width))
            .collect();

        let total = display.len();
        let max_scroll = total.saturating_sub(inner_height);
        if self.echo_follow {
            self.echo_scroll = max_scroll;
        } else {
            self.echo_scroll = self.echo_scroll.min(max_scroll);
            if self.echo_scroll >= max_scroll {
                // Scrolled back down to the bottom — resume following the tail.
                self.echo_follow = true;
            }
        }

        let start = self.echo_scroll.min(total);
        let end = (start + inner_height).min(total);
        let visible: Vec<Line> = display[start..end]
            .iter()
            .map(|l| Line::raw(l.clone()))
            .collect();

        let follow_tag = if self.echo_follow { "live" } else { "scroll" };
        let title = format!(
            " Echo: {} [{}/{} msgs:{} {}] ",
            topic.as_deref().unwrap_or(""),
            end,
            total,
            msg_count,
            follow_tag
        );

        let para = Paragraph::new(visible).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        );
        f.render_widget(para, area);

        // Scrollbar when the buffer exceeds the viewport.
        if total > inner_height {
            let mut sb_state = ScrollbarState::new(max_scroll).position(self.echo_scroll);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("^"))
                    .end_symbol(Some("v")),
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut sb_state,
            );
        }
    }

    /// Render a sparkline with Y-axis labels and X-axis time indicator
    fn render_sparkline_with_axes(
        &self,
        f: &mut Frame,
        area: Rect,
        data: &[u64],
        current_value: f64,
        title: &str,
        color: Color,
    ) {
        if data.is_empty() {
            let empty = Paragraph::new("Waiting for data...").block(
                Block::default()
                    .title(format!(" {} ", title))
                    .borders(Borders::ALL),
            );
            f.render_widget(empty, area);
            return;
        }

        // Layout: [Y-axis labels (6 chars)] [Sparkline]
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(7), // Y-axis labels
                Constraint::Min(10),   // Sparkline area
            ])
            .split(area);

        // Calculate max for scaling
        let max_val = data.iter().max().copied().unwrap_or(1).max(1);
        let scale_factor = if title.contains("KB/s") { 10.0 } else { 1.0 };

        // Y-axis labels
        let y_axis_text = format!("{:>5.1}\n\n\n{:>5.1}", max_val as f64 / scale_factor, 0.0);
        let y_axis = Paragraph::new(y_axis_text)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Right);
        f.render_widget(y_axis, h_chunks[0]);

        // Sparkline area with block
        let sparkline_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // Sparkline
                Constraint::Length(2), // X-axis (tick marks + labels)
            ])
            .split(h_chunks[1]);

        // Title with current value
        let display_value = current_value / scale_factor;
        let title_text = format!(
            " {} : {:.1} (max: {:.1}) ",
            title,
            display_value,
            max_val as f64 / scale_factor
        );

        let sparkline = Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::RIGHT | Borders::LEFT)
                    .title(title_text)
                    .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
            )
            .data(data)
            .max(max_val)
            .style(Style::default().fg(color));
        f.render_widget(sparkline, sparkline_chunks[0]);

        // X-axis with per-second tick marks
        let width = sparkline_chunks[1].width as usize;

        // Build tick marks - one per ~10 seconds for readability
        let mut axis_chars = String::new();
        axis_chars.push('└');

        let tick_interval = if width > 60 { 10 } else { 15 }; // seconds per tick
        let chars_per_sec = (width.saturating_sub(2)) as f64 / HISTORY_LENGTH as f64;

        for i in 0..width.saturating_sub(2) {
            let sec = (i as f64 / chars_per_sec) as usize;
            if sec.is_multiple_of(tick_interval) && i > 0 {
                axis_chars.push('┴');
            } else {
                axis_chars.push('─');
            }
        }
        axis_chars.push('┘');

        // Time labels below axis
        let label_line = format!(
            " {}s{:width$}now",
            HISTORY_LENGTH,
            "",
            width = width.saturating_sub(8)
        );

        let x_axis = Paragraph::new(vec![
            Line::from(Span::styled(
                axis_chars,
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                label_line,
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(x_axis, sparkline_chunks[1]);
    }
}

/// Hard-wrap a line to at most `width` characters per row, so long message
/// values are fully visible in the (narrow) echo column. Returns one row when
/// it already fits.
fn wrap_to_width(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.chars().count() <= width {
        return vec![line.to_string()];
    }
    line.chars()
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}
