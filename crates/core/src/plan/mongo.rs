//! MongoDB's `explain` answer (`queryPlanner`, and `executionStats` when
//! it ran the read) read into the same [`Plan`] PostgreSQL's plans become:
//! a find's stages, an aggregation's pipeline over the query that feeds it,
//! and a sharded cluster's plan with each shard's under it.
//!
//! MongoDB gives no costs and no row estimates: a plan it did not run says
//! only which steps it chose, and one it ran says what each returned, what
//! it examined and how long it took. Its insights are its own: a big
//! collection scan, a sort held in memory, far more examined than returned,
//! and the plans the planner weighed and rejected.

use serde_json::{Map, Value};

use super::{Actual, Buffers, Category, Insight, InsightKind, Node, Plan, Severity, count};

/// A collection scan that examines this many documents is pointed out.
const BIG_SCAN: f64 = 10_000.0;
/// A step must examine this many times what it returns, and this many in
/// all, to be pointed out.
const EXAMINED_RATIO: f64 = 10.0;
const EXAMINED_MIN: f64 = 1_000.0;

/// Whether `value` is MongoDB's explain answer: a query's plan, an
/// aggregation's stages, or a sharded aggregation's shards.
pub(super) fn is_plan(value: &Value) -> bool {
    let Some(top) = value.as_object() else {
        return false;
    };
    top.contains_key("queryPlanner")
        || top.get("stages").is_some_and(Value::is_array)
        || (top.contains_key("splitPipeline") && top.get("shards").is_some_and(Value::is_object))
}

/// Reads an explain answer (see [`is_plan`]). A read that failed while it
/// ran to be measured (an error in an expression, `maxTimeMS` run out) is
/// the server's error, not a plan: its counts stop where it failed.
pub fn parse(value: &Value) -> Result<Plan, String> {
    if let Some(error) = failure(value) {
        return Err(format!(
            "The read failed while MongoDB measured it: {error}. Explain shows its plan \
             without running it."
        ));
    }
    let mut reader = Reader::default();
    let root = reader.answer(value)?;
    let analyzed = root.actual.is_some();
    // An aggregation run as stages after its query times only the query.
    let cursor = value
        .get("stages")
        .and_then(Value::as_array)
        .and_then(|stages| stages.first())
        .and_then(|first| first.get("$cursor"))
        .unwrap_or(value);
    let planning_ms = [value, cursor].into_iter().find_map(|answer| {
        answer
            .get("queryPlanner")
            .and_then(|planner| number(planner, "optimizationTimeMillis"))
    });
    let execution_ms = [value, cursor].into_iter().find_map(|answer| {
        answer
            .get("executionStats")
            .and_then(|stats| number(stats, "executionTimeMillis"))
    });
    let mut plan = Plan {
        root,
        planning_ms,
        execution_ms,
        analyzed,
        estimates: false,
        insights: Vec::new(),
    };
    plan.insights = reader.insights(&plan);
    Ok(plan)
}

/// The server's message when the read it ran to measure failed: the
/// answer is still `ok`, with `executionSuccess: false` in its statistics
/// (an aggregation's query's, or a shard's).
fn failure(value: &Value) -> Option<String> {
    let mut answers = vec![value];
    if let Some(stages) = value.get("stages").and_then(Value::as_array) {
        answers.extend(stages.iter().filter_map(|stage| stage.get("$cursor")));
    }
    if let Some(shards) = value.get("shards").and_then(Value::as_object)
        && let Some(error) = shards.values().find_map(failure)
    {
        return Some(error);
    }
    for answer in answers {
        let Some(stats) = answer.get("executionStats") else {
            continue;
        };
        let shards = stats
            .get("executionStages")
            .and_then(|stages| stages.get("shards"))
            .and_then(Value::as_array);
        for stats in std::iter::once(stats).chain(shards.into_iter().flatten()) {
            let failed = stats.get("executionSuccess").and_then(Value::as_bool) == Some(false)
                || stats.get("failed").and_then(Value::as_bool) == Some(true);
            if failed {
                return Some(text(stats, "errorMessage").map_or_else(
                    || "the server gave no reason".into(),
                    |message| message.trim_end_matches('.').to_string(),
                ));
            }
        }
    }
    None
}

/// What is learned while the tree is read, for the insights.
#[derive(Default)]
struct Reader {
    next: usize,
    /// What each step examined, by id: documents and index keys.
    examined: Vec<(usize, Examined)>,
    /// The plans the planner rejected, each in a few words.
    rejected: Vec<String>,
    /// Lookups that read their whole collection.
    lookup_scans: Vec<LookupScan>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Examined {
    documents: Option<f64>,
    keys: Option<f64>,
}

/// A lookup that reads all of the collection it looks in.
struct LookupScan {
    node: usize,
    /// How many times, when measured.
    times: Option<f64>,
    collection: String,
    /// The field of `collection` it matches, when it matches one.
    field: Option<String>,
}

/// What a step did besides returning documents, wherever the engine
/// keeps it.
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    examined: Examined,
    spills: Option<f64>,
    used_disk: bool,
    /// How many times the slot-based engine read a whole collection for
    /// the step (a lookup's foreign collection).
    scans: Option<f64>,
}

/// What a step works on: its collection, for the steps that name none.
#[derive(Clone)]
struct Scope {
    collection: Option<String>,
}

impl Reader {
    /// The plan of one explain answer, whatever its shape.
    fn answer(&mut self, value: &Value) -> Result<Node, String> {
        if let Some(stages) = value.get("stages").and_then(Value::as_array) {
            return self.pipeline(stages);
        }
        if let Some(planner) = value.get("queryPlanner") {
            return self.query(planner, value.get("executionStats"));
        }
        if let Some(shards) = value.get("shards").and_then(Value::as_object) {
            return self.split(value, shards);
        }
        Err("The explain answer holds no plan".into())
    }

    /// A query's plan: its winning plan, with what each step did when
    /// `stats` says.
    fn query(&mut self, planner: &Value, stats: Option<&Value>) -> Result<Node, String> {
        let scope = Scope {
            collection: text(planner, "namespace").map(|namespace| collection_of(&namespace)),
        };
        let winning = planner
            .get("winningPlan")
            .ok_or("The query plan has no winning plan")?;
        self.reject(planner);
        let executed = stats.and_then(|stats| stats.get("executionStages"));
        if let Some(shards) = winning.get("shards").and_then(Value::as_array) {
            return Ok(self.sharded(winning, shards, executed));
        }
        Ok(match winning.get("queryPlan") {
            // The slot-based engine: its steps are its own, matched to the
            // plan's by `planNodeId`.
            Some(plan) => {
                let mut stages = Vec::new();
                if let Some(executed) = executed {
                    slot_stats(executed, &mut stages);
                }
                let totals = stats.map(|stats| Examined {
                    documents: number(stats, "totalDocsExamined"),
                    keys: number(stats, "totalKeysExamined"),
                });
                let slots = Slots {
                    stages: &stages,
                    totals: single_scans(plan, totals.unwrap_or_default()),
                };
                match executed {
                    Some(_) => self.stage(plan, &scope, &Measured::Slots(&slots)),
                    None => self.stage(plan, &scope, &Measured::No),
                }
            }
            None => match executed {
                // The classic engine: the winning plan, measured.
                Some(executed) => self.stage(executed, &scope, &Measured::Inline),
                None => self.stage(winning, &scope, &Measured::No),
            },
        })
    }

