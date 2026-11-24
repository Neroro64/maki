//! Function-level line diff rendering (base -> head) with `similar`, inline
//! token emphasis, and collapsible unchanged sections.

use similar::{ChangeTag, TextDiff};
use std::collections::HashSet;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
    pub text: String,
    pub emphasized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub sign: Sign,
    pub text: String,
    pub spans: Vec<InlineSpan>,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffItem {
    Line(DiffLine),
    Collapsed {
        gap_index: usize,
        hidden_count: usize,
        old_start: usize,
        old_end: usize,
        new_start: usize,
        new_end: usize,
    },
}

/// Unified diff of one function/file source between the two revisions, with
/// collapsed unchanged gaps and inline change spans.
pub fn fn_diff_items(
    base_body: Option<&str>,
    head_body: Option<&str>,
    expanded_gaps: &HashSet<usize>,
    expand_all: bool,
) -> Vec<DiffItem> {
    match (base_body, head_body) {
        (Some(base), Some(head)) if base == head => head
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let text = line.to_string();
                DiffItem::Line(DiffLine {
                    sign: Sign::Same,
                    spans: vec![InlineSpan {
                        text: text.clone(),
                        emphasized: false,
                    }],
                    text,
                    old_line: Some(index + 1),
                    new_line: Some(index + 1),
                })
            })
            .collect(),
        (Some(b), Some(h)) => {
            let diff = TextDiff::from_lines(b, h);
            let b_lines: Vec<&str> = b.lines().collect();
            let h_lines: Vec<&str> = h.lines().collect();
            let groups = diff.grouped_ops(3);
            if groups.is_empty() {
                return Vec::new();
            }
            let mut out = Vec::new();
            let mut prev_old_end = 0usize;
            let mut prev_new_end = 0usize;
            let mut gap_idx = 0usize;

            let emit_gap = |out: &mut Vec<DiffItem>,
                            gap_index: usize,
                            old_start: usize,
                            old_end: usize,
                            new_start: usize,
                            new_end: usize| {
                let count = (new_end - new_start).max(old_end - old_start);
                if count == 0 {
                    return;
                }
                if expand_all || expanded_gaps.contains(&gap_index) || count == 1 {
                    for offset in 0..count {
                        let o_idx = old_start + offset;
                        let n_idx = new_start + offset;
                        let text = h_lines
                            .get(n_idx)
                            .or_else(|| b_lines.get(o_idx))
                            .copied()
                            .unwrap_or_default()
                            .to_string();
                        out.push(DiffItem::Line(DiffLine {
                            sign: Sign::Same,
                            spans: vec![InlineSpan {
                                text: text.clone(),
                                emphasized: false,
                            }],
                            text,
                            old_line: (o_idx < b_lines.len()).then_some(o_idx + 1),
                            new_line: (n_idx < h_lines.len()).then_some(n_idx + 1),
                        }));
                    }
                } else {
                    out.push(DiffItem::Collapsed {
                        gap_index,
                        hidden_count: count,
                        old_start: old_start + 1,
                        old_end,
                        new_start: new_start + 1,
                        new_end,
                    });
                }
            };

            for group in &groups {
                let Some(first) = group.first() else { continue };
                let Some(last) = group.last() else { continue };
                let cur_old_start = first.old_range().start;
                let cur_new_start = first.new_range().start;
                if cur_old_start > prev_old_end || cur_new_start > prev_new_end {
                    emit_gap(
                        &mut out,
                        gap_idx,
                        prev_old_end,
                        cur_old_start,
                        prev_new_end,
                        cur_new_start,
                    );
                    gap_idx += 1;
                }
                for op in group {
                    for ch in diff.iter_inline_changes(op) {
                        let sign = match ch.tag() {
                            ChangeTag::Equal => Sign::Same,
                            ChangeTag::Delete => Sign::Minus,
                            ChangeTag::Insert => Sign::Plus,
                        };
                        let mut spans = Vec::new();
                        let mut full = String::new();
                        for (emphasized, piece) in ch.iter_strings_lossy() {
                            let clean = piece
                                .trim_end_matches('\n')
                                .trim_end_matches('\r')
                                .to_string();
                            if !clean.is_empty() {
                                full.push_str(&clean);
                                spans.push(InlineSpan {
                                    text: clean,
                                    emphasized,
                                });
                            }
                        }
                        out.push(DiffItem::Line(DiffLine {
                            sign,
                            text: full,
                            spans,
                            old_line: ch.old_index().map(|line| line + 1),
                            new_line: ch.new_index().map(|line| line + 1),
                        }));
                    }
                }
                prev_old_end = last.old_range().end;
                prev_new_end = last.new_range().end;
            }

            if prev_old_end < b_lines.len() || prev_new_end < h_lines.len() {
                emit_gap(
                    &mut out,
                    gap_idx,
                    prev_old_end,
                    b_lines.len(),
                    prev_new_end,
                    h_lines.len(),
                );
            }
            out
        }
        (None, Some(h)) => h
            .lines()
            .enumerate()
            .map(|(index, l)| {
                let text = l.to_string();
                DiffItem::Line(DiffLine {
                    sign: Sign::Plus,
                    spans: vec![InlineSpan {
                        text: text.clone(),
                        emphasized: false,
                    }],
                    text,
                    old_line: None,
                    new_line: Some(index + 1),
                })
            })
            .collect(),
        (Some(b), None) => b
            .lines()
            .enumerate()
            .map(|(index, l)| {
                let text = l.to_string();
                DiffItem::Line(DiffLine {
                    sign: Sign::Minus,
                    spans: vec![InlineSpan {
                        text: text.clone(),
                        emphasized: false,
                    }],
                    text,
                    old_line: Some(index + 1),
                    new_line: None,
                })
            })
            .collect(),
        (None, None) => vec![],
    }
}

/// Unified diff returning lines only (collapsed gaps omitted).
pub fn fn_diff(base_body: Option<&str>, head_body: Option<&str>) -> Vec<DiffLine> {
    fn_diff_items(base_body, head_body, &HashSet::new(), false)
        .into_iter()
        .filter_map(|item| match item {
            DiffItem::Line(dl) => Some(dl),
            DiffItem::Collapsed { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{DiffItem, Sign, fn_diff, fn_diff_items};
    use std::collections::HashSet;

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

    #[test]
    fn collapsed_sections_emitted_and_expandable_individually_or_globally() {
        let base: String = (1..=30).map(|line| format!("line {line}\n")).collect();
        let head = base
            .replace("line 8\n", "changed 8\n")
            .replace("line 24\n", "changed 24\n");
        let items = fn_diff_items(Some(&base), Some(&head), &HashSet::new(), false);
        let collapsed: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                DiffItem::Collapsed {
                    gap_index,
                    hidden_count,
                    ..
                } => Some((*gap_index, *hidden_count)),
                _ => None,
            })
            .collect();
        // Leading (1..4 = 4 lines), middle (12..20 = 9 lines), trailing (28..30 = 3 lines)
        assert_eq!(collapsed, vec![(0, 4), (1, 9), (2, 3)]);

        let expanded_mid = fn_diff_items(Some(&base), Some(&head), &HashSet::from([1]), false);
        let remaining: Vec<_> = expanded_mid
            .iter()
            .filter_map(|i| match i {
                DiffItem::Collapsed { gap_index, .. } => Some(*gap_index),
                _ => None,
            })
            .collect();
        assert_eq!(remaining, vec![0, 2]);

        let all = fn_diff_items(Some(&base), Some(&head), &HashSet::new(), true);
        assert!(!all.iter().any(|i| matches!(i, DiffItem::Collapsed { .. })));
    }
}
