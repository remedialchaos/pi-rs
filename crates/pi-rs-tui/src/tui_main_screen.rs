//! Differential renderer for the primary screen and scrollback.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use indexmap::IndexSet;

use crate::terminal::{Terminal, TerminalEvent};
use crate::terminal_image::{delete_kitty_image, get_kitty_image_placement_rows, is_image_line};
use crate::tui::{
    Component, Tui, TuiBase, TuiMode, TuiStopOptions, extract_cursor_position, reset_line,
    resolve_fake_cursors, strip_prompt_markers,
};
use crate::utils::visible_width;

pub(crate) const SYNC_ON: &str = "\x1b[?2026h";
pub(crate) const SYNC_OFF: &str = "\x1b[?2026l";
const CLEAR_LINE: &str = "\x1b[2K";
const MAX_WRITE: usize = 1024 * 1024;

pub(crate) struct Output<'a> {
    terminal: &'a mut dyn Terminal,
    chunk: String,
}

impl<'a> Output<'a> {
    pub(crate) fn new(terminal: &'a mut dyn Terminal) -> Self {
        Self {
            terminal,
            chunk: String::new(),
        }
    }

    pub(crate) fn push(&mut self, mut text: &str) {
        while text.len() > MAX_WRITE - self.chunk.len() {
            let mut end = MAX_WRITE - self.chunk.len();
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.chunk.push_str(&text[..end]);
            self.flush();
            text = &text[end..];
        }
        self.chunk.push_str(text);
    }

    pub(crate) fn flush(&mut self) {
        if !self.chunk.is_empty() {
            self.terminal.write(&self.chunk);
            self.chunk.clear();
        }
    }

    fn move_rows(&mut self, delta: isize) {
        if delta != 0 {
            self.push(&format!(
                "\x1b[{}{}",
                delta.unsigned_abs(),
                if delta > 0 { 'B' } else { 'A' }
            ));
        }
    }

    fn clear_images(&mut self, ids: IndexSet<u32>) {
        for id in ids {
            self.push(&delete_kitty_image(id));
        }
    }
}

fn image_ids(lines: &[String]) -> IndexSet<u32> {
    let mut ids = IndexSet::new();
    for line in lines {
        for (index, seq) in line.match_indices("\x1b_G") {
            let start = index + seq.len();
            let Some((controls, _)) = line[start..].split_once(';') else {
                continue;
            };
            for control in controls.split(',') {
                if let Some(id) = control.strip_prefix("i=").and_then(|id| id.parse().ok()) {
                    ids.insert(id);
                }
            }
        }
    }
    ids
}

fn reserved_rows(lines: &[String], index: usize, last: usize) -> usize {
    let rows = get_kitty_image_placement_rows(&lines[index])
        .unwrap_or(1)
        .max(1);
    if rows == 1 {
        return 1;
    }
    let end = index.saturating_add(rows).min(last + 1);
    1 + lines[index + 1..end]
        .iter()
        .take_while(|line| !is_image_line(line) && visible_width(line) == 0)
        .count()
}

fn deltas(old: &[String], new: &[String]) -> Option<(usize, usize)> {
    let count = old.len().max(new.len());
    let changed =
        |i| old.get(i).map_or("", String::as_str) != new.get(i).map_or("", String::as_str);
    let first = (0..count)
        .find(|&i| changed(i))
        .or_else(|| (new.len() > old.len()).then_some(old.len()))?;
    let last = if new.len() > old.len() {
        new.len() - 1
    } else {
        (first..count).rfind(|&i| changed(i)).unwrap_or(first)
    };
    let mut expanded = (first, last);
    for lines in [old, new] {
        for (i, line) in lines.iter().enumerate() {
            if !line.contains("\x1b_G") || !line.contains(",i=") {
                continue;
            }
            let end = i + reserved_rows(lines, i, lines.len() - 1) - 1;
            if i >= first || (i <= last && end >= first) {
                expanded = (expanded.0.min(i), expanded.1.max(end));
            }
        }
    }
    Some(expanded)
}