    /// The plans the planner weighed and did not pick.
    fn reject(&mut self, planner: &Value) {
        let Some(rejected) = planner.get("rejectedPlans").and_then(Value::as_array) else {
            return;
        };
        for plan in rejected {
            let plan = plan.get("queryPlan").unwrap_or(plan);
            let collection = text(planner, "namespace").map(|namespace| collection_of(&namespace));
            let mut scans = Vec::new();
            scans_of(plan, collection.as_deref(), &mut scans);
            if scans.is_empty() {
                scans.push(text(plan, "stage").unwrap_or_else(|| "another plan".into()));
            }
            self.rejected.push(scans.join(" and "));
        }
    }

    /// A find on a sharded cluster: each shard's plan under the merge.
    fn sharded(&mut self, winning: &Value, shards: &[Value], executed: Option<&Value>) -> Node {
        let id = self.id();
        let merged = executed.and_then(|executed| executed.get("shards"));
        let children = shards
            .iter()
            .map(|shard| {
                let name = text(shard, "shardName").unwrap_or_else(|| "a shard".into());
                let stats = merged.and_then(Value::as_array).and_then(|stats| {
                    stats
                        .iter()
                        .find(|s| text(s, "shardName").as_deref() == Some(name.as_str()))
                });
                self.shard(&name, shard, stats)
            })
            .collect();
        let stage = text(winning, "stage").unwrap_or_else(|| "SHARD_MERGE".into());
        let title = match stage.as_str() {
            "SINGLE_SHARD" => "Read from one shard".to_string(),
            "SHARD_MERGE_SORT" => format!("Merge the sorted results of {} shards", shards.len()),
            _ => format!("Merge the results of {} shards", shards.len()),
        };
        let mut node = blank(id, stage, title, Category::Other);
        if let Some(executed) = executed {
            measure(&mut node, executed, "executionTimeMillis");
        }
        node.children = children;
        finish(node)
    }

    /// One shard's part of a sharded find.
    fn shard(&mut self, name: &str, shard: &Value, stats: Option<&Value>) -> Node {
        let id = self.id();
        let mut node = blank(id, "Shard".into(), format!("On {name}"), Category::Other);
        if let Some(connection) = text(shard, "connectionString") {
            node.details.push(("Shard".into(), connection));
        }
        if let Some(stats) = stats {
            measure(&mut node, stats, "executionTimeMillis");
        }
        let planned = self.query(shard, stats);
        match planned {
            Ok(child) => node.children.push(child),
            Err(error) => node.details.push(("Plan".into(), error)),
        }
        finish(node)
    }

    /// A sharded aggregation: each shard's part under the merge.
    fn split(&mut self, value: &Value, shards: &Map<String, Value>) -> Result<Node, String> {
        let id = self.id();
        let mut node = blank(
            id,
            text(value, "mergeType").unwrap_or_else(|| "merge".into()),
            format!("Merge the results of {} shards", shards.len()),
            Category::Other,
        );
        for (name, shard) in shards {
            let id = self.id();
            let mut on = blank(id, "Shard".into(), format!("On {name}"), Category::Other);
            on.children.push(self.answer(shard)?);
            node.children.push(finish(on));
        }
        Ok(finish(node))
    }

    /// An aggregation that MongoDB runs as stages after the query that
    /// feeds them: each stage reads from the one before it.
    fn pipeline(&mut self, stages: &[Value]) -> Result<Node, String> {
        let (last, before) = stages.split_last().ok_or("The pipeline has no stages")?;
        let Some((name, spec)) = last.as_object().and_then(|stage| {
            stage
                .iter()
                .find(|(key, _)| key.starts_with('$'))
                .map(|(key, spec)| (key.clone(), spec))
        }) else {
            return Err("A pipeline stage has no name".into());
        };
        if name == "$cursor" {
            let planner = spec
                .get("queryPlanner")
                .ok_or("The pipeline's query has no plan")?;
            return self.query(planner, spec.get("executionStats"));
        }
        let id = self.id();
        let (title, category) = describe_stage(&name, spec);
        let mut node = blank(id, name.clone(), title, category);
        measure(&mut node, last, "executionTimeMillisEstimate");
        stage_details(&mut node, &name, spec, last);
        let examined = Examined {
            documents: number(last, "totalDocsExamined"),
            keys: number(last, "totalKeysExamined"),
        };
        if examined.documents.is_some() || examined.keys.is_some() {
            self.examined.push((id, examined));
        }
        if name == "$lookup"
            && let Some(scans) = number(last, "collectionScans").filter(|n| *n > 0.0)
        {
            self.lookup_scans.push(LookupScan {
                node: id,
                times: Some(scans),
                collection: text(spec, "from").unwrap_or_else(|| "the collection".into()),
                field: text(spec, "foreignField"),
            });
        }
        if !before.is_empty() {
            node.children.push(self.pipeline(before)?);
        }
        Ok(finish(node))
    }

