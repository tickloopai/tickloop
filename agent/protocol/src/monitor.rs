use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;

/// An instance this Chat opened; the Chat owns its lifecycle.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MonitorMember {
    pub instance: InstanceRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The most handled marks a Chat keeps, and the most used marks; the oldest go first, since an
/// instance handled or used that long ago has long left Monitor or shows its later updates as
/// unread anyway.
const MAX_MARKS: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MonitorMembership {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MonitorMember>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub removed: BTreeSet<InstanceRef>,
    /// When this Chat last handled each instance's updates, in Unix milliseconds: an Agent turn
    /// took them in or read the instance with a tool, the user opened the instance's row, or the
    /// instance joined Monitor. A later update is unread.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub handled: BTreeMap<InstanceRef, u64>,
    /// When an Agent turn of this Chat last took in each instance's updates, in Unix
    /// milliseconds: a subscription delivery, a completion notification or a claimed feedback
    /// request; its instances are among those `handled` marks.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub taken_in: BTreeMap<InstanceRef, u64>,
    /// When an Agent turn of this Chat last read or operated each instance with a tool and the call
    /// succeeded, in Unix milliseconds, whether or not the instance had updates to handle.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub used: BTreeMap<InstanceRef, u64>,
}

impl MonitorMembership {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
            && self.removed.is_empty()
            && self.handled.is_empty()
            && self.taken_in.is_empty()
            && self.used.is_empty()
    }

    pub fn is_removed(&self, instance: &InstanceRef) -> bool {
        self.removed.contains(instance)
    }

    /// What a Branch or copied Chat starts with: no opened Ticks, because a new Chat owns
    /// nothing, and no uses, because its Agent has used nothing yet, but the same removals, so
    /// copied Loop subscriptions do not bring back rows the user removed.
    pub fn for_new_chat(&self) -> Self {
        Self {
            members: Vec::new(),
            removed: self.removed.clone(),
            handled: self.handled.clone(),
            taken_in: self.taken_in.clone(),
            used: BTreeMap::new(),
        }
    }

    /// When this Chat last handled the instance's updates.
    pub fn handled_at(&self, instance: &InstanceRef) -> Option<u64> {
        self.handled.get(instance).copied()
    }

    /// When an Agent turn of this Chat last took in the instance's updates.
    pub fn taken_in_at(&self, instance: &InstanceRef) -> Option<u64> {
        self.taken_in.get(instance).copied()
    }

    /// When an Agent turn of this Chat last read or operated the instance with a tool.
    pub fn used_at(&self, instance: &InstanceRef) -> Option<u64> {
        self.used.get(instance).copied()
    }

    /// Records that this Chat handled the instance's updates at `at_ms` without an Agent turn
    /// taking them in; an earlier time never replaces a later one. Returns whether the mark moved.
    pub fn mark_handled(&mut self, instance: InstanceRef, at_ms: u64) -> bool {
        let mark = self.handled.entry(instance).or_default();
        if *mark >= at_ms {
            return false;
        }
        *mark = at_ms;
        while self.handled.len() > MAX_MARKS {
            let Some(oldest) = oldest_mark(&self.handled) else {
                break;
            };
            self.handled.remove(&oldest);
            self.taken_in.remove(&oldest);
        }
        true
    }

    /// Records that an Agent turn of this Chat took in the instance's updates at `at_ms`, which
    /// also handles them. Returns whether the handled mark moved.
    pub fn mark_taken_in(&mut self, instance: InstanceRef, at_ms: u64) -> bool {
        if !self.mark_handled(instance.clone(), at_ms) {
            return false;
        }
        self.taken_in.insert(instance, at_ms);
        true
    }

    /// Records that an Agent turn of this Chat read or operated the instance with a tool at
    /// `at_ms`; an earlier time never replaces a later one. Returns whether the mark moved.
    pub fn mark_used(&mut self, instance: InstanceRef, at_ms: u64) -> bool {
        let mark = self.used.entry(instance).or_default();
        if *mark >= at_ms {
            return false;
        }
        *mark = at_ms;
        while self.used.len() > MAX_MARKS {
            let Some(oldest) = oldest_mark(&self.used) else {
                break;
            };
            self.used.remove(&oldest);
        }
        true
    }

    /// Records a verified open. A removal mark stays: a removed row stays hidden.
    pub fn record_opened(&mut self, instance: InstanceRef, title: Option<String>) {
        match self
            .members
            .iter_mut()
            .find(|member| member.instance == instance)
        {
            Some(member) => {
                if title.is_some() {
                    member.title = title;
                }
            }
            None => self.members.push(MonitorMember { instance, title }),
        }
    }

    pub fn remove(&mut self, instance: InstanceRef) {
        self.removed.insert(instance);
    }
}

fn oldest_mark(marks: &BTreeMap<InstanceRef, u64>) -> Option<InstanceRef> {
    marks
        .iter()
        .min_by_key(|(_, at)| **at)
        .map(|(instance, _)| instance.clone())
}
