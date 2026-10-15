use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemberProfile {
    pub member_id: i64,
    pub nickname: String,
    pub description: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PostKind {
    /// A short post without a title.
    Post,
    /// A long-form article with a title.
    Article,
    /// A post that quotes another one.
    Quote,
}

impl PostKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Post => "Post",
            Self::Article => "Article",
            Self::Quote => "Quote",
        }
    }
}

/// One of a member's own posts; `topic_id` is the topic a reply goes under.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MemberPost {
    pub topic_id: i64,
    pub kind: PostKind,
    /// Empty for a post without a title.
    pub title: String,
    pub text: String,
    pub published_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PostedReply {
    pub reply_id: i64,
    pub topic_id: i64,
    pub body: String,
    pub created_at: String,
}

/// The failure of a reply that left but got no definite answer: it may already be public.
pub fn reply_outcome_unknown(reason: impl fmt::Display) -> Error {
    Error::network(format!(
        "reply outcome unknown ({reason}); it may already be public, so check the post before replying again"
    ))
}

#[async_trait]
pub trait CommunityApi: Send + Sync {
    async fn profile(&self, member_id: i64) -> Result<MemberProfile>;
    /// The member's newest posts, newest first, up to a count the provider keeps fixed.
    async fn posts(&self, member_id: i64) -> Result<Vec<MemberPost>>;
    /// Posts a plain-text top-level reply under a topic as the logged-in user.
    async fn reply(&self, topic_id: i64, body: &str) -> Result<PostedReply>;
}
