use indexmap::IndexMap;
use pi_rs_tui::visible_width;

use super::{Art, Role};

#[derive(Clone, Copy)]
pub(super) enum Grammar {
    Flowchart,
    State,
    Class,
    Er,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Box,
    Round,
    Decision,
}

struct Node<'a> {
    label: &'a str,
    shape: Shape,
}

struct Edge<'a> {
    from: usize,
    to: usize,
    label: Option<&'a str>,
    dotted: bool,
    arrow: bool,
}

struct Subgraph<'a> {
    title: &'a str,
    parent: Option<usize>,
    depth: usize,
}

#[derive(Default)]
struct Graph<'a> {
    nodes: IndexMap<&'a str, Node<'a>>,
    node_subgraph: Vec<Option<usize>>,
    subgraphs: Vec<Subgraph<'a>>,
    subgraph_stack: Vec<usize>,
    edges: Vec<Edge<'a>>,
}

impl<'a> Graph<'a> {
    fn node(&mut self, id: &'a str, label: Option<&'a str>, shape: Shape) -> usize {
        let current_sg = self.subgraph_stack.last().copied();
        if let Some((index, _, node)) = self.nodes.get_full_mut(id) {
            if let Some(label) = label {
                *node = Node { label, shape };
            }
            if self.node_subgraph[index].is_none() && current_sg.is_some() {
                self.node_subgraph[index] = current_sg;
            }
            index
        } else {
            let index = self
                .nodes
                .insert_full(
                    id,
                    Node {
                        label: label.unwrap_or(id),
                        shape,
                    },
                )
                .0;
            self.node_subgraph.push(current_sg);
            index
        }
    }

