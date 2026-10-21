//! SQL Server's showplan XML (`SET SHOWPLAN_XML ON`, or `SET STATISTICS XML
//! ON` for the measured plan) read into the same [`Plan`] PostgreSQL's
//! plans become.

use roxmltree::{Document, Node as Element};

use super::{Actual, Buffers, Category, Insight, InsightKind, Node, Plan, Severity};

/// Reads the first statement's plan of a showplan XML document.
pub fn parse(xml: &str) -> Result<Plan, String> {
    let document =
        Document::parse(xml.trim()).map_err(|error| format!("Not an XML plan: {error}"))?;
    let query_plan = document
        .descendants()
        .filter(|e| e.has_tag_name("QueryPlan"))
        .find(|e| child(*e, "RelOp").is_some())
        .ok_or("The XML has no query plan")?;
    let root = child(query_plan, "RelOp").ok_or("The XML has no query plan")?;
    let mut next = 0;
    let root = node(root, &mut next);
    let analyzed = root.actual.is_some();
    let mut plan = Plan {
        root,
        planning_ms: number(query_plan, "CompileTime"),
        execution_ms: descendant(query_plan, "QueryTimeStats")
            .and_then(|stats| number(stats, "ElapsedTime")),
        analyzed,
        insights: Vec::new(),
    };
    plan.insights = insights(&plan, query_plan);
    Ok(plan)
}

fn child<'a, 'i>(element: Element<'a, 'i>, name: &str) -> Option<Element<'a, 'i>> {
    element.children().find(|e| e.has_tag_name(name))
}

fn descendant<'a, 'i>(element: Element<'a, 'i>, name: &str) -> Option<Element<'a, 'i>> {
    element.descendants().find(|e| e.has_tag_name(name))
}

fn number(element: Element, name: &str) -> Option<f64> {
    element.attribute(name)?.parse().ok()
}

