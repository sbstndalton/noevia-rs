//! Tests of what Noevia changed or added in this vendored crate (see VENDORED.md).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ag_ui_core::event::{BaseEvent, Event, EventType, InterruptEvent, InterruptReason};
use ag_ui_core::types::{MessageId, RunId, ThreadId, ToolCallId};
use serde_json::json;

#[test]
fn ids_hold_any_string_and_serialize_as_one() {
    let id = MessageId::new("qa-message-7");
    assert_eq!(serde_json::to_value(&id).unwrap(), json!("qa-message-7"));
    let back: ThreadId = serde_json::from_value(json!("not-a-uuid")).unwrap();
    assert_eq!(back.to_string(), "not-a-uuid");
    assert_eq!(RunId::from("r"), RunId::new(String::from("r")));
    assert!(ToolCallId::new("qa-call-0") == ToolCallId::from("qa-call-0"));
    // A random id is still a UUID string.
    assert_eq!(MessageId::random().to_string().len(), 36);
}

#[test]
fn an_interrupt_event_round_trips_with_its_wire_names() {
    let event: Event = Event::Interrupt(InterruptEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        interrupt_id: "i-1".into(),
        tool_call_id: ToolCallId::new("c-1"),
        tool_call_name: "space_page_save".into(),
        reason: InterruptReason::ApprovalPending,
        message: None,
    });
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(
        wire,
        json!({"type": "INTERRUPT", "interruptId": "i-1", "toolCallId": "c-1",
               "toolCallName": "space_page_save", "reason": "approval_pending"})
    );
    let back: Event = serde_json::from_value(wire).unwrap();
    assert_eq!(back, event);
    assert_eq!(back.event_type(), EventType::Interrupt);
    assert_eq!(back.timestamp(), None);
}