    fn endpoint(
        &mut self,
        text: &'a str,
        target: bool,
        grammar: Grammar,
    ) -> Result<(usize, &'a str), &'static str> {
        let text = text.trim_start();
        if matches!(grammar, Grammar::State)
            && let Some(rest) = text.strip_prefix("[*]")
        {
            let id = if target { "\u{0}end" } else { "\u{0}start" };
            return Ok((self.node(id, Some("●"), Shape::Round), rest.trim_start()));
        }
        let mut id_end = 0;
        for (idx, ch) in text.char_indices() {
            if ch.is_alphanumeric() || matches!(ch, '_' | '.') {
                id_end = idx + ch.len_utf8();
            } else if ch == '-'
                && id_end > 0
                && text[idx + 1..]
                    .chars()
                    .next()
                    .is_some_and(|next| next.is_alphanumeric() || next == '_')
            {
                id_end = idx + 1;
            } else {
                break;
            }
        }
        if id_end == 0 {
            return Err("expected a node identifier");
        }
        let id = &text[..id_end];
        let text = text[id_end..].trim_start();
        let (label, rest, shape) = if matches!(grammar, Grammar::Flowchart) {
            if let Some((open, close, shape)) = [
                ("([", "])", Shape::Round),
                ("((", "))", Shape::Round),
                ("[[", "]]", Shape::Box),
                ("[", "]", Shape::Box),
                ("(", ")", Shape::Round),
                ("{", "}", Shape::Decision),
            ]
            .into_iter()
            .find(|(open, _, _)| text.starts_with(open))
            {
                let (label, rest) =
                    split_label(&text[open.len()..], close).ok_or("unclosed node label")?;
                (Some(label), rest, shape)
            } else {
                (None, text, Shape::Box)
            }
        } else {
            (
                None,
                text,
                if matches!(grammar, Grammar::State) {
                    Shape::Round
                } else {
                    Shape::Box
                },
            )
        };
        let mut rest = rest.trim_start();
        if let Some(class) = rest.strip_prefix(":::") {
            let end = class
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(class.len());
            rest = class[end..].trim_start();
        }
        Ok((self.node(id, label, shape), rest))
    }

    fn endpoint_list(
        &mut self,
        text: &'a str,
        target: bool,
        grammar: Grammar,
    ) -> Result<(Vec<usize>, &'a str), &'static str> {
        let (first, mut rest) = self.endpoint(text, target, grammar)?;
        let mut nodes = vec![first];
        while let Some(after_amp) = rest.trim_start().strip_prefix('&') {
            let (next, tail) = self.endpoint(after_amp, target, grammar)?;
            nodes.push(next);
            rest = tail;
        }
        Ok((nodes, rest))
    }

    fn statement(&mut self, text: &'a str, grammar: Grammar) -> Result<(), &'static str> {
        if text.is_empty() || text.starts_with("%%") {
            return Ok(());
        }
        let first = text.split_whitespace().next().unwrap_or_default();
        if matches!(
            first,
            "classDef" | "style" | "linkStyle" | "direction" | "click"
        ) {
            // Styling directives do not change node or edge identity.
            return Ok(());
        }
        if first == "subgraph" {
            if self.subgraph_stack.len() >= 4 {
                return Err("nested subgraphs deeper than 4 levels are not supported");
            }
            let title = parse_subgraph_title(&text["subgraph".len()..])?;
            let index = self.subgraphs.len();
            self.subgraphs.push(Subgraph {
                title,
                parent: self.subgraph_stack.last().copied(),
                depth: self.subgraph_stack.len(),
            });
            self.subgraph_stack.push(index);
            return Ok(());
        }
        if first == "end" {
            if self.subgraph_stack.pop().is_none() {
                return Err("unexpected end without subgraph");
            }
            return Ok(());
        }
        let mut text = text;
        if matches!(grammar, Grammar::State)
            && let Some(state) = text.strip_prefix("state ")
        {
            if let Some((description, id)) = state.rsplit_once(" as ") {
                let (index, rest) = self.endpoint(id, false, grammar)?;
                if !rest.is_empty() {
                    return Err("unsupported state definition");
                }
                if let Some((_, node)) = self.nodes.get_index_mut(index) {
                    node.label = description.trim().trim_matches('"');
                }
                return Ok(());
            }
            text = state.trim();
        }
        if matches!(grammar, Grammar::Class)
            && let Some(class) = text.strip_prefix("class ")
        {
            text = class.trim();
        }
        let (mut from_nodes, mut tail) = self.endpoint_list(text, false, grammar)?;
        if from_nodes.len() == 1
            && let Some(label) = tail.strip_prefix(':')
        {
            if let Some((_, node)) = self.nodes.get_index_mut(from_nodes[0]) {
                node.label = label.trim();
            }
            return Ok(());
        }
        while !tail.is_empty() {
            let (operator, inline_label, mut next) =
                connector_with_inline_label(tail, grammar).ok_or("unsupported graph syntax")?;
            let mut label = inline_label;
            if label.is_none()
                && let Some(edge_label) = next.trim_start().strip_prefix('|')
            {
                let (edge_label, rest) =
                    split_label(edge_label, "|").ok_or("unclosed edge label")?;
                label = Some(edge_label);
                next = rest;
            }
            let (to_nodes, rest) = self.endpoint_list(next, true, grammar)?;
            tail = rest;
            if let Some(edge_label) = rest.strip_prefix(':') {
                label = Some(edge_label.trim());
                tail = "";
            }
            for &from in &from_nodes {
                for &to in &to_nodes {
                    self.edges.push(Edge {
                        from,
                        to,
                        label,
                        dotted: operator.contains('.'),
                        arrow: operator.contains('>'),
                    });
                }
            }
            from_nodes = to_nodes;
        }
        Ok(())
    }
}

fn parse_subgraph_title(rest: &str) -> Result<&str, &'static str> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Err("missing subgraph title");
    }
    if let Some(quoted) = trimmed.strip_prefix('"')
        && let Some(title) = quoted.strip_suffix('"')
        && !title.trim().is_empty()
    {
        return Ok(title.trim());
    }
    if let Some(open) = trimmed.find('[')
        && let Some(inner) = trimmed[open + 1..].strip_suffix(']')
    {
        let inner = inner.trim().trim_matches('"').trim();
        if !inner.is_empty() {
            return Ok(inner);
        }
    }
    Ok(trimmed)
}

fn connector_with_inline_label(text: &str, grammar: Grammar) -> Option<(&str, Option<&str>, &str)> {
    if !matches!(grammar, Grammar::Er) {
        if let Some(rest) = text.strip_prefix("-- ") {
            if let Some(pos) = rest.find(" -->") {
                let label = rest[..pos].trim();
                if !label.is_empty() {
                    return Some(("-->", Some(label), &rest[pos + 4..]));
                }
            } else if let Some(pos) = rest.find(" ---") {
                let label = rest[..pos].trim();
                if !label.is_empty() {
                    return Some(("---", Some(label), &rest[pos + 4..]));
                }
            }
        }
        if let Some(rest) = text.strip_prefix("-. ")
            && let Some(pos) = rest.find(" .->")
        {
            let label = rest[..pos].trim();
            if !label.is_empty() {
                return Some(("-.->", Some(label), &rest[pos + 4..]));
            }
        }
        if let Some(rest) = text.strip_prefix("== ")
            && let Some(pos) = rest.find(" ==>")
        {
            let label = rest[..pos].trim();
            if !label.is_empty() {
                return Some(("==>", Some(label), &rest[pos + 4..]));
            }
        }
    }
    let (operator, rest) = connector(text, grammar)?;
    Some((operator, None, rest))
}

fn split_label<'a>(text: &'a str, delimiter: &str) -> Option<(&'a str, &'a str)> {
    if let Some(quoted) = text.strip_prefix('"') {
        let (label, rest) = quoted.split_once('"')?;
        return Some((label, rest.strip_prefix(delimiter)?));
    }
    let (label, rest) = text.split_once(delimiter)?;
    Some((label.trim(), rest))
}

