//! Probes of a Chat's hosted Ticks: the Agent lists the Chat's running hosted Ticks with a
//! process sample of each. Probing never reads Tick content, subscribes or keeps a Tick from
//! idling.

use std::collections::HashMap;

use uuid::Uuid;

use super::execution::ConsoleExecution;
use crate::monitor_rows::{MemberFindings, MemberState, ProbeError, TickProbe, TickResources};
use tick_protocol::InstanceRef;
use tickloop_harness::protocol::HostedMemberState;

pub(super) struct HostedProbe {
    agent: ConsoleExecution,
}

impl HostedProbe {
    pub(super) fn new(agent: ConsoleExecution) -> Self {
        Self { agent }
    }
}

#[async_trait::async_trait]
impl TickProbe for HostedProbe {
    fn access(&self) -> &'static str {
        tick_protocol::HOSTED_ACCESS
    }

    async fn probe(
        &self,
        chat: Uuid,
        instances: &[InstanceRef],
    ) -> Result<MemberFindings, ProbeError> {
        let listed = match self.agent.hosted_ticks(chat).await {
            Ok(listed) => listed,
            // While the user keeps a Tickhost of another build, its Ticks may still run but
            // serve nothing here; any other failure keeps the earlier findings.
            Err(error) => {
                return match self.agent.other_tickhost().await {
                    Ok(Some(_)) => Ok(MemberFindings {
                        states: instances
                            .iter()
                            .map(|instance| (instance.clone(), MemberState::Unavailable))
                            .collect(),
                        ..MemberFindings::default()
                    }),
                    _ => Err(ProbeError(error.to_string())),
                };
            }
        };
        let running = listed
            .into_iter()
            .map(|member| (member.instance.clone(), member))
            .collect::<HashMap<_, _>>();
        let mut findings = MemberFindings::default();
        for instance in instances {
            let Some(member) = running.get(instance) else {
                findings.states.insert(instance.clone(), MemberState::Gone);
                continue;
            };
            if member.is_terminal {
                findings.terminals.insert(instance.clone());
            }
            let state = match member.state {
                HostedMemberState::Running => MemberState::Live,
                HostedMemberState::Exited => MemberState::Exited(member.exit),
                HostedMemberState::Lost => MemberState::Lost,
            };
            findings.states.insert(instance.clone(), state);
            if let Some(updated_at_ms) = member.updated_at_ms {
                findings.updates.insert(instance.clone(), updated_at_ms);
            }
            if let Some(sample) = &member.resources {
                findings.resources.insert(
                    instance.clone(),
                    TickResources {
                        pid: sample.business_pid,
                        cpu_time_ms: sample.cpu_time_ms,
                        cpu_percent: None,
                        rss_bytes: sample.rss_bytes,
                        sampled_at_ms: sample.sampled_at_ms,
                    },
                );
            }
        }
        Ok(findings)
    }
}
