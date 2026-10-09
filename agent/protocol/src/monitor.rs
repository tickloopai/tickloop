use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorMemberSource {
    /// This Chat opened the instance; the Chat owns its lifecycle.
    Opened,
    /// The user added an existing instance; ownership and permissions stay where they were.
    Added,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MonitorMember {
    pub instance: InstanceRef,
    pub source: MonitorMemberSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The most handled marks a Chat keeps; the oldest go first, since an instance handled that long
/// ago has long left Monitor or shows its later updates as unread anyway.
const MAX_HANDLED_MARKS: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MonitorMembership {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MonitorMember>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub removed: BTreeSet<InstanceRef>,
    /// When this Chat last handled each instance's updates, in Unix milliseconds: an Agent turn
    /// took them in, or the user opened the instance's row. A later update is unread.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub handled: BTreeMap<InstanceRef, u64>,
}

impl MonitorMembership {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.removed.is_empty() && self.handled.is_empty()
    }

    pub fn is_removed(&self, instance: &InstanceRef) -> bool {
        self.removed.contains(instance)
    }

    /// What a Branch or copied Chat starts with: no opened or added Ticks, because a new Chat
    /// owns nothing, but the same removals, so copied Loop subscriptions do not bring back rows
    /// the user removed.
    pub fn for_new_chat(&self) -> Self {
        Self {
            members: Vec::new(),
            removed: self.removed.clone(),
            handled: self.handled.clone(),
        }
    }

    /// When this Chat last handled the instance's updates.
    pub fn handled_at(&self, instance: &InstanceRef) -> Option<u64> {
        self.handled.get(instance).copied()
    }

    /// Records that this Chat handled the instance's updates at `at_ms`; an earlier time never
    /// replaces a later one. Returns whether the mark moved.
    pub fn mark_handled(&mut self, instance: InstanceRef, at_ms: u64) -> bool {
        let mark = self.handled.entry(instance).or_default();
        if *mark >= at_ms {
            return false;
        }
        *mark = at_ms;
        while self.handled.len() > MAX_HANDLED_MARKS {
            let Some(oldest) = self
                .handled
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(instance, _)| instance.clone())
            else {
                break;
            };
            self.handled.remove(&oldest);
        }
        true
    }

    /// Records a verified open. A removal mark stays: only an explicit add brings a row back.
    pub fn record_opened(&mut self, instance: InstanceRef, title: Option<String>) {
        self.upsert(instance, title).source = MonitorMemberSource::Opened;
    }

    /// An explicit add clears the removal mark and keeps an earlier Opened source.
    pub fn add(&mut self, instance: InstanceRef, title: Option<String>) {
        self.removed.remove(&instance);
        self.upsert(instance, title);
    }

    pub fn remove(&mut self, instance: InstanceRef) {
        self.removed.insert(instance);
    }

    fn upsert(&mut self, instance: InstanceRef, title: Option<String>) -> &mut MonitorMember {
        let index = match self
            .members
            .iter()
            .position(|member| member.instance == instance)
        {
            Some(index) => index,
            None => {
                self.members.push(MonitorMember {
                    instance,
                    source: MonitorMemberSource::Added,
                    title: None,
                });
                self.members.len() - 1
            }
        };
        let member = &mut self.members[index];
        if title.is_some() {
            member.title = title;
        }
        member
    }
}