fn connector(text: &str, grammar: Grammar) -> Option<(&str, &str)> {
    if matches!(grammar, Grammar::Er) {
        let end = text
            .bytes()
            .take_while(|&byte| matches!(byte, b'|' | b'{' | b'}' | b'o' | b'-' | b'.'))
            .count();
        let operator = &text[..end];
        if operator.contains("--") || operator.contains("..") {
            return Some((operator, &text[end..]));
        }
        return None;
    }
    for operator in [
        "<-->", "-.->", "<|--", "--|>", "<|..", "..|>", "*--", "o--", "--*", "--o", "==>", "-->",
        "..>", "---", "--", "..",
    ] {
        if let Some(rest) = text.strip_prefix(operator) {
            return Some((&text[..operator.len()], rest));
        }
    }
    None
}

pub(super) fn render<'a>(
    direction: Option<&str>,
    lines: impl Iterator<Item = &'a str>,
    width: usize,
    grammar: Grammar,
) -> Result<Option<Art>, &'static str> {
    let (horizontal, reverse) = match direction {
        None | Some("TD" | "TB") => (false, false),
        Some("BT") => (false, true),
        Some("LR") => (true, false),
        Some("RL") => (true, true),
        _ => return Err("unsupported flowchart direction"),
    };
    let mut graph = Graph::default();
    for line in lines {
        let mut depth = 0usize;
        let mut quoted = false;
        let mut start = 0;
        for (i, c) in line.char_indices() {
            match c {
                '"' => quoted = !quoted,
                '(' | '[' | '{' if !quoted => depth += 1,
                ')' | ']' | '}' if !quoted => depth = depth.saturating_sub(1),
                ';' if !quoted && depth == 0 => {
                    graph.statement(line[start..i].trim(), grammar)?;
                    start = i + 1;
                }
                _ => {}
            }
        }
        graph.statement(line[start..].trim(), grammar)?;
        if graph.nodes.len() > 128 || graph.edges.len() > 256 {
            return Ok(None);
        }
    }
    if !graph.subgraph_stack.is_empty() {
        return Err("unclosed subgraph");
    }
    if graph.nodes.is_empty() {
        return Ok(None);
    }
    let Some(layout) = Layout::build(&graph, horizontal, reverse, width) else {
        return Ok(None);
    };
    Ok(Some(layout.draw(&graph)))
}

#[derive(Clone, Copy, Default)]
struct Position {
    axis: usize,
    lane: usize,
    major: usize,
    minor: usize,
}

