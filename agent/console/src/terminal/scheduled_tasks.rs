use std::{any::Any, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Offset};
use chrono_tz::Tz;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_agent_protocol::{ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskKind};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use super::{
    scheduled_task_rows::{ScheduleView, ScheduledTaskView},
    tick_actions::{
        PreparedTickAction, TickActionBackend, TickActionContext, TickActionExecutor, TickActionUi,
        TickDetailKey, TickDetailLine, TickDetailView,
    },
};

const MINUTES_PER_HOUR: u32 = 60;
const HOURS_PER_DAY: u32 = 24;
const MINUTES_PER_DAY: u32 = HOURS_PER_DAY * MINUTES_PER_HOUR;

fn instruction(prompt: &str) -> &str {
    if matches!(
        prompt,
        tickloop_agent_config::loop_skill::DEFAULT_INSTRUCTIONS_PROMPT | "/loop"
    ) {
        "loop.md or maintenance instructions"
    } else {
        prompt
            .strip_prefix("/loop ")
            .map(str::trim_start)
            .unwrap_or(prompt)
    }
}

pub(super) fn local_time(millis: u64, zone: &str) -> Option<OffsetDateTime> {
    let millis = i64::try_from(millis).ok()?;
    let datetime = DateTime::from_timestamp_millis(millis)?;
    let zone = zone.parse::<Tz>().ok()?;
    let seconds = datetime
        .with_timezone(&zone)
        .offset()
        .fix()
        .local_minus_utc();
    let offset = UtcOffset::from_whole_seconds(seconds).ok()?;
    Some(
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
            .ok()?
            .to_offset(offset),
    )
}

pub(super) fn view(
    task: &ScheduledTask,
    paused: bool,
    triggered: Option<u64>,
    waiting: bool,
) -> ScheduledTaskView<'_> {
    let schedule = match &task.kind {
        ScheduledTaskKind::SelfPaced { .. } => ScheduleView::SelfPaced,
        ScheduledTaskKind::OneShot { cron } => ScheduleView::Once(cron),
        ScheduledTaskKind::Recurring { cron } => {
            let fields = cron.split_whitespace().collect::<Vec<_>>();
            let minutes = match fields.as_slice() {
                ["*", "*", "*", "*", "*"] => Some(1),
                ["0", "*", "*", "*", "*"] => Some(MINUTES_PER_HOUR),
                ["0", "0", "*", "*", "*"] => Some(MINUTES_PER_DAY),
                ["0", "0", day, "*", "*"] => day
                    .strip_prefix("*/")
                    .and_then(|n| n.parse::<u32>().ok())
                    .and_then(|n| n.checked_mul(MINUTES_PER_DAY)),
                [minute, "*", "*", "*", "*"] => minute
                    .strip_prefix("*/")
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|n| *n > 0 && MINUTES_PER_HOUR.is_multiple_of(*n)),
                ["0", hour, "*", "*", "*"] => hour
                    .strip_prefix("*/")
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|n| *n > 0 && HOURS_PER_DAY.is_multiple_of(*n))
                    .and_then(|n| n.checked_mul(MINUTES_PER_HOUR)),
                _ => None,
            };
            minutes.map_or(ScheduleView::Cron(cron), |minutes| ScheduleView::Interval {
                minutes,
                cron,
            })
        }
    };
    let at = |millis| local_time(millis, &task.time_zone);
    ScheduledTaskView {
        schedule,
        instruction: instruction(&task.prompt),
        created_at: at(task.created_at_ms).unwrap_or(OffsetDateTime::UNIX_EPOCH),
        expires_at: task.expires_at_ms.and_then(at),
        last_trigger_at: task.last_fired_at_ms.and_then(at),
        next_trigger_at: if waiting {
            task.last_fired_at_ms.and_then(at)
        } else {
            task.next_fire_at_ms.and_then(at)
        },
        scheduled_at: if waiting {
            task.last_scheduled_for_ms
                .or(task.last_fired_at_ms)
                .and_then(at)
        } else {
            task.scheduled_for_ms.and_then(at)
        },
        source: match task.creator {
            tickloop_agent_protocol::ScheduledTaskCreator::User => "user",
            tickloop_agent_protocol::ScheduledTaskCreator::Agent => "agent",
        },
        skipped: task.skipped,
        pending_confirmation: task.confirmation == Some(ScheduledTaskConfirmation::Pending),
        triggers_paused: paused || task.paused_at_ms.is_some(),
        triggered_at_ms: triggered,
    }
}