/// `[dbo]` → `dbo`.
fn unbracket(name: &str) -> String {
    name.trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

/// The elements of a step, leaving out the steps under it.
fn own_elements<'a, 'i>(op: Element<'a, 'i>) -> Vec<Element<'a, 'i>> {
    fn visit<'a, 'i>(element: Element<'a, 'i>, found: &mut Vec<Element<'a, 'i>>) {
        for e in element.children().filter(Element::is_element) {
            if !e.has_tag_name("RelOp") {
                found.push(e);
                visit(e, found);
            }
        }
    }
    let mut found = Vec::new();
    visit(op, &mut found);
    found
}

/// The steps right under a step: its `RelOp`s, wherever the operator's own
/// element nests them.
fn sub_steps<'a, 'i>(op: Element<'a, 'i>) -> Vec<Element<'a, 'i>> {
    fn visit<'a, 'i>(element: Element<'a, 'i>, found: &mut Vec<Element<'a, 'i>>) {
        for e in element.children().filter(Element::is_element) {
            if e.has_tag_name("RelOp") {
                found.push(e);
            } else {
                visit(e, found);
            }
        }
    }
    let mut found = Vec::new();
    visit(op, &mut found);
    found
}

/// The expression text of the first `ScalarOperator` under `element`.
fn scalar(element: Element) -> Option<String> {
    descendant(element, "ScalarOperator")
        .and_then(|e| e.attribute("ScalarString"))
        .map(str::to_string)
}

/// The column names of the `ColumnReference`s right under `element`.
fn columns(element: Element) -> Vec<String> {
    element
        .children()
        .filter(|e| e.has_tag_name("ColumnReference"))
        .filter_map(|e| e.attribute("Column"))
        .map(unbracket)
        .collect()
}

/// `total > (100.00)` for each range of an index seek.
fn seek_predicate(own: &[Element]) -> Option<String> {
    let mut parts = Vec::new();
    for range in own.iter().filter(|e| {
        ["Prefix", "StartRange", "EndRange"]
            .iter()
            .any(|n| e.has_tag_name(*n))
    }) {
        let op = match range.attribute("ScanType").unwrap_or("EQ") {
            "GT" => ">",
            "GE" => ">=",
            "LT" => "<",
            "LE" => "<=",
            "NE" => "<>",
            _ => "=",
        };
        let names = child(*range, "RangeColumns")
            .map(columns)
            .unwrap_or_default();
        let values: Vec<String> = child(*range, "RangeExpressions")
            .map(|e| {
                e.children()
                    .filter(|c| c.has_tag_name("ScalarOperator"))
                    .filter_map(|c| c.attribute("ScalarString"))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for (name, value) in names.iter().zip(&values) {
            parts.push(format!("{name} {op} {value}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

/// What a step's warnings say: a title and what to do about it, each.
fn warnings(element: Element) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for attribute in element.attributes() {
        if !matches!(attribute.value(), "1" | "true") {
            continue;
        }
        found.push(match attribute.name() {
            "NoJoinPredicate" => (
                "A join without a join condition".to_string(),
                "Every row of one side meets every row of the other; a condition is probably missing."
                    .to_string(),
            ),
            "SpillOccurred" => (
                "Spilled to tempdb".to_string(),
                "It did not get enough memory and used disk.".to_string(),
            ),
            other => (spaced(other), String::new()),
        });
    }
    for e in element.children().filter(Element::is_element) {
        let names = || {
            e.children()
                .filter(|c| c.has_tag_name("ColumnReference"))
                .filter_map(|c| c.attribute("Column"))
                .map(unbracket)
                .collect::<Vec<_>>()
                .join(", ")
        };
        found.push(match e.tag_name().name() {
            "ColumnsWithNoStatistics" => (
                format!("No statistics on {}", names()),
                "Without statistics the estimates are guesses; they are created when the query runs \
                 with automatic statistics on, or with CREATE STATISTICS."
                    .to_string(),
            ),
            "ColumnsWithStaleStatistics" => (
                format!("Out of date statistics on {}", names()),
                "UPDATE STATISTICS refreshes them.".to_string(),
            ),
            "SpillToTempDb" | "SortSpillDetails" | "HashSpillDetails" => (
                "Spilled to tempdb".to_string(),
                "It did not get enough memory and used disk.".to_string(),
            ),
            "PlanAffectingConvert" => (
                "A type conversion affects the plan".to_string(),
                format!(
                    "{}: {}. Comparing columns of the same type avoids it.",
                    e.attribute("ConvertIssue").unwrap_or("Conversion"),
                    e.attribute("Expression").unwrap_or("an expression")
                ),
            ),
            "Wait" => (
                format!("Waited on {}", e.attribute("WaitType").unwrap_or("a resource")),
                String::new(),
            ),
            "MemoryGrantWarning" => (
                "The memory grant was not right".to_string(),
                e.attribute("GrantWarningKind").unwrap_or_default().to_string(),
            ),
            other => (spaced(other), String::new()),
        });
    }
    found
}

/// `SpillToTempDb` → `Spill to temp db`.
fn spaced(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn node(op: Element, next: &mut usize) -> Node {
    let id = *next;
    *next += 1;
    let own = own_elements(op);
    let find = |name: &str| own.iter().copied().find(|e| e.has_tag_name(name));
    let physical = op.attribute("PhysicalOp").unwrap_or("Step");
    // A key lookup is a seek with `Lookup="1"` (older versions name it).
    let node_type = if find("IndexScan").is_some_and(|scan| scan.attribute("Lookup") == Some("1")) {
        "Key Lookup".to_string()
    } else {
        physical.to_string()
    };
    let object = find("Object");
    let relation = object.and_then(|o| {
        let table = unbracket(o.attribute("Table")?);
        Some(match o.attribute("Schema") {
            Some(schema) => format!("{}.{table}", unbracket(schema)),
            None => table,
        })
    });
    let index = object.and_then(|o| o.attribute("Index")).map(unbracket);

    // Measured, summed over the threads that ran it. They run side by side,
    // so the slowest sets the time.
    let counters: Vec<Element> = own
        .iter()
        .copied()
        .filter(|e| e.has_tag_name("RunTimeCountersPerThread"))
        .collect();
    let actual_total = (!counters.is_empty()).then(|| {
        let sum = |name: &str| {
            counters
                .iter()
                .filter_map(|c| number(*c, name))
                .sum::<f64>()
        };
        let elapsed = counters
            .iter()
            .filter_map(|c| number(*c, "ActualElapsedms"))
            .fold(0.0, f64::max);
        (sum("ActualRows"), sum("ActualExecutions"), elapsed)
    });
    let actual = actual_total.map(|(rows, loops, elapsed)| Actual {
        rows_per_loop: if loops > 0.0 { rows / loops } else { 0.0 },
        loops,
        startup_ms: 0.0,
        total_ms_per_loop: elapsed / loops.max(1.0),
    });

    let children: Vec<Node> = sub_steps(op)
        .into_iter()
        .map(|step| node(step, next))
        .collect();
    let total_cost = number(op, "EstimatedTotalSubtreeCost").unwrap_or(0.0);
    let children_cost: f64 = children.iter().map(|c| c.total_cost).sum();
    let inclusive_ms = actual_total.map(|(_, _, elapsed)| elapsed);
    let self_ms = inclusive_ms.map(|inclusive| {
        let children_ms: f64 = children.iter().filter_map(|c| c.inclusive_ms).sum();
        (inclusive - children_ms).max(0.0)
    });

    let mut details = Vec::new();
    let mut detail = |label: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            details.push((label.to_string(), value));
        }
    };
    detail(
        "Logical operation",
        op.attribute("LogicalOp").map(str::to_string),
    );
    detail(
        "Execution mode",
        op.attribute("EstimatedExecutionMode").map(str::to_string),
    );
    detail("Seek predicate", seek_predicate(&own));
    // The filter of a scan or a filter step, or the join's own condition.
    detail("Predicate", find("Predicate").and_then(scalar));
    detail("Join condition", find("Residual").and_then(scalar));
    let sort_key: Vec<String> = own
        .iter()
        .filter(|e| e.has_tag_name("OrderByColumn"))
        .flat_map(|e| columns(*e))
        .collect();
    detail("Sort key", Some(sort_key.join(", ")));
    let group_key = find("GroupBy").map(columns).unwrap_or_default();
    detail("Group key", Some(group_key.join(", ")));
    let step_warnings: Vec<(String, String)> = own
        .iter()
        .filter(|e| e.has_tag_name("Warnings"))
        .flat_map(|e| warnings(*e))
        .collect();
    detail(
        "Warnings",
        Some(
            step_warnings
                .iter()
                .map(|(title, _)| title.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ),
    );

    let (title, category) = describe(
        op,
        &node_type,
        relation.as_deref(),
        index.as_deref(),
        &sort_key,
        !group_key.is_empty(),
    );
    Node {
        id,
        title,
        category,
        relation,
        index,
        parent_relationship: None,
        estimated_rows: number(op, "EstimateRows").unwrap_or(0.0),
        startup_cost: 0.0,
        total_cost,
        actual,
        inclusive_ms,
        self_ms,
        self_cost: (total_cost - children_cost).max(0.0),
        rows_removed: None,
        buffers: Buffers::default(),
        details,
        node_type,
        children,
    }
}

/// A step's title in plain words, and its category.
fn describe(
    op: Element,
    node_type: &str,
    relation: Option<&str>,
    index: Option<&str>,
    sort_key: &[String],
    grouped: bool,
) -> (String, Category) {
    let table = relation.unwrap_or("the table");
    let index = index.unwrap_or("an index");
    let logical = op.attribute("LogicalOp").unwrap_or_default();
    let join = || match logical {
        "Inner Join" => "Join".to_string(),
        l if l.contains("Anti Semi") => "Keep rows that have no match".to_string(),
        l if l.contains("Semi") => "Keep rows that have a match".to_string(),
        "Left Outer Join" => "Left join".to_string(),
        "Right Outer Join" => "Right join".to_string(),
        "Full Outer Join" => "Full join".to_string(),
        _ => "Join".to_string(),
    };
    match node_type {
        "Table Scan" | "Clustered Index Scan" | "Columnstore Index Scan" => {
            (format!("Read all of {table}"), Category::Scan)
        }
        "Index Scan" => (
            format!("Read all of {table} through {index}"),
            Category::Scan,
        ),
        "Index Seek" | "Clustered Index Seek" => {
            (format!("Look up {table} through {index}"), Category::Scan)
        }
        "Key Lookup" | "RID Lookup" => (
            format!("Fetch the matching rows of {table}"),
            Category::Scan,
        ),
        "Nested Loops" => (
            format!("{}, row by row (nested loop)", join()),
            Category::Join,
        ),
        "Merge Join" => (format!("{} of two sorted inputs", join()), Category::Join),
        "Hash Match" if logical.contains("Join") => {
            (format!("{} through a hash table", join()), Category::Join)
        }
        "Hash Match" => ("Group rows with a hash table".into(), Category::Aggregate),
        "Stream Aggregate" if grouped => ("Group sorted rows".into(), Category::Aggregate),
        "Stream Aggregate" => ("Aggregate all rows".into(), Category::Aggregate),
        "Sort" | "Top N Sort" => (
            format!(
                "Sort by {}",
                if sort_key.is_empty() {
                    "a key".to_string()
                } else {
                    sort_key.join(", ")
                }
            ),
            Category::Sort,
        ),
        other => {
            let verb = ["Insert", "Update", "Delete", "Merge"]
                .into_iter()
                .find(|verb| {
                    ["Table ", "Clustered Index ", "Index "]
                        .iter()
                        .any(|prefix| other.strip_prefix(prefix) == Some(verb))
                });
            match verb {
                Some("Insert") => (format!("Insert into {table}"), Category::Modify),
                Some("Update") => (format!("Update {table}"), Category::Modify),
                Some("Delete") => (format!("Delete from {table}"), Category::Modify),
                Some(_) => (format!("Merge into {table}"), Category::Modify),
                None => (other.to_string(), Category::Other),
            }
        }
    }
}

/// The insights PostgreSQL's plans get that make sense here, then the
/// database's own: the indexes it wishes for and its warnings.
fn insights(plan: &Plan, query_plan: Element) -> Vec<Insight> {
    let mut found = super::insights(plan);
    for insight in &mut found {
        if insight.kind == InsightKind::Misestimate {
            insight.detail = format!(
                "A wrong estimate leads SQL Server to the wrong join or scan. Fresh statistics \
                 often fix it{}.",
                insight
                    .node
                    .and_then(|id| plan.node(id))
                    .and_then(|node| node.relation.as_ref())
                    .map(|r| format!(": UPDATE STATISTICS {r}"))
                    .unwrap_or_default()
            );
        }
    }
    for missing in query_plan
        .descendants()
        .filter(|e| e.has_tag_name("MissingIndex"))
    {
        let names = |usages: &[&str]| -> Vec<String> {
            missing
                .children()
                .filter(|g| usages.contains(&g.attribute("Usage").unwrap_or_default()))
                .flat_map(|g| {
                    g.children()
                        .filter_map(|c| c.attribute("Name"))
                        .map(unbracket)
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let mut keys = names(&["EQUALITY", "INEQUALITY"]);
        let include = names(&["INCLUDE"]);
        if keys.is_empty() {
            keys = include.clone();
        }
        let table = missing
            .attribute("Table")
            .map(|t| match missing.attribute("Schema") {
                Some(schema) => format!("{}.{}", unbracket(schema), unbracket(t)),
                None => unbracket(t),
            })
            .unwrap_or_else(|| "the table".into());
        let impact = missing
            .parent()
            .and_then(|group| number(group, "Impact"))
            .map(|impact| format!(" would cut this query's cost by about {impact:.0}%"))
            .unwrap_or_else(|| " would make this query cheaper".into());
        found.push(Insight {
            kind: InsightKind::MissingIndex,
            severity: Severity::Warning,
            node: None,
            title: format!("An index on {table} ({}) could help", keys.join(", ")),
            detail: format!(
                "SQL Server estimates that this index{impact}{}.",
                if include.is_empty() || keys == include {
                    String::new()
                } else {
                    format!(", with {} included", include.join(", "))
                }
            ),
        });
    }
    // The warnings of the plan, then of each step (`own_elements` keeps a
    // step's warnings from counting for the steps above it).
    let mut warn = |element: Element, node: Option<usize>| {
        for (title, detail) in warnings(element) {
            found.push(Insight {
                kind: InsightKind::Warning,
                severity: Severity::Warning,
                node,
                title,
                detail,
            });
        }
    };
    if let Some(element) = child(query_plan, "Warnings") {
        warn(element, None);
    }
    let mut ops = Vec::new();
    collect_ops(query_plan, &mut ops);
    for (id, op) in ops.into_iter().enumerate() {
        for element in own_elements(op)
            .into_iter()
            .filter(|e| e.has_tag_name("Warnings"))
        {
            warn(element, Some(id));
        }
    }
    found.sort_by_key(|insight| insight.severity);
    found
}

/// The steps under `element`, in the depth first order of the node ids.
fn collect_ops<'a, 'i>(element: Element<'a, 'i>, ops: &mut Vec<Element<'a, 'i>>) {
    for step in sub_steps(element) {
        ops.push(step);
        collect_ops(step, ops);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Category, InsightKind, parse};

    // The fixtures come from SQL Server 2022 (16.0.4305) with this setup, and
    // the query `select o.*, c.name from dbo.orders o join dbo.customers c on
    // c.id = o.customer_id where o.total > 100` under `set showplan_xml on`
    // (estimated) and `set statistics xml on` (actual), `sqlcmd -y 0`:
    //
    //   create database kuery_plans; use kuery_plans;
    //   create table dbo.customers(id int primary key, name nvarchar(40));
    //   create table dbo.orders(id int primary key, customer_id int, total decimal(10,2));
    //   insert into dbo.customers select top (2000) row_number() over (order by (select 1)),
    //     concat(N'customer ', row_number() over (order by (select 1)))
    //     from sys.all_objects a cross join sys.all_objects b;
    //   insert into dbo.orders select n, 1 + abs(checksum(newid())) % 2000,
    //     case when n % 2000 = 0 then 500 + n % 400 else n % 100 end
    //     from (select top (100000) row_number() over (order by (select 1)) n
    //           from sys.all_objects a cross join sys.all_objects b) t;
    //   create nonclustered index ix_orders_total on dbo.orders(total);
    const ESTIMATED: &str = include_str!("../../fixtures/plans/mssql_orders_estimated.xml");
    const ACTUAL: &str = include_str!("../../fixtures/plans/mssql_orders_actual.xml");

    /// A plan around one `RelOp`, for what the fixtures do not show.
    fn wrap(plan_extras: &str, relop: &str) -> String {
        format!(
            r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements><StmtSimple><QueryPlan CompileTime="7">{plan_extras}{relop}</QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#
        )
    }

    #[test]
    fn estimated_plan_is_a_tree_of_plain_steps() {
        let plan = parse(ESTIMATED).unwrap();
        assert_eq!(plan.root.category, Category::Join);
        assert!(!plan.analyzed);
        assert!(plan.root.walk().iter().all(|n| n.actual.is_none()));
        assert_eq!(plan.planning_ms, Some(110.0));
        let nodes = plan.root.walk();
        let seek = nodes
            .iter()
            .find(|n| n.node_type == "Index Seek")
            .expect("an index seek");
        assert_eq!(seek.relation.as_deref(), Some("dbo.orders"));
        assert_eq!(seek.index.as_deref(), Some("ix_orders_total"));
        assert_eq!(seek.title, "Look up dbo.orders through ix_orders_total");
        assert_eq!(seek.category, Category::Scan);
        assert_eq!(seek.estimated_rows, 50.0);
        assert!(
            seek.details
                .iter()
                .any(|(label, value)| label == "Seek predicate" && value.contains("> (100.00)"))
        );
        let lookup = nodes
            .iter()
            .find(|n| n.node_type == "Key Lookup")
            .expect("a key lookup");
        assert_eq!(lookup.title, "Fetch the matching rows of dbo.orders");
        let scan = nodes
            .iter()
            .find(|n| n.node_type == "Clustered Index Scan")
            .unwrap();
        assert_eq!(scan.title, "Read all of dbo.customers");
        assert!(nodes.iter().any(|n| n.category == Category::Sort));
        let join = nodes
            .iter()
            .find(|n| n.node_type == "Nested Loops")
            .unwrap();
        assert_eq!(join.title, "Join, row by row (nested loop)");
        assert!(
            join.details
                .iter()
                .any(|(l, v)| l == "Logical operation" && v == "Inner Join")
        );
        // Costs: the root holds everything, each step only its own part.
        assert!((plan.root.total_cost - 0.188303).abs() < 1e-9);
        let own: f64 = nodes.iter().map(|n| n.self_cost).sum();
        assert!((own - plan.root.total_cost).abs() < 1e-9);
        assert!(
            plan.insights
                .iter()
                .any(|i| i.kind == InsightKind::Estimated)
        );
    }

    #[test]
    fn actual_plan_has_measured_steps() {
        let plan = parse(ACTUAL).unwrap();
        assert!(plan.analyzed);
        assert!(plan.root.walk().iter().all(|n| n.actual.is_some()));
        assert!(plan.total_ms().is_some());
        assert_eq!(plan.root.actual_rows(), Some(50.0));
        // The key lookup ran once per row the seek found.
        let lookup = plan
            .root
            .walk()
            .into_iter()
            .find(|n| n.node_type == "Key Lookup")
            .unwrap();
        assert_eq!(lookup.actual.unwrap().loops, 50.0);
        assert_eq!(lookup.actual_rows(), Some(50.0));
        assert!(
            !plan
                .insights
                .iter()
                .any(|i| i.kind == InsightKind::Estimated)
        );
    }

    #[test]
    fn not_a_plan_is_an_error() {
        assert!(parse("<ShowPlanXML/>").is_err());
        assert!(parse("<nonsense").is_err());
    }

    #[test]
    fn postgres_plans_are_unchanged() {
        assert!(
            parse(include_str!(
                "../../fixtures/plans/orders_before_index.json"
            ))
            .is_ok()
        );
    }

    #[test]
    fn measured_numbers_add_up_over_threads() {
        let xml = wrap(
            "",
            r#"<RelOp NodeId="0" PhysicalOp="Table Scan" LogicalOp="Table Scan" EstimateRows="10" EstimatedTotalSubtreeCost="1.5"><IndexScan><Object Schema="[dbo]" Table="[t]"/><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="0" ActualElapsedms="0" ActualExecutions="1"/><RunTimeCountersPerThread Thread="1" ActualRows="30" ActualElapsedms="8" ActualExecutions="1"/><RunTimeCountersPerThread Thread="2" ActualRows="50" ActualElapsedms="9" ActualExecutions="1"/></RunTimeInformation></IndexScan></RelOp>"#,
        );
        let plan = parse(&xml).unwrap();
        let actual = plan.root.actual.unwrap();
        assert_eq!(actual.loops, 3.0);
        assert_eq!(plan.root.actual_rows(), Some(80.0));
        // Threads run side by side: the slowest sets the time.
        assert_eq!(plan.root.inclusive_ms, Some(9.0));
    }

    #[test]
    fn missing_indexes_and_warnings_become_insights() {
        let xml = wrap(
            r#"<MissingIndexes><MissingIndexGroup Impact="85.2"><MissingIndex Database="[shop]" Schema="[dbo]" Table="[orders]"><ColumnGroup Usage="EQUALITY"><Column Name="[customer_id]" ColumnId="2"/></ColumnGroup><ColumnGroup Usage="INEQUALITY"><Column Name="[created]" ColumnId="3"/></ColumnGroup><ColumnGroup Usage="INCLUDE"><Column Name="[total]" ColumnId="4"/></ColumnGroup></MissingIndex></MissingIndexGroup></MissingIndexes><Warnings><PlanAffectingConvert ConvertIssue="Seek Plan" Expression="CONVERT_IMPLICIT(int,[o].[code],0)"/></Warnings>"#,
            r#"<RelOp NodeId="0" PhysicalOp="Hash Match" LogicalOp="Aggregate" EstimateRows="1" EstimatedTotalSubtreeCost="1"><Warnings NoJoinPredicate="1"><ColumnsWithNoStatistics><ColumnReference Column="code"/></ColumnsWithNoStatistics></Warnings><Hash/></RelOp>"#,
        );
        let plan = parse(&xml).unwrap();
        assert_eq!(plan.root.category, Category::Aggregate);
        let titles: Vec<&str> = plan.insights.iter().map(|i| i.title.as_str()).collect();
        assert!(
            titles.contains(&"An index on dbo.orders (customer_id, created) could help"),
            "{titles:?}"
        );
        let index = plan
            .insights
            .iter()
            .find(|i| i.kind == InsightKind::MissingIndex)
            .unwrap();
        assert!(index.detail.contains("85%") && index.detail.contains("total"));
        let warnings: Vec<_> = plan
            .insights
            .iter()
            .filter(|i| i.kind == InsightKind::Warning)
            .collect();
        assert_eq!(warnings.len(), 3, "{titles:?}");
        assert_eq!(
            warnings.iter().filter(|w| w.node == Some(0)).count(),
            2,
            "step warnings point at the step"
        );
    }

    #[test]
    fn modifying_steps_are_modifies() {
        let xml = wrap(
            "",
            r#"<RelOp NodeId="0" PhysicalOp="Clustered Index Delete" LogicalOp="Delete" EstimateRows="1" EstimatedTotalSubtreeCost="1"><Update><Object Schema="[dbo]" Table="[orders]" Index="[PK_orders]"/></Update></RelOp>"#,
        );
        let plan = parse(&xml).unwrap();
        assert_eq!(plan.root.category, Category::Modify);
        assert_eq!(plan.root.title, "Delete from dbo.orders");
    }
}
