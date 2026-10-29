//! Runs a console statement's command and reads its answer: documents for
//! the sink, counts for the outcome.

use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::document::adapter::{DocumentSink, Outcome};
use kuery_core::session::QueryError;

use crate::command::{Built, Reply, Write};
use crate::{Mongo, convert};

impl Mongo {
    /// Runs a built command; a cursor's documents go to `sink` as they
    /// arrive.
    pub(crate) fn run_built(
        &mut self,
        built: Built,
        sink: &mut dyn DocumentSink,
    ) -> Result<Outcome, QueryError> {
        let Built {
            database,
            command,
            reply,
        } = built;
        match reply {
            Reply::Cursor => {
                let returned = self.cursor_into(&database, command, sink)?;
                Ok(Outcome {
                    returned,
                    ..Outcome::default()
                })
            }
            Reply::Counted => {
                let mut n = Bson::Int32(0);
                self.cursor(&database, command, &mut |batch| {
                    if let Some(counted) = batch.first().and_then(|first| first.get("n")) {
                        n = counted.clone();
                    }
                    Ok(())
                })?;
                send(sink, vec![doc! { "count": n }])
            }
            reply => {
                let answer = self.command(&database, command)?;
                failed(&answer)?;
                match reply {
                    Reply::Write(write) => Ok(written(write, &answer)),
                    Reply::FindAndModify => {
                        let outcome = found_and_modified(&answer);
                        let documents = match answer.get("value") {
                            Some(Bson::Document(document)) => vec![document.clone()],
                            _ => Vec::new(),
                        };
                        let returned = documents.len() as u64;
                        sink.documents(documents.iter().map(convert::document).collect())?;
                        Ok(Outcome {
                            returned,
                            ..outcome
                        })
                    }
                    reply => send(sink, documents(&reply, answer)),
                }
            }
        }
    }
}

fn send(sink: &mut dyn DocumentSink, documents: Vec<BsonDocument>) -> Result<Outcome, QueryError> {
    let returned = documents.len() as u64;
    sink.documents(documents.iter().map(convert::document).collect())?;
    Ok(Outcome {
        returned,
        ..Outcome::default()
    })
}

/// The documents an answer holds, as the reply says.
pub(crate) fn documents(reply: &Reply, mut answer: BsonDocument) -> Vec<BsonDocument> {
    let array = |answer: &BsonDocument, field: &str| -> Vec<Bson> {
        answer
            .get_array(field)
            .map(|items| items.to_vec())
            .unwrap_or_default()
    };
    match reply {
        Reply::Listed(field) => array(&answer, field)
            .into_iter()
            .filter_map(|item| match item {
                Bson::Document(document) => Some(document),
                _ => None,
            })
            .collect(),
        Reply::Values(field) => array(&answer, "values")
            .into_iter()
            .map(|value| doc! { field.as_str(): value })
            .collect(),
        Reply::Count => vec![doc! { "count": answer.get("n").cloned().unwrap_or(Bson::Int32(0)) }],
        _ => {
            // The cluster's bookkeeping, which says nothing about the result.
            for field in ["$clusterTime", "operationTime"] {
                answer.remove(field);
            }
            vec![answer]
        }
    }
}

/// A write the server answered `ok` to can still have failed, document by
/// document: the first failure is the error.
pub(crate) fn failed(answer: &BsonDocument) -> Result<(), QueryError> {
    let first = answer
        .get_array("writeErrors")
        .ok()
        .and_then(|errors| errors.first())
        .and_then(Bson::as_document)
        .or_else(|| answer.get_document("writeConcernError").ok());
    let Some(error) = first else {
        return Ok(());
    };
    let code = error.get("code").and_then(crate::catalog::number);
    Err(QueryError::Database {
        code: code.map(|code| code.to_string()).unwrap_or_default(),
        message: error.get_str("errmsg").unwrap_or("The write failed").into(),
    })
}

