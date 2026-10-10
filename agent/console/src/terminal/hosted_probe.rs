//! Probes of a Chat's hosted Ticks: the Agent lists the Chat's running hosted Ticks with a
//! process sample of each. Probing never reads Tick content, subscribes or keeps a Tick from
//! idling.

use std::collections::HashMap;

use uuid::Uuid;

use super::execution::ConsoleExecution;
use crate::monitor_rows::{MemberFindings, MemberState, ProbeError, TickProbe, TickResources};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{HostedMemberState, HostedProgramExit};

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
    fn tick_type(&self) -> &'static str {
        tick_protocol::HOSTED_INSTANCE_TYPE
    }

    async fn probe(
        &self,
        chat: Uuid,
        instances: &[InstanceRef],
    ) -> Result<MemberFindings, ProbeError> {
        let running = self
            .agent
            .hosted_ticks(chat)
            .await
            .map_err(|error| ProbeError(error.to_string()))?
            .into_iter()
            .map(|member| (member.instance.clone(), member))
            .collect::<HashMap<_, _>>();
        let mut findings = MemberFindings::default();
        for instance in instances {
            let Some(member) = running.get(instance) else {
                findings.states.insert(instance.clone(), MemberState::Gone);
                continue;
            };
            let state = match member.state {
                HostedMemberState::Running => MemberState::Live,
                HostedMemberState::Exited => MemberState::Exited(match member.exit {
                    Some(HostedProgramExit {
                        code: Some(code), ..
                    }) => format!("exited {code}"),
                    Some(HostedProgramExit {
                        signal: Some(signal),
                        ..
                    }) => format!("exited by signal {signal}"),
                    _ => "exited".into(),
                }),
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