#[derive(Clone, Debug, Default)]
pub struct TuiMainScreenRenderState {
    pub previous_lines: Vec<String>,
    pub previous_width: usize,
    pub previous_height: usize,
    pub cursor_row: usize,
    pub hardware_cursor_row: usize,
    pub max_lines_rendered: usize,
    pub previous_viewport_top: usize,
}

pub struct TuiMainScreen {
    pub base: TuiBase,
    state: TuiMainScreenRenderState,
    log_directory: Option<PathBuf>,
    force_clear: bool,
}

impl TuiMainScreen {
    pub fn new(
        terminal: impl Terminal + 'static,
        hardware_cursor: bool,
        log_directory: Option<PathBuf>,
    ) -> Self {
        Self {
            base: TuiBase::new(Box::new(terminal), hardware_cursor),
            state: TuiMainScreenRenderState::default(),
            log_directory,
            force_clear: false,
        }
    }

    pub fn capture_render_state(&self) -> TuiMainScreenRenderState {
        self.state.clone()
    }
    pub fn restore_render_state(&mut self, mut state: TuiMainScreenRenderState) {
        for line in &mut state.previous_lines {
            if is_image_line(line) {
                line.clear();
            }
        }
        self.state = state;
    }

    pub fn step(&mut self, timeout: Duration) -> io::Result<()> {
        for event in self
            .base
            .terminal
            .poll_input(timeout.min(Duration::from_millis(16)))?
        {
            match event {
                TerminalEvent::Input(input) => self.base.handle_input(&input),
                TerminalEvent::Resize { .. } => self.base.request_render(false),
            }
        }
        let now = Instant::now();
        self.base.tick_components(now);
        self.base.expire_color_queries(now);
        if self.base.render_due(now) {
            self.render_now(false)?;
        }
        Ok(())
    }

    fn full_redraw(
        &mut self,
        lines: Vec<String>,
        cursor: Option<(usize, usize)>,
        width: usize,
        height: usize,
        clear: bool,
    ) {
        self.base.full_redraws += 1;
        let mut output = Output::new(self.base.terminal.as_mut());
        output.push(SYNC_ON);
        if clear {
            output.clear_images(image_ids(&self.state.previous_lines));
            output.push("\x1b[2J\x1b[H\x1b[3J");
        }
        let mut i = 0;
        while i < lines.len() {
            if i > 0 {
                output.push("\r\n");
            }
            let count = reserved_rows(&lines, i, lines.len() - 1);
            if count > 1 && count <= height {
                for _ in 1..count {
                    output.push("\r\n");
                }
                output.move_rows(1 - count as isize);
                output.push(&lines[i]);
                output.move_rows(count as isize - 1);
                i += count;
            } else {
                output.push(&lines[i]);
                i += 1;
            }
        }
        output.push(SYNC_OFF);
        output.flush();
        self.state.cursor_row = lines.len().saturating_sub(1);
        self.state.hardware_cursor_row = self.state.cursor_row;
        self.state.max_lines_rendered = if clear {
            lines.len()
        } else {
            self.state.max_lines_rendered.max(lines.len())
        };
        self.state.previous_viewport_top = lines.len().saturating_sub(height);
        self.place_cursor(cursor, lines.len());
        self.state.previous_lines = lines;
        self.state.previous_width = width;
        self.state.previous_height = height;
    }

    fn place_cursor(&mut self, cursor: Option<(usize, usize)>, count: usize) {
        if let Some((row, col)) = cursor.filter(|_| count != 0) {
            let row = row.min(count - 1);
            let show = self.base.show_hardware_cursor();
            let mut output = Output::new(self.base.terminal.as_mut());
            output.move_rows(row as isize - self.state.hardware_cursor_row as isize);
            output.push(&format!("\x1b[{}G", col + 1));
            output.flush();
            self.state.hardware_cursor_row = row;
            self.base
                .terminal
                .write(if show { "\x1b[?25h" } else { "\x1b[?25l" });
        } else {
            self.base.terminal.write("\x1b[?25l");
        }
    }

