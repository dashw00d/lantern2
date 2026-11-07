use lantern_core::model::LogLine;
use std::{collections::VecDeque, ops::Range};

const CAPACITY: usize = 20_000;
pub(crate) const ROW_HEIGHT: f32 = 18.0;

#[derive(Debug)]
pub(crate) struct LogBuffer {
    lines: VecDeque<LogLine>,
    pub follow: bool,
    pub scroll_y: f32,
    pub viewport_h: f32,
}
impl Default for LogBuffer {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            follow: true,
            scroll_y: 0.0,
            viewport_h: 600.0,
        }
    }
}
impl LogBuffer {
    pub fn clear(&mut self) {
        self.lines.clear();
    }
    pub fn replace_with_backlog(&mut self, backlog: Vec<LogLine>, live: Vec<LogLine>) {
        // REST and SSE overlap while the request is in flight; preserve each occurrence once.
        let overlap = (0..=backlog.len().min(live.len()))
            .rev()
            .find(|&n| backlog[backlog.len() - n..] == live[..n])
            .unwrap_or(0);
        self.clear();
        for line in backlog.into_iter().chain(live.into_iter().skip(overlap)) {
            self.push(line);
        }
    }
    pub fn len(&self) -> usize {
        self.lines.len()
    }
    pub fn push(&mut self, mut line: LogLine) {
        line.line = strip_ansi(&line.line);
        if self.lines.len() == CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }
    pub fn range(&self) -> Range<usize> {
        visible_range(self.len(), self.scroll_y, self.viewport_h)
    }
    pub fn get(&self, index: usize) -> Option<&LogLine> {
        self.lines.get(index)
    }
}
fn visible_range(total: usize, scroll_y: f32, viewport_h: f32) -> Range<usize> {
    let first = ((scroll_y.max(0.0) / ROW_HEIGHT) as usize)
        .saturating_sub(5)
        .min(total);
    let last = first
        .saturating_add((viewport_h.max(0.0) / ROW_HEIGHT).ceil() as usize)
        .saturating_add(12)
        .min(total);
    first..last
}
// Remove terminal control sequences before untrusted logs enter the widget tree.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !ch.is_control() || ch == '\t' {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_core::{ids::InstanceId, model::Stream};
    fn line(n: usize) -> LogLine {
        LogLine {
            instance: InstanceId::parse("example").unwrap(),
            stream: Stream::Stdout,
            line: format!("{n}"),
            ts: "2026-11-06T00:00:00Z".parse().unwrap(),
        }
    }
    #[test]
    fn ring_retains_latest_twenty_thousand() {
        let mut logs = LogBuffer::default();
        for i in 0..20_025 {
            logs.push(line(i));
        }
        assert_eq!(logs.len(), 20_000);
        assert_eq!(logs.get(0).unwrap().line, "25");
        assert_eq!(logs.get(19_999).unwrap().line, "20024");
        logs.clear();
        assert_eq!(logs.len(), 0);
    }
    #[test]
    fn window_clamps_and_keeps_a_small_margin() {
        assert_eq!(visible_range(0, 0.0, 500.0), 0..0);
        assert_eq!(visible_range(20_000, 18_000.0, 360.0), 995..1027);
        assert_eq!(visible_range(100, 99_999.0, 300.0), 100..100);
        assert_eq!(visible_range(10, -20.0, 360.0), 0..10);
    }
    #[test]
    fn strips_terminal_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m \x1b]0;title\x07safe"), "red safe");
    }
    #[test]
    fn backlog_and_live_overlap_is_not_duplicated() {
        let mut logs = LogBuffer::default();
        logs.replace_with_backlog((0..5).map(line).collect(), (3..8).map(line).collect());
        assert_eq!(logs.len(), 8);
        assert_eq!(logs.get(7).unwrap().line, "7");
    }
}