#[derive(Clone)]
pub(super) struct TaskAction {
    pub(super) task: ScheduledTask,
    pub(super) delete: bool,
}
impl TaskAction {
    pub(super) fn new(task: ScheduledTask, delete: bool) -> Self {
        Self { task, delete }
    }
}
impl TickActionExecutor for TaskAction {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        if !self.delete {
            let chat = ui.chat().ok_or("No selected Chat")?;
            ui.open_detail(Box::new(TaskDetail {
                id: Uuid::new_v4(),
                chat,
                task: self.task.clone(),
                top: 0,
            }));
        }
        Ok(PreparedTickAction::new(
            self.task.id.clone(),
            if self.delete { 'x' } else { '\n' },
            Arc::new(self.clone()),
        ))
    }
}
#[async_trait]
impl TickActionBackend for TaskAction {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        if !self.delete {
            return Ok(());
        }
        ctx.agent
            .delete_scheduled_task(ctx.chat, self.task.id.clone())
            .await
            .map_err(|error| error.to_string())?;
        ctx.console.remove_scheduled_task(ctx.chat, &self.task.id)?;
        Ok(())
    }
}

#[derive(Clone)]
pub(super) enum PauseAction {
    Scheduled(String),
    Subscription(tick_protocol::InstanceRef),
}

impl TickActionExecutor for PauseAction {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let id = match self {
            Self::Scheduled(id) => id.clone(),
            Self::Subscription(instance) => instance.to_string(),
        };
        Ok(PreparedTickAction::new(
            id,
            crate::monitor_rows::MONITOR_PAUSE_KEY,
            Arc::new(self.clone()),
        ))
    }
}

#[async_trait]
impl TickActionBackend for PauseAction {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        // The row itself shows the change, so the toggle reports nothing in the status line.
        match self {
            Self::Scheduled(id) => {
                let status = ctx
                    .agent
                    .status(ctx.chat)
                    .await
                    .map_err(|error| error.to_string())?;
                let task = status
                    .scheduled_tasks
                    .iter()
                    .find(|task| task.id == *id)
                    .ok_or("Scheduled task no longer exists")?;
                let paused = task.paused_at_ms.is_none();
                ctx.agent
                    .set_scheduled_task_paused(ctx.chat, id.clone(), paused)
                    .await
                    .map_err(|error| error.to_string())?;
                ctx.console.update_monitor_chat(
                    ctx.agent
                        .status(ctx.chat)
                        .await
                        .map_err(|error| error.to_string())?,
                )?;
            }
            Self::Subscription(instance) => {
                let subscriptions = ctx
                    .agent
                    .tick_subscriptions(ctx.chat)
                    .await
                    .map_err(|error| error.to_string())?;
                let row = subscriptions
                    .iter()
                    .find(|row| row.instance == *instance && row.frequency.period().is_some())
                    .ok_or("Pausing this Tick is not supported")?;
                let paused = !row.paused;
                ctx.agent
                    .update_tick_subscription(
                        ctx.chat,
                        row.id,
                        tickloop_agent_protocol::TickSubscriptionChange::Paused(paused),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let rows = ctx
                    .agent
                    .tick_subscriptions(ctx.chat)
                    .await
                    .map_err(|error| error.to_string())?;
                ctx.console.update_subscribed_ticks(ctx.chat, &rows)?;
            }
        }
        Ok(())
    }
}

