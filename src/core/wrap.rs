//! Width-adaptive folding of the statusline main line.
//!
//! Claude Code injects the terminal width into `$COLUMNS` before invoking the
//! statusLine command (v2.1.153+); the statusLine process has no controlling
//! tty, so `$COLUMNS` is the only reliable width source. ccsp otherwise renders
//! the main line as a single row that a narrow terminal (e.g. phone Termux)
//! clips. This module greedily repacks the per-component segments across as
//! many physical lines as needed to fit the width. Note the drawable width is
//! less than `$COLUMNS` — Claude Code pads the statusline and truncates with
//! `…` (see `[style] wrap_margin`); callers pass the already-reduced width.
//!
//! Folding is progressive. Components that fit stay whole and pack greedily at
//! component boundaries. A component wider than the terminal (e.g. the tokens
//! bar with its cache rate, or rate-limit windows joined in-component) folds
//! at its internal `" | "` joins first so semantic units stay together, then
//! at word (space) boundaries, and a single word wider than the terminal
//! hard-breaks at character boundaries — nothing ever overflows. SGR color
//! state is closed with a reset at each fold and re-applied on the
//! continuation row, so split segments keep their colors; the `" | "` join is
//! dropped when a fold lands on it.
//!
//! Wide terminals keep everything on one line; the same code adapts to both.
//! The display-width math mirrors the `east_asian_width` approach: strip ANSI
//! escapes, then count CJK/fullwidth cells as 2.
//!
//! Note (macOS): Claude Code renders only the first line of a multi-line
//! statusLine (upstream issue #35176). Folding only produces multiple lines on
//! a narrow terminal, so a wide macOS terminal (single line) is unaffected.

use unicode_width::UnicodeWidthChar;

const SGR_RESET: &str = "\u{1b}[0m";

/// The plain in-component join (e.g. between rate-limit windows) used as the
/// preferred fold point inside an oversized component.
const CLAUSE_SEPARATOR: &str = " | ";

/// Read the terminal width Claude Code injected into `$COLUMNS`.
///
/// Returns `None` when the variable is absent or not a positive integer, in
/// which case callers should keep the single-line behaviour.
#[must_use]
pub fn terminal_columns() -> Option<usize> {
    let raw = std::env::var("COLUMNS").ok()?;
    let value: usize = raw.trim().parse().ok()?;
    (value > 0).then_some(value)
}

/// Visible column width of a rendered segment: ANSI escapes stripped, wide
/// (CJK/fullwidth) characters counted as two columns, zero-width joiners and
/// combining marks counted as zero.
#[must_use]
pub fn display_width(text: &str) -> usize {
    let mut width = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        // Strip CSI sequences like `\x1b[38;2;1;2;3m` — everything up to and
        // including the terminating byte in `@`..=`~`.
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }

        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }

    width
}

