//! An edited document as the smallest change to the one that was loaded:
//! the leaves that changed are set, the fields that went are removed (never
//! set to null), and each touched field carries its earlier value so the
//! driver applies the change only to a document that still holds it.
//!
//! An array that changed is set whole: an element's position is not an
//! identity a concurrent edit keeps. Moving fields around without changing
//! them is not a change.

use std::fmt;

use crate::document::change::DocChange;
use crate::document::value::{Document, FieldPath, PathPart, Value};

/// Why an edit cannot become a change.
#[derive(Debug, Clone, PartialEq)]
pub enum DiffError {
    /// The loaded document has no `_id` to find it by.
    NoKey,
    /// The edit changed `_id`, which would be another document.
    KeyChanged,
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffError::NoKey => f.write_str("This document has no _id to find it by"),
            DiffError::KeyChanged => {
                f.write_str("The _id cannot change: duplicate the document instead")
            }
        }
    }
}

/// The update that turns `before` into `after`; `None` when nothing
/// changed.
pub fn diff(before: &Document, after: &Document) -> Result<Option<DocChange>, DiffError> {
    let key = before.key().ok_or(DiffError::NoKey)?;
    match after.get("_id") {
        Some(id) if same(id, &key.0) => {}
        _ => return Err(DiffError::KeyChanged),
    }
    let mut changes = Changes::default();
    changes.compare(&FieldPath(Vec::new()), before, after);
    if changes.set.is_empty() && changes.unset.is_empty() {
        return Ok(None);
    }
    let expect = changes
        .set
        .iter()
        .map(|(path, _)| path)
        .chain(&changes.unset)
        .map(|path| (path.clone(), before.get_path(path).cloned()))
        .collect();
    Ok(Some(DocChange::Update {
        key,
        version: None,
        set: changes.set,
        unset: changes.unset,
        expect,
    }))
}

#[derive(Default)]
struct Changes {
    set: Vec<(FieldPath, Value)>,
    unset: Vec<FieldPath>,
}

impl Changes {
    fn compare(&mut self, at: &FieldPath, before: &Document, after: &Document) {
        for (name, new) in &after.0 {
            let path = at.child(PathPart::Field(name.clone()));
            match (before.get(name), new) {
                (Some(Value::Object(old)), Value::Object(new)) => self.compare(&path, old, new),
                (Some(old), new) if same(old, new) => {}
                _ => self.set.push((path, new.clone())),
            }
        }
        for (name, _) in &before.0 {
            if after.get(name).is_none() {
                self.unset.push(at.child(PathPart::Field(name.clone())));
            }
        }
    }
}

/// Equal values of the same type, with NaN equal to itself (engines match
/// it so) and fields in the same order.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits() || a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.0.len() == b.0.len()
                && a.0
                    .iter()
                    .zip(&b.0)
                    .all(|((name_a, a), (name_b, b))| name_a == name_b && same(a, b))
        }
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::value::DocKey;

    fn doc(fields: Vec<(&str, Value)>) -> Document {
        Document(
            fields
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        )
    }

    fn order() -> Document {
        doc(vec![
            ("_id", Value::Int32(1)),
            ("total", Value::Decimal("9.99".into())),
            ("qty", Value::Int64(2)),
            (
                "address",
                Value::Object(doc(vec![
                    ("city", Value::String("Lima".into())),
                    ("zip", Value::String("15001".into())),
                ])),
            ),
            ("tags", Value::Array(vec![Value::String("a".into())])),
            ("ratio", Value::Double(f64::NAN)),
        ])
    }

    #[test]
    fn sets_changed_leaves_and_unsets_removed_fields() {
        let before = order();
        let mut after = before.clone();
        let mut change = |name: &str, value: Value| {
            let field = after.0.iter_mut().find(|(field, _)| field == name).unwrap();
            field.1 = value;
        };
        // A nested leaf changed and another removed, an array grown, a
        // field removed and one added.
        change(
            "address",
            Value::Object(doc(vec![("city", Value::String("Cusco".into()))])),
        );
        change(
            "tags",
            Value::Array(vec![Value::String("a".into()), Value::String("b".into())]),
        );
        after.0.retain(|(name, _)| name != "qty");
        after.0.push(("coupon".into(), Value::Null));

        let Some(DocChange::Update {
            key,
            set,
            unset,
            expect,
            ..
        }) = diff(&before, &after).unwrap()
        else {
            panic!("expected an update");
        };
        assert_eq!(key, DocKey(Value::Int32(1)));
        let set: Vec<String> = set.iter().map(|(path, _)| path.dotted()).collect();
        assert_eq!(set, ["address.city", "tags", "coupon"]);
        let unset: Vec<String> = unset.iter().map(FieldPath::dotted).collect();
        assert_eq!(unset, ["address.zip", "qty"]);
        let expect: Vec<(String, Option<Value>)> = expect
            .into_iter()
            .map(|(path, value)| (path.dotted(), value))
            .collect();
        assert_eq!(
            expect,
            [
                ("address.city".into(), Some(Value::String("Lima".into()))),
                (
                    "tags".into(),
                    Some(Value::Array(vec![Value::String("a".into())]))
                ),
                // It was absent, which the check must say.
                ("coupon".into(), None),
                ("address.zip".into(), Some(Value::String("15001".into()))),
                ("qty".into(), Some(Value::Int64(2))),
            ]
        );
    }

    #[test]
    fn a_type_change_is_a_change_and_nothing_is_none() {
        let before = order();
        assert_eq!(diff(&before, &before.clone()), Ok(None));

        let mut after = before.clone();
        // NumberLong(2) edited to a plain 2.
        after.0[2].1 = Value::Int32(2);
        let Some(DocChange::Update { set, .. }) = diff(&before, &after).unwrap() else {
            panic!("expected an update");
        };
        assert_eq!(set, [(FieldPath::field("qty"), Value::Int32(2))]);
    }

    #[test]
    fn the_key_must_stay() {
        let before = order();
        let mut after = before.clone();
        after.0[0].1 = Value::Int32(2);
        assert_eq!(diff(&before, &after), Err(DiffError::KeyChanged));
        after.0.remove(0);
        assert_eq!(diff(&before, &after), Err(DiffError::KeyChanged));
        let keyless = doc(vec![("a", Value::Int32(1))]);
        assert_eq!(diff(&keyless, &keyless), Err(DiffError::NoKey));
    }
}