    /// One step of a query plan and the steps it reads from.
    fn stage(&mut self, value: &Value, scope: &Scope, measured: &Measured) -> Node {
        let id = self.id();
        let stage = text(value, "stage").unwrap_or_else(|| "STEP".into());
        let collection = match stage.as_str() {
            "EQ_LOOKUP" => text(value, "foreignCollection").map(|name| collection_of(&name)),
            _ => scope.collection.clone(),
        };
        let index = text(value, "indexName");
        let relation = match category_of(&stage) {
            Category::Scan | Category::Modify | Category::Join => collection.clone(),
            _ => None,
        };
        let (title, category) = describe(value, &stage, relation.as_deref(), index.as_deref());
        let mut node = blank(id, stage.clone(), title, category);
        node.relation = relation;
        node.index = index;
        let stats = match measured {
            Measured::No => Stats::default(),
            Measured::Inline => {
                measure(&mut node, value, "executionTimeMillisEstimate");
                Stats {
                    examined: Examined {
                        documents: number(value, "docsExamined"),
                        keys: number(value, "keysExamined"),
                    },
                    spills: number(value, "spills"),
                    used_disk: value.get("usedDisk").and_then(Value::as_bool) == Some(true),
                    scans: None,
                }
            }
            Measured::Slots(slots) => {
                let own: Vec<&Value> = number(value, "planNodeId")
                    .map(|id| {
                        slots
                            .stages
                            .iter()
                            .filter(|(node, _)| *node == id)
                            .map(|(_, stats)| *stats)
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(top) = own.first() {
                    measure(&mut node, top, "executionTimeMillisEstimate");
                }
                let mut stats = slot_measures(&own);
                if stats.examined.documents.is_none() && reads_documents(&stage) {
                    stats.examined.documents = slots.totals.documents;
                }
                if stats.examined.keys.is_none() && reads_keys(&stage) {
                    stats.examined.keys = slots.totals.keys;
                }
                stats
            }
        };
        query_details(&mut node, value, &stats);
        let examined = stats.examined;
        if examined.documents.is_some() || examined.keys.is_some() {
            self.examined.push((id, examined));
        }
        if let (Some(read), Some(kept)) = (examined.documents, node.actual_rows())
            && filter_of(value).is_some()
        {
            node.rows_removed = Some((read - kept).max(0.0));
        }
        if stage == "EQ_LOOKUP" && text(value, "strategy").as_deref() == Some("NestedLoopJoin") {
            self.lookup_scans.push(LookupScan {
                node: id,
                times: stats.scans,
                collection: node
                    .relation
                    .clone()
                    .unwrap_or_else(|| "the collection".into()),
                field: text(value, "foreignField"),
            });
        }
        let inner = scope.clone();
        node.children = inputs(value)
            .into_iter()
            .map(|input| self.stage(input, &inner, measured))
            .collect();
        finish(node)
    }

    fn id(&mut self) -> usize {
        let id = self.next;
        self.next += 1;
        id
    }

    fn insights(&self, plan: &Plan) -> Vec<Insight> {
        let mut found: Vec<Insight> = super::insights(plan)
            .into_iter()
            .filter(|insight| insight.kind == InsightKind::Hottest)
            .collect();
        let mut scanned = Vec::new();
        for node in plan.root.walk() {
            let examined = self.examined_by(node.id);
            let collection = node.relation.as_deref().unwrap_or("the collection");
            if node.node_type == "COLLSCAN"
                && let Some(documents) = examined.documents
                && documents >= BIG_SCAN
            {
                scanned.push(node.id);
                let filter = detail(node, "Filter");
                found.push(Insight {
                    kind: InsightKind::FullScan,
                    severity: Severity::Warning,
                    node: Some(node.id),
                    title: format!("Reads all {} documents of {collection}", count(documents)),
                    detail: match (filter, node.actual_rows()) {
                        (Some(filter), Some(kept)) => format!(
                            "Its filter {filter} keeps {} of them. An index on the filtered \
                             fields lets MongoDB find them without reading the rest.",
                            count(kept)
                        ),
                        _ => "Every document is read. A filter on indexed fields, or a limit, \
                              would read fewer."
                            .into(),
                    },
                });
            }
            if node.category == Category::Sort && node.node_type == "SORT" {
                let spilled = detail(node, "Spilled").is_some();
                let sorted = node
                    .actual_rows()
                    .map(|rows| format!(" {} documents", count(rows)))
                    .unwrap_or_default();
                let key = detail(node, "Sort key").unwrap_or_else(|| "its sort".into());
                found.push(Insight {
                    kind: if spilled {
                        InsightKind::SortOnDisk
                    } else {
                        InsightKind::SortInMemory
                    },
                    severity: Severity::Warning,
                    node: Some(node.id),
                    title: match spilled {
                        true => format!("Sorts{sorted} in memory and spills to disk"),
                        false => format!("Sorts{sorted} in memory"),
                    },
                    detail: format!(
                        "No index returns the documents in the order {key}, so MongoDB holds \
                         them all before it returns the first, up to 100 MB. An index in that \
                         order lets it return them as it reads."
                    ),
                });
            }
            if node.node_type == "$sort" && detail(node, "Spilled").is_some() {
                found.push(Insight {
                    kind: InsightKind::SortOnDisk,
                    severity: Severity::Warning,
                    node: Some(node.id),
                    title: "The sort did not fit in memory and spilled to disk".into(),
                    detail: "Sorting fewer documents (filter or group first) keeps it in \
                             memory."
                        .into(),
                });
            }
        }
        for node in plan.root.walk() {
            let examined = self.examined_by(node.id);
            let Some(returned) = node.actual_rows() else {
                continue;
            };
            let most = examined
                .documents
                .unwrap_or(0.0)
                .max(examined.keys.unwrap_or(0.0));
            // A lookup returns one document per document it is given, with
            // what it found inside: what it examined is said by its own
            // insight.
            if matches!(node.node_type.as_str(), "$lookup" | "EQ_LOOKUP")
                || scanned.contains(&node.id)
                || most < EXAMINED_MIN
                || most < returned.max(1.0) * EXAMINED_RATIO
            {
                continue;
            }
            let what = match examined.documents.unwrap_or(0.0) >= examined.keys.unwrap_or(0.0) {
                true => "documents",
                false => "index keys",
            };
            found.push(Insight {
                kind: InsightKind::DiscardedRows,
                severity: Severity::Warning,
                node: Some(node.id),
                title: format!(
                    "Examines {} {what} to return {}",
                    count(most),
                    count(returned)
                ),
                detail: "Most of what it reads is thrown away. An index that matches the whole \
                         filter (equality fields first, then sort, then ranges) reads only \
                         what it returns."
                    .into(),
            });
        }
        for scan in &self.lookup_scans {
            let collection = &scan.collection;
            let times = match scan.times.map(|n| n as u64) {
                Some(1) => "once".to_string(),
                Some(n) => format!("{} times", count(n as f64)),
                None => "for each document".to_string(),
            };
            found.push(Insight {
                kind: InsightKind::FullScan,
                severity: Severity::Warning,
                node: Some(scan.node),
                title: format!("Reads all of {collection} {times} to look up matches"),
                detail: match &scan.field {
                    Some(field) => format!(
                        "An index on {collection}.{field} lets each lookup find its documents \
                         directly."
                    ),
                    None => format!(
                        "An index on the fields of {collection} its pipeline matches lets each \
                         lookup find its documents directly."
                    ),
                },
            });
        }
        if !self.rejected.is_empty() {
            let others = match self.rejected.len() {
                1 => "1 other plan".to_string(),
                n => format!("{n} other plans"),
            };
            found.push(Insight {
                kind: InsightKind::RejectedPlans,
                severity: Severity::Info,
                node: None,
                title: format!("MongoDB chose this plan over {others}"),
                detail: format!(
                    "It also tried: {}. It keeps the winner for queries of the same shape \
                     until the collection or its indexes change.",
                    self.rejected.join("; ")
                ),
            });
        }
        if !plan.analyzed {
            found.push(Insight {
                kind: InsightKind::Estimated,
                severity: Severity::Info,
                node: None,
                title: "This plan was not run".into(),
                detail: "It is the plan MongoDB chose. Explain with Statistics runs the read \
                         and shows what each step returned and examined."
                    .into(),
            });
        }
        found.sort_by_key(|insight| insight.severity);
        found
    }

    fn examined_by(&self, id: usize) -> Examined {
        self.examined
            .iter()
            .find(|(node, _)| *node == id)
            .map(|(_, examined)| *examined)
            .unwrap_or_default()
    }
}

/// Where a step's measurements are.
enum Measured<'a> {
    /// Not measured.
    No,
    /// In the step itself (the classic engine's `executionStages`).
    Inline,
    /// In the slot-based engine's steps, by `planNodeId`.
    Slots(&'a Slots<'a>),
}

/// The slot-based engine's measurements of a plan.
struct Slots<'a> {
    /// Its steps with the plan step each belongs to, the topmost of each
    /// plan step first: that one returns what the plan step returns.
    stages: &'a [(f64, &'a Value)],
    /// The whole read's examined counts, for a plan with a single step
    /// that examines documents (or index keys) and so examined them all.
    totals: Examined,
}

