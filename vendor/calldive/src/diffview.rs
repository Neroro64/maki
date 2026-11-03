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
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

/// Unified diff of one function's source between the two revisions.
pub fn fn_diff(base_body: Option<&str>, head_body: Option<&str>) -> Vec<DiffLine> {
    match (base_body, head_body) {
        (Some(base), Some(head)) if base == head => head
            .lines()
            .enumerate()
            .map(|(index, line)| DiffLine {
                sign: Sign::Same,
                text: line.to_string(),
                old_line: Some(index + 1),
                new_line: Some(index + 1),
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
                        out.push(DiffLine {
                            sign,
                            text,
                            old_line: ch.old_index().map(|line| line + 1),
                            new_line: ch.new_index().map(|line| line + 1),
                        });
                    }
                }
            }
            out
        }
        (None, Some(h)) => h
            .lines()
            .enumerate()
            .map(|(index, l)| DiffLine {
                sign: Sign::Plus,
                text: l.to_string(),
                old_line: None,
                new_line: Some(index + 1),
            })
            .collect(),
        (Some(b), None) => b
            .lines()
            .enumerate()
            .map(|(index, l)| DiffLine {
                sign: Sign::Minus,
                text: l.to_string(),
                old_line: Some(index + 1),
                new_line: None,
            })
            .collect(),
        (None, None) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::{Sign, fn_diff};

    #[test]
    fn anchors_preserve_line_numbers_across_separated_hunks() {
        let base: String = (1..=30).map(|line| format!("line {line}\n")).collect();
        let head = base
            .replace("line 2\n", "changed 2\n")
            .replace("line 28\n", "changed 28\n");
        let diff = fn_diff(Some(&base), Some(&head));
        let deleted: Vec<_> = diff
            .iter()
            .filter(|line| line.sign == Sign::Minus)
            .map(|line| line.old_line)
            .collect();
        let inserted: Vec<_> = diff
            .iter()
            .filter(|line| line.sign == Sign::Plus)
            .map(|line| line.new_line)
            .collect();
        assert_eq!(deleted, vec![Some(2), Some(28)]);
        assert_eq!(inserted, vec![Some(2), Some(28)]);
    }
}
