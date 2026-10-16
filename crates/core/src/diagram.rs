//! Schema diagrams: a schema's tables with their columns and the foreign
//! keys between them, laid out in layers so relations read one way. Tables
//! others refer to (customers, products) sit on the left and the tables that
//! depend on them to their right, ordered to keep lines from crossing;
//! tables without relations are set apart. Each foreign key is a wire from
//! the referencing column to the referenced one.
//!
//! Everything here is geometry over plain data, so it is testable without a
//! database and shared by the diagram tab and its exports (SVG, Mermaid and
//! DBML). A diagram can also be narrowed to one table's neighborhood
//! ([`Diagram::around`]), and cards can be moved and placed back
//! ([`Layout::place`]), pushing the cards they land on out of the way
//! ([`Layout::push_apart`]), and tidied up again keeping their columns
//! ([`untangle`]).
//!
//! The layers are ordered to cross few lines: relations that skip layers
//! get a waypoint in each layer they pass, so they count when ordering and
//! get room between cards; layers are swept by where their links lead
//! (down to the row each line leaves from) and neighbors swapped while that
//! crosses less, keeping the order with the fewest crossings. Cards then sit
//! level with the tables they relate to, and every wire passes the columns
//! between its ends through their gaps.

use crate::catalog::TableRef;

/// Card width, header height and column row height, in points.
pub const CARD_WIDTH: f32 = 248.;
pub const HEADER: f32 = 34.;
pub const ROW: f32 = 22.;
/// Space under the last column.
const PADDING: f32 = 6.;
/// Between layers, and between cards of a layer.
const LAYER_GAP: f32 = 150.;
const CARD_GAP: f32 = 36.;
/// The least space between columns [`untangle`] keeps.
const MIN_LAYER_GAP: f32 = 80.;
const MARGIN: f32 = 40.;
/// The room a relation passing through a layer takes, and its space from
/// the cards around it.
const WAYPOINT: f32 = 8.;
const WAYPOINT_GAP: f32 = 14.;
/// How far a wire keeps from the cards it passes.
const CLEARANCE: f32 = 12.;
/// The grid cards snap to, and the least space a dropped card leaves.
pub const GRID: f32 = 20.;
const PUSH_SPACE: f32 = 20.;
/// Ordering sweeps, rounds of swapping neighbors, and alignment passes.
const SWEEPS: usize = 8;
const TRANSPOSES: usize = 12;
const ALIGN_PASSES: usize = 9;

#[derive(Debug, Clone, PartialEq)]
pub struct Diagram {
    pub schema: String,
    pub tables: Vec<Table>,
    pub relations: Vec<Relation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub table: TableRef,
    /// The planner's estimate; `None` before the table is analyzed.
    pub rows: Option<u64>,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// Part of a foreign key.
    pub foreign_key: bool,
}

impl Column {
    pub fn is_key(&self) -> bool {
        self.primary_key || self.foreign_key
    }

    /// The type in the short forms PostgreSQL also accepts
    /// (`timestamptz` for `timestamp with time zone`), to fit a card.
    pub fn short_type(&self) -> String {
        const SHORT: [(&str, &str); 7] = [
            ("timestamp with time zone", "timestamptz"),
            ("timestamp without time zone", "timestamp"),
            ("time with time zone", "timetz"),
            ("time without time zone", "time"),
            ("character varying", "varchar"),
            ("double precision", "float8"),
            ("character", "char"),
        ];
        let data_type = self.data_type.as_str();
        for (long, short) in SHORT {
            if let Some(rest) = data_type.strip_prefix(long) {
                return format!("{short}{rest}");
            }
        }
        match data_type {
            "boolean" => "bool".into(),
            "integer" => "int4".into(),
            "bigint" => "int8".into(),
            "smallint" => "int2".into(),
            other => other.to_string(),
        }
    }
}

/// What deleting a referenced row does to the rows that refer to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl OnDelete {
    /// From the rule's name, as SQLite and MySQL report it (`CASCADE`,
    /// `SET NULL`…).
    pub fn from_rule(rule: &str) -> Self {
        match rule.trim().to_uppercase().as_str() {
            "RESTRICT" => OnDelete::Restrict,
            "CASCADE" => OnDelete::Cascade,
            "SET NULL" => OnDelete::SetNull,
            "SET DEFAULT" => OnDelete::SetDefault,
            _ => OnDelete::NoAction,
        }
    }

    /// From `pg_constraint.confdeltype`.
    pub fn from_code(code: &str) -> Self {
        match code {
            "r" => OnDelete::Restrict,
            "c" => OnDelete::Cascade,
            "n" => OnDelete::SetNull,
            "d" => OnDelete::SetDefault,
            _ => OnDelete::NoAction,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OnDelete::NoAction => "NO ACTION",
            OnDelete::Restrict => "RESTRICT",
            OnDelete::Cascade => "CASCADE",
            OnDelete::SetNull => "SET NULL",
            OnDelete::SetDefault => "SET DEFAULT",
        }
    }
}

/// A foreign key: rows of `from` refer to rows of `to`.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub name: String,
    pub from: TableRef,
    pub from_columns: Vec<String>,
    pub to: TableRef,
    pub to_columns: Vec<String>,
    /// A referencing column may be NULL: the relation is optional.
    pub optional: bool,
    pub on_delete: OnDelete,
}

impl Table {
    /// The columns a card lists: all of them, or only the keys.
    pub fn shown_columns(&self, keys_only: bool) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|column| !keys_only || column.is_key())
            .collect()
    }

    pub fn card_height(&self, keys_only: bool) -> f32 {
        HEADER + self.shown_columns(keys_only).len() as f32 * ROW + PADDING
    }

    /// The middle of a column's row, from the top of the card; the header's
    /// middle when the column is not shown.
    pub fn column_y(&self, column: &str, keys_only: bool) -> f32 {
        self.shown_columns(keys_only)
            .iter()
            .position(|c| c.name == column)
            .map_or(HEADER / 2., |row| HEADER + row as f32 * ROW + ROW / 2.)
    }
}

impl Diagram {
    pub fn index_of(&self, table: &TableRef) -> Option<usize> {
        self.tables.iter().position(|t| t.table == *table)
    }

    /// Relations between two tables of the diagram (references to tables
    /// elsewhere are left out).
    pub fn drawn_relations(&self) -> impl Iterator<Item = (usize, &Relation)> {
        self.relations
            .iter()
            .enumerate()
            .filter(|(_, r)| self.index_of(&r.from).is_some() && self.index_of(&r.to).is_some())
    }

    /// The part of the diagram within `hops` relations of `table`: those
    /// tables and the relations between them.
    pub fn around(&self, table: usize, hops: usize) -> Diagram {
        let mut kept = vec![table];
        let mut frontier = vec![table];
        for _ in 0..hops {
            let mut next = Vec::new();
            for &current in &frontier {
                for neighbor in self.neighbors(current) {
                    if !kept.contains(&neighbor) {
                        kept.push(neighbor);
                        next.push(neighbor);
                    }
                }
            }
            frontier = next;
        }
        kept.sort_unstable();
        let tables: Vec<Table> = kept.iter().map(|i| self.tables[*i].clone()).collect();
        let relations = self
            .relations
            .iter()
            .filter(|r| {
                tables.iter().any(|t| t.table == r.from) && tables.iter().any(|t| t.table == r.to)
            })
            .cloned()
            .collect();
        Diagram {
            schema: self.schema.clone(),
            tables,
            relations,
        }
    }

    /// A table's name as the text exports write it: bare in `public`,
    /// qualified elsewhere.
    fn export_name(&self, table: &TableRef) -> String {
        if table.schema == "public" {
            table.name.clone()
        } else {
            format!("{}.{}", table.schema, table.name)
        }
    }