#[derive(Clone, Copy)]
struct SubgraphBox {
    index: usize,
    depth: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

struct Layout {
    horizontal: bool,
    reverse: bool,
    major: usize,
    canvas_width: usize,
    canvas_height: usize,
    shift_x: usize,
    shift_y: usize,
    positions: Vec<Position>,
    rank: Vec<usize>,
    extra_lane: usize,
    subgraph_boxes: Vec<SubgraphBox>,
}

impl Layout {
    fn build(graph: &Graph<'_>, horizontal: bool, reverse: bool, width: usize) -> Option<Self> {
        let rank = ranks(graph);
        let mut groups = vec![Vec::new(); rank.iter().max().copied()? + 1];
        let mut positions = Vec::with_capacity(graph.nodes.len());
        for (index, node) in graph.nodes.values().enumerate() {
            let node_width = visible_width(node.label) + 4;
            let (major, minor) = if horizontal {
                (node_width, 3)
            } else {
                (3, node_width)
            };
            positions.push(Position {
                major,
                minor,
                ..Position::default()
            });
            groups[rank[index]].push(index);
        }
        let has_subgraphs = !graph.subgraphs.is_empty();
        if has_subgraphs {
            for group in &mut groups {
                group.sort_by_key(|&node_idx| graph.node_subgraph[node_idx]);
            }
        }
        let widths: Vec<_> = groups
            .iter()
            .map(|items| {
                let mut used = 0usize;
                for (pos, &item) in items.iter().enumerate() {
                    if pos > 0 {
                        let same_sg = !has_subgraphs
                            || graph.node_subgraph[items[pos - 1]] == graph.node_subgraph[item];
                        used += if same_sg { 3 } else { 7 };
                    }
                    used += positions[item].minor;
                }
                used
            })
            .collect();
        let base_minor = widths.iter().max().copied()?;
        let extra = graph
            .edges
            .iter()
            .any(|edge| rank[edge.to] != rank[edge.from] + 1);
        let extra_lane = base_minor + 2;
        let minor = if extra { extra_lane + 1 } else { base_minor };
        let gap = if horizontal || graph.edges.iter().any(|edge| edge.label.is_some()) {
            4
        } else {
            2
        };
        let mut axis = usize::from(extra) * 2;
        for (rank_idx, (group, used)) in groups.iter().zip(widths).enumerate() {
            let mut lane = (base_minor - used) / 2;
            let major = group
                .iter()
                .map(|&index| positions[index].major)
                .max()
                .unwrap_or(0);
            for (pos, &index) in group.iter().enumerate() {
                if pos > 0 {
                    let same_sg = !has_subgraphs
                        || graph.node_subgraph[group[pos - 1]] == graph.node_subgraph[index];
                    lane += if same_sg { 3 } else { 7 };
                }
                let place = &mut positions[index];
                place.axis = axis + (major - place.major) / 2;
                place.lane = lane;
                lane += place.minor;
            }
            let cross_sg_rank = has_subgraphs
                && groups.get(rank_idx + 1).is_some_and(|next_group| {
                    group.iter().any(|&a| {
                        next_group
                            .iter()
                            .any(|&b| graph.node_subgraph[a] != graph.node_subgraph[b])
                    })
                });
            axis += major + gap + if cross_sg_rank { 4 } else { 0 };
        }
        let last_cross_gap = if has_subgraphs && groups.len() >= 2 {
            // Only the gaps between ranks exist; the final rank did not need trailing cross gap.
            let prev = &groups[groups.len() - 2];
            let last = &groups[groups.len() - 1];
            let _ = (prev, last);
            0
        } else {
            0
        };
        let major = axis - gap + last_cross_gap + usize::from(extra) * 2;
        let (base_width, base_height) = if horizontal {
            (major, minor)
        } else {
            (minor, major)
        };

        if !has_subgraphs {
            if base_width > width || major.saturating_mul(minor) > 200_000 {
                return None;
            }
            return Some(Self {
                horizontal,
                reverse,
                major,
                canvas_width: base_width,
                canvas_height: base_height,
                shift_x: 0,
                shift_y: 0,
                positions,
                rank,
                extra_lane,
                subgraph_boxes: Vec::new(),
            });
        }

        let raw_xy = |a: usize, l: usize| -> (isize, isize) {
            let a = if reverse { major - 1 - a } else { a };
            if horizontal {
                (a as isize, l as isize)
            } else {
                (l as isize, a as isize)
            }
        };

        let mut sg_bounds: Vec<Option<(isize, isize, isize, isize)>> =
            vec![None; graph.subgraphs.len()];
        let extend_bounds = |slot: &mut Option<(isize, isize, isize, isize)>,
                             rect: (isize, isize, isize, isize)| {
            *slot = Some(match *slot {
                Some((x0, y0, x1, y1)) => (
                    x0.min(rect.0),
                    y0.min(rect.1),
                    x1.max(rect.2),
                    y1.max(rect.3),
                ),
                None => rect,
            });
        };

        for (node_idx, place) in positions.iter().enumerate() {
            if let Some(sg_idx) = graph.node_subgraph[node_idx] {
                let (x0, y0) = raw_xy(place.axis, place.lane);
                let (x1, y1) = raw_xy(place.axis + place.major - 1, place.lane + place.minor - 1);
                extend_bounds(
                    &mut sg_bounds[sg_idx],
                    (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)),
                );
            }
        }