pub(super) struct TaskDetail {
    id: Uuid,
    chat: Uuid,
    pub(super) task: ScheduledTask,
    top: usize,
}
impl TickDetailView for TaskDetail {
    fn id(&self) -> Uuid {
        self.id
    }
    fn chat(&self) -> Uuid {
        self.chat
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn hint(&self) -> String {
        "j/k scroll · ctrl+x remove · esc back".into()
    }
    fn lines(&mut self, width: usize, height: usize, _now: u64) -> Vec<TickDetailLine> {
        let view = view(&self.task, false, None, false);
        let total = view.detail_lines(width, 0, usize::MAX).len();
        self.top = self.top.min(total.saturating_sub(height));
        view.detail_lines(width, self.top, height)
            .into_iter()
            .map(|line| TickDetailLine { line })
            .collect()
    }
    fn key(&mut self, key: KeyEvent, width: usize, height: usize) -> TickDetailKey {
        match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Left, _) => return TickDetailKey::Back,
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.top = self.top.saturating_add(1),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.top = self.top.saturating_sub(1),
            (KeyCode::PageDown, _) => self.top = self.top.saturating_add(height),
            (KeyCode::PageUp, _) => self.top = self.top.saturating_sub(height),
            _ => {}
        }
        let _ = self.lines(width, height, 0);
        TickDetailKey::None
    }
}