    /// The diagram as a Mermaid `erDiagram`: each table with its columns
    /// (types without spaces or commas, which Mermaid does not take) and
    /// each relation from the referenced table to the referencing one.
    pub fn to_mermaid(&self) -> String {
        let name = |table: &TableRef| {
            let name = self.export_name(table);
            if name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                name
            } else {
                format!("\"{}\"", name.replace('"', "'"))
            }
        };
        let word = |text: &str| -> String {
            text.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric()
                        || c == '_'
                        || c == '('
                        || c == ')'
                        || c == '['
                        || c == ']'
                    {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        let mut out = String::from("erDiagram\n");
        for table in &self.tables {
            out.push_str(&format!("    {} {{\n", name(&table.table)));
            for column in &table.columns {
                let keys = match (column.primary_key, column.foreign_key) {
                    (true, true) => " PK, FK",
                    (true, false) => " PK",
                    (false, true) => " FK",
                    (false, false) => "",
                };
                out.push_str(&format!(
                    "        {} {}{keys}\n",
                    word(&column.short_type()),
                    word(&column.name)
                ));
            }
            out.push_str("    }\n");
        }
        for relation in &self.relations {
            // `||` exactly one parent, `|o` at most one when the key is
            // optional; `o{` any number of children.
            let parent = if relation.optional { "|o" } else { "||" };
            out.push_str(&format!(
                "    {} {parent}--o{{ {} : \"{}\"\n",
                name(&relation.to),
                name(&relation.from),
                relation.from_columns.join(", ").replace('"', "'")
            ));
        }
        out
    }

    /// The diagram as DBML (dbdiagram.io): tables with their columns and a
    /// `Ref` per foreign key, many-to-one, with its delete rule.
    pub fn to_dbml(&self) -> String {
        let quote = |text: &str| {
            if text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                text.to_string()
            } else {
                format!("\"{}\"", text.replace('"', "\\\""))
            }
        };
        let table_name = |table: &TableRef| {
            if table.schema == "public" {
                quote(&table.name)
            } else {
                format!("{}.{}", quote(&table.schema), quote(&table.name))
            }
        };
        let columns = |table: &TableRef, columns: &[String]| {
            let names: Vec<String> = columns.iter().map(|c| quote(c)).collect();
            if names.len() == 1 {
                format!("{}.{}", table_name(table), names[0])
            } else {
                format!("{}.({})", table_name(table), names.join(", "))
            }
        };
        let mut out = String::new();
        for table in &self.tables {
            out.push_str(&format!("Table {} {{\n", table_name(&table.table)));
            for column in &table.columns {
                let mut settings = Vec::new();
                if column.primary_key {
                    settings.push("pk");
                } else if !column.nullable {
                    settings.push("not null");
                }
                let settings = if settings.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", settings.join(", "))
                };
                out.push_str(&format!(
                    "  {} {}{settings}\n",
                    quote(&column.name),
                    quote(&column.data_type)
                ));
            }
            out.push_str("}\n\n");
        }
        for relation in &self.relations {
            let delete = match relation.on_delete {
                OnDelete::NoAction => String::new(),
                other => format!(" [delete: {}]", other.label().to_lowercase()),
            };
            out.push_str(&format!(
                "Ref {}: {} > {}{delete}\n",
                quote(&relation.name),
                columns(&relation.from, &relation.from_columns),
                columns(&relation.to, &relation.to_columns)
            ));
        }
        out
    }

    /// Tables related to `table` either way.
    pub fn neighbors(&self, table: usize) -> Vec<usize> {
        let name = &self.tables[table].table;
        let mut found: Vec<usize> = self
            .drawn_relations()
            .filter_map(|(_, r)| {
                if r.from == *name {
                    self.index_of(&r.to)
                } else if r.to == *name {
                    self.index_of(&r.from)
                } else {
                    None
                }
            })
            .filter(|i| *i != table)
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    }
}

/// Where a card goes, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Card {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Card {
    /// The card overlaps `other`, or comes closer than `space` to it.
    fn crowds(&self, other: &Card, space: f32) -> bool {
        self.x < other.x + other.width + space
            && other.x < self.x + self.width + space
            && self.y < other.y + other.height + space
            && other.y < self.y + self.height + space
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// One per table, in the diagram's order.
    pub cards: Vec<Card>,
    pub width: f32,
    pub height: f32,
}

/// `value` on the grid cards snap to while dragged.
pub fn snap(value: f32) -> f32 {
    (value / GRID).round() * GRID
}

impl Layout {
    /// Moves card `index` to `(x, y)` (never above or left of the margin)
    /// and grows the layout to hold it.
    pub fn place(&mut self, index: usize, x: f32, y: f32) {
        let Some(card) = self.cards.get_mut(index) else {
            return;
        };
        card.x = x.max(MARGIN / 2.);
        card.y = y.max(MARGIN / 2.);
        self.fit();
    }

    /// Pushes the cards that card `moved` lands on (or comes too close to)
    /// out of its way, the shortest way out and away from it, and the
    /// cards those land on out of theirs. Returns the cards pushed.
    pub fn push_apart(&mut self, moved: usize) -> Vec<usize> {
        let count = self.cards.len();
        if moved >= count {
            return Vec::new();
        }
        let mut settled = vec![false; count];
        settled[moved] = true;
        let mut queue = vec![moved];
        let mut pushed = Vec::new();
        while let Some(pusher) = queue.pop() {
            for other in 0..count {
                let blocker = self.cards[pusher];
                if settled[other] || !self.cards[other].crowds(&blocker, PUSH_SPACE) {
                    continue;
                }
                let direction = self.way_out(other, &blocker);
                // Further along the same way while it lands on a card that
                // already stays put.
                for _ in 0..count {
                    let card = self.cards[other];
                    let Some(block) = (0..count)
                        .filter(|i| settled[*i])
                        .map(|i| self.cards[i])
                        .find(|c| card.crowds(c, PUSH_SPACE))
                    else {
                        break;
                    };
                    let card = &mut self.cards[other];
                    match direction {
                        (dx, _) if dx > 0. => card.x = block.x + block.width + PUSH_SPACE,
                        (dx, _) if dx < 0. => card.x = block.x - PUSH_SPACE - card.width,
                        (_, dy) if dy > 0. => card.y = block.y + block.height + PUSH_SPACE,
                        _ => card.y = block.y - PUSH_SPACE - card.height,
                    }
                }
                settled[other] = true;
                queue.push(other);
                pushed.push(other);
            }
        }
        self.fit();
        pushed
    }

    /// The shortest way to move card `index` off `blocker`, preferring to
    /// move away from it and never past the margin: a unit step on one
    /// axis.
    fn way_out(&self, index: usize, blocker: &Card) -> (f32, f32) {
        let card = self.cards[index];
        let center = |c: &Card| (c.x + c.width / 2., c.y + c.height / 2.);
        let (cx, cy) = center(&card);
        let (bx, by) = center(blocker);
        let ways = [
            (
                (1., 0.),
                blocker.x + blocker.width + PUSH_SPACE - card.x,
                cx >= bx,
            ),
            (
                (-1., 0.),
                card.x + card.width + PUSH_SPACE - blocker.x,
                cx < bx,
            ),
            (
                (0., 1.),
                blocker.y + blocker.height + PUSH_SPACE - card.y,
                cy >= by,
            ),
            (
                (0., -1.),
                card.y + card.height + PUSH_SPACE - blocker.y,
                cy < by,
            ),
        ];
        ways.into_iter()
            .filter(|((dx, dy), distance, _)| {
                card.x + dx * distance >= MARGIN / 2. && card.y + dy * distance >= MARGIN / 2.
            })
            .map(|(way, distance, away)| (way, if away { distance } else { distance * 3. }))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or((0., 1.), |(way, _)| way)
    }

    /// The size that holds every card, with a margin.
    fn fit(&mut self) {
        self.width = self.cards.iter().map(|c| c.x + c.width).fold(0.0, f32::max) + MARGIN;
        self.height = self
            .cards
            .iter()
            .map(|c| c.y + c.height)
            .fold(0.0, f32::max)
            + MARGIN;
    }
}

