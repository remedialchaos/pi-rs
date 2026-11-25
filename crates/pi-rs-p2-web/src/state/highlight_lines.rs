//! Line-by-line code highlighting that keeps spans across newlines (upstream `highlight-lines.ts`).

/// A line of a file or patch as shown in `Code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeLine {
    pub kind: CodeLineKind,
    pub text: String,
}

/// The side a patch line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeLineKind {
    Add,
    Del,
    Context,
    Hunk,
    Meta,
}

/// A node of lowlight's (hast) tree: text, or an element with its classes and children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HastNode {
    pub node_type: String,
    pub value: Option<String>,
    pub class_names: Vec<String>,
    pub children: Vec<HastNode>,
}

impl HastNode {
    /// A text node.
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            node_type: "text".into(),
            value: Some(value.into()),
            class_names: vec![],
            children: vec![],
        }
    }
    /// An element node carrying one or more classes.
    pub fn element(class: &str, children: Vec<HastNode>) -> Self {
        Self {
            node_type: "element".into(),
            value: None,
            class_names: class.split_whitespace().map(str::to_owned).collect(),
            children,
        }
    }
}

/// Highlights one text as lowlight's tree; `None` when it cannot (upstream `Highlighter`).
pub type Highlighter = dyn Fn(&str) -> Option<Vec<HastNode>>;

/// Each line highlighted in its context (plain text where `highlighter` cannot).
///
/// Between hunk and meta lines, which stay plain, the new side (context and added lines)
/// and the old side (context and deleted lines) are each highlighted as one text; a line
/// takes its row of its side. A file's lines are all context.
pub fn highlight_lines(lines: &[CodeLine], highlighter: &Highlighter) -> Vec<Vec<HastNode>> {
    let mut shown: Vec<Vec<HastNode>> = lines
        .iter()
        .map(|line| vec![HastNode::text(line.text.clone())])
        .collect();
    let mut start = 0;
    for end in 0..=lines.len() {
        let kind = lines.get(end).map(|line| line.kind);
        // Hunk and meta lines (and the end sentinel) close a highlight run; other lines
        // stay inside it and are processed only when the run ends.
        if matches!(
            kind,
            Some(CodeLineKind::Add | CodeLineKind::Del | CodeLineKind::Context)
        ) {
            continue;
        }
        let rows: Vec<usize> = (start..end).collect();
        let new_side: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|&row| lines[row].kind != CodeLineKind::Del)
            .collect();
        let old_side: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|&row| lines[row].kind != CodeLineKind::Add)
            .collect();
        highlight_side(lines, &new_side, highlighter, &mut shown, |_| true);
        if old_side
            .iter()
            .any(|&row| lines[row].kind == CodeLineKind::Del)
        {
            highlight_side(lines, &old_side, highlighter, &mut shown, |row| {
                lines[row].kind == CodeLineKind::Del
            });
        }
        start = end + 1;
    }
    shown
}