    fn invalid_width(
        &mut self,
        lines: &[String],
        index: usize,
        width: usize,
        actual: usize,
    ) -> io::Error {
        let path = self
            .log_directory
            .clone()
            .unwrap_or_else(std::env::temp_dir)
            .join("pi-tui-crash.log");
        let mut log = format!(
            "Crash at {}\nTerminal width: {width}\nLine {index} visible width: {actual}\n\n=== All rendered lines ===\n",
            chrono::Utc::now()
        );
        for (i, line) in lines.iter().enumerate() {
            let _ = writeln!(log, "[{i}] (w={}) {line}", visible_width(line));
        }
        // A failed write (full or unwritable temp dir) must not hide the width error or skip `stop()`.
        let write_result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, log));
        let crash_log_status = match write_result {
            Ok(()) => format!("Debug log written to: {}", path.display()),
            Err(error) => format!("Could not write debug log to {}: {error}", path.display()),
        };
        let _ = self.stop(TuiStopOptions::default());
        let error = format!(
            "Rendered line {index} exceeds terminal width ({actual} > {width}).\n\nThis is likely caused by a custom TUI component not truncating its output.\nUse visibleWidth() to measure and truncateToWidth() to truncate lines.\n\n{crash_log_status}"
        );
        io::Error::new(io::ErrorKind::InvalidData, error)
    }

    fn draw(&mut self) -> io::Result<()> {
        if self.base.stopped {
            return Ok(());
        }
        let width = self.base.terminal.width().max(1);
        let height = self.base.terminal.height().max(1);
        let mut lines = self.base.content.render(width);
        for line in &mut lines {
            let prompt = line.len() - strip_prompt_markers(line).len();
            if prompt != 0 {
                line.drain(..prompt);
            }
            resolve_fake_cursors(line, self.base.show_hardware_cursor());
        }
        lines = self.base.composite_overlays(lines, width, height);
        let cursor = extract_cursor_position(&mut lines, height);
        for line in &mut lines {
            reset_line(line);
        }
        let prev = &self.state;
        let width_changed = prev.previous_width != 0 && prev.previous_width != width;
        let height_changed = prev.previous_height != 0 && prev.previous_height != height;
        let shrink = self.base.clear_on_shrink()
            && lines.len() < prev.max_lines_rendered
            && !self.base.has_overlay_entries();
        let resize =
            width_changed || (height_changed && std::env::var_os("TERMUX_VERSION").is_none());
        if std::mem::take(&mut self.force_clear) || resize || shrink {
            self.full_redraw(lines, cursor, width, height, true);
            return Ok(());
        }
        if prev.previous_lines.is_empty() && !height_changed {
            self.full_redraw(lines, cursor, width, height, false);
            return Ok(());
        }
        let mut top = if height_changed {
            (prev.previous_viewport_top + prev.previous_height).saturating_sub(height)
        } else {
            prev.previous_viewport_top
        };
        let Some((first, last)) = deltas(&prev.previous_lines, &lines) else {
            self.place_cursor(cursor, lines.len());
            self.state.previous_viewport_top = top;
            self.state.previous_height = height;
            return Ok(());
        };
        let end_new = lines.len().saturating_sub(1);
        if first >= lines.len() {
            let extra = prev.previous_lines.len().saturating_sub(lines.len());
            if end_new < top || extra > height {
                self.full_redraw(lines, cursor, width, height, true);
                return Ok(());
            }
            if extra > 0 {
                let mut output = Output::new(self.base.terminal.as_mut());
                output.push(SYNC_ON);
                output.clear_images(image_ids(
                    &prev.previous_lines[first..=last.min(prev.previous_lines.len() - 1)],
                ));
                output.move_rows(end_new as isize - prev.hardware_cursor_row as isize);
                output.push("\r");
                let offset = usize::from(!lines.is_empty());
                if offset != 0 {
                    output.push("\x1b[1B");
                }
                for i in 0..extra {
                    output.push("\r\x1b[2K");
                    if i + 1 < extra {
                        output.push("\x1b[1B");
                    }
                }
                output.move_rows(-((extra - 1 + offset) as isize));
                output.push(SYNC_OFF);
                output.flush();
                self.state.hardware_cursor_row = end_new;
                self.state.cursor_row = end_new;
            }
        } else {
            if first < top {
                self.full_redraw(lines, cursor, width, height, true);
                return Ok(());
            }
            let appended = lines.len() > prev.previous_lines.len()
                && first == prev.previous_lines.len()
                && first > 0;
            let move_to = first - usize::from(appended);
            let bottom = top + height - 1;
            let scroll = move_to.saturating_sub(bottom);
            let render_end = last.min(end_new);
            let mut i = first;
            while i <= render_end {
                let rows = reserved_rows(&lines, i, render_end);
                let screen_row = i - top - scroll;
                if rows > 1 && screen_row + rows > height {
                    self.full_redraw(lines, cursor, width, height, true);
                    return Ok(());
                }
                if !is_image_line(&lines[i]) {
                    let actual = visible_width(&lines[i]);
                    if actual > width {
                        return Err(self.invalid_width(&lines, i, width, actual));
                    }
                }
                i += rows;
            }
            let mut output = Output::new(self.base.terminal.as_mut());
            output.push(SYNC_ON);
            let old_end = (last + 1).min(prev.previous_lines.len());
            if first < old_end {
                output.clear_images(image_ids(&prev.previous_lines[first..old_end]));
            }
            let mut hardware = prev.hardware_cursor_row;
            if scroll != 0 {
                let screen_row = hardware.saturating_sub(top).min(height - 1);
                output.move_rows((height - 1 - screen_row) as isize);
                for _ in 0..scroll {
                    output.push("\r\n");
                }
                top += scroll;
                hardware = move_to;
            }
            output.move_rows(move_to as isize - hardware as isize);
            output.push(if appended { "\r\n" } else { "\r" });
            let mut i = first;
            while i <= render_end {
                if i > first {
                    output.push("\r\n");
                }
                output.push(CLEAR_LINE);
                let count = reserved_rows(&lines, i, render_end);
                if count > 1 {
                    for _ in 1..count {
                        output.push("\r\n\x1b[2K");
                    }
                    output.move_rows(1 - count as isize);
                }
                output.push(&lines[i]);
                output.move_rows(count as isize - 1);
                i += count;
            }
            let extra = prev.previous_lines.len().saturating_sub(lines.len());
            let mut final_row = render_end;
            if extra > 0 {
                if render_end < end_new {
                    output.move_rows((end_new - render_end) as isize);
                    final_row = end_new;
                }
                for _ in 0..extra {
                    output.push("\r\n\x1b[2K");
                }
                output.move_rows(-(extra as isize));
            }
            output.push(SYNC_OFF);
            output.flush();
            self.state.cursor_row = end_new;
            self.state.hardware_cursor_row = final_row;
            self.state.max_lines_rendered = self.state.max_lines_rendered.max(lines.len());
            top = top.max((final_row + 1).saturating_sub(height));
        }
        self.place_cursor(cursor, lines.len());
        self.state.previous_lines = lines;
        self.state.previous_width = width;
        self.state.previous_height = height;
        self.state.previous_viewport_top = top;
        Ok(())
    }
}

impl Component for TuiMainScreen {
    fn render(&self, width: usize) -> Vec<String> {
        self.base.content.render(width)
    }
    fn handle_input(&mut self, input: &str) {
        self.base.handle_input(input);
    }
    fn invalidate(&mut self) {
        self.base.invalidate();
    }
}

impl Tui for TuiMainScreen {
    fn mode(&self) -> TuiMode {
        TuiMode::Regular
    }
    fn base(&self) -> &TuiBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut TuiBase {
        &mut self.base
    }
    fn start(&mut self) -> io::Result<()> {
        self.base.start()
    }
    fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        self.base.begin_stop();
        if !options.preserve_screen && !self.state.previous_lines.is_empty() {
            self.base.terminal.write(" ");
            let target = self.state.previous_lines.len() as isize;
            let mut out = Output::new(self.base.terminal.as_mut());
            out.move_rows(target - self.state.hardware_cursor_row as isize);
            out.push("\r\n");
            out.flush();
        }
        self.base.finish_stop()
    }
    fn render_now(&mut self, force: bool) -> io::Result<()> {
        if self.base.begin_frame(force) {
            self.force_clear = true;
        }
        self.draw()
    }
}