/// Lays the tables out in layers (see the module docs).
pub fn layout(diagram: &Diagram, keys_only: bool) -> Layout {
    let count = diagram.tables.len();
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (_, relation) in diagram.drawn_relations() {
        let (Some(child), Some(parent)) = (
            diagram.index_of(&relation.from),
            diagram.index_of(&relation.to),
        ) else {
            continue;
        };
        if child != parent && !parents[child].contains(&parent) {
            parents[child].push(parent);
            children[parent].push(child);
        }
    }

    // Each table one layer right of the furthest table it refers to;
    // references that close a cycle are ignored.
    let mut layers = vec![0usize; count];
    let mut state = vec![0u8; count];
    for table in 0..count {
        assign_layer(table, &parents, &mut layers, &mut state);
    }
    let related: Vec<bool> = (0..count)
        .map(|t| !parents[t].is_empty() || !children[t].is_empty())
        .collect();
    let deepest = (0..count).filter(|t| related[*t]).map(|t| layers[t]).max();

    let mut columns: Vec<Vec<usize>> = match deepest {
        Some(deepest) => vec![Vec::new(); deepest + 1],
        None => Vec::new(),
    };
    let mut by_name: Vec<usize> = (0..count).collect();
    by_name.sort_by(|a, b| diagram.tables[*a].table.cmp(&diagram.tables[*b].table));
    for &table in &by_name {
        if related[table] {
            columns[layers[table]].push(table);
        }
    }
    let column_x = |index: usize| MARGIN + index as f32 * (CARD_WIDTH + LAYER_GAP);
    let xs: Vec<f32> = (0..columns.len()).map(column_x).collect();
    let mut cards: Vec<Card> = diagram
        .tables
        .iter()
        .map(|table| Card {
            x: 0.,
            y: 0.,
            width: CARD_WIDTH,
            height: table.card_height(keys_only),
        })
        .collect();
    let first_alone = columns.len();
    arrange(diagram, keys_only, columns, &xs, None, &mut cards);

    // Tables without relations, in columns as tall as the related ones (or
    // of a few cards when nothing is related), right of them.
    let alone: Vec<usize> = by_name.iter().copied().filter(|t| !related[*t]).collect();
    let tallest = (0..count)
        .filter(|t| related[*t])
        .map(|t| cards[t].y + cards[t].height - MARGIN)
        .fold(0.0, f32::max);
    let limit = if tallest > 0.0 {
        tallest
    } else {
        // About a square block.
        let side = (alone.len() as f32).sqrt().ceil().max(1.0) as usize;
        alone
            .iter()
            .take(side)
            .map(|t| cards[*t].height)
            .sum::<f32>()
            + CARD_GAP * side as f32
    };
    let (mut column, mut y) = (first_alone, MARGIN);
    for &table in &alone {
        let height = cards[table].height;
        if y > MARGIN && y + height - MARGIN > limit {
            column += 1;
            y = MARGIN;
        }
        cards[table].x = column_x(column);
        cards[table].y = y;
        y += height + CARD_GAP;
    }
    let mut layout = Layout {
        cards,
        width: 0.,
        height: 0.,
    };
    layout.fit();
    layout
}

/// Arranges a layout someone changed by hand: keeps its columns (cards
/// whose left edges are close) and about where each card is, but orders
/// each column to cross fewer lines, lines cards up with the tables they
/// relate to, makes room for relations that pass through a column and
/// pulls apart cards that overlap.
pub fn untangle(diagram: &Diagram, layout: &Layout, keys_only: bool) -> Layout {
    let count = diagram.tables.len().min(layout.cards.len());
    let mut by_x: Vec<usize> = (0..count).collect();
    by_x.sort_by(|a, b| layout.cards[*a].x.total_cmp(&layout.cards[*b].x));
    let mut columns: Vec<Vec<usize>> = Vec::new();
    let mut start = f32::NEG_INFINITY;
    for table in by_x {
        let x = layout.cards[table].x;
        if columns.is_empty() || x - start > CARD_WIDTH / 2. {
            columns.push(Vec::new());
            start = x;
        }
        if let Some(column) = columns.last_mut() {
            column.push(table);
        }
    }
    let mut xs: Vec<f32> = Vec::with_capacity(columns.len());
    for column in &mut columns {
        column.sort_by(|a, b| layout.cards[*a].y.total_cmp(&layout.cards[*b].y));
        let mean = column.iter().map(|t| layout.cards[*t].x).sum::<f32>() / column.len() as f32;
        let x = match xs.last() {
            Some(previous) => snap(mean).max(previous + CARD_WIDTH + MIN_LAYER_GAP),
            None => snap(mean).max(MARGIN / 2.),
        };
        xs.push(x);
    }
    let preferred: Vec<f32> = layout.cards.iter().map(|card| card.y).collect();
    let mut cards = layout.cards.clone();
    for (card, table) in cards.iter_mut().zip(&diagram.tables) {
        card.height = table.card_height(keys_only);
    }
    arrange(
        diagram,
        keys_only,
        columns,
        &xs,
        Some(&preferred),
        &mut cards,
    );
    let mut untangled = Layout {
        cards,
        width: 0.,
        height: 0.,
    };
    untangled.fit();
    untangled
}

/// Orders the tables of each column (left to right, each listed top to
/// bottom) to cross fewer lines and sets the tops of their cards so lines
/// run as straight as they can, near the `preferred` tops when given;
/// column `i` at `xs[i]`.
fn arrange(
    diagram: &Diagram,
    keys_only: bool,
    columns: Vec<Vec<usize>>,
    xs: &[f32],
    preferred: Option<&[f32]>,
    cards: &mut [Card],
) {
    let mut graph = Layered::new(diagram, keys_only, columns);
    graph.order();
    let tops = graph.align(preferred);
    let highest = graph
        .layers
        .iter()
        .flatten()
        .map(|node| tops[*node])
        .fold(f32::INFINITY, f32::min);
    if !highest.is_finite() {
        return;
    }
    // From the margin down, or where the cards were.
    let first = match preferred {
        Some(preferred) => graph
            .layers
            .iter()
            .flatten()
            .filter(|node| **node < graph.tables)
            .map(|node| preferred[*node])
            .fold(f32::INFINITY, f32::min)
            .max(MARGIN / 2.),
        None => MARGIN,
    };
    for (layer, nodes) in graph.layers.iter().enumerate() {
        for &node in nodes {
            if node < graph.tables {
                cards[node].x = xs[layer];
                cards[node].y = tops[node] - highest + first;
            }
        }
    }
}

fn assign_layer(
    table: usize,
    parents: &[Vec<usize>],
    layers: &mut [usize],
    state: &mut [u8],
) -> usize {
    match state[table] {
        2 => return layers[table],
        // On the current path: a cycle. This reference adds no layer.
        1 => return 0,
        _ => {}
    }
    state[table] = 1;
    let mut layer = 0;
    for &parent in &parents[table] {
        let parent_layer = assign_layer(parent, parents, layers, state);
        if state[parent] == 2 {
            layer = layer.max(parent_layer + 1);
        }
    }
    state[table] = 2;
    layers[table] = layer;
    layer
}

/// A link between nodes of neighboring layers: the other node, where the
/// link meets this node (from its top) and where it meets the other.
type Link = (usize, f32, f32);

/// The layered graph the layout orders: tables (nodes `0..tables`) and a
/// waypoint in every layer a relation passes through, so long relations
/// count when ordering and get room between cards.
struct Layered {
    tables: usize,
    heights: Vec<f32>,
    layers: Vec<Vec<usize>>,
    /// Each node's links to the layer on its left and on its right.
    left: Vec<Vec<Link>>,
    right: Vec<Vec<Link>>,
}

