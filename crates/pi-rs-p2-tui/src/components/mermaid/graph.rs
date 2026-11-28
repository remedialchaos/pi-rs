use indexmap::IndexMap;
use pi_rs_tui::visible_width;

use super::{Art, RenderOutcome, Role};

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

#[derive(Default)]
struct Graph<'a> {
    nodes: IndexMap<&'a str, Node<'a>>,
    edges: Vec<Edge<'a>>,
}

impl<'a> Graph<'a> {
    fn node(&mut self, id: &'a str, label: Option<&'a str>, shape: Shape) -> usize {
        if let Some((index, _, node)) = self.nodes.get_full_mut(id) {
            if let Some(label) = label {
                *node = Node { label, shape };
            }
            index
        } else {
            self.nodes
                .insert_full(
                    id,
                    Node {
                        label: label.unwrap_or(id),
                        shape,
                    },
                )
                .0
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
        let id_end = text
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.')))
            .unwrap_or(text.len());
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
        if first == "subgraph" || first == "end" {
            return Err("nested subgraphs are not supported");
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
        let (mut from, mut tail) = self.endpoint(text, false, grammar)?;
        if let Some(label) = tail.strip_prefix(':') {
            if let Some((_, node)) = self.nodes.get_index_mut(from) {
                node.label = label.trim();
            }
            return Ok(());
        }
        while !tail.is_empty() {
            let (operator, mut next) =
                connector(tail, grammar).ok_or("unsupported graph syntax")?;
            let mut label = None;
            if let Some(edge_label) = next.trim_start().strip_prefix('|') {
                let (edge_label, rest) =
                    split_label(edge_label, "|").ok_or("unclosed edge label")?;
                label = Some(edge_label);
                next = rest;
            }
            let (to, rest) = self.endpoint(next, true, grammar)?;
            tail = rest;
            if let Some(edge_label) = rest.strip_prefix(':') {
                label = Some(edge_label.trim());
                tail = "";
            }
            self.edges.push(Edge {
                from,
                to,
                label,
                dotted: operator.contains('.'),
                arrow: operator.contains('>'),
            });
            from = to;
        }
        Ok(())
    }
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
) -> Result<RenderOutcome, &'static str> {
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
            return Ok(RenderOutcome::Empty);
        }
    }
    if graph.nodes.is_empty() {
        return Ok(RenderOutcome::Empty);
    }
    let layout = match Layout::build(&graph, horizontal, reverse, width) {
        LayoutOutcome::Built(layout) => layout,
        LayoutOutcome::TooWide(needed) => return Ok(RenderOutcome::TooWide { needed }),
        LayoutOutcome::Empty => return Ok(RenderOutcome::Empty),
    };
    Ok(RenderOutcome::Rendered(layout.draw(&graph)))
}

#[derive(Clone, Copy, Default)]
struct Position {
    axis: usize,
    lane: usize,
    major: usize,
    minor: usize,
}

struct Layout {
    horizontal: bool,
    reverse: bool,
    major: usize,
    minor: usize,
    positions: Vec<Position>,
    rank: Vec<usize>,
    extra_lane: usize,
}

enum LayoutOutcome {
    Built(Layout),
    TooWide(usize),
    Empty,
}

impl Layout {
    fn build(graph: &Graph<'_>, horizontal: bool, reverse: bool, width: usize) -> LayoutOutcome {
        let rank = ranks(graph);
        let Some(max_rank) = rank.iter().max().copied() else {
            return LayoutOutcome::Empty;
        };
        let mut groups = vec![Vec::new(); max_rank + 1];
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
        let widths: Vec<_> = groups
            .iter()
            .map(|items| {
                items
                    .iter()
                    .map(|&item| positions[item].minor)
                    .sum::<usize>()
                    + 3 * items.len().saturating_sub(1)
            })
            .collect();
        let Some(base_minor) = widths.iter().max().copied() else {
            return LayoutOutcome::Empty;
        };
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
        for (group, used) in groups.iter().zip(widths) {
            let mut lane = (base_minor - used) / 2;
            let major = group
                .iter()
                .map(|&index| positions[index].major)
                .max()
                .unwrap_or(0);
            for &index in group {
                let place = &mut positions[index];
                place.axis = axis + (major - place.major) / 2;
                place.lane = lane;
                lane += place.minor + 3;
            }
            axis += major + gap;
        }
        let major = axis - gap + usize::from(extra) * 2;
        let visible = if horizontal { major } else { minor };
        if major.saturating_mul(minor) > 200_000 {
            return LayoutOutcome::Empty;
        }
        if visible > width {
            return LayoutOutcome::TooWide(visible);
        }
        LayoutOutcome::Built(Self {
            horizontal,
            reverse,
            major,
            minor,
            positions,
            rank,
            extra_lane,
        })
    }

    fn xy(&self, axis: usize, lane: usize) -> (usize, usize) {
        let axis = if self.reverse {
            self.major - 1 - axis
        } else {
            axis
        };
        if self.horizontal {
            (axis, lane)
        } else {
            (lane, axis)
        }
    }

    fn draw(&self, graph: &Graph<'_>) -> Art {
        let (width, height) = if self.horizontal {
            (self.major, self.minor)
        } else {
            (self.minor, self.major)
        };
        let mut canvas = Canvas::new(width, height);
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
                        && (role == Role::Text || row[x..x + width].iter().all(|cell| !cell.border))
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
