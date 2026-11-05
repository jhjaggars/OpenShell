// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Builder for Process Activity [1007] events.

use crate::builders::ContextRef;
use crate::enums::{ActionId, ActivityId, DispositionId, LaunchTypeId, SeverityId, StatusId};
use crate::events::base_event::BaseEventData;
use crate::events::{OcsfEvent, ProcessActivityEvent};
use crate::objects::{Actor, Process};

/// Builder for Process Activity [1007] events.
pub struct ProcessActivityBuilder<'a> {
    ctx: ContextRef<'a>,
    activity: ActivityId,
    severity: SeverityId,
    status: Option<StatusId>,
    action: Option<ActionId>,
    disposition: Option<DispositionId>,
    process: Option<Process>,
    actor: Option<Actor>,
    launch_type: Option<LaunchTypeId>,
    exit_code: Option<i32>,
    message: Option<String>,
    unmapped: serde_json::Map<String, serde_json::Value>,
}

impl<'a> ProcessActivityBuilder<'a> {
    #[must_use]
    pub fn new(ctx: impl Into<ContextRef<'a>>) -> Self {
        Self {
            ctx: ctx.into(),
            activity: ActivityId::Unknown,
            severity: SeverityId::Informational,
            status: None,
            action: None,
            disposition: None,
            process: None,
            actor: None,
            launch_type: None,
            exit_code: None,
            message: None,

            unmapped: serde_json::Map::new(),
        }
    }

    #[must_use]
    pub fn process(mut self, proc: Process) -> Self {
        self.process = Some(proc);
        self
    }
    #[must_use]
    pub fn launch_type(mut self, lt: LaunchTypeId) -> Self {
        self.launch_type = Some(lt);
        self
    }
    #[must_use]
    pub fn exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }

    /// Add an unmapped field.
    #[must_use]
    pub fn unmapped(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.unmapped.insert(key.to_string(), value.into());
        self
    }

    /// Mark the event's payload as guest-sourced: the data originated
    /// from the in-guest boundary (untrusted after start in root-workload
    /// mode), not from a supervisor-verified observation.
    #[must_use]
    pub fn guest_sourced(self) -> Self {
        self.unmapped("data_origin", "guest")
    }

    #[must_use]
    pub fn build(self) -> OcsfEvent {
        let activity_name = self.activity.process_label().to_string();
        let mut base = BaseEventData::new(
            1007,
            "Process Activity",
            1,
            "System Activity",
            self.activity.as_u8(),
            &activity_name,
            self.severity,
            self.ctx
                .metadata(&["security_control", "container", "host"]),
        );
        self.ctx
            .apply_common_fields(&mut base, self.status, self.message);
        if !self.unmapped.is_empty() {
            base.unmapped = Some(serde_json::Value::Object(self.unmapped));
        }

        OcsfEvent::ProcessActivity(ProcessActivityEvent {
            base,
            process: self.process.unwrap_or_else(|| Process::new("unknown", 0)),
            actor: self.actor,
            launch_type: self.launch_type,
            exit_code: self.exit_code,
            action: self.action,
            disposition: self.disposition,
        })
    }
}

impl_activity_setter!(ProcessActivityBuilder);
impl_action_disposition_setters!(ProcessActivityBuilder);
impl_actor_process_setter!(ProcessActivityBuilder);
impl_builder_setters!(ProcessActivityBuilder);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::test_sandbox_context;

    #[test]
    fn test_process_activity_builder_launch() {
        let ctx = test_sandbox_context();
        let event = ProcessActivityBuilder::new(&ctx)
            .activity(ActivityId::Open) // Launch
            .action(ActionId::Allowed)
            .disposition(DispositionId::Allowed)
            .severity(SeverityId::Informational)
            .launch_type(LaunchTypeId::Spawn)
            .process(Process::new("python3", 42).with_cmd_line("python3 /app/main.py"))
            .actor_process(Process::new("openshell-sandbox", 1))
            .message("Process started: python3 /app/main.py")
            .build();

        let json = event.to_json().unwrap();
        assert_eq!(json["class_uid"], 1007);
        assert_eq!(json["launch_type"], "Spawn");
        assert_eq!(json["process"]["name"], "python3");
        assert_eq!(json["actor"]["process"]["name"], "openshell-sandbox");
    }

    #[test]
    fn test_process_activity_builder_terminate() {
        let ctx = test_sandbox_context();
        let event = ProcessActivityBuilder::new(&ctx)
            .activity(ActivityId::Close) // Terminate
            .severity(SeverityId::Informational)
            .process(Process::new("python3", 42))
            .exit_code(0)
            .build();

        let json = event.to_json().unwrap();
        assert_eq!(json["exit_code"], 0);
    }
}

#[cfg(test)]
mod guest_sourced_tests {
    use super::*;
    use crate::{ActivityId, SeverityId};

    #[test]
    fn guest_sourced_process_exit_carries_the_marker() {
        let event = ProcessActivityBuilder::new(crate::ctx::ctx())
            .activity(ActivityId::Fail)
            .severity(SeverityId::Informational)
            .exit_code(137)
            .guest_sourced()
            .message("main process exit reported by the boundary: code 137")
            .build();
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(
            json.get("unmapped")
                .and_then(|u| u.get("data_origin"))
                .and_then(serde_json::Value::as_str),
            Some("guest")
        );
        let shorthand = event.format_shorthand();
        assert!(
            shorthand.contains("data_origin:guest"),
            "the shorthand must render the marker: {shorthand}"
        );
    }

    #[test]
    fn unmarked_process_exit_has_no_marker() {
        let event = ProcessActivityBuilder::new(crate::ctx::ctx())
            .activity(ActivityId::Fail)
            .severity(SeverityId::Informational)
            .exit_code(0)
            .message("main process exited: code 0")
            .build();
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(
            json.get("unmapped")
                .and_then(|u| u.get("data_origin"))
                .is_none()
        );
    }
}
