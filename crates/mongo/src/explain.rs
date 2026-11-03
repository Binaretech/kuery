//! A console statement's plan, from the server's `explain` command: what
//! the planner chose (`queryPlanner`), or also what each step did when the
//! read ran (`executionStats`). A write is only ever planned: the explain
//! of a write with statistics changes nothing on the server, but Kuery
//! never asks for it, so a write's plan never runs it. The answer goes back
//! as relaxed Extended JSON, which [`kuery_core::plan::parse`] reads.

use bson::{Bson, doc};
use kuery_core::analysis::StatementKind;
use kuery_core::document::value::Value;
use kuery_core::session::QueryError;

use crate::shell::{Cursor, Method, Op};
use crate::{Mongo, classify, command};

impl Mongo {
    /// The plan of `statement` (see the module): measured only when
    /// `execute` and it reads, whatever the session checked.
    pub(crate) fn explain_statement(
        &mut self,
        statement: &str,
        execute: bool,
    ) -> Result<String, QueryError> {
        let op = crate::one(statement)?;
        let info = classify::classify(&op);
        if !info.explainable {
            return Err(QueryError::Explain(format!(
                "{} has no plan to show",
                info.summary
            )));
        }
        if execute && info.kind != StatementKind::Read {
            return Err(QueryError::Explain(format!(
                "Measuring a plan runs the statement, and {} writes: explain it without \
                 measuring, which never runs it",
                info.summary
            )));
        }
        let Some(built) = command::build(&planned(op), &self.database)? else {
            return Err(QueryError::Explain(
                "There is no statement to explain".into(),
            ));
        };
        let mut command = doc! {
            "explain": built.command,
            "verbosity": if execute { "executionStats" } else { "queryPlanner" },
        };
        if execute && !self.limits.max_time.is_zero() {
            let millis = i64::try_from(self.limits.max_time.as_millis()).unwrap_or(i64::MAX);
            command.insert("maxTimeMS", millis);
        }
        let mut answer = self.command(&built.database, command)?;
        // The cluster's clock, not the plan.
        answer.remove("$clusterTime");
        answer.remove("operationTime");
        Ok(Bson::Document(answer).into_relaxed_extjson().to_string())
    }
}

/// The statement without an explain of its own (`.explain()`, or an
/// aggregation's `{explain: true}`): Kuery says how much to explain.
fn planned(op: Op) -> Op {
    match op {
        Op::Collection {
            target,
            method,
            mut args,
            mut chain,
        } => {
            chain.retain(|(cursor, _)| *cursor != Cursor::Explain);
            if method == Method::Aggregate
                && let Some(Value::Object(options)) = args.get_mut(1)
            {
                options.0.retain(|(key, _)| key != "explain");
            }
            Op::Collection {
                target,
                method,
                args,
                chain,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell;

    #[test]
    fn explains_the_statement_without_its_own_explain() {
        let op = |text: &str| shell::statements(text).unwrap().remove(0).1;
        assert_eq!(
            planned(op(
                "db.orders.find({a: 1}).sort({b: 1}).explain('executionStats')"
            )),
            op("db.orders.find({a: 1}).sort({b: 1})")
        );
        assert_eq!(
            planned(op("db.orders.explain().deleteMany({a: 1})")),
            op("db.orders.deleteMany({a: 1})")
        );
        assert_eq!(
            planned(op(
                "db.orders.aggregate([{$match: {a: 1}}], {explain: true, allowDiskUse: true})"
            )),
            op("db.orders.aggregate([{$match: {a: 1}}], {allowDiskUse: true})")
        );
        assert_eq!(planned(op("show dbs")), op("show dbs"));
    }
}
