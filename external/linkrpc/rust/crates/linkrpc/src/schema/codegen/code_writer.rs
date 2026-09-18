//! A tiny indentation-aware writer for emitting deterministic Rust source.

/// Accumulates lines with a current indent level. Four-space indentation,
/// matching `rustfmt` defaults so generated output needs no post-formatting.
#[derive(Default)]
pub struct CodeWriter {
    buf: String,
    indent: usize,
}

impl CodeWriter {
    pub fn new() -> Self {
        CodeWriter::default()
    }

    pub fn indent(&mut self) {
        self.indent += 1;
    }

    pub fn dedent(&mut self) {
        self.indent = self.indent.saturating_sub(1);
    }

    /// Write `text` as its own line at the current indent. An empty `text`
    /// emits a bare blank line (no trailing spaces).
    pub fn line(&mut self, text: &str) {
        if text.is_empty() {
            self.buf.push('\n');
            return;
        }
        for _ in 0..self.indent {
            self.buf.push_str("    ");
        }
        self.buf.push_str(text);
        self.buf.push('\n');
    }

    /// Write a blank separator line.
    pub fn blank(&mut self) {
        self.buf.push('\n');
    }

    /// Emit a `///` doc comment block from `text`, splitting on newlines.
    pub fn doc(&mut self, text: Option<&str>) {
        let Some(text) = text else { return };
        if text.is_empty() {
            return;
        }
        for raw in text.split('\n') {
            let line = raw.trim_end();
            if line.is_empty() {
                self.line("///");
            } else {
                self.line(&format!("/// {line}"));
            }
        }
    }

    pub fn finish(self) -> String {
        self.buf
    }
}
