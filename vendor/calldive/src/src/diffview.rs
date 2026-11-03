//! Function-level line diff rendering (base -> head) with `similar`.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    Same,
    Plus,
    Minus,
}

impl Sign {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Plus => "+ ",
            Self::Minus => "- ",
            Self::Same => "  ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub sign: Sign,
    pub text: String,
}

/// Unified diff of one function's source between the two revisions.
pub fn fn_diff(base_body: Option<&str>, head_body: Option<&str>) -> Vec<DiffLine> {
    match (base_body, head_body) {
        (Some(b), Some(h)) if b == h => h
            .lines()
            .map(|line| DiffLine {
                sign: Sign::Same,
                text: line.to_string(),
            })
            .collect(),
        (Some(b), Some(h)) => {
            let diff = TextDiff::from_lines(b, h);
            let mut out = Vec::new();
            for group in diff.grouped_ops(3) {
                for op in group {
                    for ch in diff.iter_changes(&op) {
                        let sign = match ch.tag() {
                            ChangeTag::Equal => Sign::Same,
                            ChangeTag::Delete => Sign::Minus,
                            ChangeTag::Insert => Sign::Plus,
                        };
                        let text = ch
                            .to_string()
                            .trim_end_matches('\n')
                            .trim_end_matches('\r')
                            .to_string();
                        out.push(DiffLine { sign, text });
                    }
                }
            }
            out
        }
        (None, Some(h)) => h
            .lines()
            .map(|l| DiffLine {
                sign: Sign::Plus,
                text: l.to_string(),
            })
            .collect(),
        (Some(b), None) => b
            .lines()
            .map(|l| DiffLine {
                sign: Sign::Minus,
                text: l.to_string(),
            })
            .collect(),
        (None, None) => vec![],
    }
}
