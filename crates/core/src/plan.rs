//! Query plans, made readable: PostgreSQL's `EXPLAIN (FORMAT JSON)` output
//! as a tree of steps with plain titles ("Read all of orders" rather than
//! "Seq Scan"), the time or cost each step takes by itself, and insights
//! about what looks wrong (a full scan that throws most rows away, a badly
//! wrong row estimate, a sort that spilled to disk…).
//!
//! The tree is built from the JSON alone, so it is testable without a
//! database. [`explain_sql`] writes the `EXPLAIN` to send.

use serde_json::Value;

use crate::script;

/// PostgreSQL's block size, for turning buffer counts into bytes.
pub const BLOCK_BYTES: u64 = 8 * 1024;

/// The `EXPLAIN` that returns `statement`'s plan as JSON: estimated, or
/// with `analyze` measured by running it (with buffers). `statement` must
/// be a single statement, not an `EXPLAIN` already.
pub fn explain_sql(statement: &str, analyze: bool) -> Result<String, String> {
    let statement = statement.trim().trim_end_matches(';').trim();
    if statement.is_empty() {
        return Err("There is no statement to explain".into());
    }
    if let Some(ranges) = script::statements(statement)
        && ranges.len() > 1
    {
        return Err("Explain one statement at a time: put the cursor on it".into());
    }
    if is_explain(statement) {
        return Err("This is an EXPLAIN already: run it to see its plan".into());
    }
    let options = if analyze {
        "ANALYZE, BUFFERS, VERBOSE, FORMAT JSON"
    } else {
        "VERBOSE, FORMAT JSON"
    };
    Ok(format!("EXPLAIN ({options}) {statement}"))
}

/// Whether `sql` starts with the `EXPLAIN` keyword (after comments).
pub fn is_explain(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.get(..7)
        .is_some_and(|word| word.eq_ignore_ascii_case("explain"))
        && rest[7..].starts_with(|c: char| c.is_whitespace() || c == '(')
}

/// What a step does, for its icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Scan,
    Join,
    Aggregate,
    Sort,
    Modify,
    Other,
}

/// Measured numbers of a step (EXPLAIN ANALYZE). PostgreSQL reports rows
/// and times per loop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Actual {
    pub rows_per_loop: f64,
    pub loops: f64,
    pub startup_ms: f64,
    pub total_ms_per_loop: f64,
}

/// Blocks of 8 kB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Buffers {
    /// Found in PostgreSQL's cache.
    pub hit: u64,
    /// Read from the operating system (disk or its cache).
    pub read: u64,
    pub dirtied: u64,
    pub written: u64,
    /// Temporary files, for sorts and hashes that did not fit in memory.
    pub temp_read: u64,
    pub temp_written: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Position in the tree, depth first from 0.
    pub id: usize,
    /// PostgreSQL's name for it: `Seq Scan`, `Hash Join`…
    pub node_type: String,
    /// What it does, in plain words.
    pub title: String,
    pub category: Category,
    /// `schema.table`, when it reads or changes one.
    pub relation: Option<String>,
    pub index: Option<String>,
    /// How the parent uses it: `Outer`, `Inner`, `InitPlan`, `SubPlan`…
    pub parent_relationship: Option<String>,
    pub estimated_rows: f64,
    pub startup_cost: f64,
    pub total_cost: f64,
    pub actual: Option<Actual>,
    /// Time of this step and everything under it, all loops.
    pub inclusive_ms: Option<f64>,
    /// Time of this step alone.
    pub self_ms: Option<f64>,
    /// Cost of this step alone.
    pub self_cost: f64,
    /// Rows read and then discarded by its filters.
    pub rows_removed: Option<f64>,
    pub buffers: Buffers,
    /// Labelled facts to show when expanded: conditions, sort keys…
    pub details: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Node {
    /// Rows it produced over all loops.
    pub fn actual_rows(&self) -> Option<f64> {
        self.actual.map(|a| a.rows_per_loop * a.loops)
    }

    /// Actual rows divided by estimated rows (both per loop); above 1 means
    /// it was underestimated.
    pub fn misestimate(&self) -> Option<f64> {
        let actual = self.actual?;
        let estimated = self.estimated_rows.max(1.0);
        Some(actual.rows_per_loop.max(1.0) / estimated)
    }

    /// This node and every node under it, depth first.
    pub fn walk(&self) -> Vec<&Node> {
        let mut nodes = vec![self];
        for child in &self.children {
            nodes.extend(child.walk());
        }
        nodes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Info,
}

/// Something worth knowing about a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Insight {
    pub severity: Severity,
    /// The step it is about.
    pub node: Option<usize>,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub root: Node,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    /// Whether times and rows were measured (EXPLAIN ANALYZE).
    pub analyzed: bool,
    pub insights: Vec<Insight>,
}

