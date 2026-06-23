//! Syntax highlighting for decompiled C.
//!
//! Hybrid scheme: angr's codegen hands us *semantic* spans (types, variables,
//! constants, calls, struct fields) that are exact and non-overlapping. We paint
//! those, then a small lexer fills the gaps with keywords, comments, strings and
//! stray numbers. The result is a list of per-line token runs that both the TUI
//! (ratatui spans) and the plain dump (ANSI) render.

use crate::model::HlSpan;

/// Token category, driving the color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    Plain,
    Keyword,
    Type,
    Var,
    Call,
    Number,
    Str,
    Comment,
    Field,
}

/// A run of contiguous characters sharing one kind.
#[derive(Debug, Clone)]
pub struct Tok {
    pub text: String,
    pub kind: TokKind,
}

/// Control-flow / storage keywords (type keywords are handled separately so they
/// match angr's `type` spans).
const KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "do", "switch", "case", "default", "break", "continue", "return",
    "goto", "sizeof", "typedef", "struct", "union", "enum", "extern", "static", "const",
    "volatile", "register", "inline",
];

const TYPE_KEYWORDS: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "unsigned", "signed", "_Bool",
    "bool",
];

/// Highlight `text` into per-line token runs, painting `spans` first.
pub fn highlight_lines(text: &str, spans: &[HlSpan]) -> Vec<Vec<Tok>> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut kinds = vec![TokKind::Plain; n];

    // 1. Paint the semantic spans from angr (disjoint, so order doesn't matter).
    for HlSpan(start, len, kind) in spans {
        let (start, len) = (*start, *len);
        if start >= n || len == 0 {
            continue;
        }
        let end = (start + len).min(n);
        let tk = match kind.as_str() {
            "type" => TokKind::Type,
            "var" => TokKind::Var,
            "call" => TokKind::Call,
            "field" => TokKind::Field,
            // angr lumps string and numeric literals together as constants.
            "const" => match chars[start] {
                '"' | '\'' => TokKind::Str,
                _ => TokKind::Number,
            },
            _ => continue,
        };
        for k in kinds.iter_mut().take(end).skip(start) {
            *k = tk;
        }
    }

    // 2. Lex the gaps for keywords, comments, strings and stray numbers.
    lex_gaps(&chars, &mut kinds);

    // 3. Coalesce into per-line runs.
    coalesce_lines(&chars, &kinds)
}

fn lex_gaps(chars: &[char], kinds: &mut [TokKind]) {
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if kinds[i] != TokKind::Plain {
            i += 1;
            continue;
        }
        let c = chars[i];
        // Comments.
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let mut j = i;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            paint(kinds, i, j, TokKind::Comment);
            i = j;
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let mut j = i + 2;
            while j < n && !(chars[j - 1] == '*' && chars[j] == '/') {
                j += 1;
            }
            let end = (j + 1).min(n);
            paint(kinds, i, end, TokKind::Comment);
            i = end;
            continue;
        }
        // String / char literals.
        if c == '"' || c == '\'' {
            let quote = c;
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == quote {
                    j += 1;
                    break;
                }
                j += 1;
            }
            paint(kinds, i, j.min(n), TokKind::Str);
            i = j.min(n);
            continue;
        }
        // Identifiers / keywords.
        if is_ident_start(c) {
            let mut j = i + 1;
            while j < n && is_ident_continue(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            if KEYWORDS.contains(&word.as_str()) {
                paint(kinds, i, j, TokKind::Keyword);
            } else if TYPE_KEYWORDS.contains(&word.as_str()) {
                paint(kinds, i, j, TokKind::Type);
            }
            // else: an unmapped identifier; leave it Plain.
            i = j;
            continue;
        }
        // Numbers not covered by a constant span (e.g. array sizes).
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            paint(kinds, i, j, TokKind::Number);
            i = j;
            continue;
        }
        i += 1;
    }
}

fn paint(kinds: &mut [TokKind], start: usize, end: usize, kind: TokKind) {
    for k in kinds.iter_mut().take(end).skip(start) {
        *k = kind;
    }
}

fn coalesce_lines(chars: &[char], kinds: &[TokKind]) -> Vec<Vec<Tok>> {
    let mut lines: Vec<Vec<Tok>> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let mut run = String::new();
    let mut run_kind = TokKind::Plain;

    let flush = |cur: &mut Vec<Tok>, run: &mut String, run_kind: TokKind| {
        if !run.is_empty() {
            cur.push(Tok {
                text: std::mem::take(run),
                kind: run_kind,
            });
        }
    };

    for (idx, &c) in chars.iter().enumerate() {
        if c == '\n' {
            flush(&mut cur, &mut run, run_kind);
            lines.push(std::mem::take(&mut cur));
            continue;
        }
        let k = kinds[idx];
        if k != run_kind && !run.is_empty() {
            flush(&mut cur, &mut run, run_kind);
        }
        run_kind = k;
        run.push(c);
    }
    flush(&mut cur, &mut run, run_kind);
    // Mirror `str::lines()`: no trailing empty line for a final newline.
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// ANSI SGR parameter for a kind, or `None` to leave the text uncolored.
pub fn ansi_code(kind: TokKind) -> Option<&'static str> {
    match kind {
        TokKind::Keyword => Some("35;1"),     // bold magenta
        TokKind::Type => Some("36"),          // cyan
        TokKind::Call => Some("94"),          // bright blue
        TokKind::Number => Some("33"),        // yellow
        TokKind::Str => Some("32"),           // green
        TokKind::Comment => Some("38;5;245"), // medium gray (readable on dark bg)
        TokKind::Field | TokKind::Var | TokKind::Plain => None,
    }
}