/// The slot-based engine's steps, in the order they nest.
fn slot_stats<'a>(value: &'a Value, found: &mut Vec<(f64, &'a Value)>) {
    if let Some(id) = number(value, "planNodeId") {
        found.push((id, value));
    }
    for input in inputs(value) {
        slot_stats(input, found);
    }
}

/// What the slot-based engine's steps of one plan step did: the documents
/// its scans and seeks read, the keys its index steps examined, its
/// spills, and how many times it read a whole collection.
fn slot_measures(stages: &[&Value]) -> Stats {
    let sum = |kinds: &[&str], key: &str| {
        stages
            .iter()
            .filter(|stats| {
                kinds.is_empty()
                    || text(stats, "stage").is_some_and(|s| kinds.contains(&s.as_str()))
            })
            .filter_map(|stats| number(stats, key))
            .reduce(|a, b| a + b)
    };
    Stats {
        examined: Examined {
            documents: sum(&["scan", "seek"], "numReads"),
            keys: sum(&[], "keysExamined"),
        },
        spills: sum(&[], "spills"),
        used_disk: stages
            .iter()
            .any(|stats| stats.get("usedDisk").and_then(Value::as_bool) == Some(true)),
        scans: sum(&["scan"], "opens"),
    }
}

/// `totals` for the plan's step that examines documents and the one that
/// examines keys, each only when the plan has one: then it examined them
/// all.
fn single_scans(plan: &Value, totals: Examined) -> Examined {
    let mut steps = Vec::new();
    all_stages(plan, &mut steps);
    let only = |reads: fn(&str) -> bool| steps.iter().filter(|stage| reads(stage)).count() == 1;
    Examined {
        documents: totals.documents.filter(|_| only(reads_documents)),
        keys: totals.keys.filter(|_| only(reads_keys)),
    }
}

fn all_stages(value: &Value, found: &mut Vec<String>) {
    found.push(text(value, "stage").unwrap_or_default());
    for input in inputs(value) {
        all_stages(input, found);
    }
}

/// Whether a plan step reads documents from a collection.
fn reads_documents(stage: &str) -> bool {
    matches!(
        stage,
        "COLLSCAN"
            | "FETCH"
            | "IDHACK"
            | "CLUSTERED_IXSCAN"
            | "EXPRESS_CLUSTERED_IXSCAN"
            | "EQ_LOOKUP"
            | "TEXT_MATCH"
    )
}

/// Whether a plan step examines index keys.
fn reads_keys(stage: &str) -> bool {
    matches!(
        stage,
        "IXSCAN"
            | "EXPRESS_IXSCAN"
            | "COUNT_SCAN"
            | "DISTINCT_SCAN"
            | "GEO_NEAR_2D"
            | "GEO_NEAR_2DSPHERE"
    )
}

/// The steps a step reads from, wherever it keeps them.
fn inputs(value: &Value) -> Vec<&Value> {
    let mut found = Vec::new();
    for key in [
        "inputStage",
        "outerStage",
        "innerStage",
        "thenStage",
        "elseStage",
    ] {
        if let Some(input) = value.get(key).filter(|v| v.is_object()) {
            found.push(input);
        }
    }
    if let Some(inputs) = value.get("inputStages").and_then(Value::as_array) {
        found.extend(inputs.iter().filter(|v| v.is_object()));
    }
    found
}

/// The scans a rejected plan would have run, as their titles.
fn scans_of(value: &Value, collection: Option<&str>, found: &mut Vec<String>) {
    let stage = text(value, "stage").unwrap_or_default();
    if category_of(&stage) == Category::Scan && stage != "FETCH" {
        let index = text(value, "indexName");
        found.push(describe(value, &stage, collection, index.as_deref()).0);
    }
    for input in inputs(value) {
        scans_of(input, collection, found);
    }
}

/// A step with nothing measured yet.
fn blank(id: usize, node_type: String, title: String, category: Category) -> Node {
    Node {
        id,
        node_type,
        title,
        category,
        relation: None,
        index: None,
        parent_relationship: None,
        estimated_rows: 0.0,
        startup_cost: 0.0,
        total_cost: 0.0,
        actual: None,
        inclusive_ms: None,
        self_ms: None,
        self_cost: 0.0,
        rows_removed: None,
        buffers: Buffers::default(),
        details: Vec::new(),
        children: Vec::new(),
    }
}

/// What a step returned and how long it took with what it reads from.
fn measure(node: &mut Node, stats: &Value, time: &str) {
    let Some(returned) = number(stats, "nReturned") else {
        return;
    };
    let ms = number(stats, time).unwrap_or(0.0);
    node.actual = Some(Actual {
        rows_per_loop: returned,
        loops: 1.0,
        startup_ms: 0.0,
        total_ms_per_loop: ms,
    });
    node.inclusive_ms = Some(ms);
}

/// The time a step takes by itself, once its children are read.
fn finish(mut node: Node) -> Node {
    node.self_ms = node.inclusive_ms.map(|inclusive| {
        let children: f64 = node.children.iter().filter_map(|c| c.inclusive_ms).sum();
        (inclusive - children).max(0.0)
    });
    node
}

/// `shop.orders` → `orders`.
fn collection_of(namespace: &str) -> String {
    namespace
        .split_once('.')
        .map_or(namespace, |(_, name)| name)
        .to_string()
}