impl Layered {
    fn new(diagram: &Diagram, keys_only: bool, layers: Vec<Vec<usize>>) -> Self {
        let tables = diagram.tables.len();
        let mut layer_of = vec![usize::MAX; tables];
        for (layer, nodes) in layers.iter().enumerate() {
            for &table in nodes {
                layer_of[table] = layer;
            }
        }
        let mut graph = Layered {
            tables,
            heights: diagram
                .tables
                .iter()
                .map(|t| t.card_height(keys_only))
                .collect(),
            layers,
            left: vec![Vec::new(); tables],
            right: vec![Vec::new(); tables],
        };
        for (_, relation) in diagram.drawn_relations() {
            let (Some(from), Some(to)) = (
                diagram.index_of(&relation.from),
                diagram.index_of(&relation.to),
            ) else {
                continue;
            };
            let (from_layer, to_layer) = (layer_of[from], layer_of[to]);
            if from == to
                || from_layer == usize::MAX
                || to_layer == usize::MAX
                || from_layer == to_layer
            {
                continue;
            }
            let port = |table: usize, columns: &[String]| {
                diagram.tables[table]
                    .column_y(columns.first().map_or("", String::as_str), keys_only)
            };
            let from_end = (from, port(from, &relation.from_columns), from_layer);
            let to_end = (to, port(to, &relation.to_columns), to_layer);
            let (start, end) = if from_layer < to_layer {
                (from_end, to_end)
            } else {
                (to_end, from_end)
            };
            let (mut node, mut port) = (start.0, start.1);
            for layer in start.2 + 1..end.2 {
                let waypoint = graph.heights.len();
                graph.heights.push(WAYPOINT);
                graph.left.push(Vec::new());
                graph.right.push(Vec::new());
                graph.layers[layer].push(waypoint);
                graph.link(node, port, waypoint, WAYPOINT / 2.);
                (node, port) = (waypoint, WAYPOINT / 2.);
            }
            graph.link(node, port, end.0, end.1);
        }
        graph
    }

    fn link(&mut self, left: usize, left_port: f32, right: usize, right_port: f32) {
        self.right[left].push((right, left_port, right_port));
        self.left[right].push((left, right_port, left_port));
    }

    /// Each node's place in its layer.
    fn places(&self) -> Vec<usize> {
        let mut at = vec![0; self.heights.len()];
        for nodes in &self.layers {
            for (index, &node) in nodes.iter().enumerate() {
                at[node] = index;
            }
        }
        at
    }

    /// Where a link meets a node, in order down its layer: its place, plus
    /// how far down the node the link meets it.
    fn slot(&self, at: &[usize], node: usize, port: f32) -> f32 {
        at[node] as f32 + (port / self.heights[node]).clamp(0., 0.99)
    }

    /// How many pairs of links cross.
    fn crossings(&self) -> usize {
        let places = self.places();
        let at = places.as_slice();
        let mut total = 0;
        for nodes in &self.layers {
            let links: Vec<(f32, f32)> = nodes
                .iter()
                .flat_map(|&node| {
                    self.right[node].iter().map(move |&(other, own, theirs)| {
                        (self.slot(at, node, own), self.slot(at, other, theirs))
                    })
                })
                .collect();
            for (index, a) in links.iter().enumerate() {
                for b in &links[index + 1..] {
                    if (a.0 - b.0) * (a.1 - b.1) < 0. {
                        total += 1;
                    }
                }
            }
        }
        total
    }

    /// Orders every layer: sweeps that sort each layer by where its links
    /// lead in the layer before, then swaps of neighbors that cross fewer
    /// links, keeping the order that crosses least.
    fn order(&mut self) {
        let mut best = self.layers.clone();
        let mut fewest = self.crossings();
        for sweep in 0..SWEEPS {
            if fewest == 0 {
                break;
            }
            self.sweep(sweep % 2 == 0);
            self.transpose();
            let crossings = self.crossings();
            if crossings < fewest {
                fewest = crossings;
                best = self.layers.clone();
            }
        }
        self.layers = best;
    }

