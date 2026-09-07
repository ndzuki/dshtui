//! Approval serial queue (REQ-006 §5 `ApprovalQueueState`; D-036 additive).
//!
//! Extends REQ-003's single-decision approval modal into a serial queue while
//! keeping the single-decision modality intact:
//! - at most one item is ever active / in flight (AC-006-15);
//! - an arrival while something is active only enqueues — never overwrites
//!   the active slot (fixes the existing in-flight overwrite hazard);
//! - granted (allowed) event ids go into a bounded dedupe set so a replayed /
//!   re-delivered approval is never authorized twice (AC-006-15/18);
//! - a failed reply keeps its item retryable (fail-closed) while the rest of
//!   the queue may continue (AC-006-14);
//! - danger-full-access items need a second local ack before an allow is
//!   emitted; acknowledging never upgrades the outcome to always-allow
//!   (AC-006-16 / D-037 — the TUI never switches the remote policy).
//!
//! This type is pure (no IO, no locks): the decision matrix is
//! table-driven-testable in the model layer (AC-006-14/15/16 live here).

use std::collections::VecDeque;

use crate::api::types::ApprovalEvent;

/// Bounded granted-dedupe capacity (replay protection, AC-006-15/18).
const GRANTED_CAP: usize = 256;

/// One queued approval item and its local decision state.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalItem {
    pub event: ApprovalEvent,
    /// `danger-full-access` / sandbox-mode markers need a second local ack
    /// before an allow (AC-006-16).
    pub danger: bool,
    /// Second-level risk acknowledgement for the danger item.
    pub acked: bool,
}

/// Read-only summary for list/status rendering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalSummary {
    pub pending: usize,
    pub failed: usize,
    pub has_active: bool,
}