fn category_of(stage: &str) -> Category {
    match stage {
        "COLLSCAN"
        | "IXSCAN"
        | "FETCH"
        | "IDHACK"
        | "EXPRESS_IXSCAN"
        | "EXPRESS_CLUSTERED_IXSCAN"
        | "CLUSTERED_IXSCAN"
        | "COUNT_SCAN"
        | "DISTINCT_SCAN"
        | "TEXT_MATCH"
        | "GEO_NEAR_2D"
        | "GEO_NEAR_2DSPHERE" => Category::Scan,
        "EQ_LOOKUP" | "AND_HASH" | "AND_SORTED" => Category::Join,
        "SORT" | "SORT_KEY_GENERATOR" | "SORT_MERGE" => Category::Sort,
        "GROUP" | "COUNT" => Category::Aggregate,
        "UPDATE" | "DELETE" | "BATCHED_DELETE" | "EXPRESS_UPDATE" | "EXPRESS_DELETE" => {
            Category::Modify
        }
        _ => Category::Other,
    }
}

/// A query step's title in plain words, and its category.
fn describe(
    value: &Value,
    stage: &str,
    collection: Option<&str>,
    index: Option<&str>,
) -> (String, Category) {
    let table = collection.unwrap_or("the collection");
    let index = index.unwrap_or("an index");
    let title = match stage {
        "COLLSCAN" => match text(value, "direction").as_deref() {
            Some("backward") => format!("Read all of {table}, newest first"),
            _ => format!("Read all of {table}"),
        },
        "IXSCAN" => format!("Use the index {index}"),
        "FETCH" => format!("Fetch the matching documents of {table}"),
        "IDHACK" => format!("Look up {table} by _id"),
        "EXPRESS_IXSCAN" | "EXPRESS_CLUSTERED_IXSCAN" | "CLUSTERED_IXSCAN" => {
            format!("Look up {table} through {index}")
        }
        "COUNT_SCAN" => format!("Count the keys of the index {index}"),
        "DISTINCT_SCAN" => format!("Read the distinct values of the index {index}"),
        "TEXT_MATCH" => "Match the text search".into(),
        "GEO_NEAR_2D" | "GEO_NEAR_2DSPHERE" => format!("Find the nearest through {index}"),
        "EQ_LOOKUP" => format!("Look up matching documents in {table}"),
        "AND_HASH" | "AND_SORTED" => "Keep what every index scan found".into(),
        "OR" => "Combine the results of several scans".into(),
        "SORT" => match value.get("sortPattern") {
            Some(pattern) => format!("Sort by {} in memory", compact(pattern)),
            None => "Sort in memory".into(),
        },
        "SORT_KEY_GENERATOR" => "Compute the sort keys".into(),
        "SORT_MERGE" => "Merge sorted scans".into(),
        "LIMIT" => match number(value, "limitAmount") {
            Some(n) => format!("Keep only the first {}", count(n)),
            None => "Keep only the first documents".into(),
        },
        "SKIP" => match number(value, "skipAmount") {
            Some(n) => format!("Skip the first {}", count(n)),
            None => "Skip the first documents".into(),
        },
        "PROJECTION_SIMPLE" | "PROJECTION_DEFAULT" | "PROJECTION_COVERED" => {
            "Keep only the fields asked for".into()
        }
        "SHARDING_FILTER" => "Leave out documents this shard does not own".into(),
        "GROUP" => "Group documents".into(),
        "COUNT" => "Count documents".into(),
        "UPDATE" | "EXPRESS_UPDATE" => format!("Update documents of {table}"),
        "DELETE" | "BATCHED_DELETE" | "EXPRESS_DELETE" => format!("Delete documents of {table}"),
        "EOF" => "Return nothing: the collection does not exist".into(),
        "CACHED_PLAN" => "Run the cached plan".into(),
        "SUBPLAN" => "Plan each $or branch on its own".into(),
        "MULTI_PLAN" => "Try several plans".into(),
        other => other.to_string(),
    };
    (title, category_of(stage))
}

/// A pipeline stage's title in plain words, and its category.
fn describe_stage(name: &str, spec: &Value) -> (String, Category) {
    match name {
        "$lookup" => (
            format!(
                "Look up matching documents in {}",
                text(spec, "from").unwrap_or_else(|| "another collection".into())
            ),
            Category::Join,
        ),
        "$graphLookup" => (
            format!(
                "Follow links through {}",
                text(spec, "from").unwrap_or_else(|| "another collection".into())
            ),
            Category::Join,
        ),
        "$unionWith" => (
            "Add the documents of another collection".into(),
            Category::Join,
        ),
        "$group" => match spec.get("_id") {
            Some(Value::Null) | None => ("Aggregate all documents".into(), Category::Aggregate),
            Some(key) => (format!("Group by {}", compact(key)), Category::Aggregate),
        },
        "$bucket" | "$bucketAuto" => ("Group documents into buckets".into(), Category::Aggregate),
        "$sortByCount" => ("Count and sort by value".into(), Category::Aggregate),
        "$count" => ("Count documents".into(), Category::Aggregate),
        "$setWindowFields" => ("Compute window fields".into(), Category::Aggregate),
        "$facet" => ("Run several pipelines".into(), Category::Aggregate),
        "$sort" => match spec.get("sortKey") {
            Some(key) => (format!("Sort by {}", compact(key)), Category::Sort),
            None => ("Sort".into(), Category::Sort),
        },
        "$limit" => (
            match spec.as_f64() {
                Some(n) => format!("Keep only the first {}", count(n)),
                None => "Keep only the first documents".into(),
            },
            Category::Other,
        ),
        "$skip" => ("Skip the first documents".into(), Category::Other),
        "$match" => ("Keep matching documents".into(), Category::Other),
        "$unwind" => (
            match spec
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| spec.as_str())
            {
                Some(path) => format!("One document per element of {path}"),
                None => "One document per array element".into(),
            },
            Category::Other,
        ),
        "$project" | "$addFields" | "$set" | "$unset" | "$replaceRoot" | "$replaceWith" => {
            ("Reshape documents".into(), Category::Other)
        }
        other => (other.to_string(), Category::Other),
    }
}

/// What a query step shows when opened.
fn query_details(node: &mut Node, value: &Value, stats: &Stats) {
    let mut add = |label: &str, text: Option<String>| {
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            node.details.push((label.to_string(), text));
        }
    };
    add("Filter", filter_of(value).map(compact));
    add("Keys", value.get("keyPattern").map(compact));
    add("Bounds", value.get("indexBounds").map(bounds));
    if text(value, "direction").as_deref() == Some("backward") {
        add("Direction", Some("backward".into()));
    }
    if value.get("isMultiKey").and_then(Value::as_bool) == Some(true) {
        add("Multikey", Some("yes: an array is indexed".into()));
    }
    add("Sort key", value.get("sortPattern").map(compact));
    add("Fields", value.get("transformBy").map(compact));
    if let (Some(local), Some(foreign)) = (text(value, "localField"), text(value, "foreignField")) {
        add("Matches", Some(format!("{local} = {foreign}")));
    }
    add("Strategy", text(value, "strategy"));
    add("Documents examined", stats.examined.documents.map(count));
    add("Keys examined", stats.examined.keys.map(count));
    if stats.used_disk || stats.spills.is_some_and(|n| n > 0.0) {
        add("Spilled", Some(spill(stats.spills)));
    }
}