        let mut order: Vec<usize> = (0..graph.subgraphs.len()).collect();
        order.sort_by_key(|&idx| std::cmp::Reverse(graph.subgraphs[idx].depth));
        for &sg_idx in &order {
            if let Some((x0, y0, x1, y1)) = sg_bounds[sg_idx] {
                let min_w = visible_width(graph.subgraphs[sg_idx].title) as isize + 6;
                let expanded_x0 = x0 - 2;
                let expanded_y0 = y0 - 2;
                let expanded_x1 = (x1 + 2).max(expanded_x0 + min_w - 1);
                let expanded_y1 = y1 + 1;
                let expanded = (expanded_x0, expanded_y0, expanded_x1, expanded_y1);
                sg_bounds[sg_idx] = Some(expanded);
                if let Some(parent) = graph.subgraphs[sg_idx].parent {
                    extend_bounds(&mut sg_bounds[parent], expanded);
                }
            }
        }

        let mut min_x = 0isize;
        let mut min_y = 0isize;
        let mut max_x = base_width.saturating_sub(1) as isize;
        let mut max_y = base_height.saturating_sub(1) as isize;
        for bounds in sg_bounds.iter().flatten() {
            min_x = min_x.min(bounds.0);
            min_y = min_y.min(bounds.1);
            max_x = max_x.max(bounds.2);
            max_y = max_y.max(bounds.3);
        }
        let shift_x = (-min_x).max(0) as usize;
        let shift_y = (-min_y).max(0) as usize;
        let canvas_width = (max_x - min_x + 1) as usize;
        let canvas_height = (max_y - min_y + 1) as usize;
        if canvas_width > width || canvas_width.saturating_mul(canvas_height) > 200_000 {
            return None;
        }
        let mut subgraph_boxes = Vec::new();
        for (idx, bounds) in sg_bounds.into_iter().enumerate() {
            if let Some((x0, y0, x1, y1)) = bounds {
                subgraph_boxes.push(SubgraphBox {
                    index: idx,
                    depth: graph.subgraphs[idx].depth,
                    x0: (x0 + shift_x as isize) as usize,
                    y0: (y0 + shift_y as isize) as usize,
                    x1: (x1 + shift_x as isize) as usize,
                    y1: (y1 + shift_y as isize) as usize,
                });
            }
        }
        subgraph_boxes.sort_by_key(|sg| sg.depth);

        Some(Self {
            horizontal,
            reverse,
            major,
            canvas_width,
            canvas_height,
            shift_x,
            shift_y,
            positions,
            rank,
            extra_lane,
            subgraph_boxes,
        })
    }

    fn xy(&self, axis: usize, lane: usize) -> (usize, usize) {
        let axis = if self.reverse {
            self.major - 1 - axis
        } else {
            axis
        };
        let (x, y) = if self.horizontal {
            (axis, lane)
        } else {
            (lane, axis)
        };
        (x + self.shift_x, y + self.shift_y)
    }