/// Serial approval queue (pure model). Default = empty queue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApprovalQueue {
    /// Arrival-ordered items not yet shown.
    queue: VecDeque<ApprovalItem>,
    /// The single item currently displayed (mirror of `ApprovalState.event`).
    active: Option<ApprovalItem>,
    /// Items whose reply failed fail-closed; individually retryable.
    failed: Vec<ApprovalItem>,
    /// event_ids already granted (allowed): bounded, replay never re-authors.
    granted: VecDeque<String>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a new arrival. Returns false (no-op) when the event_id is
    /// already pending/active/failed, or has already been granted — the
    /// replay / duplicate-delivery dedupe (AC-006-15/18).
    pub fn enqueue(&mut self, event: ApprovalEvent, danger: bool) -> bool {
        let id = event.event_id.as_str();
        if self.is_granted(id) {
            return false;
        }
        if self.iter_all().any(|item| item.event.event_id == id) {
            return false;
        }
        self.queue.push_back(ApprovalItem {
            event,
            danger,
            acked: false,
        });
        true
    }

    /// The currently displayed item (None when idle).
    pub fn active(&self) -> Option<&ApprovalItem> {
        self.active.as_ref()
    }

    /// Whether an item is currently active (≤1 in flight invariant).
    pub fn has_active(&self) -> bool {
        self.active.is_some()
    }

    /// Promote the oldest pending item to active. Returns the promoted event
    /// (caller mirrors it into `ApprovalState.event`) or None when the queue
    /// is empty / something is already active.
    pub fn promote(&mut self) -> Option<ApprovalEvent> {
        if self.active.is_some() {
            return None;
        }
        let item = self.queue.pop_front()?;
        self.active = Some(item);
        self.active_event()
    }

    /// The active item's event (mirror).
    pub fn active_event(&self) -> Option<ApprovalEvent> {
        self.active.as_ref().map(|item| item.event.clone())
    }

    /// Settle the active item after a successful outcome delivery, then
    /// auto-advance to the next pending item (serial pump: ≤1 in flight).
    /// `granted=true` (allowed-once) records the id in the bounded dedupe set
    /// (AC-006-15: an allowed event replayed later must not be re-authorized).
    /// The caller re-mirrors `active_event()` into `ApprovalState.event`.
    pub fn settle(&mut self, granted: bool) {
        if let Some(item) = self.active.take() {
            if granted {
                self.remember_granted(&item.event.event_id);
            }
        }
        self.promote();
    }

    /// Whether the id was already granted (allowed-once).
    pub fn is_granted(&self, id: &str) -> bool {
        self.granted.iter().any(|g| g == id)
    }

    /// Reply failure: keep the active item retryable (fail-closed) and
    /// auto-advance to the next pending item so the rest can continue
    /// (AC-006-14). Returns the kept (failed) event id.
    pub fn fail_active(&mut self) -> Option<ApprovalEvent> {
        let mut item = self.active.take()?;
        item.acked = false;
        let event = item.event.clone();
        if !self
            .failed
            .iter()
            .any(|f| f.event.event_id == item.event.event_id)
        {
            self.failed.push(item);
        }
        self.promote();
        Some(event)
    }

    /// Retry a failed item: bring it back as the active item (≤1 in flight).
    /// Returns the retried event when it became active.
    pub fn retry_failed(&mut self, event_id: &str) -> Option<ApprovalEvent> {
        if self.active.is_some() {
            return None;
        }
        let idx = self
            .failed
            .iter()
            .position(|item| item.event.event_id == event_id)?;
        let mut item = self.failed.remove(idx);
        item.acked = false;
        self.active = Some(item);
        self.active_event()
    }

    /// Second-level risk ack for the active danger item (AC-006-16). Returns
    /// false when there is no active danger item or it was already acked.
    pub fn ack_active(&mut self) -> bool {
        let Some(item) = self.active.as_mut() else {
            return false;
        };
        if !item.danger || item.acked {
            return false;
        }
        item.acked = true;
        true
    }

    /// Whether an allow is currently blocked until the danger ack (AC-006-16
    /// stop point: batch auto-advance must pause here).
    pub fn head_requires_ack(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|item| item.danger && !item.acked)
    }

    /// Whether the active item is acked (for a "confirmed" display).
    pub fn head_acked(&self) -> bool {
        self.active.as_ref().is_some_and(|item| item.acked)
    }

    /// Whether an event id is currently a failed (retryable) item.
    pub fn is_failed(&self, id: &str) -> bool {
        self.failed.iter().any(|item| item.event.event_id == id)
    }

    /// Drop any item (used when leaving the approval flow: Esc/q on the list).
    pub fn clear(&mut self) {
        self.queue.clear();
        self.active = None;
        self.failed.clear();
    }

    /// Summary counts for list/status rendering.
    pub fn summary(&self) -> ApprovalSummary {
        ApprovalSummary {
            pending: self.queue.len(),
            failed: self.failed.len(),
            has_active: self.active.is_some(),
        }
    }

    /// All items for the list view in display order: active first, then
    /// pending in arrival order, then failed (arrival order within each).
    pub fn list(&self) -> Vec<&ApprovalItem> {
        let mut out: Vec<&ApprovalItem> = Vec::with_capacity(self.len());
        if let Some(item) = self.active.as_ref() {
            out.push(item);
        }
        out.extend(self.queue.iter());
        out.extend(self.failed.iter());
        out
    }

    /// Total item count.
    pub fn len(&self) -> usize {
        self.queue.len() + usize::from(self.active.is_some()) + self.failed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter_all(&self) -> impl Iterator<Item = &ApprovalItem> {
        self.queue
            .iter()
            .chain(self.active.iter())
            .chain(self.failed.iter())
    }

    fn remember_granted(&mut self, id: &str) {
        if self.is_granted(id) {
            return;
        }
        if self.granted.len() >= GRANTED_CAP {
            self.granted.pop_front();
        }
        self.granted.push_back(id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str) -> ApprovalEvent {
        ApprovalEvent {
            client_id: format!("c-{id}"),
            event_id: id.to_string(),
            raw: serde_json::json!({}),
        }
    }

    #[test]
    fn enqueue_then_promote_keeps_one_active() {
        let mut q = ApprovalQueue::new();
        assert!(q.enqueue(ev("e1"), false));
        assert!(q.enqueue(ev("e2"), false));
        assert_eq!(q.summary().pending, 2);
        assert!(!q.has_active());
        // promote → active, ≤1 in flight.
        assert_eq!(q.promote(), Some(ev("e1")));
        assert_eq!(q.active().unwrap().event.event_id, "e1");
        // second promote is a no-op while active.
        assert_eq!(q.promote(), None);
        assert_eq!(q.summary().pending, 1);
    }

    #[test]
    fn duplicate_and_granted_events_are_rejected() {
        let mut q = ApprovalQueue::new();
        assert!(q.enqueue(ev("e1"), false));
        // Same id while pending → false.
        assert!(!q.enqueue(ev("e1"), false));
        q.promote();
        q.settle(true); // granted
        assert!(q.is_granted("e1"));
        // Replay of a granted event → false (never re-authorized, AC-006-15/18).
        assert!(!q.enqueue(ev("e1"), false));
        // A fresh id still works.
        assert!(q.enqueue(ev("e2"), false));
    }

    #[test]
    fn settle_granted_then_next_promotes() {
        let mut q = ApprovalQueue::new();
        q.enqueue(ev("e1"), false);
        q.enqueue(ev("e2"), false);
        q.promote();
        q.settle(true);
        assert_eq!(q.active_event(), Some(ev("e2")), "队列自动续发下一项");
        q.settle(false); // rejected → not granted
        assert!(!q.is_granted("e2"));
        assert!(q.is_empty());
    }

    #[test]
    fn failed_item_is_kept_and_retryable_others_continue() {
        let mut q = ApprovalQueue::new();
        q.enqueue(ev("e1"), false);
        q.enqueue(ev("e2"), false);
        q.promote();
        // e1 reply fails → kept as failed, e2 auto-advances (AC-006-14).
        assert_eq!(q.fail_active(), Some(ev("e1")));
        assert_eq!(q.summary().failed, 1);
        assert_eq!(q.active().unwrap().event.event_id, "e2");
        // e2 also fails → still auto-advances, queue now empty but e1 failed.
        assert_eq!(q.fail_active(), Some(ev("e2")));
        assert_eq!(q.summary().failed, 2);
        assert!(!q.has_active());
        // e1 can be individually retried.
        assert_eq!(q.retry_failed("e1"), Some(ev("e1")));
        assert_eq!(q.active().unwrap().event.event_id, "e1");
        // settling a retried item does not resurrect the other failed item.
        q.settle(false);
        assert_eq!(q.summary().failed, 1, "仅 e2 仍失败，e1 已处置");
        assert_eq!(q.summary().pending, 0);
    }

    #[test]
    fn danger_requires_ack_before_allow_but_never_upgrades() {
        let mut q = ApprovalQueue::new();
        q.enqueue(ev("d1"), true);
        q.promote();
        assert!(q.head_requires_ack());
        assert!(!q.head_acked());
        // ack only applies to the active danger item.
        assert!(q.ack_active());
        assert!(!q.head_requires_ack());
        assert!(q.head_acked());
        // ack is idempotent.
        assert!(!q.ack_active());
        // settle granted still grants only once (allowed-once semantics).
        q.settle(true);
        assert!(q.is_granted("d1"));
        assert!(!q.enqueue(ev("d1"), true), "重放已授权事件不得再入队");
    }

    #[test]
    fn non_danger_never_requires_ack() {
        let mut q = ApprovalQueue::new();
        q.enqueue(ev("n1"), false);
        q.promote();
        assert!(!q.head_requires_ack());
        assert!(!q.ack_active(), "非 danger 项无 ack 语义");
    }

    #[test]
    fn list_order_active_then_pending_then_failed() {
        let mut q = ApprovalQueue::new();
        q.enqueue(ev("e1"), false);
        q.enqueue(ev("e2"), false);
        q.enqueue(ev("e3"), false);
        q.promote(); // e1 active
        let ids: Vec<&str> = q.list().iter().map(|i| i.event.event_id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
        // e1 fails → active cleared, listed after pending.
        q.fail_active();
        let ids: Vec<&str> = q.list().iter().map(|i| i.event.event_id.as_str()).collect();
        assert_eq!(ids, vec!["e2", "e3", "e1"]);
    }

    #[test]
    fn clear_drops_everything() {
        let mut q = ApprovalQueue::new();
        q.enqueue(ev("e1"), false);
        q.enqueue(ev("e2"), true);
        q.promote();
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.summary(), ApprovalSummary::default());
    }
}