/// A step's filter, unless it keeps everything (`{}`).
fn filter_of(value: &Value) -> Option<&Value> {
    value
        .get("filter")
        .filter(|filter| filter.as_object().is_none_or(|fields| !fields.is_empty()))
}

/// What a pipeline stage shows when opened.
fn stage_details(node: &mut Node, name: &str, spec: &Value, stage: &Value) {
    let mut add = |label: &str, text: Option<String>| {
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            node.details.push((label.to_string(), text));
        }
    };
    match name {
        "$lookup" => {
            add("From", text(spec, "from"));
            if let (Some(local), Some(foreign)) =
                (text(spec, "localField"), text(spec, "foreignField"))
            {
                add("Matches", Some(format!("{local} = {foreign}")));
            }
            add("Pipeline", spec.get("pipeline").map(compact));
            add(
                "Indexes used",
                stage
                    .get("indexesUsed")
                    .and_then(Value::as_array)
                    .map(|used| {
                        used.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }),
            );
            add(
                "Collection scans",
                number(stage, "collectionScans").map(count),
            );
        }
        "$sort" => add("Sort key", spec.get("sortKey").map(compact)),
        "$group" => add("Group key", spec.get("_id").map(compact)),
        _ => add("Stage", Some(compact(spec))),
    }
    add(
        "Documents examined",
        number(stage, "totalDocsExamined").map(count),
    );
    add(
        "Keys examined",
        number(stage, "totalKeysExamined").map(count),
    );
    let spills = number(stage, "spills");
    if stage.get("usedDisk").and_then(Value::as_bool) == Some(true)
        || spills.is_some_and(|n| n > 0.0)
    {
        add("Spilled", Some(spill(spills)));
    }
}

/// How a step wrote to disk what did not fit in memory.
fn spill(spills: Option<f64>) -> String {
    match spills.filter(|n| *n > 0.0) {
        Some(n) => format!("to disk, {} times", count(n)),
        None => "to disk".into(),
    }
}

/// `status: ["paid", "paid"]; date: [MaxKey, MinKey]`.
fn bounds(value: &Value) -> String {
    match value.as_object() {
        Some(fields) => fields
            .iter()
            .map(|(field, ranges)| {
                let ranges = match ranges.as_array() {
                    Some(ranges) => ranges
                        .iter()
                        .map(|r| r.as_str().map_or_else(|| compact(r), str::to_string))
                        .collect::<Vec<_>>()
                        .join(", "),
                    None => compact(ranges),
                };
                format!("{field}: {ranges}")
            })
            .collect::<Vec<_>>()
            .join("; "),
        None => compact(value),
    }
}