pub(super) fn result_lines(
    name: &str,
    input: Option<&serde_json::Value>,
    content: &str,
) -> Option<Vec<String>> {
    if !matches!(
        name,
        "CronCreate" | "CronList" | "CronDelete" | "ScheduleWakeup"
    ) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let next = |task: &serde_json::Value| {
        if task["paused"] == true {
            return "paused".to_owned();
        }
        task["next_run"]
            .as_str()
            .and_then(|next| next.split_whitespace().nth(1))
            .and_then(|time| time.get(..5))
            .map_or_else(
                || "not scheduled".to_owned(),
                |time| format!("next run {time}"),
            )
    };
    let line = |task: &serde_json::Value| {
        format!(
            "{} · {} · {} · {}{}",
            task["id"].as_str().unwrap_or(""),
            task["schedule"].as_str().unwrap_or(""),
            super::safe_line(instruction(task["prompt"].as_str().unwrap_or(""))),
            next(task),
            if task["awaiting_confirmation"] == true {
                " · pending confirmation"
            } else {
                ""
            }
        )
    };
    Some(match name {
        "CronList" => {
            let tasks = value["tasks"].as_array()?;
            if tasks.is_empty() {
                vec!["No scheduled tasks".into()]
            } else {
                tasks.iter().map(line).collect()
            }
        }
        "CronDelete" => vec![format!(
            "Deleted scheduled task {}",
            value["deleted"].as_str()?
        )],
        "ScheduleWakeup" if value.get("stopped").is_some() => vec![if value["stopped"].is_null() {
            "No self-paced loop is running".into()
        } else {
            "Self-paced loop stopped".into()
        }],
        "ScheduleWakeup" => {
            let mut lines = vec![format!("Wakeup · {}", next(&value))];
            if let Some(reason) = input.and_then(|input| input["reason"].as_str()) {
                lines.push(format!("Reason: {}", super::safe_line(reason)));
            }
            lines
        }
        "CronCreate" => vec![line(&value)],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn task() -> ScheduledTask {
        serde_json::from_value(json!({
            "id": "0123abcd", "kind": {"type": "recurring", "cron": "* * * * *"},
            "prompt": "提醒我看天气", "creator": "user", "time_zone": "Asia/Shanghai",
            "created_at_ms": 0, "scheduled_for_ms": 60000, "next_fire_at_ms": 60000,
            "jitter_ms": 0, "skipped": 2
        }))
        .unwrap()
    }

    #[test]
    fn task_projection_uses_each_instants_zone_and_detects_intervals() {
        let task = task();
        let projected = view(&task, false, None, false);
        assert_eq!(
            projected.schedule,
            ScheduleView::Interval {
                minutes: 1,
                cron: "* * * * *"
            }
        );
        assert_eq!(projected.created_at.hour(), 8);
        assert_eq!(projected.skipped, 2);
        // New York's spring transition uses a different offset on each side.
        let before = local_time(1_710_053_940_000, "America/New_York").unwrap();
        let after = local_time(1_710_054_000_000, "America/New_York").unwrap();
        assert_eq!((before.hour(), before.minute()), (1, 59));
        assert_eq!((after.hour(), after.minute()), (3, 0));
    }

    #[test]
    fn scheduling_results_show_next_run_list_and_wakeup_reason() {
        let task = json!({"id":"0123abcd", "schedule":"* * * * *", "prompt":"weather", "next_run":"2026-10-23 08:01:00 CST", "awaiting_confirmation":false});
        assert_eq!(
            result_lines("CronCreate", None, &task.to_string()).unwrap(),
            ["0123abcd · * * * * * · weather · next run 08:01"]
        );
        let mut paused = task.clone();
        paused["paused"] = json!(true);
        assert_eq!(
            result_lines("CronList", None, &json!({"tasks":[paused]}).to_string()).unwrap(),
            ["0123abcd · * * * * * · weather · paused"]
        );
        let list = json!({"tasks":[task.clone(),task.clone()]});
        assert_eq!(
            result_lines("CronList", None, &list.to_string())
                .unwrap()
                .len(),
            2
        );
        let input = json!({"reason":"waiting for CI"});
        assert_eq!(
            result_lines("ScheduleWakeup", Some(&input), &task.to_string()).unwrap(),
            ["Wakeup · next run 08:01", "Reason: waiting for CI"]
        );
        assert_eq!(
            result_lines("CronList", None, r#"{"tasks":[]}"#).unwrap(),
            ["No scheduled tasks"]
        );
        assert_eq!(
            result_lines("ScheduleWakeup", None, r#"{"stopped":"0123abcd"}"#).unwrap(),
            ["Self-paced loop stopped"]
        );
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    #[test]
    fn monitor_projects_nominal_time_and_details_keep_jittered_next_run() {
        let mut task = super::tests::task();
        task.kind = ScheduledTaskKind::Recurring {
            cron: "0 12 * * 2".into(),
        };
        task.scheduled_for_ms = Some(14_400_000);
        task.next_fire_at_ms = Some(16_200_000);
        task.jitter_ms = 1_800_000;
        let projected = view(&task, false, None, false);
        let lines = projected.row_lines(
            48,
            false,
            crate::terminal::scheduled_task_rows::ScheduledTaskRender {
                display: crate::terminal::display::DisplayProfile::default(),
                ascii: false,
                now: local_time(0, "Asia/Shanghai").unwrap(),
                now_ms: 0,
            },
        );
        assert_eq!(lines[0].text(), "◷ 0 12 * * 2 · 12:00");
        let details = projected
            .detail_lines(80, 0, 100)
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(details.contains("Next run: 1970-01-01T12:30:00+08:00"));
        assert!(details.contains("Cron: 0 12 * * 2"));
        assert!(details.contains("Source: user"));
        task.next_fire_at_ms = Some(14_415_123);
        let exact = view(&task, false, None, false)
            .detail_lines(80, 0, 100)
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(exact.contains("Next run: 1970-01-01T12:00:15.123+08:00"));
        task.kind = ScheduledTaskKind::OneShot {
            cron: "0 12 * * *".into(),
        };
        assert_eq!(
            view(&task, false, None, false).schedule,
            ScheduleView::Once("0 12 * * *")
        );
        task.kind = ScheduledTaskKind::SelfPaced {
            delay_seconds: Some(60),
            reason: None,
            fallback: false,
        };
        task.prompt = "/loop 看看 CI 状态".into();
        assert_eq!(view(&task, false, None, false).instruction, "看看 CI 状态");
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    #[test]
    fn uneven_cron_steps_keep_the_original_expression() {
        for cron in ["*/7 * * * *", "0 */5 * * *", "0 12 * * 2"] {
            let mut task = super::tests::task();
            task.kind = ScheduledTaskKind::Recurring { cron: cron.into() };
            assert_eq!(
                view(&task, false, None, false).schedule,
                ScheduleView::Cron(cron)
            );
        }
    }
}
