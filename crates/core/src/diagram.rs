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
//! ([`Layout::place`]).

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
const MARGIN: f32 = 40.;
/// Barycenter sweeps when ordering each layer.
const SWEEPS: usize = 6;

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

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// One per table, in the diagram's order.
    pub cards: Vec<Card>,
    pub width: f32,
    pub height: f32,
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
    order_layers(&mut columns, &parents, &children);

    // Tables without relations, in columns as tall as the tallest layer
    // (or of a few cards when nothing is related).
    let alone: Vec<usize> = by_name.iter().copied().filter(|t| !related[*t]).collect();
    let height_of = |table: usize| diagram.tables[table].card_height(keys_only);
    let stack = |tables: &[usize]| -> f32 {
        tables.iter().map(|t| height_of(*t)).sum::<f32>()
            + CARD_GAP * tables.len().saturating_sub(1) as f32
    };
    let tallest = columns.iter().map(|c| stack(c)).fold(0.0, f32::max);
    let limit = if tallest > 0.0 {
        tallest
    } else {
        // About a square block.
        let side = (alone.len() as f32).sqrt().ceil().max(1.0) as usize;
        alone.iter().take(side).map(|t| height_of(*t)).sum::<f32>() + CARD_GAP * side as f32
    };
    let mut current: Vec<usize> = Vec::new();
    for &table in &alone {
        let next = stack(&current) + CARD_GAP + height_of(table);
        if !current.is_empty() && next > limit {
            columns.push(std::mem::take(&mut current));
        }
        current.push(table);
    }
    if !current.is_empty() {
        columns.push(current);
    }

    let tallest = columns.iter().map(|c| stack(c)).fold(0.0, f32::max);
    let mut cards = vec![
        Card {
            x: 0.,
            y: 0.,
            width: CARD_WIDTH,
            height: 0.,
        };
        count
    ];
    for (index, column) in columns.iter().enumerate() {
        let x = MARGIN + index as f32 * (CARD_WIDTH + LAYER_GAP);
        let mut y = MARGIN + (tallest - stack(column)) / 2.;
        for &table in column {
            let height = height_of(table);
            cards[table] = Card {
                x,
                y,
                width: CARD_WIDTH,
                height,
            };
            y += height + CARD_GAP;
        }
    }
    Layout {
        cards,
        width: MARGIN * 2.
            + columns.len() as f32 * CARD_WIDTH
            + columns.len().saturating_sub(1) as f32 * LAYER_GAP,
        height: MARGIN * 2. + tallest,
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

/// Orders each layer by the average position of its neighbors in the
/// layers already ordered, sweeping right and then left, so lines cross
/// less.
fn order_layers(columns: &mut [Vec<usize>], parents: &[Vec<usize>], children: &[Vec<usize>]) {
    let count = parents.len();
    let mut position = vec![0.0f32; count];
    let mut layer_of = vec![usize::MAX; count];
    let place = |columns: &[Vec<usize>], position: &mut [f32], layer_of: &mut [usize]| {
        for (layer, column) in columns.iter().enumerate() {
            let size = column.len().max(1) as f32;
            for (index, &table) in column.iter().enumerate() {
                position[table] = (index as f32 + 0.5) / size;
                layer_of[table] = layer;
            }
        }
    };
    place(columns, &mut position, &mut layer_of);
    for sweep in 0..SWEEPS {
        let rightward = sweep % 2 == 0;
        let layers: Vec<usize> = if rightward {
            (1..columns.len()).collect()
        } else {
            (0..columns.len().saturating_sub(1)).rev().collect()
        };
        for layer in layers {
            let key = |table: usize| {
                let neighbors: Vec<f32> = parents[table]
                    .iter()
                    .chain(&children[table])
                    .filter(|n| {
                        let other = layer_of[**n];
                        if rightward {
                            other < layer
                        } else {
                            other > layer && other != usize::MAX
                        }
                    })
                    .map(|n| position[*n])
                    .collect();
                if neighbors.is_empty() {
                    position[table]
                } else {
                    neighbors.iter().sum::<f32>() / neighbors.len() as f32
                }
            };
            let mut keyed: Vec<(f32, usize)> =
                columns[layer].iter().map(|t| (key(*t), *t)).collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
            columns[layer] = keyed.into_iter().map(|(_, t)| t).collect();
            let size = columns[layer].len().max(1) as f32;
            for (index, &table) in columns[layer].iter().enumerate() {
                position[table] = (index as f32 + 0.5) / size;
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

/// A relation's line: from the referencing column (the "many" end) to the
/// referenced one (the "one" end).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wire {
    /// Index into the diagram's relations.
    pub relation: usize,
    pub from: (f32, f32),
    pub from_side: Side,
    pub to: (f32, f32),
    pub to_side: Side,
}

impl Wire {
    /// The cubic Bézier control points for the line, leaving each end
    /// straight out of its card.
    pub fn controls(&self) -> ((f32, f32), (f32, f32)) {
        let reach = ((self.to.0 - self.from.0).abs() / 2.).clamp(40., 160.);
        (
            (self.from.0 + self.from_side.sign() * reach, self.from.1),
            (self.to.0 + self.to_side.sign() * reach, self.to.1),
        )
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
            Some(Wire {
                relation: index,
                from: (edge(from_card, from_side), from_y),
                from_side,
                to: (edge(to_card, to_side), to_y),
                to_side,
            })
        })
        .collect()
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
        let (c1, c2) = wire.controls();
        svg.push_str(&format!(
            "<path d=\"M{} {} C{} {} {} {} {} {}\" fill=\"none\" stroke=\"#6e6e73\" stroke-width=\"1.4\"{}/>\n",
            wire.from.0,
            wire.from.1,
            c1.0,
            c1.1,
            c2.0,
            c2.1,
            wire.to.0,
            wire.to.1,
            if relation.optional { " stroke-dasharray=\"5 4\"" } else { "" }
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