/// What an `insert`, `update` or `delete` did.
pub(crate) fn written(write: Write, answer: &BsonDocument) -> Outcome {
    let count = |field: &str| {
        answer
            .get(field)
            .and_then(crate::catalog::number)
            .unwrap_or(0)
    };
    let upserted = answer
        .get_array("upserted")
        .map_or(0, |upserted| upserted.len() as u64);
    match write {
        Write::Insert => Outcome {
            inserted: count("n"),
            ..Outcome::default()
        },
        Write::Update => Outcome {
            matched: count("n").saturating_sub(upserted),
            modified: count("nModified"),
            upserted,
            ..Outcome::default()
        },
        Write::Delete => Outcome {
            deleted: count("n"),
            ..Outcome::default()
        },
    }
}

/// What a `findAndModify` did, from its `lastErrorObject`.
pub(crate) fn found_and_modified(answer: &BsonDocument) -> Outcome {
    let last = answer
        .get_document("lastErrorObject")
        .cloned()
        .unwrap_or_default();
    let n = last.get("n").and_then(crate::catalog::number).unwrap_or(0);
    let upserted = u64::from(last.contains_key("upserted"));
    match last.get_bool("updatedExisting") {
        Ok(updated) => Outcome {
            matched: n - upserted.min(n),
            modified: u64::from(updated) * n,
            upserted,
            ..Outcome::default()
        },
        // Only a removal leaves `updatedExisting` out.
        Err(_) => Outcome {
            matched: n,
            deleted: n,
            ..Outcome::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_writes_did() {
        let update =
            doc! { "n": 3, "nModified": 2, "upserted": [{ "index": 0, "_id": 7 }], "ok": 1.0 };
        assert_eq!(
            written(Write::Update, &update),
            Outcome {
                matched: 2,
                modified: 2,
                upserted: 1,
                ..Outcome::default()
            }
        );
        assert_eq!(written(Write::Insert, &doc! { "n": 2 }).inserted, 2);
        assert_eq!(written(Write::Delete, &doc! { "n": 4 }).deleted, 4);
        assert_eq!(
            failed(
                &doc! { "n": 0, "writeErrors": [{ "index": 0, "code": 11000, "errmsg": "E11000 duplicate key" }] }
            ),
            Err(QueryError::Database {
                code: "11000".into(),
                message: "E11000 duplicate key".into()
            })
        );
        assert_eq!(failed(&doc! { "n": 1, "ok": 1.0 }), Ok(()));
        let removed = doc! { "lastErrorObject": { "n": 1 }, "value": { "_id": 1 } };
        assert_eq!(found_and_modified(&removed).deleted, 1);
        let updated = doc! { "lastErrorObject": { "n": 1, "updatedExisting": true } };
        assert_eq!(found_and_modified(&updated).modified, 1);
        let inserted =
            doc! { "lastErrorObject": { "n": 1, "updatedExisting": false, "upserted": 5 } };
        assert_eq!(found_and_modified(&inserted).upserted, 1);
    }

    #[test]
    fn reads_documents_out_of_answers() {
        let databases = doc! { "databases": [{ "name": "shop" }, { "name": "admin" }], "ok": 1.0 };
        assert_eq!(
            documents(&Reply::Listed("databases"), databases),
            [doc! { "name": "shop" }, doc! { "name": "admin" }]
        );
        assert_eq!(
            documents(
                &Reply::Values("status".into()),
                doc! { "values": ["a", "b"] }
            ),
            [doc! { "status": "a" }, doc! { "status": "b" }]
        );
        assert_eq!(
            documents(&Reply::Count, doc! { "n": 7_i64, "ok": 1.0 }),
            [doc! { "count": 7_i64 }]
        );
        assert_eq!(
            documents(
                &Reply::Answer,
                doc! { "ok": 1.0, "$clusterTime": {}, "operationTime": bson::Timestamp { time: 1, increment: 1 } }
            ),
            [doc! { "ok": 1.0 }]
        );
    }
}
