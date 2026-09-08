//! Timeline strip state (REQ-007 AC-007-29; pure model).
//!
//! The timeline is PURELY a client-side UI — the official web assembles it
//! from session events (no remote). The TUI strip is projected from the local
//! transcript window (turn/step boundary events); no extra remote reads.

/// One projected timeline marker (local window projection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineMarker {
    /// 0-based index within the projected strip.
    pub index: usize,
    pub kind: TimelineMarkerKind,
}

/// Marker kinds derived from local transcript rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineMarkerKind {
    User,
    Assistant,
    Tool,
}

/// Timeline display state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimelineState {
    /// Show/hide toggle (config `[ui].show_timeline`, default false).
    pub show: bool,
    /// Markers projected from the current transcript window.
    pub markers: Vec<TimelineMarker>,
    /// Cursor (selected marker, for the mini-map pointer).
    pub cursor: usize,
}

impl TimelineState {
    /// Rebuild markers from a per-block classification list (block index →
    /// kind). Callers (app) classify transcript rows; this stays pure.
    pub fn rebuild(&mut self, kinds: &[TimelineMarkerKind]) {
        self.markers = kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| TimelineMarker { index, kind: *kind })
            .collect();
        if self.markers.is_empty() {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(self.markers.len() - 1);
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.markers.is_empty() {
            return;
        }
        self.cursor =
            (self.cursor as isize + delta).clamp(0, self.markers.len() as isize - 1) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_projects_markers_from_kinds() {
        let mut t = TimelineState::default();
        t.rebuild(&[
            TimelineMarkerKind::User,
            TimelineMarkerKind::Assistant,
            TimelineMarkerKind::Tool,
            TimelineMarkerKind::Assistant,
        ]);
        assert_eq!(t.markers.len(), 4);
        assert_eq!(t.markers[0].index, 0);
        assert_eq!(t.markers[0].kind, TimelineMarkerKind::User);
        assert_eq!(t.markers[3].kind, TimelineMarkerKind::Assistant);
    }

    #[test]
    fn empty_strip_no_cursor_and_no_panic() {
        let mut t = TimelineState::default();
        t.rebuild(&[]);
        assert_eq!(t.cursor, 0);
        t.move_cursor(3); // no panic
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn cursor_clamped_to_strip() {
        let mut t = TimelineState::default();
        t.rebuild(&[TimelineMarkerKind::User, TimelineMarkerKind::Assistant]);
        t.move_cursor(5);
        assert_eq!(t.cursor, 1);
        t.move_cursor(-9);
        assert_eq!(t.cursor, 0);
    }
}
