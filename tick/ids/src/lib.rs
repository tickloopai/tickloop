//! Tickloop's own ids: every one is a UUID v7 generated here, typed by the kind of object it
//! names. Storage keeps the hyphenated UUID; users and links read the 22-character base62 form
//! ([`Id::public`]); the model reads whatever its Loop's [`IdContext`] writes and passes ids back
//! as [`ModelRef`]s that only the context resolves. Ids that other systems assign (Herdr panes,
//! provider tool calls, Artifact assets) are not Tickloop ids and never pass through here.

pub mod base62;

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// How much of a value the model passed is echoed back in an error.
const ECHO_LIMIT: usize = 64;

/// A kind of object with its own id type.
pub trait Kind: 'static {
    /// How errors name one object of this kind, such as `scheduled task`.
    const NOUN: &'static str;
    /// Where the model finds ids of this kind, as a sentence such as
    /// `CronList gives each task's id.`
    const SOURCE: &'static str = "";
    /// Written before the id in model text; optional when the model passes the id back.
    const MODEL_PREFIX: &'static str = "";
}

/// The id of one object of kind `K`.
pub struct Id<K> {
    uuid: Uuid,
    kind: PhantomData<fn() -> K>,
}

impl<K> Id<K> {
    /// A new UUID v7: the only way Tickloop creates an object id.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::from_uuid(generate())
    }

    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self {
            uuid,
            kind: PhantomData,
        }
    }

    pub const fn nil() -> Self {
        Self::from_uuid(Uuid::nil())
    }

    /// The UUID, whose `Display` is the hyphenated storage form (file and socket names, logs).
    pub const fn uuid(self) -> Uuid {
        self.uuid
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.uuid.as_bytes()
    }

    /// The form users and links read: 22 base62 characters. Model text goes through
    /// [`IdContext::model_text`] instead.
    pub fn public(self) -> String {
        base62::encode(self.uuid)
    }

    /// Either text form ([`base62::decode_either`]), with or without the kind's model prefix.
    /// Stateless; ids the model passes back resolve through [`IdContext::resolve`].
    pub fn parse(text: &str) -> Option<Self>
    where
        K: Kind,
    {
        let text = text.strip_prefix(K::MODEL_PREFIX).unwrap_or(text);
        base62::decode_either(text).map(Self::from_uuid)
    }

    /// The same UUID as an id of another kind, where one object is named in two roles.
    pub const fn cast<Other>(self) -> Id<Other> {
        Id::from_uuid(self.uuid)
    }
}

/// A fresh UUID v7 for values that only correlate requests, menus or connections and never name
/// an object to the model or a user. Object ids use [`Id::new`].
pub fn generate() -> Uuid {
    Uuid::now_v7()
}

/// 32 random lowercase hex characters for secrets and unguessable names (OAuth state, staged
/// file names). Unlike a v7 id, every character is random, so any prefix of it is too.
pub fn random_token() -> String {
    Uuid::new_v4().simple().to_string()
}

impl<K> Clone for Id<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Id<K> {}

impl<K> PartialEq for Id<K> {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl<K> Eq for Id<K> {}

impl<K> PartialOrd for Id<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for Id<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.uuid.cmp(&other.uuid)
    }
}

impl<K> Hash for Id<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.uuid.hash(state);
    }
}

impl<K> Default for Id<K> {
    fn default() -> Self {
        Self::nil()
    }
}

impl<K> fmt::Debug for Id<K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Id({})", self.uuid)
    }
}

impl<K> From<Uuid> for Id<K> {
    fn from(uuid: Uuid) -> Self {
        Self::from_uuid(uuid)
    }
}

impl<K> From<Id<K>> for Uuid {
    fn from(id: Id<K>) -> Self {
        id.uuid
    }
}

/// Stored as the hyphenated UUID, unchanged from before typed ids.
impl<K> Serialize for Id<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.uuid.serialize(serializer)
    }
}

/// Reads either text form, and any form `Uuid` itself reads.
impl<'de, K> Deserialize<'de> for Id<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            return Uuid::deserialize(deserializer).map(Self::from_uuid);
        }
        let text = String::deserialize(deserializer)?;
        base62::decode_either(&text)
            .or_else(|| Uuid::try_parse(&text).ok())
            .map(Self::from_uuid)
            .ok_or_else(|| {
                serde::de::Error::invalid_value(
                    serde::de::Unexpected::Str(&text),
                    &"a base62 id or a UUID",
                )
            })
    }
}

/// An id of kind `K` exactly as the model wrote it in a tool argument. Only
/// [`IdContext::resolve`] turns it into an [`Id`].
pub struct ModelRef<K> {
    text: String,
    kind: PhantomData<fn() -> K>,
}

impl<K> ModelRef<K> {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: PhantomData,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl<K> Clone for ModelRef<K> {
    fn clone(&self) -> Self {
        Self::new(self.text.clone())
    }
}

impl<K> PartialEq for ModelRef<K> {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl<K> Eq for ModelRef<K> {}

impl<K> fmt::Debug for ModelRef<K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ModelRef({:?})", self.text)
    }
}

impl<K> From<&str> for ModelRef<K> {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl<K> From<String> for ModelRef<K> {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl<K> Serialize for ModelRef<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de, K> Deserialize<'de> for ModelRef<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Why a [`ModelRef`] names no id. Its text is the error the model reads.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The text is no id of the kind in any accepted form.
    Malformed { message: String },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ResolveError {}

/// The ids of one Loop as the model reads and writes them. Every id written into model text and
/// every id the model passes back goes through the Loop's context, so the form the model sees can
/// change here alone. It holds nothing yet: model text is the base62 form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdContext {
    _state: (),
}

impl IdContext {
    pub const fn new() -> Self {
        Self { _state: () }
    }

    /// `id` as written into a tool result, notification, injected message or host state. Written
    /// once when the content is produced and kept with it in the Transcript.
    pub fn model_text<K: Kind>(&self, id: Id<K>) -> String {
        format!("{}{}", K::MODEL_PREFIX, base62::encode(id.uuid))
    }

    /// The id `reference` names: base62 or a hyphenated UUID, with or without the kind's model
    /// prefix.
    pub fn resolve<K: Kind>(&self, reference: &ModelRef<K>) -> Result<Id<K>, ResolveError> {
        Id::parse(reference.as_str().trim()).ok_or_else(|| ResolveError::Malformed {
            message: not_found::<K>(reference.as_str()),
        })
    }

    /// The error for an object that exists but cannot take the request, such as one already
    /// stopped or deleted: `<Noun> <id> <state>`, with `state` such as `is not running`.
    pub fn state_error<K: Kind>(&self, id: Id<K>, state: &str) -> String {
        let mut noun = K::NOUN.chars();
        let noun = noun
            .next()
            .map(|first| first.to_uppercase().chain(noun).collect::<String>())
            .unwrap_or_default();
        format!("{noun} {} {state}", self.model_text(id))
    }
}

/// The error for a value the model passed that names no object of kind `K`, malformed or
/// unknown alike: the value as written (cut at 64 characters) and where the right id comes from.
pub fn not_found<K: Kind>(input: &str) -> String {
    let mut echoed: String = input.chars().take(ECHO_LIMIT).collect();
    if input.chars().nth(ECHO_LIMIT).is_some() {
        echoed.push('…');
    }
    let message = format!("No {} \"{echoed}\" found.", K::NOUN);
    if K::SOURCE.is_empty() {
        message
    } else {
        format!("{message} {}", K::SOURCE)
    }
}