    fn draw(&self, graph: &Graph<'_>) -> Art {
        let mut canvas = Canvas::new(self.canvas_width, self.canvas_height);
        for sg in &self.subgraph_boxes {
            canvas.subgraph(sg.x0, sg.y0, sg.x1, sg.y1, graph.subgraphs[sg.index].title);
        }
        for (node, place) in graph.nodes.values().zip(&self.positions) {
            let (x0, y0) = self.xy(place.axis, place.lane);
            let (x1, y1) = self.xy(place.axis + place.major - 1, place.lane + place.minor - 1);
            canvas.node(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1), node);
        }
        for edge in &graph.edges {
            let from = self.positions[edge.from];
            let to = self.positions[edge.to];
            let first_axis = from.axis + from.major - 1;
            let last_axis = to.axis - 1;
            let first_lane = from.lane + from.minor / 2;
            let last_lane = to.lane + to.minor / 2;
            let arrow = if self.horizontal {
                if self.reverse { '◀' } else { '▶' }
            } else if self.reverse {
                '▲'
            } else {
                '▼'
            };
            if self.rank[edge.to] == self.rank[edge.from] + 1 {
                let middle = (first_axis + last_axis).div_ceil(2).min(last_axis - 1);
                canvas.path(
                    &[
                        (first_axis, first_lane),
                        (middle, first_lane),
                        (middle, last_lane),
                        (last_axis, last_lane),
                    ]
                    .map(|(a, l)| self.xy(a, l)),
                    edge.dotted,
                );
                if let Some(label) = edge.label {
                    let (x, y) = self.xy(middle, first_lane);
                    canvas.label(
                        y.saturating_sub(usize::from(self.horizontal)),
                        x + usize::from(!self.horizontal),
                        label,
                        Role::Edge,
                    );
                }
            } else {
                // Long, back, and self edges take an outside lane so they never draw through a node.
                let points = [
                    (first_axis, first_lane),
                    (first_axis + 1, first_lane),
                    (first_axis + 1, self.extra_lane),
                    (last_axis - 1, self.extra_lane),
                    (last_axis - 1, last_lane),
                    (last_axis, last_lane),
                ];
                canvas.path(&points.map(|(a, l)| self.xy(a, l)), edge.dotted);
                if let Some(label) = edge.label {
                    let (x, y) = self.xy(first_axis + 1, self.extra_lane);
                    canvas.label(
                        y.saturating_sub(1),
                        x + usize::from(!self.horizontal),
                        label,
                        Role::Edge,
                    );
                }
            }
            let (x, y) = self.xy(last_axis, last_lane);
            if edge.arrow {
                canvas.mark(x, y, arrow);
            }
        }
        canvas.into_art()
    }
}

fn ranks(graph: &Graph<'_>) -> Vec<usize> {
    let count = graph.nodes.len();
    let mut outgoing = vec![Vec::new(); count];
    let mut incoming = vec![0usize; count];
    for edge in &graph.edges {
        if edge.from != edge.to {
            outgoing[edge.from].push(edge.to);
            incoming[edge.to] += 1;
        }
    }
    let mut done = vec![false; count];
    let mut rank = vec![0; count];
    while let Some(index) = (0..count)
        .find(|&i| !done[i] && incoming[i] == 0)
        .or_else(|| (0..count).find(|&i| !done[i]))
    {
        done[index] = true;
        for &next in &outgoing[index] {
            if !done[next] {
                incoming[next] -= 1;
                rank[next] = rank[next].max(rank[index] + 1);
            }
        }
    }
    rank
}

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

#[derive(Clone, Copy, Default)]
struct Cell {
    links: u8,
    border: bool,
    round: bool,
    dotted: bool,
    mark: Option<char>,
}

struct Canvas<'a> {
    width: usize,
    height: usize,
    grid: Vec<Cell>,
    labels: Vec<Vec<(usize, &'a str, Role)>>,
}