impl Plan {
    /// The part of the whole that a step takes by itself, 0 to 1: of the
    /// time when measured, of the cost otherwise.
    pub fn share(&self, node: &Node) -> f64 {
        let (part, whole) = match (node.self_ms, self.root.inclusive_ms) {
            (Some(part), Some(whole)) => (part, whole),
            _ => (node.self_cost, self.root.total_cost),
        };
        if whole > 0.0 {
            (part / whole).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Buffers of the whole plan (the root counts everything under it).
    pub fn buffers(&self) -> Buffers {
        self.root.buffers
    }

    /// The step that takes the largest share by itself.
    pub fn hottest(&self) -> &Node {
        self.root
            .walk()
            .into_iter()
            .max_by(|a, b| self.share(a).total_cmp(&self.share(b)))
            .unwrap_or(&self.root)
    }
}

/// Reads PostgreSQL's `EXPLAIN (FORMAT JSON)` output: `[{"Plan": …}]`.
pub fn parse(json: &str) -> Result<Plan, String> {
    let value: Value =
        serde_json::from_str(json.trim()).map_err(|error| format!("Not a JSON plan: {error}"))?;
    let top = match &value {
        Value::Array(items) => items.first(),
        Value::Object(_) => Some(&value),
        _ => None,
    }
    .ok_or("The plan is empty")?;
    let plan = top.get("Plan").ok_or("The JSON has no \"Plan\"")?;
    let mut next = 0;
    let root = node(plan, &mut next);
    let analyzed = root.actual.is_some();
    let mut plan = Plan {
        root,
        planning_ms: number(top, "Planning Time"),
        execution_ms: number(top, "Execution Time"),
        analyzed,
        insights: Vec::new(),
    };
    plan.insights = insights(&plan);
    Ok(plan)
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key)?.as_f64()
}

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map_or_else(|| item.to_string(), str::to_string)
                })
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn blocks(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn node(value: &Value, next: &mut usize) -> Node {
    let id = *next;
    *next += 1;
    let node_type = text(value, "Node Type").unwrap_or_else(|| "Step".into());
    let relation = text(value, "Relation Name").map(|name| match text(value, "Schema") {
        Some(schema) if schema != "public" => format!("{schema}.{name}"),
        _ => name,
    });
    let index = text(value, "Index Name");
    let actual = match (
        number(value, "Actual Rows"),
        number(value, "Actual Loops"),
        number(value, "Actual Total Time"),
    ) {
        (Some(rows), Some(loops), Some(total)) => Some(Actual {
            rows_per_loop: rows,
            loops,
            startup_ms: number(value, "Actual Startup Time").unwrap_or(0.0),
            total_ms_per_loop: total,
        }),
        // ANALYZE with TIMING OFF: rows without times.
        (Some(rows), Some(loops), None) => Some(Actual {
            rows_per_loop: rows,
            loops,
            startup_ms: 0.0,
            total_ms_per_loop: 0.0,
        }),
        _ => None,
    };
    let children: Vec<Node> = value
        .get("Plans")
        .and_then(Value::as_array)
        .map(|plans| plans.iter().map(|plan| node(plan, next)).collect())
        .unwrap_or_default();

    let total_cost = number(value, "Total Cost").unwrap_or(0.0);
    let children_cost: f64 = children.iter().map(|c| c.total_cost).sum();
    let inclusive_ms = actual
        .filter(|a| a.total_ms_per_loop > 0.0 || a.loops == 0.0)
        .map(|a| a.total_ms_per_loop * a.loops);
    let self_ms = inclusive_ms.map(|inclusive| {
        let children_ms: f64 = children.iter().filter_map(|c| c.inclusive_ms).sum();
        (inclusive - children_ms).max(0.0)
    });
    let removed: Vec<f64> = [
        "Rows Removed by Filter",
        "Rows Removed by Join Filter",
        "Rows Removed by Index Recheck",
    ]
    .iter()
    .filter_map(|key| number(value, key))
    .collect();
    let loops = actual.map_or(1.0, |a| a.loops.max(1.0));
    let rows_removed = (!removed.is_empty()).then(|| removed.iter().sum::<f64>() * loops);

    let mut details = Vec::new();
    let mut detail = |label: &str, key: &str| {
        if let Some(value) = text(value, key).filter(|v| !v.is_empty()) {
            details.push((label.to_string(), value));
        }
    };
    detail("Index condition", "Index Cond");
    detail("Recheck condition", "Recheck Cond");
    detail("Filter", "Filter");
    detail("Join condition", "Hash Cond");
    detail("Join condition", "Merge Cond");
    detail("Join filter", "Join Filter");
    detail("Sort key", "Sort Key");
    detail("Group key", "Group Key");
    detail("Sort method", "Sort Method");
    detail("Strategy", "Strategy");
    detail("Join type", "Join Type");
    detail("Workers", "Workers Launched");
    detail("Heap fetches", "Heap Fetches");
    detail("Hash batches", "Hash Batches");
    detail("Cache hits", "Cache Hits");
    detail("Cache misses", "Cache Misses");
    detail("Output", "Output");
    if let (Some(space), Some(kind)) = (
        number(value, "Sort Space Used"),
        text(value, "Sort Space Type"),
    ) {
        details.push((
            "Sort space".into(),
            format!("{space} kB ({})", kind.to_lowercase()),
        ));
    }
    if let Some(memory) = number(value, "Peak Memory Usage") {
        details.push(("Hash memory".into(), format!("{memory} kB")));
    }

    let (title, category) = describe(value, &node_type, relation.as_deref(), index.as_deref());
    Node {
        id,
        title,
        category,
        relation,
        index,
        parent_relationship: text(value, "Parent Relationship"),
        estimated_rows: number(value, "Plan Rows").unwrap_or(0.0),
        startup_cost: number(value, "Startup Cost").unwrap_or(0.0),
        total_cost,
        actual,
        inclusive_ms,
        self_ms,
        self_cost: (total_cost - children_cost).max(0.0),
        rows_removed,
        buffers: Buffers {
            hit: blocks(value, "Shared Hit Blocks"),
            read: blocks(value, "Shared Read Blocks"),
            dirtied: blocks(value, "Shared Dirtied Blocks"),
            written: blocks(value, "Shared Written Blocks"),
            temp_read: blocks(value, "Temp Read Blocks"),
            temp_written: blocks(value, "Temp Written Blocks"),
        },
        details,
        node_type,
        children,
    }
}

/// A step's title in plain words, and its category.
fn describe(
    value: &Value,
    node_type: &str,
    relation: Option<&str>,
    index: Option<&str>,
) -> (String, Category) {
    let table = relation.unwrap_or("the table");
    let index = index.unwrap_or("an index");
    let join = || {
        text(value, "Join Type")
            .map(|kind| match kind.as_str() {
                "Inner" => "Join".to_string(),
                "Semi" => "Keep rows that have a match".to_string(),
                "Anti" => "Keep rows that have no match".to_string(),
                other => format!("{other} join"),
            })
            .unwrap_or_else(|| "Join".into())
    };
    match node_type {
        "Seq Scan" => (format!("Read all of {table}"), Category::Scan),
        "Parallel Seq Scan" => (format!("Read all of {table}, in parallel"), Category::Scan),
        "Index Scan" => (format!("Look up {table} through {index}"), Category::Scan),
        "Index Only Scan" => (
            format!("Read {index} without visiting {table}"),
            Category::Scan,
        ),
        "Bitmap Index Scan" => (format!("Find matching rows in {index}"), Category::Scan),
        "Bitmap Heap Scan" => (
            format!("Fetch the matching rows of {table}"),
            Category::Scan,
        ),
        "Tid Scan" | "Tid Range Scan" => {
            (format!("Fetch rows of {table} by position"), Category::Scan)
        }
        "CTE Scan" => (
            format!(
                "Read the CTE {}",
                text(value, "CTE Name").unwrap_or_default()
            ),
            Category::Scan,
        ),
        "Subquery Scan" => ("Read a subquery's rows".into(), Category::Scan),
        "Function Scan" => (
            format!(
                "Call {}",
                text(value, "Function Name").unwrap_or_else(|| "a function".into())
            ),
            Category::Scan,
        ),
        "Values Scan" => ("Read a VALUES list".into(), Category::Scan),
        "Nested Loop" => (
            format!("{}, row by row (nested loop)", join()),
            Category::Join,
        ),
        "Hash Join" => (format!("{} through a hash table", join()), Category::Join),
        "Merge Join" => (format!("{} of two sorted inputs", join()), Category::Join),
        "Hash" => ("Build a hash table".into(), Category::Join),
        "Sort" => (
            format!(
                "Sort by {}",
                text(value, "Sort Key").unwrap_or_else(|| "a key".into())
            ),
            Category::Sort,
        ),
        "Incremental Sort" => ("Sort, reusing an existing order".into(), Category::Sort),
        "Aggregate" => match text(value, "Strategy").as_deref() {
            Some("Hashed") => ("Group rows with a hash table".into(), Category::Aggregate),
            Some("Sorted") => ("Group sorted rows".into(), Category::Aggregate),
            Some("Mixed") => ("Group rows (grouping sets)".into(), Category::Aggregate),
            _ => ("Aggregate all rows".into(), Category::Aggregate),
        },
        "Group" => ("Group sorted rows".into(), Category::Aggregate),
        "WindowAgg" => ("Compute window functions".into(), Category::Aggregate),
        "Unique" => ("Remove duplicate rows".into(), Category::Aggregate),
        "SetOp" => (
            "Combine with INTERSECT or EXCEPT".into(),
            Category::Aggregate,
        ),
        "Limit" => ("Keep only the first rows".into(), Category::Other),
        "Gather" | "Gather Merge" => (
            format!(
                "Combine the work of {} parallel workers",
                text(value, "Workers Launched")
                    .or_else(|| text(value, "Workers Planned"))
                    .unwrap_or_else(|| "the".into())
            ),
            Category::Other,
        ),
        "Materialize" => (
            "Keep rows in memory to read them again".into(),
            Category::Other,
        ),
        "Memoize" => ("Cache the inner lookups".into(), Category::Other),
        "Append" | "Merge Append" => ("Put several results together".into(), Category::Other),
        "Result" => ("Compute a result".into(), Category::Other),
        "LockRows" => ("Lock the rows (FOR UPDATE)".into(), Category::Other),
        "ModifyTable" => (
            match text(value, "Operation").as_deref() {
                Some("Insert") => format!("Insert into {table}"),
                Some("Update") => format!("Update {table}"),
                Some("Delete") => format!("Delete from {table}"),
                Some("Merge") => format!("Merge into {table}"),
                _ => format!("Change {table}"),
            },
            Category::Modify,
        ),
        other => (other.to_string(), Category::Other),
    }
}

/// How far off an estimate has to be, either way, to be worth a word.
const MISESTIMATE: f64 = 10.0;
/// Rows a step must handle before its estimate matters.
const ESTIMATE_ROWS: f64 = 1_000.0;
/// A filter must discard this many rows per row kept, and this many rows in
/// all, to suggest an index.
const DISCARD_RATIO: f64 = 10.0;
const DISCARD_ROWS: f64 = 10_000.0;
/// Inner loops above this are worth a word.
const MANY_LOOPS: f64 = 1_000.0;
/// A step taking more than this share of the whole is pointed out.
const HOT_SHARE: f64 = 0.4;

fn insights(plan: &Plan) -> Vec<Insight> {
    let mut found = Vec::new();
    for node in plan.root.walk() {
        let table = node.relation.as_deref().unwrap_or("the table");
        if let (Some(removed), Some(kept)) = (node.rows_removed, node.actual_rows())
            && node.category == Category::Scan
            && removed >= DISCARD_ROWS
            && removed >= kept.max(1.0) * DISCARD_RATIO
        {
            let filter = node
                .details
                .iter()
                .find(|(label, _)| label == "Filter")
                .map(|(_, value)| value.clone());
            found.push(Insight {
                severity: Severity::Warning,
                node: Some(node.id),
                title: format!(
                    "Reads {} rows of {table} to keep {}",
                    count(removed + kept),
                    count(kept)
                ),
                detail: match filter {
                    Some(filter) => format!(
                        "The filter {filter} throws away {:.1}% of what it reads. An index on \
                         the filtered columns would let it find the rows directly.",
                        removed / (removed + kept) * 100.0
                    ),
                    None => "Most rows read are thrown away; an index could find the rest \
                             directly."
                        .into(),
                },
            });
        }
        if let (Some(ratio), Some(actual)) = (node.misestimate(), node.actual)
            && (ratio >= MISESTIMATE || ratio <= 1.0 / MISESTIMATE)
            && (actual.rows_per_loop * actual.loops).max(node.estimated_rows) >= ESTIMATE_ROWS
        {
            let direction = if ratio >= 1.0 { "more" } else { "fewer" };
            let factor = if ratio >= 1.0 { ratio } else { 1.0 / ratio };
            found.push(Insight {
                severity: Severity::Warning,
                node: Some(node.id),
                title: format!(
                    "Expected {} rows, got {} ({factor:.0}× {direction})",
                    count(node.estimated_rows),
                    count(actual.rows_per_loop)
                ),
                detail: format!(
                    "A wrong estimate leads PostgreSQL to the wrong join or scan. Fresh \
                     statistics often fix it{}.",
                    node.relation
                        .as_ref()
                        .map(|r| format!(": ANALYZE {r}"))
                        .unwrap_or_default()
                ),
            });
        }
        let sort_on_disk = node
            .details
            .iter()
            .any(|(label, value)| label == "Sort space" && value.contains("disk"))
            || node
                .details
                .iter()
                .any(|(label, value)| label == "Sort method" && value.contains("external"));
        if sort_on_disk {
            found.push(Insight {
                severity: Severity::Warning,
                node: Some(node.id),
                title: "The sort did not fit in memory and used disk".into(),
                detail: "Raising work_mem (for this session: SET work_mem = '64MB') keeps it \
                         in memory; an index in the sort order avoids sorting."
                    .into(),
            });
        }
        if let Some((_, batches)) = node
            .details
            .iter()
            .find(|(label, _)| label == "Hash batches")
            && batches.parse::<u64>().is_ok_and(|b| b > 1)
        {
            found.push(Insight {
                severity: Severity::Warning,
                node: Some(node.id),
                title: format!("The hash table was split in {batches} batches on disk"),
                detail: "It did not fit in work_mem. Raising work_mem keeps it in one batch."
                    .into(),
            });
        }
        if node.node_type == "Nested Loop"
            && let Some(inner) = node.children.get(1)
            && let Some(actual) = inner.actual
            && actual.loops >= MANY_LOOPS
        {
            found.push(Insight {
                severity: Severity::Info,
                node: Some(inner.id),
                title: format!(
                    "\u{201c}{}\u{201d} runs {} times, once per outer row",
                    inner.title,
                    count(actual.loops)
                ),
                detail: "Fine when each run is quick (an index lookup); if not, a hash join \
                         may do better, often after fixing the outer estimate."
                    .into(),
            });
        }
    }
    if plan.root.buffers.temp_written > 0 && !found.iter().any(|i| i.title.contains("disk")) {
        found.push(Insight {
            severity: Severity::Warning,
            node: None,
            title: format!(
                "Wrote {} of temporary files",
                bytes(plan.root.buffers.temp_written * BLOCK_BYTES)
            ),
            detail: "Some step did not fit in work_mem.".into(),
        });
    }
    let hottest = plan.hottest();
    let share = plan.share(hottest);
    if share >= HOT_SHARE && (hottest.id != plan.root.id || plan.root.children.is_empty()) {
        found.push(Insight {
            severity: Severity::Info,
            node: Some(hottest.id),
            title: format!(
                "{:.0}% of the {} goes to \u{201c}{}\u{201d}",
                share * 100.0,
                if plan.analyzed {
                    "time"
                } else {
                    "estimated cost"
                },
                hottest.title
            ),
            detail: "This is where to look first.".into(),
        });
    }
    if !plan.analyzed {
        found.push(Insight {
            severity: Severity::Info,
            node: None,
            title: "These are estimates".into(),
            detail: "Explain Analyze runs the query and shows real times and rows.".into(),
        });
    }
    found.sort_by_key(|insight| insight.severity);
    found
}

/// `1234567` → `1,234,567`.
pub fn count(n: f64) -> String {
    let digits = (n.max(0.0).round() as u64).to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Bytes in kB, MB or GB.
pub fn bytes(n: u64) -> String {
    match n {
        0..1_048_576 => format!("{:.0} kB", n as f64 / 1_024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", n as f64 / 1_073_741_824.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` of a join with a filtered
    /// full scan, a misestimate and a sort that spilled to disk.
    const ANALYZED: &str = r#"[
      {
        "Plan": {
          "Node Type": "Sort", "Startup Cost": 1000.0, "Total Cost": 1100.0,
          "Plan Rows": 100, "Plan Width": 40,
          "Actual Startup Time": 90.0, "Actual Total Time": 100.0,
          "Actual Rows": 12000, "Actual Loops": 1,
          "Sort Key": ["o.created_at DESC"], "Sort Method": "external merge",
          "Sort Space Used": 4200, "Sort Space Type": "Disk",
          "Shared Hit Blocks": 120, "Shared Read Blocks": 4000, "Temp Written Blocks": 525,
          "Plans": [
            {
              "Node Type": "Hash Join", "Parent Relationship": "Outer", "Join Type": "Inner",
              "Startup Cost": 10.0, "Total Cost": 900.0, "Plan Rows": 100, "Plan Width": 40,
              "Actual Startup Time": 5.0, "Actual Total Time": 70.0,
              "Actual Rows": 12000, "Actual Loops": 1,
              "Hash Cond": "(o.customer_id = c.id)",
              "Plans": [
                {
                  "Node Type": "Seq Scan", "Parent Relationship": "Outer",
                  "Relation Name": "orders", "Schema": "public", "Alias": "o",
                  "Startup Cost": 0.0, "Total Cost": 800.0, "Plan Rows": 100, "Plan Width": 30,
                  "Actual Startup Time": 0.1, "Actual Total Time": 60.0,
                  "Actual Rows": 12000, "Actual Loops": 1,
                  "Filter": "(status = 'refunded'::text)", "Rows Removed by Filter": 988000
                },
                {
                  "Node Type": "Hash", "Parent Relationship": "Inner",
                  "Startup Cost": 5.0, "Total Cost": 5.0, "Plan Rows": 200, "Plan Width": 10,
                  "Actual Startup Time": 2.0, "Actual Total Time": 2.0,
                  "Actual Rows": 200, "Actual Loops": 1, "Hash Batches": 1,
                  "Plans": [
                    {
                      "Node Type": "Seq Scan", "Parent Relationship": "Outer",
                      "Relation Name": "customers", "Schema": "sales", "Alias": "c",
                      "Startup Cost": 0.0, "Total Cost": 4.0, "Plan Rows": 200, "Plan Width": 10,
                      "Actual Startup Time": 0.0, "Actual Total Time": 1.0,
                      "Actual Rows": 200, "Actual Loops": 1
                    }
                  ]
                }
              ]
            }
          ]
        },
        "Planning Time": 0.4,
        "Execution Time": 101.2
      }
    ]"#;

    #[test]
    fn writes_the_explain_to_send() {
        assert_eq!(
            explain_sql("select * from orders;", false).unwrap(),
            "EXPLAIN (VERBOSE, FORMAT JSON) select * from orders"
        );
        assert!(
            explain_sql("delete from t", true)
                .unwrap()
                .starts_with("EXPLAIN (ANALYZE, BUFFERS")
        );
        assert!(explain_sql("select 1; select 2", false).is_err());
        assert!(explain_sql("explain select 1", false).is_err());
        assert!(explain_sql("  ", false).is_err());
        assert!(is_explain("-- why slow?\nEXPLAIN (ANALYZE) select 1"));
        assert!(is_explain("explain select 1"));
        assert!(!is_explain("explained_rows"));
        assert!(!is_explain("select 'explain'"));
    }

    #[test]
    fn builds_the_tree_with_plain_titles_and_self_times() {
        let plan = parse(ANALYZED).unwrap();
        assert!(plan.analyzed);
        assert_eq!(
            (plan.planning_ms, plan.execution_ms),
            (Some(0.4), Some(101.2))
        );
        let nodes = plan.root.walk();
        assert_eq!(nodes.len(), 5);
        assert_eq!(
            nodes.iter().map(|n| n.id).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(plan.root.title, "Sort by o.created_at DESC");
        assert_eq!(nodes[1].title, "Join through a hash table");
        assert_eq!(nodes[2].title, "Read all of orders");
        assert_eq!(nodes[4].title, "Read all of sales.customers");
        // Sort: 100 ms in all, 70 of them in the join under it.
        assert_eq!(plan.root.self_ms, Some(30.0));
        assert_eq!(nodes[1].self_ms, Some(8.0));
        assert!((plan.share(&plan.root) - 0.3).abs() < 1e-9);
        assert_eq!(plan.hottest().id, 2);
        assert_eq!(plan.buffers().read, 4000);
        assert!(nodes[2].details.iter().any(|(label, _)| label == "Filter"));
    }

    #[test]
    fn points_out_what_looks_wrong() {
        let plan = parse(ANALYZED).unwrap();
        let titles: Vec<&str> = plan.insights.iter().map(|i| i.title.as_str()).collect();
        assert!(
            titles
                .iter()
                .any(|t| t.starts_with("Reads 1,000,000 rows of orders to keep 12,000")),
            "{titles:?}"
        );
        assert!(
            titles
                .iter()
                .any(|t| t.starts_with("Expected 100 rows, got 12,000"))
        );
        assert!(titles.iter().any(|t| t.contains("used disk")));
        assert!(
            titles
                .iter()
                .any(|t| t.contains("goes to \u{201c}Read all of orders"))
        );
        assert_eq!(plan.insights[0].severity, Severity::Warning);
        assert!(!titles.iter().any(|t| t.contains("estimates")));
    }

    #[test]
    fn estimates_use_costs() {
        let json = r#"[{"Plan": {"Node Type": "Limit", "Startup Cost": 0, "Total Cost": 10,
            "Plan Rows": 5, "Plans": [{"Node Type": "Index Scan", "Relation Name": "orders",
            "Index Name": "orders_pkey", "Startup Cost": 0, "Total Cost": 8, "Plan Rows": 5}]}}]"#;
        let plan = parse(json).unwrap();
        assert!(!plan.analyzed);
        assert_eq!(
            plan.root.children[0].title,
            "Look up orders through orders_pkey"
        );
        assert!((plan.share(&plan.root.children[0]) - 0.8).abs() < 1e-9);
        assert!(
            plan.insights
                .iter()
                .any(|i| i.title == "These are estimates")
        );
        assert!(parse("not json").is_err());
        assert!(parse("[]").is_err());
    }

    #[test]
    fn formats_counts_and_sizes() {
        assert_eq!(count(1_234_567.0), "1,234,567");
        assert_eq!(count(12.4), "12");
        assert_eq!(bytes(4_300_000), "4.1 MB");
    }
}