/// Highlights `rows` of `lines` as one text and puts the rows `keep` takes into `shown`.
fn highlight_side(
    lines: &[CodeLine],
    rows: &[usize],
    highlighter: &Highlighter,
    shown: &mut [Vec<HastNode>],
    keep: impl Fn(usize) -> bool,
) {
    if rows.is_empty() {
        return;
    }
    let joined = rows
        .iter()
        .map(|&row| lines[row].text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let Some(tree) = highlighter(&joined) else {
        return;
    };
    let split = split_lines(&tree);
    rows.iter().enumerate().for_each(|(index, &row)| {
        if keep(row) {
            shown[row] = split.get(index).cloned().unwrap_or_default();
        }
    });
}

/// Lowlight's tree split at its newlines into one tree per line: the spans open at a line
/// break are opened again on the next line (their classes copied), so every line has its
/// own span stack.
pub fn split_lines(nodes: &[HastNode]) -> Vec<Vec<HastNode>> {
    let mut splitter = Splitter {
        lines: vec![vec![]],
        open: vec![],
    };
    splitter.walk(nodes);
    splitter.lines
}

/// The shared state of one `split_lines` walk (upstream's `lines`, `open`, `append`,
/// `breakLine` and `walk` closures).
struct Splitter {
    lines: Vec<Vec<HastNode>>,
    /// The child-index path of each span open here, outermost first, inside the current line.
    /// A path is used instead of a shared mutable node so a reopened span and the line that
    /// holds it are the same object (upstream's `open` and `lines` share the copy).
    open: Vec<Vec<usize>>,
}

impl Splitter {
    fn node_at<'a>(line: &'a [HastNode], path: &[usize]) -> Option<&'a HastNode> {
        let mut node = line.get(*path.first()?)?;
        for &index in &path[1..] {
            node = node.children.get(index)?;
        }
        Some(node)
    }

    /// Appends a node to the innermost open span (or the line's root), returning its path.
    fn append_at(&mut self, open: &[Vec<usize>], node: HastNode) -> Vec<usize> {
        let mut path = open.last().cloned().unwrap_or_default();
        let line = self.lines.last_mut().expect("a line always exists");
        let mut container: &mut Vec<HastNode> = line;
        for &index in &path {
            container = &mut container[index].children;
        }
        let index = container.len();
        container.push(node);
        path.push(index);
        path
    }

    fn append(&mut self, node: HastNode) {
        self.append_at(&self.open.clone(), node);
    }

    fn break_line(&mut self) {
        let previous = self.lines.last().expect("a line always exists");
        let copies: Vec<HastNode> = self
            .open
            .iter()
            .filter_map(|path| Self::node_at(previous, path))
            .map(|span| HastNode {
                node_type: span.node_type.clone(),
                value: None,
                class_names: span.class_names.clone(),
                children: vec![],
            })
            .collect();
        self.lines.push(vec![]);
        let mut reopened: Vec<Vec<usize>> = vec![];
        for copy in copies {
            let path = self.append_at(&reopened, copy);
            reopened.push(path);
        }
        self.open = reopened;
    }

    fn walk(&mut self, list: &[HastNode]) {
        for node in list {
            if node.node_type == "text" {
                for (index, part) in node.value.as_deref().unwrap_or("").split('\n').enumerate() {
                    if index > 0 {
                        self.break_line();
                    }
                    if !part.is_empty() {
                        self.append(HastNode::text(part));
                    }
                }
                continue;
            }
            let copy = HastNode {
                node_type: node.node_type.clone(),
                value: None,
                class_names: node.class_names.clone(),
                children: vec![],
            };
            let path = self.append_at(&self.open.clone(), copy);
            self.open.push(path);
            self.walk(&node.children);
            self.open.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_splits_into_lines() {
        let tree = vec![HastNode::text("a\nb\nc")];
        let split = split_lines(&tree);
        assert_eq!(split.len(), 3);
        assert_eq!(split[0], vec![HastNode::text("a")]);
        assert_eq!(split[2], vec![HastNode::text("c")]);
    }

    #[test]
    fn spans_open_at_a_line_break_are_reopened() {
        let tree = vec![HastNode::element(
            "hljs-comment",
            vec![HastNode::text("a\nb")],
        )];
        let split = split_lines(&tree);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].len(), 1);
        assert_eq!(split[0][0].class_names, vec!["hljs-comment"]);
        assert_eq!(split[1].len(), 1);
        assert_eq!(split[1][0].class_names, vec!["hljs-comment"]);
        assert_eq!(split[1][0].children, vec![HastNode::text("b")]);
    }

    #[test]
    fn highlight_lines_keeps_hunk_and_meta_plain() {
        let lines = vec![
            CodeLine {
                kind: CodeLineKind::Hunk,
                text: "@@ -1 +1 @@".into(),
            },
            CodeLine {
                kind: CodeLineKind::Context,
                text: "same".into(),
            },
            CodeLine {
                kind: CodeLineKind::Add,
                text: "new".into(),
            },
        ];
        let highlighter = |text: &str| {
            if text.contains("same") {
                Some(vec![HastNode::element("k", vec![HastNode::text(text)])])
            } else {
                None
            }
        };
        let shown = highlight_lines(&lines, &highlighter);
        assert_eq!(shown[0], vec![HastNode::text("@@ -1 +1 @@".to_owned())]);
        // Context and added lines share the new side, so both keep the highlighter's span.
        assert_eq!(shown[1][0].class_names, vec!["k"]);
        assert_eq!(
            shown[1][0].children,
            vec![HastNode::text("same".to_owned())]
        );
        assert_eq!(shown[2][0].class_names, vec!["k"]);
        assert_eq!(shown[2][0].children, vec![HastNode::text("new".to_owned())]);
    }
}