impl<'a> Canvas<'a> {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            grid: vec![Cell::default(); width * height],
            labels: vec![Vec::new(); height],
        }
    }

    fn mark(&mut self, x: usize, y: usize, mark: char) {
        self.grid[y * self.width + x].mark = Some(mark);
    }

    fn label(&mut self, y: usize, x: usize, label: &'a str, role: Role) {
        if x + visible_width(label) <= self.width && y < self.height {
            self.labels[y].push((x, label, role));
        }
    }

    fn subgraph(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, title: &'a str) {
        self.path(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)], false);
        for y in y0..=y1 {
            for x in x0..=x1 {
                if x == x0 || x == x1 || y == y0 || y == y1 {
                    let cell = &mut self.grid[y * self.width + x];
                    cell.border = true;
                    cell.round = false;
                }
            }
        }
        self.label(y0, x0 + 2, title, Role::Title);
    }

    fn node(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, node: &Node<'a>) {
        self.path(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)], false);
        for y in y0..=y1 {
            for x in x0..=x1 {
                if x == x0 || x == x1 || y == y0 || y == y1 {
                    let cell = &mut self.grid[y * self.width + x];
                    cell.border = true;
                    cell.round = node.shape != Shape::Box;
                }
            }
        }
        self.label(y0 + 1, x0 + 2, node.label, Role::Text);
    }

    fn path(&mut self, points: &[(usize, usize)], dotted: bool) {
        for pair in points.windows(2) {
            let [(mut x, mut y), (end_x, end_y)] = [pair[0], pair[1]];
            while (x, y) != (end_x, end_y) {
                let (next_x, next_y, dir, inverse) = if x < end_x {
                    (x + 1, y, RIGHT, LEFT)
                } else if x > end_x {
                    (x - 1, y, LEFT, RIGHT)
                } else if y < end_y {
                    (x, y + 1, DOWN, UP)
                } else {
                    (x, y - 1, UP, DOWN)
                };
                let current = &mut self.grid[y * self.width + x];
                current.links |= dir;
                current.dotted |= dotted;
                let next = &mut self.grid[next_y * self.width + next_x];
                next.links |= inverse;
                next.dotted |= dotted;
                (x, y) = (next_x, next_y);
            }
        }
    }

    fn into_art(self) -> Art {
        let mut art = Art::default();
        for (row, mut labels) in self.grid.chunks(self.width).zip(self.labels) {
            labels.sort_unstable_by_key(|(x, _, role)| (*x, *role == Role::Edge));
            let mut labels = labels.into_iter().peekable();
            let mut spans = Vec::<(Role, String)>::new();
            let mut x = 0;
            while x < self.width {
                while labels.peek().is_some_and(|(start, _, _)| *start < x) {
                    labels.next();
                }
                if labels.peek().is_some_and(|(start, _, _)| *start == x)
                    && let Some((_, label, role)) = labels.next()
                    && !label.is_empty()
                {
                    let width = visible_width(label);
                    if width != 0
                        && (matches!(role, Role::Text | Role::Title)
                            || row[x..x + width].iter().all(|cell| !cell.border))
                    {
                        spans.push((role, label.to_owned()));
                        x += width;
                        continue;
                    }
                }
                let cell = row[x];
                let role = if cell.border {
                    Role::Border
                } else {
                    Role::Edge
                };
                let glyph = glyph(cell);
                if let Some((current, text)) =
                    spans.last_mut().filter(|(r, _)| *r == role || glyph == ' ')
                {
                    text.push(glyph);
                    if glyph != ' ' {
                        *current = role;
                    }
                } else {
                    spans.push((role, glyph.to_string()));
                }
                x += 1;
            }
            while let Some((_, tail)) = spans.last_mut() {
                tail.truncate(tail.trim_end_matches(' ').len());
                if tail.is_empty() {
                    spans.pop();
                } else {
                    break;
                }
            }
            art.0.push(spans);
        }
        while art.0.last().is_some_and(Vec::is_empty) {
            art.0.pop();
        }
        art
    }
}

fn glyph(cell: Cell) -> char {
    if let Some(mark) = cell.mark {
        return mark;
    }
    match cell.links {
        1..=3 => {
            if cell.dotted && !cell.border {
                '┆'
            } else {
                '│'
            }
        }
        4 | 8 | 12 => {
            if cell.dotted && !cell.border {
                '╌'
            } else {
                '─'
            }
        }
        5 => {
            if cell.round {
                '╯'
            } else {
                '┘'
            }
        }
        6 => {
            if cell.round {
                '╮'
            } else {
                '┐'
            }
        }
        9 => {
            if cell.round {
                '╰'
            } else {
                '└'
            }
        }
        10 => {
            if cell.round {
                '╭'
            } else {
                '┌'
            }
        }
        7 => '┤',
        11 => '├',
        13 => '┴',
        14 => '┬',
        15 => '┼',
        _ => ' ',
    }
}