    fn sweep(&mut self, rightward: bool) {
        let count = self.layers.len();
        let layers: Vec<usize> = if rightward {
            (1..count).collect()
        } else {
            (0..count.saturating_sub(1)).rev().collect()
        };
        let mut sizes: Vec<f32> = self
            .layers
            .iter()
            .map(|nodes| nodes.len().max(1) as f32)
            .collect();
        let mut layer_of = vec![0; self.heights.len()];
        for (layer, nodes) in self.layers.iter().enumerate() {
            for &node in nodes {
                layer_of[node] = layer;
            }
        }
        for layer in layers {
            let at = self.places();
            let mut keyed: Vec<(f32, usize)> = self.layers[layer]
                .iter()
                .map(|&node| {
                    let links = if rightward {
                        &self.left[node]
                    } else {
                        &self.right[node]
                    };
                    let key = if links.is_empty() {
                        (at[node] as f32 + 0.5) / sizes[layer]
                    } else {
                        links
                            .iter()
                            .map(|&(other, _, theirs)| {
                                self.slot(&at, other, theirs) / sizes[layer_of[other]]
                            })
                            .sum::<f32>()
                            / links.len() as f32
                    };
                    (key, node)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
            self.layers[layer] = keyed.into_iter().map(|(_, node)| node).collect();
            sizes[layer] = self.layers[layer].len().max(1) as f32;
        }
    }

    /// Swaps neighbors in a layer while that crosses fewer links.
    fn transpose(&mut self) {
        let mut at = self.places();
        for _ in 0..TRANSPOSES {
            let mut improved = false;
            for layer in 0..self.layers.len() {
                for index in 0..self.layers[layer].len().saturating_sub(1) {
                    let (upper, lower) = (self.layers[layer][index], self.layers[layer][index + 1]);
                    if self.pair_crossings(&at, lower, upper)
                        < self.pair_crossings(&at, upper, lower)
                    {
                        self.layers[layer].swap(index, index + 1);
                        at[upper] = index + 1;
                        at[lower] = index;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
    }

    /// Crossings between the links of two neighbors of a layer with
    /// `upper` right above `lower`.
    fn pair_crossings(&self, at: &[usize], upper: usize, lower: usize) -> usize {
        let mut count = 0;
        for side in [&self.left, &self.right] {
            for &(a, _, a_port) in &side[upper] {
                for &(b, _, b_port) in &side[lower] {
                    if self.slot(at, a, a_port) > self.slot(at, b, b_port) {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    fn gap(&self, a: usize, b: usize) -> f32 {
        if a < self.tables && b < self.tables {
            CARD_GAP
        } else {
            WAYPOINT_GAP
        }
    }

    /// The top of every node: each layer stacked in its order, then moved
    /// in passes so each node sits level with the nodes it links to (and
    /// near its `preferred` top), never closer to its neighbors than a gap.
    fn align(&self, preferred: Option<&[f32]>) -> Vec<f32> {
        let mut tops = vec![0.0f32; self.heights.len()];
        for nodes in &self.layers {
            let mut top = 0.;
            for (index, &node) in nodes.iter().enumerate() {
                if index > 0 {
                    top += self.gap(nodes[index - 1], node);
                }
                tops[node] = top;
                top += self.heights[node];
            }
        }
        if let Some(preferred) = preferred {
            for (layer, nodes) in self.layers.iter().enumerate() {
                let wanted: Vec<(f32, f32)> = nodes
                    .iter()
                    .map(|&node| match preferred.get(node) {
                        Some(top) if node < self.tables => (*top, 1.),
                        _ => (tops[node], 0.25),
                    })
                    .collect();
                self.settle(layer, &wanted, &mut tops);
            }
        }
        let count = self.layers.len();
        for pass in 0..ALIGN_PASSES {
            // Level with the left, then the right, then both.
            let (left, right, rightward) = match pass % 3 {
                0 => (true, false, true),
                1 => (false, true, false),
                _ => (true, true, true),
            };
            let layers: Vec<usize> = if rightward {
                (0..count).collect()
            } else {
                (0..count).rev().collect()
            };
            for layer in layers {
                let wanted: Vec<(f32, f32)> = self.layers[layer]
                    .iter()
                    .map(|&node| {
                        let (mut sum, mut weight) = (0., 0.);
                        let sides = [(left, &self.left[node]), (right, &self.right[node])];
                        for (used, links) in sides {
                            if used {
                                for &(other, own, theirs) in links {
                                    sum += tops[other] + theirs - own;
                                    weight += 1.;
                                }
                            }
                        }
                        if let Some(top) = preferred.and_then(|p| p.get(node))
                            && node < self.tables
                        {
                            sum += top;
                            weight += 1.;
                        }
                        if weight == 0. {
                            (tops[node], 0.25)
                        } else {
                            (sum / weight, weight)
                        }
                    })
                    .collect();
                self.settle(layer, &wanted, &mut tops);
            }
        }
        tops
    }

    /// Sets the tops of a layer's nodes as close to the `wanted` ones
    /// (each with a weight) as they get keeping their order and gaps: the
    /// least squares fit, by pooling neighbors that would overlap.
    fn settle(&self, layer: usize, wanted: &[(f32, f32)], tops: &mut [f32]) {
        let nodes = &self.layers[layer];
        // Each node's distance from the first when packed tight.
        let mut offsets = Vec::with_capacity(nodes.len());
        let mut offset = 0.;
        for (index, &node) in nodes.iter().enumerate() {
            if index > 0 {
                let above = nodes[index - 1];
                offset += self.heights[above] + self.gap(above, node);
            }
            offsets.push(offset);
        }
        // Blocks of nodes packed tight: (weight, weighted sum of wanted
        // starts, nodes).
        let mut blocks: Vec<(f32, f32, usize)> = Vec::new();
        for (index, &(top, weight)) in wanted.iter().enumerate() {
            blocks.push((weight, weight * (top - offsets[index]), 1));
            while let [.., (w1, s1, n1), (w2, s2, n2)] = blocks[..] {
                if s1 / w1 <= s2 / w2 {
                    break;
                }
                blocks.pop();
                if let Some(last) = blocks.last_mut() {
                    *last = (w1 + w2, s1 + s2, n1 + n2);
                }
            }
        }
        let mut index = 0;
        for (weight, sum, nodes_in_block) in blocks {
            let start = sum / weight;
            for _ in 0..nodes_in_block {
                tops[nodes[index]] = start + offsets[index];
                index += 1;
            }
        }
    }
}

/// Which side of a card a wire leaves from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    /// -1 to the left, 1 to the right.
    pub fn sign(self) -> f32 {
        match self {
            Side::Left => -1.,
            Side::Right => 1.,
        }
    }
}

/// A cubic Bézier: start, two control points, end.
pub type Curve = [(f32, f32); 4];

/// A relation's line: from the referencing column (the "many" end) to the
/// referenced one (the "one" end), through the gaps of the columns of cards
/// it crosses.
#[derive(Debug, Clone, PartialEq)]
pub struct Wire {
    /// Index into the diagram's relations.
    pub relation: usize,
    pub from: (f32, f32),
    pub from_side: Side,
    pub to: (f32, f32),
    pub to_side: Side,
    /// Where it enters and leaves each column it passes, from `from`.
    pub via: Vec<(f32, f32)>,
}

impl Wire {
    /// The curves that draw it, each leaving and reaching its ends level,
    /// straight out of the cards.
    pub fn curves(&self) -> Vec<Curve> {
        if self.via.is_empty() {
            let reach = ((self.to.0 - self.from.0).abs() / 2.).clamp(40., 160.);
            return vec![[
                self.from,
                (self.from.0 + self.from_side.sign() * reach, self.from.1),
                (self.to.0 + self.to_side.sign() * reach, self.to.1),
                self.to,
            ]];
        }
        let mut points = vec![self.from];
        points.extend(&self.via);
        points.push(self.to);
        points
            .windows(2)
            .map(|pair| {
                let (a, b) = (pair[0], pair[1]);
                let reach = (b.0 - a.0) / 2.;
                [a, (a.0 + reach, a.1), (b.0 - reach, b.1), b]
            })
            .collect()
    }
}

/// The wire of every drawn relation.
pub fn wires(diagram: &Diagram, layout: &Layout, keys_only: bool) -> Vec<Wire> {
    diagram
        .drawn_relations()
        .filter_map(|(index, relation)| {
            let from_index = diagram.index_of(&relation.from)?;
            let to_index = diagram.index_of(&relation.to)?;
            let (from_card, to_card) = (layout.cards[from_index], layout.cards[to_index]);
            let from_y = from_card.y
                + diagram.tables[from_index].column_y(
                    relation.from_columns.first().map_or("", String::as_str),
                    keys_only,
                );
            let to_y = to_card.y
                + diagram.tables[to_index].column_y(
                    relation.to_columns.first().map_or("", String::as_str),
                    keys_only,
                );
            let (from_side, to_side) =
                if from_index == to_index || (from_card.x - to_card.x).abs() < 1. {
                    // A table referring to itself, or to one in the same layer:
                    // a loop out of their right sides.
                    (Side::Right, Side::Right)
                } else if from_card.x > to_card.x {
                    (Side::Left, Side::Right)
                } else {
                    (Side::Right, Side::Left)
                };
            let edge = |card: Card, side: Side| match side {
                Side::Left => card.x,
                Side::Right => card.x + card.width,
            };
            let from = (edge(from_card, from_side), from_y);
            let to = (edge(to_card, to_side), to_y);
            let via = if from_side == to_side {
                Vec::new()
            } else {
                route(layout, [from_index, to_index], from, to)
            };
            Some(Wire {
                relation: index,
                from,
                from_side,
                to,
                to_side,
                via,
            })
        })
        .collect()
}

/// Where a wire from `from` to `to` enters and leaves each column of cards
/// between them (other than its own cards): at the free height nearest the
/// straight line from where it is to its end.
fn route(layout: &Layout, ends: [usize; 2], from: (f32, f32), to: (f32, f32)) -> Vec<(f32, f32)> {
    let (low, high) = (from.0.min(to.0), from.0.max(to.0));
    let mut between: Vec<Card> = layout
        .cards
        .iter()
        .enumerate()
        .filter(|(index, card)| !ends.contains(index) && card.x < high && card.x + card.width > low)
        .map(|(_, card)| *card)
        .collect();
    between.sort_by(|a, b| a.x.total_cmp(&b.x));
    // Columns: cards that overlap across.
    let mut columns: Vec<(f32, f32, Vec<Card>)> = Vec::new();
    for card in between {
        match columns.last_mut() {
            Some((_, right, cards)) if card.x < *right => {
                *right = right.max(card.x + card.width);
                cards.push(card);
            }
            _ => columns.push((card.x, card.x + card.width, vec![card])),
        }
    }
    let rightward = to.0 > from.0;
    if !rightward {
        columns.reverse();
    }
    let mut via = Vec::new();
    let mut at = from;
    for (left, right, cards) in columns {
        let (left, right) = (left.max(low), right.min(high));
        let (enter, leave) = if rightward {
            (left, right)
        } else {
            (right, left)
        };
        let span = to.0 - at.0;
        let line = if span.abs() < 1. {
            to.1
        } else {
            at.1 + (enter - at.0) / span * (to.1 - at.1)
        };
        let y = free_height(&cards, line);
        via.push((enter, y));
        via.push((leave, y));
        at = (leave, y);
    }
    via
}

/// The height nearest `y` that runs clear of every card of a column.
fn free_height(cards: &[Card], y: f32) -> f32 {
    let mut blocked: Vec<(f32, f32)> = cards
        .iter()
        .map(|card| (card.y - CLEARANCE, card.y + card.height + CLEARANCE))
        .collect();
    blocked.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f32, f32)> = Vec::new();
    for (top, bottom) in blocked {
        match merged.last_mut() {
            Some(last) if top <= last.1 => last.1 = last.1.max(bottom),
            _ => merged.push((top, bottom)),
        }
    }
    for (top, bottom) in merged {
        if y > top && y < bottom {
            return if y - top <= bottom - y { top } else { bottom };
        }
    }
    y
}

/// A straight line from one point to another.
pub type Segment = ((f32, f32), (f32, f32));

/// Short line segments that mark a wire's ends, crow's foot notation: three
/// prongs at the "many" end (with a ring when the relation is optional, a
/// bar when not) and two bars at the "one" end. Each is ((x0, y0), (x1, y1)).
pub fn end_marks(wire: &Wire, optional: bool) -> (Vec<Segment>, Option<(f32, f32)>) {
    let mut lines = Vec::new();
    let (fx, fy) = wire.from;
    let out = wire.from_side.sign();
    // Crow's foot: prongs from 12 pt out to the card's edge.
    for spread in [-7., 0., 7.] {
        lines.push(((fx + out * 12., fy), (fx, fy + spread)));
    }
    let ring = if optional {
        Some((fx + out * 19., fy))
    } else {
        lines.push(((fx + out * 17., fy - 6.), (fx + out * 17., fy + 6.)));
        None
    };
    let (tx, ty) = wire.to;
    let back = wire.to_side.sign();
    for offset in [8., 13.] {
        lines.push(((tx + back * offset, ty - 6.), (tx + back * offset, ty + 6.)));
    }
    (lines, ring)
}

/// The diagram as an SVG image, dark like the app.
pub fn to_svg(diagram: &Diagram, layout: &Layout, keys_only: bool) -> String {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" \
         font-family=\"-apple-system, 'Segoe UI', sans-serif\">\n\
         <rect width=\"100%\" height=\"100%\" fill=\"#161618\"/>\n",
        w = layout.width.ceil(),
        h = layout.height.ceil()
    );
    for wire in wires(diagram, layout, keys_only) {
        let relation = &diagram.relations[wire.relation];
        let mut path = format!("M{} {}", wire.from.0, wire.from.1);
        for [_, c1, c2, end] in wire.curves() {
            path.push_str(&format!(
                " C{} {} {} {} {} {}",
                c1.0, c1.1, c2.0, c2.1, end.0, end.1
            ));
        }
        svg.push_str(&format!(
            "<path d=\"{path}\" fill=\"none\" stroke=\"#6e6e73\" stroke-width=\"1.4\"{}/>\n",
            if relation.optional {
                " stroke-dasharray=\"5 4\""
            } else {
                ""
            }
        ));
        let (lines, ring) = end_marks(&wire, relation.optional);
        for ((x0, y0), (x1, y1)) in lines {
            svg.push_str(&format!(
                "<line x1=\"{x0}\" y1=\"{y0}\" x2=\"{x1}\" y2=\"{y1}\" stroke=\"#aeaeb2\" stroke-width=\"1.4\"/>\n"
            ));
        }
        if let Some((x, y)) = ring {
            svg.push_str(&format!(
                "<circle cx=\"{x}\" cy=\"{y}\" r=\"3.5\" fill=\"#161618\" stroke=\"#aeaeb2\" stroke-width=\"1.4\"/>\n"
            ));
        }
    }
    for (table, card) in diagram.tables.iter().zip(&layout.cards) {
        svg.push_str(&format!(
            "<g transform=\"translate({} {})\">\n\
             <rect width=\"{}\" height=\"{}\" rx=\"8\" fill=\"#1d1d20\" stroke=\"#2e2e33\"/>\n\
             <text x=\"12\" y=\"22\" fill=\"#ececef\" font-size=\"13\" font-weight=\"600\">{}</text>\n",
            card.x,
            card.y,
            card.width,
            card.height,
            escape(&table.table.name)
        ));
        if let Some(rows) = table.rows {
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"22\" fill=\"#6e6e73\" font-size=\"11\" text-anchor=\"end\">{rows} rows</text>\n",
                card.width - 12.
            ));
        }
        svg.push_str(&format!(
            "<line x1=\"0\" y1=\"{HEADER}\" x2=\"{}\" y2=\"{HEADER}\" stroke=\"#2a2a2e\"/>\n",
            card.width
        ));
        for (row, column) in table.shown_columns(keys_only).iter().enumerate() {
            let y = HEADER + row as f32 * ROW + 15.;
            let (mark, tone) = if column.primary_key {
                ("PK ", "#ffb340")
            } else if column.foreign_key {
                ("FK ", "#4da3ff")
            } else {
                ("", "#aeaeb2")
            };
            svg.push_str(&format!(
                "<text x=\"12\" y=\"{y}\" font-size=\"12\" fill=\"{tone}\">{mark}<tspan fill=\"#ececef\">{}</tspan></text>\n\
                 <text x=\"{}\" y=\"{y}\" font-size=\"11\" fill=\"#8e8e93\" text-anchor=\"end\" \
                 font-family=\"ui-monospace, monospace\">{}{}</text>\n",
                escape(&column.name),
                card.width - 12.,
                escape(&{
                    let short = column.short_type();
                    if short.chars().count() > 16 {
                        format!("{}…", short.chars().take(15).collect::<String>())
                    } else {
                        short
                    }
                }),
                if column.nullable { "?" } else { "" }
            ));
        }
        svg.push_str("</g>\n");
    }
    svg.push_str("</svg>\n");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, primary_key: bool, foreign_key: bool, nullable: bool) -> Column {
        Column {
            name: name.into(),
            data_type: "bigint".into(),
            nullable,
            primary_key,
            foreign_key,
        }
    }

    fn table(name: &str, columns: Vec<Column>) -> Table {
        Table {
            table: TableRef::new("public", name),
            rows: Some(10),
            columns,
        }
    }

    fn relation(from: &str, column: &str, to: &str, optional: bool) -> Relation {
        Relation {
            name: format!("{from}_{column}_fkey"),
            from: TableRef::new("public", from),
            from_columns: vec![column.into()],
            to: TableRef::new("public", to),
            to_columns: vec!["id".into()],
            optional,
            on_delete: OnDelete::NoAction,
        }
    }

    /// customers ← orders ← order_items → products, a lone audit_log, and
    /// employees referring to themselves.
    fn shop() -> Diagram {
        Diagram {
            schema: "public".into(),
            tables: vec![
                table(
                    "order_items",
                    vec![
                        column("id", true, false, false),
                        column("order_id", false, true, false),
                        column("product_id", false, true, false),
                        column("note", false, false, true),
                    ],
                ),
                table("audit_log", vec![column("id", true, false, false)]),
                table(
                    "orders",
                    vec![
                        column("id", true, false, false),
                        column("customer_id", false, true, true),
                    ],
                ),
                table("products", vec![column("id", true, false, false)]),
                table("customers", vec![column("id", true, false, false)]),
                table(
                    "employees",
                    vec![
                        column("id", true, false, false),
                        column("manager_id", false, true, true),
                    ],
                ),
            ],
            relations: vec![
                relation("orders", "customer_id", "customers", true),
                relation("order_items", "order_id", "orders", false),
                relation("order_items", "product_id", "products", false),
                relation("employees", "manager_id", "employees", true),
                Relation {
                    to: TableRef::new("billing", "invoices"),
                    ..relation("orders", "customer_id", "invoices", false)
                },
            ],
        }
    }

    #[test]
    fn referenced_tables_go_left_of_the_tables_that_refer_to_them() {
        let diagram = shop();
        let layout = layout(&diagram, false);
        let x =
            |name: &str| layout.cards[diagram.index_of(&TableRef::new("public", name)).unwrap()].x;
        assert!(x("customers") < x("orders"));
        assert!(x("orders") < x("order_items"));
        assert!(x("products") < x("order_items"));
        // Tables without relations (and self-references only) are set apart, right.
        assert!(x("audit_log") > x("order_items"));
        assert!(x("employees") > x("order_items"));
        // No two cards overlap.
        for (i, a) in layout.cards.iter().enumerate() {
            for b in &layout.cards[i + 1..] {
                let apart = a.x + a.width <= b.x
                    || b.x + b.width <= a.x
                    || a.y + a.height <= b.y
                    || b.y + b.height <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
        assert!(layout.width > 0. && layout.height > 0.);
    }

    #[test]
    fn cycles_do_not_break_the_layout() {
        let mut diagram = shop();
        diagram
            .relations
            .push(relation("customers", "id", "order_items", false));
        let layout = layout(&diagram, false);
        assert_eq!(layout.cards.len(), diagram.tables.len());
        assert!(
            layout
                .cards
                .iter()
                .all(|c| c.x.is_finite() && c.y.is_finite())
        );
    }

    #[test]
    fn wires_run_from_the_referencing_column_to_the_key() {
        let diagram = shop();
        let layout = layout(&diagram, false);
        let wires = wires(&diagram, &layout, false);
        // The relation to billing.invoices has no card here.
        assert_eq!(wires.len(), 4);
        let items = diagram
            .index_of(&TableRef::new("public", "order_items"))
            .unwrap();
        let orders = diagram
            .index_of(&TableRef::new("public", "orders"))
            .unwrap();
        let wire = wires.iter().find(|w| w.relation == 1).unwrap();
        assert_eq!(wire.from_side, Side::Left);
        assert_eq!(wire.to_side, Side::Right);
        assert_eq!(wire.from.0, layout.cards[items].x);
        assert_eq!(wire.from.1, layout.cards[items].y + HEADER + ROW + ROW / 2.);
        assert_eq!(wire.to.0, layout.cards[orders].x + CARD_WIDTH);
        let own = wires.iter().find(|w| w.relation == 3).unwrap();
        assert_eq!((own.from_side, own.to_side), (Side::Right, Side::Right));
        let (lines, ring) = end_marks(wire, false);
        assert_eq!((lines.len(), ring), (6, None));
        let (_, ring) = end_marks(own, true);
        assert!(ring.is_some());
    }

    #[test]
    fn keys_only_cards_list_only_keys() {
        let diagram = shop();
        let items = &diagram.tables[0];
        assert_eq!(items.shown_columns(true).len(), 3);
        assert_eq!(items.card_height(true), HEADER + 3. * ROW + PADDING);
        assert_eq!(items.column_y("note", true), HEADER / 2.);
        assert_eq!(
            diagram
                .neighbors(
                    diagram
                        .index_of(&TableRef::new("public", "orders"))
                        .unwrap()
                )
                .len(),
            2
        );
    }

    #[test]
    fn shortens_long_types() {
        let typed = |data_type: &str| Column {
            data_type: data_type.into(),
            ..column("c", false, false, false)
        };
        assert_eq!(
            typed("timestamp with time zone").short_type(),
            "timestamptz"
        );
        assert_eq!(typed("character varying(80)").short_type(), "varchar(80)");
        assert_eq!(typed("character(3)").short_type(), "char(3)");
        assert_eq!(typed("bigint").short_type(), "int8");
        assert_eq!(typed("numeric(10,2)").short_type(), "numeric(10,2)");
        assert_eq!(typed("user_role[]").short_type(), "user_role[]");
    }

    #[test]
    fn narrows_to_a_tables_neighborhood() {
        let diagram = shop();
        let orders = diagram
            .index_of(&TableRef::new("public", "orders"))
            .unwrap();
        let near = diagram.around(orders, 1);
        let names: Vec<&str> = near.tables.iter().map(|t| t.table.name.as_str()).collect();
        assert_eq!(names, ["order_items", "orders", "customers"]);
        assert_eq!(near.relations.len(), 2);
        let wider = diagram.around(orders, 2);
        assert!(wider.tables.iter().any(|t| t.table.name == "products"));
        assert!(!wider.tables.iter().any(|t| t.table.name == "audit_log"));
    }

    #[test]
    fn moves_cards_and_grows_the_layout() {
        let diagram = shop();
        let mut layout = layout(&diagram, false);
        let height = layout.height;
        layout.place(1, 900., height + 400.);
        assert_eq!(
            (layout.cards[1].x, layout.cards[1].y),
            (900., height + 400.)
        );
        assert!(layout.height > height + 400.);
        layout.place(1, -50., -50.);
        assert!(layout.cards[1].x > 0. && layout.cards[1].y > 0.);
    }

    #[test]
    fn exports_mermaid_and_dbml() {
        let diagram = shop();
        let mermaid = diagram.to_mermaid();
        assert!(mermaid.starts_with("erDiagram\n"));
        assert!(
            mermaid.contains("    orders {\n        int8 id PK\n        int8 customer_id FK\n")
        );
        assert!(mermaid.contains("customers |o--o{ orders : \"customer_id\""));
        assert!(mermaid.contains("orders ||--o{ order_items : \"order_id\""));
        assert!(mermaid.contains("billing.invoices"));

        let dbml = diagram.to_dbml();
        assert!(dbml.contains("Table orders {\n  id bigint [pk]\n  customer_id bigint\n}"));
        assert!(dbml.contains("Ref order_items_order_id_fkey: order_items.order_id > orders.id\n"));
        assert!(dbml.contains("orders.customer_id > billing.invoices.id"));
        let mut cascade = diagram.clone();
        cascade.relations[0].on_delete = OnDelete::Cascade;
        assert!(cascade.to_dbml().contains("customers.id [delete: cascade]"));
    }

    /// Points along a wire, close enough to test what it passes through.
    fn sample(wire: &Wire) -> Vec<(f32, f32)> {
        let mut points = Vec::new();
        for [a, b, c, d] in wire.curves() {
            for step in 0..=24 {
                let t = step as f32 / 24.;
                let u = 1. - t;
                let mix = |p: [f32; 4]| {
                    u * u * u * p[0]
                        + 3. * u * u * t * p[1]
                        + 3. * u * t * t * p[2]
                        + t * t * t * p[3]
                };
                points.push((mix([a.0, b.0, c.0, d.0]), mix([a.1, b.1, c.1, d.1])));
            }
        }
        points
    }

    /// How many times two wires cross, leaving out where they meet at a
    /// card they share.
    fn crossings(diagram: &Diagram, layout: &Layout) -> usize {
        let wires = wires(diagram, layout, false);
        let cross = |p: (f32, f32), q: (f32, f32), r: (f32, f32), s: (f32, f32)| {
            let side = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
                (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
            };
            side(p, q, r) * side(p, q, s) <= 0. && side(r, s, p) * side(r, s, q) <= 0.
        };
        let mut count = 0;
        for (i, a) in wires.iter().enumerate() {
            for b in &wires[i + 1..] {
                let (pa, pb) = (sample(a), sample(b));
                let ends = [a.from, a.to, b.from, b.to];
                let near_end = |p: (f32, f32)| {
                    ends.iter()
                        .any(|e| (p.0 - e.0).abs() < 3. && (p.1 - e.1).abs() < 3.)
                };
                let crossed = pa.windows(2).any(|x| {
                    pb.windows(2).any(|y| {
                        ![x[0], x[1], y[0], y[1]].into_iter().any(near_end)
                            && cross(x[0], x[1], y[0], y[1])
                    })
                });
                count += usize::from(crossed);
            }
        }
        count
    }

    fn overlapping(layout: &Layout) -> Option<(usize, usize)> {
        for (i, a) in layout.cards.iter().enumerate() {
            for (j, b) in layout.cards.iter().enumerate().skip(i + 1) {
                if a.crowds(b, 0.) {
                    return Some((i, j));
                }
            }
        }
        None
    }

    fn at(diagram: &Diagram, layout: &Layout, name: &str) -> Card {
        layout.cards[diagram.index_of(&TableRef::new("public", name)).unwrap()]
    }

    #[test]
    fn orders_layers_so_lines_do_not_cross() {
        // By name, a and b sit above c and d, and c → b, d → a cross.
        let diagram = Diagram {
            schema: "public".into(),
            tables: vec![
                table("a", vec![column("id", true, false, false)]),
                table("b", vec![column("id", true, false, false)]),
                table(
                    "c",
                    vec![
                        column("id", true, false, false),
                        column("b_id", false, true, false),
                    ],
                ),
                table(
                    "d",
                    vec![
                        column("id", true, false, false),
                        column("a_id", false, true, false),
                    ],
                ),
            ],
            relations: vec![
                relation("c", "b_id", "b", false),
                relation("d", "a_id", "a", false),
            ],
        };
        let mut layout = layout(&diagram, false);
        assert_eq!(crossings(&diagram, &layout), 0);
        // And each table level with the one it refers to.
        let wires = wires(&diagram, &layout, false);
        assert!(
            wires.iter().all(|w| (w.from.1 - w.to.1).abs() < 1.),
            "{wires:?}"
        );
        // Swapped by hand, they cross.
        let (a, b) = (layout.cards[0], layout.cards[1]);
        layout.place(0, b.x, b.y);
        layout.place(1, a.x, a.y);
        assert_eq!(crossings(&diagram, &layout), 1);
    }

    /// A schema of `count` tables where each refers to one to three
    /// earlier ones, picked by a fixed pseudo-random sequence.
    fn tangled(count: usize) -> Diagram {
        let mut seed = 7u64;
        let mut next = |below: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % below.max(1)
        };
        let mut tables = Vec::new();
        let mut relations = Vec::new();
        for index in 0..count {
            let name = format!("t{index:02}");
            let mut columns = vec![column("id", true, false, false)];
            if index > 0 {
                for _ in 0..1 + next(3) {
                    let parent = next(index);
                    let key = format!("t{parent:02}_id");
                    if columns.iter().any(|c| c.name == key) {
                        continue;
                    }
                    columns.push(column(&key, false, true, false));
                    relations.push(relation(&name, &key, &format!("t{parent:02}"), false));
                }
            }
            columns.push(column("note", false, false, true));
            tables.push(table(&name, columns));
        }
        Diagram {
            schema: "public".into(),
            tables,
            relations,
        }
    }

    #[test]
    fn lays_out_a_tangled_schema_cleanly() {
        let diagram = tangled(40);
        let layout = layout(&diagram, false);
        assert_eq!(overlapping(&layout), None);
        assert!(
            layout
                .cards
                .iter()
                .all(|c| c.x.is_finite() && c.y.is_finite())
        );
        for wire in wires(&diagram, &layout, false) {
            let relation = &diagram.relations[wire.relation];
            let ends = [
                diagram.index_of(&relation.from).unwrap(),
                diagram.index_of(&relation.to).unwrap(),
            ];
            for (index, card) in layout.cards.iter().enumerate() {
                if ends.contains(&index) {
                    continue;
                }
                let through = sample(&wire).iter().any(|(x, y)| {
                    *x > card.x + 1.
                        && *x < card.x + card.width - 1.
                        && *y > card.y + 1.
                        && *y < card.y + card.height - 1.
                });
                assert!(!through, "{} runs through card {index}", relation.name);
            }
        }
        let untangled = untangle(&diagram, &layout, false);
        assert_eq!(overlapping(&untangled), None);
    }

    #[test]
    fn orders_by_the_row_a_line_leaves_from() {
        // line's z_id is above its a_id: z goes above a.
        let diagram = Diagram {
            schema: "public".into(),
            tables: vec![
                table("a", vec![column("id", true, false, false)]),
                table("z", vec![column("id", true, false, false)]),
                table(
                    "line",
                    vec![
                        column("id", true, false, false),
                        column("z_id", false, true, false),
                        column("a_id", false, true, false),
                    ],
                ),
            ],
            relations: vec![
                relation("line", "z_id", "z", false),
                relation("line", "a_id", "a", false),
            ],
        };
        let layout = layout(&diagram, false);
        assert!(at(&diagram, &layout, "z").y < at(&diagram, &layout, "a").y);
        assert_eq!(crossings(&diagram, &layout), 0);
    }

    #[test]
    fn long_relations_pass_between_cards() {
        let diagram = shop();
        let layout = layout(&diagram, false);
        let wires = wires(&diagram, &layout, false);
        // order_items → products skips the orders column.
        let long = wires.iter().find(|w| w.relation == 2).unwrap();
        assert_eq!(long.via.len(), 2);
        for wire in &wires {
            let relation = &diagram.relations[wire.relation];
            let ends = [
                diagram.index_of(&relation.from).unwrap(),
                diagram.index_of(&relation.to).unwrap(),
            ];
            for (index, card) in layout.cards.iter().enumerate() {
                if ends.contains(&index) {
                    continue;
                }
                for (x, y) in sample(wire) {
                    let inside = x > card.x
                        && x < card.x + card.width
                        && y > card.y
                        && y < card.y + card.height;
                    assert!(!inside, "{} runs through card {index}", relation.name);
                }
            }
        }
    }

    #[test]
    fn routes_around_cards_moved_into_the_way() {
        let diagram = shop();
        let mut layout = layout(&diagram, false);
        let items = at(&diagram, &layout, "order_items");
        let products = diagram
            .index_of(&TableRef::new("public", "products"))
            .unwrap();
        let audit = diagram
            .index_of(&TableRef::new("public", "audit_log"))
            .unwrap();
        // products far left, level with order_items, and audit_log between.
        layout.place(products, 20., items.y + 40.);
        layout.place(audit, (20. + items.x) / 2., items.y + 20.);
        let wire = wires(&diagram, &layout, false)
            .into_iter()
            .find(|w| w.relation == 2)
            .unwrap();
        let card = layout.cards[audit];
        assert!(!sample(&wire).iter().any(|(x, y)| {
            *x > card.x && *x < card.x + card.width && *y > card.y && *y < card.y + card.height
        }));
    }

    #[test]
    fn untangles_a_layout_moved_by_hand() {
        let diagram = shop();
        let mut layout = layout(&diagram, false);
        let orders = at(&diagram, &layout, "orders");
        // customers dropped onto orders, products below everything.
        let customers = diagram
            .index_of(&TableRef::new("public", "customers"))
            .unwrap();
        let products = diagram
            .index_of(&TableRef::new("public", "products"))
            .unwrap();
        layout.place(customers, orders.x + 30., orders.y + 10.);
        layout.place(products, 40., layout.height + 300.);
        assert!(overlapping(&layout).is_some());
        let untangled = untangle(&diagram, &layout, false);
        assert_eq!(overlapping(&untangled), None);
        assert_eq!(crossings(&diagram, &untangled), 0);
        // Columns stay: products left of order_items, customers with orders.
        assert!(at(&diagram, &untangled, "products").x < at(&diagram, &untangled, "order_items").x);
        assert_eq!(
            at(&diagram, &untangled, "customers").x,
            at(&diagram, &untangled, "orders").x
        );
    }

    #[test]
    fn a_dropped_card_pushes_the_cards_under_it() {
        let diagram = shop();
        let mut layout = layout(&diagram, false);
        let orders = diagram
            .index_of(&TableRef::new("public", "orders"))
            .unwrap();
        let target = layout.cards[orders];
        layout.place(1, target.x + 10., target.y + 5.);
        let dropped = layout.cards[1];
        let pushed = layout.push_apart(1);
        assert!(pushed.contains(&orders));
        assert_eq!(layout.cards[1], dropped);
        assert_eq!(overlapping(&layout), None);
        assert!(
            layout
                .cards
                .iter()
                .all(|c| c.x >= MARGIN / 2. && c.y >= MARGIN / 2.)
        );
    }

    #[test]
    fn snaps_to_the_grid() {
        assert_eq!(snap(29.), 20.);
        assert_eq!(snap(31.), 40.);
        assert_eq!(snap(-4.), 0.);
    }

    #[test]
    fn exports_svg() {
        let diagram = shop();
        let layout = layout(&diagram, false);
        let svg = to_svg(&diagram, &layout, false);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains(">order_items<"));
        assert!(svg.contains("stroke-dasharray"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert_eq!(OnDelete::from_code("c"), OnDelete::Cascade);
    }
}
