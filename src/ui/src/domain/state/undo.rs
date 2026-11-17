// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::VecDeque;

pub const UNDO_LIMIT: usize = 100;
const RESTORE_SYNC_FRAMES: u32 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoAction {
    Undo,
    Redo,
}

#[derive(Debug, Clone)]
pub struct UndoStack<T> {
    undo: VecDeque<T>,
    redo: Vec<T>,
    baseline: Option<T>,
    restoring: Option<u32>,
    limit: usize,
}

impl<T> Default for UndoStack<T> {
    fn default() -> Self {
        Self::with_limit(UNDO_LIMIT)
    }
}

impl<T> UndoStack<T> {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            baseline: None,
            restoring: None,
            limit,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.undo.len()
    }

    #[cfg(test)]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

impl<T: Clone + PartialEq> UndoStack<T> {
    pub fn push(&mut self, before: T) {
        self.redo.clear();
        self.undo.push_back(before);
        while self.undo.len() > self.limit {
            self.undo.pop_front();
        }
    }

    pub fn observe(&mut self, current: &T, settled: bool) {
        if let Some(frames_left) = self.restoring {
            if self.baseline.as_ref() == Some(current) || frames_left == 0 {
                self.restoring = None;
                self.baseline = Some(current.clone());
            } else {
                self.restoring = Some(frames_left - 1);
            }
            return;
        }
        match &self.baseline {
            None => self.baseline = Some(current.clone()),
            Some(baseline) if settled && baseline != current => {
                let before = baseline.clone();
                self.push(before);
                self.baseline = Some(current.clone());
            }
            Some(_) => {}
        }
    }

    pub fn undo(&mut self, current: &T) -> Option<T> {
        let previous = self.undo.pop_back()?;
        self.redo.push(current.clone());
        Some(self.restore(previous))
    }

    pub fn redo(&mut self, current: &T) -> Option<T> {
        let next = self.redo.pop()?;
        self.undo.push_back(current.clone());
        Some(self.restore(next))
    }

    pub fn apply(&mut self, action: UndoAction, current: &T) -> Option<T> {
        if self.restoring.is_some() {
            return None;
        }
        match action {
            UndoAction::Undo => self.undo(current),
            UndoAction::Redo => self.redo(current),
        }
    }

    fn restore(&mut self, target: T) -> T {
        self.baseline = Some(target.clone());
        self.restoring = Some(RESTORE_SYNC_FRAMES);
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn settle(stack: &mut UndoStack<u32>, value: u32) {
        stack.observe(&value, true);
    }

    #[test]
    fn push_undo_redo_round_trip() {
        let mut stack = UndoStack::default();
        settle(&mut stack, 0);
        settle(&mut stack, 1);
        settle(&mut stack, 2);
        assert_eq!(stack.undo(&2), Some(1));
        assert_eq!(stack.undo(&1), Some(0));
        assert_eq!(stack.undo(&0), None);
        assert_eq!(stack.redo(&0), Some(1));
        assert_eq!(stack.redo(&1), Some(2));
        assert_eq!(stack.redo(&2), None);
    }

    #[test]
    fn a_new_edit_after_an_undo_drops_the_redo_branch() {
        let mut stack = UndoStack::default();
        settle(&mut stack, 0);
        settle(&mut stack, 1);
        assert_eq!(stack.undo(&1), Some(0));
        settle(&mut stack, 0);
        settle(&mut stack, 5);
        assert!(!stack.can_redo());
        assert_eq!(stack.undo(&5), Some(0));
    }

    #[test]
    fn an_unsettled_drag_records_one_entry_on_release() {
        let mut stack = UndoStack::default();
        settle(&mut stack, 0);
        for frame in 1..30 {
            stack.observe(&frame, false);
        }
        stack.observe(&30, true);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.undo(&30), Some(0));
    }

    #[test]
    fn a_second_undo_before_the_restore_lands_is_ignored() {
        let mut stack = UndoStack::default();
        settle(&mut stack, 0);
        settle(&mut stack, 1);
        settle(&mut stack, 2);
        assert_eq!(stack.apply(UndoAction::Undo, &2), Some(1));
        assert_eq!(stack.apply(UndoAction::Undo, &2), None);
        assert_eq!(stack.apply(UndoAction::Redo, &2), None);
        settle(&mut stack, 1);
        assert_eq!(stack.apply(UndoAction::Undo, &1), Some(0));
    }

    #[test]
    fn a_restore_that_never_lands_is_adopted_after_the_sync_window() {
        let mut stack = UndoStack::default();
        settle(&mut stack, 0);
        settle(&mut stack, 1);
        assert_eq!(stack.undo(&1), Some(0));
        for _ in 0..RESTORE_SYNC_FRAMES {
            stack.observe(&7, false);
            assert_eq!(stack.apply(UndoAction::Undo, &7), None);
        }
        stack.observe(&7, true);
        assert_eq!(stack.len(), 0);
        settle(&mut stack, 8);
        assert_eq!(stack.undo(&8), Some(7));
    }

    #[test]
    fn a_restore_that_lands_late_is_not_recorded_as_an_edit() {
        let mut stack = UndoStack::default();
        settle(&mut stack, 0);
        settle(&mut stack, 1);
        assert_eq!(stack.undo(&1), Some(0));
        settle(&mut stack, 1);
        settle(&mut stack, 0);
        assert!(stack.can_redo());
        assert_eq!(stack.len(), 0);
    }

    proptest! {
        #[test]
        fn the_bound_holds(limit in 1usize..20, edits in 0u32..200) {
            let mut stack = UndoStack::with_limit(limit);
            for value in 0..=edits {
                stack.observe(&value, true);
            }
            prop_assert_eq!(stack.len(), (edits as usize).min(limit));
        }
    }
}