/// Greedily pack rendered segments into width-fitting lines.
///
/// Segments on a line are joined with `separator`. A segment wider than
/// `width` starts a fresh line and folds internally (see [`fold_oversized`]);
/// its last folded row stays open so following segments can pack after it.
/// Always returns at least one line when `parts` is non-empty.
#[must_use]
pub fn pack(parts: &[String], separator: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let separator_width = display_width(separator);

    for part in parts {
        let part_width = display_width(part);

        // Oversized segment: give it a fresh line and fold it internally.
        if part_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut folded = fold_oversized(part, width);
            current = folded.pop().unwrap_or_default();
            current_width = display_width(&current);
            lines.append(&mut folded);
            continue;
        }

        let added = if current.is_empty() {
            part_width
        } else {
            part_width + separator_width
        };

        if !current.is_empty() && current_width + added > width {
            lines.push(std::mem::take(&mut current));
            current.push_str(part);
            current_width = part_width;
        } else {
            if !current.is_empty() {
                current.push_str(separator);
            }
            current.push_str(part);
            current_width += added;
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

/// Fold a single rendered segment that is wider than `width` into fitting rows.
///
/// The segment first splits on its internal `" | "` joins into clauses (e.g.
/// the 5h/7d rate-limit windows) which are kept whole when they fit; a clause
/// wider than `width` falls back to word folding, and a word wider than
/// `width` hard-breaks at character boundaries. The `" | "` join is dropped at
/// a fold. Active SGR state is closed with a reset at each fold and re-applied
/// on the continuation row, so colors survive the split.
fn fold_oversized(part: &str, width: usize) -> Vec<String> {
    let mut folder = Folder::new(width);

    let mut clause_sgr = String::new();
    for (index, clause) in part.split(CLAUSE_SEPARATOR).enumerate() {
        if index > 0 {
            if clause.is_empty() {
                continue;
            }
            folder.clause_boundary(display_width(clause), &clause_sgr);
        }
        folder.feed(clause);
        clause_sgr = folder.active_sgr.clone();
    }

    folder.finish()
}

/// Streaming folder for one rendered segment, tracking SGR state so
/// continuation rows re-open the colors that were active at the fold.
struct Folder {
    width: usize,
    lines: Vec<String>,
    line: String,
    line_width: usize,
    word: String,
    word_width: usize,
    /// SGR state active when the current word buffer started.
    word_sgr: String,
    /// Concatenated SGR sequences currently in effect (cleared on reset).
    active_sgr: String,
}

impl Folder {
    fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            lines: Vec::new(),
            line: String::new(),
            line_width: 0,
            word: String::new(),
            word_width: 0,
            word_sgr: String::new(),
            active_sgr: String::new(),
        }
    }

    /// Feed a run of text (no clause separators) through the word folder.
    fn feed(&mut self, text: &str) {
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                let mut raw = String::from("\u{1b}[");
                let mut params = String::new();
                for c in chars.by_ref() {
                    raw.push(c);
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                    params.push(c);
                }
                self.push_escape(&raw, &params);
            } else if ch == ' ' {
                self.end_word();
            } else {
                self.push_char(ch);
            }
        }

        self.end_word();
    }

    /// Handle the `" | "` join before the next clause: keep it in-row when the
    /// whole clause fits after it, otherwise fold here (dropping the join) so
    /// the clause starts a fresh row and stays together.
    fn clause_boundary(&mut self, next_clause_width: usize, clause_sgr: &str) {
        if self.line_width == 0 {
            return;
        }

        let sep_width = CLAUSE_SEPARATOR.len();
        if self.line_width + sep_width + next_clause_width > self.width {
            let state = self.active_sgr.clone();
            self.break_line(&state);
            self.line.push_str(clause_sgr);
        } else {
            self.line.push_str(CLAUSE_SEPARATOR);
            self.line_width += sep_width;
        }
    }

    /// Append a full escape sequence to the current word; SGR sequences also
    /// update the tracked color state (a bare/`0` parameter resets it).
    fn push_escape(&mut self, raw: &str, params: &str) {
        self.snapshot_word_sgr();
        self.word.push_str(raw);
        if raw.ends_with('m') {
            if params.is_empty() || params == "0" {
                self.active_sgr.clear();
            } else {
                self.active_sgr.push_str(raw);
            }
        }
    }

    fn push_char(&mut self, ch: char) {
        self.snapshot_word_sgr();
        self.word.push(ch);
        self.word_width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }

    /// Record the SGR state a fresh word buffer starts under, so the word can
    /// re-open it if it ends up leading a continuation row.
    fn snapshot_word_sgr(&mut self) {
        if self.word.is_empty() {
            self.word_sgr = self.active_sgr.clone();
        }
    }

    /// Flush the buffered word into the current line, folding as needed.
    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        let word_width = std::mem::take(&mut self.word_width);
        let word_sgr = std::mem::take(&mut self.word_sgr);

        // Escape-only word (e.g. a stray reset): attach without spacing math.
        if word_width == 0 {
            self.line.push_str(&word);
            return;
        }

        // A word wider than the terminal hard-breaks anyway — fill the rest of
        // the current row instead of breaking first, so e.g. an icon stays
        // attached to the long name it labels.
        if word_width > self.width {
            if self.line_width > 0 {
                self.line.push(' ');
                self.line_width += 1;
            }
            self.hard_break_word(&word, &word_sgr);
            return;
        }

        // A folded row may hold only an invisible SGR prefix, so "line in use"
        // means visible width, not byte emptiness.
        let joined = self.line_width + usize::from(self.line_width > 0) + word_width;

        if self.line_width > 0 && joined > self.width {
            self.break_line(&word_sgr);
            self.line.push_str(&word_sgr);
        } else if self.line_width > 0 {
            self.line.push(' ');
            self.line_width += 1;
        }

        self.line.push_str(&word);
        self.line_width += word_width;
    }

    /// Close the current row: close the SGR state active at the fold point and
    /// start the next row empty.
    fn break_line(&mut self, state_at_break: &str) {
        if !state_at_break.is_empty() && !self.line.ends_with(SGR_RESET) {
            self.line.push_str(SGR_RESET);
        }
        self.lines.push(std::mem::take(&mut self.line));
        self.line_width = 0;
    }

    /// Emit a word wider than the terminal by breaking at character
    /// boundaries, carrying the SGR state across each break. `word_sgr` is the
    /// state the word starts under; escapes embedded in the word update it.
    fn hard_break_word(&mut self, word: &str, word_sgr: &str) {
        let mut active = word_sgr.to_string();
        let mut chars = word.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                let mut raw = String::from("\u{1b}[");
                let mut params = String::new();
                for c in chars.by_ref() {
                    raw.push(c);
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                    params.push(c);
                }
                self.line.push_str(&raw);
                if raw.ends_with('m') {
                    if params.is_empty() || params == "0" {
                        active.clear();
                    } else {
                        active.push_str(&raw);
                    }
                }
                continue;
            }

            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if self.line_width + ch_width > self.width {
                if !active.is_empty() && !self.line.ends_with(SGR_RESET) {
                    self.line.push_str(SGR_RESET);
                }
                self.lines.push(std::mem::take(&mut self.line));
                self.line_width = 0;
                self.line.push_str(&active);
            }
            self.line.push(ch);
            self.line_width += ch_width;
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.end_word();
        if self.line_width > 0 {
            self.lines.push(self.line);
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_strips_ansi() {
        let colored = "\x1b[38;2;1;2;3mAB\x1b[0m";
        assert_eq!(display_width(colored), 2);
    }

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
    }

    #[test]
    fn pack_keeps_single_line_when_it_fits() {
        let parts = vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()];
        let lines = pack(&parts, " | ", 200);
        assert_eq!(lines, vec!["aaa | bbb | ccc".to_string()]);
    }

    #[test]
    fn pack_wraps_on_narrow_width() {
        let parts = vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()];
        // Each segment is 3 cols; " | " is 3 cols. Width 8 fits one pair
        // ("aaa | bbb" = 9 > 8, so only one per line here at width 4).
        let lines = pack(&parts, " | ", 4);
        assert_eq!(
            lines,
            vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()]
        );
    }

    #[test]
    fn pack_packs_greedily() {
        let parts = vec![
            "aa".to_string(),
            "bb".to_string(),
            "cccccc".to_string(),
            "dd".to_string(),
        ];
        // width 9, sep " " (1). "aa bb" = 5; adding " cccccc" → 12 > 9 → wrap.
        // Then "cccccc dd" = 9 ≤ 9 → stays together.
        let lines = pack(&parts, " ", 9);
        assert_eq!(lines, vec!["aa bb".to_string(), "cccccc dd".to_string()]);
    }

    #[test]
    fn pack_folds_oversized_segment_at_word_boundaries() {
        let parts = vec!["short".to_string(), "one two three four".to_string()];
        let lines = pack(&parts, " | ", 9);
        assert_eq!(
            lines,
            vec![
                "short".to_string(),
                "one two".to_string(),
                "three".to_string(),
                "four".to_string(),
            ]
        );
    }

    #[test]
    fn pack_lets_following_segment_join_last_folded_row() {
        // The tail of a folded segment stays open for later small segments.
        let parts = vec!["one two three".to_string(), "x".to_string()];
        let lines = pack(&parts, " | ", 9);
        assert_eq!(lines, vec!["one two".to_string(), "three | x".to_string()]);
    }

    #[test]
    fn pack_hard_breaks_single_long_word() {
        let parts = vec!["short".to_string(), "waytoolongsegment".to_string()];
        let lines = pack(&parts, " | ", 6);
        assert_eq!(
            lines,
            vec![
                "short".to_string(),
                "waytoo".to_string(),
                "longse".to_string(),
                "gment".to_string(),
            ]
        );
    }

    #[test]
    fn fold_carries_sgr_state_across_rows() {
        let parts = vec!["\x1b[31maaa bbb\x1b[0m".to_string()];
        let lines = pack(&parts, " | ", 4);
        assert_eq!(
            lines,
            vec![
                "\x1b[31maaa\x1b[0m".to_string(),
                "\x1b[31mbbb\x1b[0m".to_string(),
            ]
        );
    }

    #[test]
    fn fold_prefers_clause_boundaries() {
        // "AA BB | CC DD" at width 10: plain word wrap would emit
        // "AA BB | CC" — clause folding keeps the windows whole instead.
        let parts = vec!["AA BB | CC DD".to_string()];
        let lines = pack(&parts, " | ", 10);
        assert_eq!(lines, vec!["AA BB".to_string(), "CC DD".to_string()]);
    }

    #[test]
    fn fold_keeps_clause_join_when_both_fit() {
        let parts = vec!["AA BB | CC DD".to_string()];
        let lines = pack(&parts, " | ", 13);
        assert_eq!(lines, vec!["AA BB | CC DD".to_string()]);
    }

    #[test]
    fn fold_carries_color_into_next_clause() {
        let parts = vec!["\x1b[33maa | bb\x1b[0m".to_string()];
        let lines = pack(&parts, " | ", 4);
        assert_eq!(
            lines,
            vec![
                "\x1b[33maa\x1b[0m".to_string(),
                "\x1b[33mbb\x1b[0m".to_string(),
            ]
        );
    }

    #[test]
    fn fold_word_wraps_oversized_clause() {
        let parts = vec!["aa | bb cc dd ee".to_string()];
        let lines = pack(&parts, " | ", 7);
        // Second clause (11 cols) exceeds the width → folds at word boundaries
        // on its own rows; the join before it is dropped at the fold.
        assert_eq!(
            lines,
            vec!["aa".to_string(), "bb cc".to_string(), "dd ee".to_string()]
        );
    }

    #[test]
    fn fold_hard_break_carries_color() {
        let parts = vec!["\x1b[32mabcdefgh\x1b[0m".to_string()];
        let lines = pack(&parts, " | ", 4);
        assert_eq!(
            lines,
            vec![
                "\x1b[32mabcd\x1b[0m".to_string(),
                "\x1b[32mefgh\x1b[0m".to_string(),
            ]
        );
    }

    #[test]
    fn fold_rows_never_exceed_width() {
        let parts = vec![
            " [████████░░░░░░░] 56.5% (113.0k/200k) ⚡97%".to_string(),
            "5h reset 3h1m | 7d reset 4d23h".to_string(),
        ];
        for width in 8..60 {
            for line in pack(&parts, " | ", width) {
                assert!(
                    display_width(&line) <= width,
                    "width {width}: line {line:?} is {} cols",
                    display_width(&line)
                );
            }
        }
    }
}