/// A value on one line, as the shell writes it: `{date: -1}`,
/// `{status: {$eq: "paid"}}`.
fn compact(value: &Value) -> String {
    match value {
        Value::Object(fields) => {
            let fields: Vec<String> = fields
                .iter()
                .map(|(key, value)| format!("{}: {}", key_of(key), compact(value)))
                .collect();
            match fields.is_empty() {
                true => "{}".into(),
                false => format!("{{{}}}", fields.join(", ")),
            }
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(compact).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

/// A key bare where the shell reads it so (`date`, `$eq`), quoted
/// otherwise (`"items.sku"`).
fn key_of(key: &str) -> String {
    let bare = key.chars().enumerate().all(|(at, c)| {
        c == '_' || c == '$' || c.is_ascii_alphabetic() || (at > 0 && c.is_ascii_digit())
    });
    match bare && !key.is_empty() {
        true => key.to_string(),
        false => Value::String(key.to_string()).to_string(),
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_string)
}

/// A number however Extended JSON wrote it: plain, or `{"$numberLong":
/// "…"}` and its kin.
fn number(value: &Value, key: &str) -> Option<f64> {
    let value = value.get(key)?;
    value.as_f64().or_else(|| {
        let wrapped = value.as_object()?;
        [
            "$numberLong",
            "$numberInt",
            "$numberDouble",
            "$numberDecimal",
        ]
        .iter()
        .find_map(|key| wrapped.get(*key)?.as_str()?.parse().ok())
    })
}

/// A detail of a step, by its label.
fn detail(node: &Node, label: &str) -> Option<String> {
    node.details
        .iter()
        .find(|(name, _)| name == label)
        .map(|(_, value)| value.clone())
}

#[cfg(test)]
mod tests {
    use super::super::{Category, InsightKind, Severity, parse};

    // Captured with `db.runCommand({explain: …, verbosity: …})` and printed
    // with `EJSON.stringify` (relaxed, as Kuery prints the answer), against
    // the dev services' sample `shop`:
    //
    // - 7.0 (7.0.45): `{find: "orders", filter: {status: "paid", customer:
    //   <id>}, sort: {date: -1}}`, queryPlanner;
    // - 8.0 (8.0.34): `{find: "big", filter: {n: {$lt: 5}}, sort: {label:
    //   1}}`, executionStats;
    // - 8.0: an aggregation of paid orders with a `$lookup` by pipeline into
    //   customers, `$unwind`, `$group` by country and `$sort`,
    //   executionStats (stages run after the query);
    // - 8.0: the same with a `$lookup` by fields, which the slot-based
    //   engine runs inside the query (`EQ_LOOKUP`), executionStats.
    //
    // The sharded find is hand-written after MongoDB's documented output
    // for a mongos (`SHARD_MERGE` with each shard's plan and statistics):
    // the dev services have no sharded cluster.
    const FIND_7: &str = include_str!("../../fixtures/plans/mongo_7_find.json");
    const FIND_8_STATS: &str = include_str!("../../fixtures/plans/mongo_8_find_stats.json");
    const LOOKUP_8: &str = include_str!("../../fixtures/plans/mongo_8_aggregate_lookup.json");
    const LOOKUP_8_SBE: &str =
        include_str!("../../fixtures/plans/mongo_8_aggregate_lookup_sbe.json");
    const SHARDED: &str = include_str!("../../fixtures/plans/mongo_sharded_find.json");
    // Captured the same way from 8.0 (8.0.34), on a scratch `big` of 20,000
    // documents and a `small` of 2, with the default engine choice
    // (`trySbeRestricted`, which hands `$group` and `$lookup` to the
    // slot-based engine) and its classic twin (`forceClassicEngine`):
    //
    // - `{$match: {k: 1}}, {$group: {_id: "$s"}}`;
    // - `small` looked up in `big` by a field `big` has no index on;
    // - a find sorted by `{t: 1, n: -1}` with a 100 kB sort memory limit,
    //   so it spills: the slot-based one under `trySbeEngine`;
    // - a find whose `$where` outlives `maxTimeMS: 50`, and a projection
    //   whose `$toInt` fails on the first document.
    const GROUP_SBE: &str = include_str!("../../fixtures/plans/mongo_8_group_sbe.json");
    const GROUP_CLASSIC: &str = include_str!("../../fixtures/plans/mongo_8_group_classic.json");
    const LOOKUP_SCAN_SBE: &str = include_str!("../../fixtures/plans/mongo_8_lookup_scan_sbe.json");
    const LOOKUP_SCAN_CLASSIC: &str =
        include_str!("../../fixtures/plans/mongo_8_lookup_scan_classic.json");
    const SPILL_SBE: &str = include_str!("../../fixtures/plans/mongo_8_sort_spill_sbe.json");
    const SPILL_CLASSIC: &str =
        include_str!("../../fixtures/plans/mongo_8_sort_spill_classic.json");
    const TIMED_OUT: &str = include_str!("../../fixtures/plans/mongo_8_find_timeout.json");
    const FAILED: &str = include_str!("../../fixtures/plans/mongo_8_aggregate_failed.json");

    fn titles(plan: &super::Plan) -> Vec<&str> {
        plan.root.walk().iter().map(|n| n.title.as_str()).collect()
    }

    fn kinds(plan: &super::Plan) -> Vec<InsightKind> {
        plan.insights.iter().map(|i| i.kind).collect()
    }

    #[test]
    fn a_planned_find_is_its_winning_plan() {
        let plan = parse(FIND_7).unwrap();
        assert!(!plan.analyzed && !plan.estimates);
        assert_eq!(
            titles(&plan),
            [
                "Sort by {date: -1} in memory",
                "Fetch the matching documents of orders",
                "Use the index customer_1",
            ]
        );
        let scan = &plan.root.children[0].children[0];
        assert_eq!(scan.category, Category::Scan);
        assert_eq!(
            (scan.relation.as_deref(), scan.index.as_deref()),
            (Some("orders"), Some("customer_1"))
        );
        assert!(scan.actual.is_none());
        let rejected = plan
            .insights
            .iter()
            .find(|i| i.kind == InsightKind::RejectedPlans)
            .expect("the rejected plan");
        assert_eq!(rejected.title, "MongoDB chose this plan over 1 other plan");
        assert!(
            rejected.detail.contains("Use the index status_1_date_-1"),
            "{}",
            rejected.detail
        );
        assert!(kinds(&plan).contains(&InsightKind::SortInMemory));
        assert!(kinds(&plan).contains(&InsightKind::Estimated));
    }

    #[test]
    fn a_measured_find_says_what_each_step_did() {
        let plan = parse(FIND_8_STATS).unwrap();
        assert!(plan.analyzed);
        assert_eq!(plan.execution_ms, Some(9.0));
        assert_eq!(
            titles(&plan),
            ["Sort by {label: 1} in memory", "Read all of big"]
        );
        let scan = &plan.root.children[0];
        assert_eq!(scan.node_type, "COLLSCAN");
        assert_eq!(scan.actual_rows(), Some(1000.0));
        assert_eq!(scan.rows_removed, Some(19_000.0));
        assert!(
            scan.details
                .contains(&("Documents examined".into(), "20,000".into()))
        );
        let full = plan
            .insights
            .iter()
            .find(|i| i.kind == InsightKind::FullScan)
            .expect("the big collection scan");
        assert_eq!(full.title, "Reads all 20,000 documents of big");
        assert_eq!(full.node, Some(scan.id));
        assert_eq!(full.severity, Severity::Warning);
        let sort = plan
            .insights
            .iter()
            .find(|i| i.kind == InsightKind::SortInMemory)
            .expect("the sort in memory");
        assert_eq!(sort.title, "Sorts 1,000 documents in memory");
        // The scan's discards are said once, by the scan's insight.
        assert!(!kinds(&plan).contains(&InsightKind::DiscardedRows));
        assert!(!kinds(&plan).contains(&InsightKind::Estimated));
        assert!(!kinds(&plan).contains(&InsightKind::Misestimate));
    }

    #[test]
    fn an_aggregation_reads_from_its_query() {
        let plan = parse(LOOKUP_8).unwrap();
        assert!(plan.analyzed);
        assert_eq!(
            titles(&plan),
            [
                "Sort by {n: -1}",
                "Group by \"$buyer.country\"",
                "Look up matching documents in customers",
                "Keep only the fields asked for",
                "Fetch the matching documents of orders",
                "Use the index status_1_date_-1",
            ]
        );
        let categories: Vec<Category> = plan.root.walk().iter().map(|n| n.category).collect();
        assert_eq!(
            categories[..3],
            [Category::Sort, Category::Aggregate, Category::Join]
        );
        let lookup = &plan.root.children[0].children[0];
        assert_eq!(lookup.actual_rows(), Some(125.0));
        assert!(
            lookup
                .details
                .contains(&("Indexes used".into(), "_id_".into()))
        );
        // Small, indexed and sorted after grouping: nothing to point out.
        assert!(
            plan.insights
                .iter()
                .all(|i| i.severity != Severity::Warning),
            "{:?}",
            plan.insights
        );
    }

    #[test]
    fn a_lookup_inside_the_query_is_a_join() {
        let plan = parse(LOOKUP_8_SBE).unwrap();
        assert!(plan.analyzed);
        assert_eq!(
            titles(&plan),
            [
                "Group documents",
                "Look up matching documents in customers",
                "Keep only the fields asked for",
                "Fetch the matching documents of orders",
                "Use the index status_1_date_-1",
            ]
        );
        let lookup = &plan.root.children[0];
        assert_eq!(lookup.category, Category::Join);
        assert_eq!(lookup.relation.as_deref(), Some("customers"));
        // Measured by the engine's own steps, matched by plan node.
        assert_eq!(plan.root.actual_rows(), Some(3.0));
        assert_eq!(lookup.actual_rows(), Some(125.0));
    }

    #[test]
    fn a_sharded_find_shows_each_shard() {
        let plan = parse(SHARDED).unwrap();
        assert!(plan.analyzed);
        assert_eq!(plan.root.title, "Merge the results of 2 shards");
        assert_eq!(plan.root.actual_rows(), Some(3125.0));
        let shards: Vec<&str> = plan
            .root
            .children
            .iter()
            .map(|s| s.title.as_str())
            .collect();
        assert_eq!(shards, ["On shard01", "On shard02"]);
        let second = &plan.root.children[1];
        assert_eq!(second.actual_rows(), Some(1625.0));
        assert_eq!(
            second.children[0].title,
            "Leave out documents this shard does not own"
        );
        let scan = &second.children[0].children[0];
        assert_eq!(scan.title, "Read all of orders");
        let full = plan
            .insights
            .iter()
            .find(|i| i.kind == InsightKind::FullScan)
            .expect("shard02 reads all of orders");
        assert_eq!(full.node, Some(scan.id));
        assert_eq!(full.title, "Reads all 12,500 documents of orders");
    }

    #[test]
    fn far_more_examined_than_returned_is_pointed_out() {
        let json = r#"{"queryPlanner": {"namespace": "shop.orders", "winningPlan": {"stage": "FETCH"}, "rejectedPlans": []},
            "executionStats": {"executionSuccess": true, "nReturned": 3, "executionTimeMillis": 4,
              "executionStages": {"stage": "FETCH", "filter": {"total": {"$gt": 900}}, "nReturned": 3, "executionTimeMillisEstimate": 3, "docsExamined": 4000,
                "inputStage": {"stage": "IXSCAN", "indexName": "status_1", "nReturned": 4000, "executionTimeMillisEstimate": 1, "keysExamined": 4000}}}}"#;
        let plan = parse(json).unwrap();
        let discarded = plan
            .insights
            .iter()
            .find(|i| i.kind == InsightKind::DiscardedRows)
            .expect("fetches 4,000 to keep 3");
        assert_eq!(discarded.title, "Examines 4,000 documents to return 3");
        assert_eq!(discarded.node, Some(plan.root.id));
        assert_eq!(plan.root.self_ms, Some(2.0));
    }

    /// The insights that say what is wrong, without their numbers' node
    /// ids.
    fn warnings(plan: &super::Plan) -> Vec<(InsightKind, String, String)> {
        plan.insights
            .iter()
            .filter(|i| i.severity == Severity::Warning)
            .map(|i| (i.kind, i.title.clone(), i.detail.clone()))
            .collect()
    }

    #[test]
    fn a_slot_based_plan_says_what_it_examined_as_the_classic_one_does() {
        for (sbe, classic) in [
            (GROUP_SBE, GROUP_CLASSIC),
            (LOOKUP_SCAN_SBE, LOOKUP_SCAN_CLASSIC),
            (SPILL_SBE, SPILL_CLASSIC),
        ] {
            let (sbe, classic) = (parse(sbe).unwrap(), parse(classic).unwrap());
            assert!(!warnings(&sbe).is_empty());
            assert_eq!(warnings(&sbe), warnings(&classic));
        }

        let group = parse(GROUP_SBE).unwrap();
        let scan = &group.root.children[0];
        assert_eq!(scan.node_type, "COLLSCAN");
        assert_eq!(scan.actual_rows(), Some(6_667.0));
        assert_eq!(scan.rows_removed, Some(13_333.0));
        assert!(
            scan.details
                .contains(&("Documents examined".into(), "20,000".into()))
        );
        assert_eq!(
            warnings(&group)[0].1,
            "Reads all 20,000 documents of big",
            "{:?}",
            group.insights
        );

        let spill = parse(SPILL_SBE).unwrap();
        assert!(
            spill
                .root
                .details
                .contains(&("Spilled".into(), "to disk, 104 times".into()))
        );
        assert!(kinds(&spill).contains(&InsightKind::SortOnDisk));
    }

    #[test]
    fn a_lookup_that_reads_its_whole_collection_names_the_field_to_index() {
        for json in [LOOKUP_SCAN_SBE, LOOKUP_SCAN_CLASSIC] {
            let plan = parse(json).unwrap();
            let full = plan
                .insights
                .iter()
                .find(|i| i.kind == InsightKind::FullScan)
                .expect("the lookup reads all of big");
            assert_eq!(full.node, Some(plan.root.id));
            assert_eq!(full.title, "Reads all of big 2 times to look up matches");
            assert_eq!(
                full.detail,
                "An index on big.t lets each lookup find its documents directly."
            );
            // Said once: not again as documents examined for little.
            assert!(!kinds(&plan).contains(&InsightKind::DiscardedRows));
        }
        // Planned, the slot-based engine already says it will scan.
        let planned = LOOKUP_SCAN_SBE.replace("\"executionStats\"", "\"unmeasured\"");
        let planned = parse(&planned).unwrap();
        assert!(!planned.analyzed);
        assert!(
            planned
                .insights
                .iter()
                .any(|i| i.title == "Reads all of big for each document to look up matches"),
            "{:?}",
            planned.insights
        );
    }

    #[test]
    fn a_single_scan_without_its_own_counts_examined_the_whole_read() {
        // As the slot-based engine of some versions writes it: no
        // `numReads` on its scan.
        let json = GROUP_SBE.replace("\"numReads\"", "\"reads\"");
        let plan = parse(&json).unwrap();
        let scan = &plan.root.children[0];
        assert!(
            scan.details
                .contains(&("Documents examined".into(), "20,000".into()))
        );
        assert!(kinds(&plan).contains(&InsightKind::FullScan));
        // Two steps that read documents share the total: neither gets it.
        let json = LOOKUP_SCAN_SBE.replace("\"numReads\"", "\"reads\"");
        let plan = parse(&json).unwrap();
        assert!(plan.root.walk().iter().all(|node| {
            !node
                .details
                .iter()
                .any(|(label, _)| label == "Documents examined")
        }));
    }

    #[test]
    fn a_read_that_failed_while_measured_is_its_error() {
        let timed_out = parse(TIMED_OUT).unwrap_err();
        assert!(
            timed_out.contains("operation exceeded time limit"),
            "{timed_out}"
        );
        let failed = parse(FAILED).unwrap_err();
        assert_eq!(
            failed,
            "The read failed while MongoDB measured it: Failed to parse number 'x0' in \
             $convert with no onError value: Did not consume whole string. Explain shows its \
             plan without running it."
        );
        // The query under an aggregation's stages, and a shard's.
        let cursor = format!(
            r#"{{"stages": [{{"$cursor": {}}}, {{"$group": {{"_id": "$s"}}}}]}}"#,
            TIMED_OUT
        );
        assert!(parse(&cursor).is_err());
        let shard = r#"{"queryPlanner": {"winningPlan": {"stage": "SHARD_MERGE", "shards": []}},
            "executionStats": {"nReturned": 1, "executionStages": {"stage": "SHARD_MERGE",
              "shards": [{"shardName": "a", "executionSuccess": false, "errorMessage": "interrupted"}]}}}"#;
        assert!(parse(shard).unwrap_err().contains("interrupted"));
    }

    #[test]
    fn an_aggregation_takes_its_time_from_its_query() {
        let plan = parse(GROUP_CLASSIC).unwrap();
        assert_eq!(plan.execution_ms, Some(8.0));
        assert_eq!(plan.planning_ms, Some(0.0));
    }

    #[test]
    fn plans_of_other_databases_are_not_mongo_s() {
        let postgres = parse(include_str!(
            "../../fixtures/plans/orders_before_index.json"
        ));
        assert!(postgres.unwrap().estimates);
        assert!(parse(r#"{"stages": []}"#).is_err());
        assert!(parse(r#"{"ok": 1}"#).is_err());
    }
}
