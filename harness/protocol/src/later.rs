//! Values a later Tickloop version wrote with a tag this version does not know. Such a value is
//! kept as written, so the record holding it still loads and writes back unchanged, and nothing
//! acts on it.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a tagged enum's records say of their own tags.
pub trait TaggedRecord {
    /// The field naming the variant.
    const TAG: &'static str;
    /// Every tag this version knows.
    const KNOWN: &'static [&'static str];

    /// Whether `value`, whose tag is known, holds a value only a later version knows, so it is
    /// kept like an unknown tag instead of failing.
    fn holds_later_value(_value: &Value) -> bool {
        false
    }
}

/// A variant of `E` that a later version wrote: its tag and its whole JSON. A known tag that does
/// not parse is a broken record, not a later one.
pub struct LaterVariant<E> {
    tag: String,
    value: Value,
    record: PhantomData<fn() -> E>,
}

impl<E> LaterVariant<E> {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// The record as the later version wrote it.
    pub fn value(&self) -> &Value {
        &self.value
    }
}

impl<E> Clone for LaterVariant<E> {
    fn clone(&self) -> Self {
        Self {
            tag: self.tag.clone(),
            value: self.value.clone(),
            record: PhantomData,
        }
    }
}

impl<E> std::fmt::Debug for LaterVariant<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaterVariant")
            .field("tag", &self.tag)
            .field("value", &self.value)
            .finish()
    }
}

impl<E> PartialEq for LaterVariant<E> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<E> Eq for LaterVariant<E> {}

impl<E> Serialize for LaterVariant<E> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, E: TaggedRecord> Deserialize<'de> for LaterVariant<E> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let tag = value[E::TAG]
            .as_str()
            .ok_or_else(|| serde::de::Error::custom(format!("a record has a {}", E::TAG)))?
            .to_owned();
        if E::KNOWN.contains(&tag.as_str()) && !E::holds_later_value(&value) {
            return Err(serde::de::Error::custom(format!("invalid {tag} record")));
        }
        Ok(Self {
            tag,
            value,
            record: PhantomData,
        })
    }
}

/// Whether `value` is a string that does not name a variant of the unit enum `E`.
pub fn names_later_variant<E: serde::de::DeserializeOwned>(value: &Value) -> bool {
    value.is_string() && serde_json::from_value::<E>(value.clone()).is_err()
}
