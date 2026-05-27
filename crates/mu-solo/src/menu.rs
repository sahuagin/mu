//! Inline filtered menu — reusable dropdown for slash commands, model
//! pickers, session pickers, etc.
//!
//! Renders above the input bar in the terminal's inline viewport.
//! Filters as the user types. Arrow keys navigate, Enter selects,
//! Esc dismisses. Scrolls when the list exceeds available height.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One item in the menu.
#[derive(Debug, Clone)]
pub struct MenuItem {
    /// Short label displayed on the left (e.g. "/help", "opus-4-7").
    pub name: String,
    /// Longer description displayed on the right, truncated to fit.
    pub description: String,
}

impl MenuItem {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Result of a key event processed by the menu.
pub enum MenuAction {
    /// Menu consumed the key, keep it open. Caller should re-render.
    Continue,
    /// User selected an item. Menu should close.
    Select(usize),
    /// User dismissed the menu (Esc). Menu should close.
    Dismiss,
}

/// Inline filtered menu state.
pub struct InlineMenu {
    /// All available items (unfiltered).
    items: Vec<MenuItem>,
    /// Current filter string (what the user typed after the trigger).
    filter: String,
    /// Indices into `items` that match the current filter.
    filtered: Vec<usize>,
    /// Cursor position within `filtered`.
    cursor: usize,
    /// Scroll offset — first visible item index in `filtered`.
    scroll: usize,
    /// Max visible rows (set by caller based on available terminal height).
    max_visible: usize,
}

impl InlineMenu {
    /// Create a new menu with the given items. Filter starts empty
    /// (all items visible).
    pub fn new(items: Vec<MenuItem>, max_visible: usize) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        Self {
            items,
            filter: String::new(),
            filtered,
            cursor: 0,
            scroll: 0,
            max_visible: max_visible.max(1),
        }
    }

    /// Process a key event. Returns what the caller should do.
    pub fn handle_key(&mut self, key: KeyEvent) -> MenuAction {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => MenuAction::Dismiss,
            (_, KeyCode::Esc) => MenuAction::Dismiss,
            (_, KeyCode::Enter) => {
                if let Some(&original_idx) = self.filtered.get(self.cursor) {
                    MenuAction::Select(original_idx)
                } else {
                    MenuAction::Dismiss
                }
            }
            (_, KeyCode::Up) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.ensure_visible();
                }
                MenuAction::Continue
            }
            (_, KeyCode::Down) => {
                if self.cursor + 1 < self.filtered.len() {
                    self.cursor += 1;
                    self.ensure_visible();
                }
                MenuAction::Continue
            }
            (_, KeyCode::Backspace) => {
                if self.filter.pop().is_some() {
                    self.refilter();
                    MenuAction::Continue
                } else {
                    MenuAction::Dismiss
                }
            }
            (_, KeyCode::Char(' ')) if self.filtered.len() == 1 => {
                // Space with a unique match: select the item so the
                // caller can insert command + space for arg entry.
                let idx = self.filtered[0];
                MenuAction::Select(idx)
            }
            (_, KeyCode::Char(c)) => {
                self.filter.push(c);
                self.refilter();
                if self.filtered.is_empty() {
                    MenuAction::Dismiss
                } else {
                    MenuAction::Continue
                }
            }
            _ => MenuAction::Continue,
        }
    }

    /// The current filter string.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Visible slice of filtered items for rendering.
    /// Returns (items, cursor_within_visible, has_more_above, has_more_below).
    pub fn visible_items(&self) -> (Vec<(usize, &MenuItem)>, usize, bool, bool) {
        let end = (self.scroll + self.max_visible).min(self.filtered.len());
        let visible: Vec<(usize, &MenuItem)> = self.filtered[self.scroll..end]
            .iter()
            .map(|&idx| (idx, &self.items[idx]))
            .collect();
        let cursor_in_view = self.cursor.saturating_sub(self.scroll);
        let has_above = self.scroll > 0;
        let has_below = end < self.filtered.len();
        (visible, cursor_in_view, has_above, has_below)
    }

    /// Total number of filtered items.
    pub fn filtered_count(&self) -> usize {
        self.filtered.len()
    }

    /// Total number of items (unfiltered).
    pub fn total_count(&self) -> usize {
        self.items.len()
    }

    /// Update max visible rows (e.g. on terminal resize).
    pub fn set_max_visible(&mut self, max: usize) {
        self.max_visible = max.max(1);
        self.ensure_visible();
    }

    fn refilter(&mut self) {
        let lower_filter = self.filter.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if lower_filter.is_empty() {
                    true
                } else {
                    item.name.to_lowercase().contains(&lower_filter)
                }
            })
            .map(|(i, _)| i)
            .collect();
        // Clamp cursor
        if self.filtered.is_empty() {
            self.cursor = 0;
        } else if self.cursor >= self.filtered.len() {
            self.cursor = self.filtered.len() - 1;
        }
        self.scroll = 0;
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor >= self.scroll + self.max_visible {
            self.scroll = self.cursor + 1 - self.max_visible;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("/help", "Show help for commands"),
            MenuItem::new("/model", "Change the model"),
            MenuItem::new("/provider", "Change the provider"),
            MenuItem::new("/quit", "Exit mu-solo"),
            MenuItem::new("/status", "Show session status"),
            MenuItem::new("/clear", "Clear and start fresh"),
            MenuItem::new("/effort", "Set effort level"),
            MenuItem::new("/focus", "Toggle focus mode"),
            MenuItem::new("/goal-protocol", "Set up a goal session"),
        ]
    }

    #[test]
    fn unfiltered_shows_all() {
        let menu = InlineMenu::new(test_items(), 20);
        assert_eq!(menu.filtered_count(), 9);
    }

    #[test]
    fn filter_narrows() {
        let mut menu = InlineMenu::new(test_items(), 20);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        menu.handle_key(key('m'));
        assert_eq!(menu.filter(), "m");
        // "model" and "mode" match
        assert!(menu.filtered_count() < 9);
        assert!(menu.filtered_count() >= 1);
    }

    #[test]
    fn backspace_widens() {
        let mut menu = InlineMenu::new(test_items(), 20);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        menu.handle_key(key('q'));
        let narrow = menu.filtered_count();
        menu.handle_key(bs);
        assert!(menu.filtered_count() > narrow);
    }

    #[test]
    fn backspace_on_empty_dismisses() {
        let mut menu = InlineMenu::new(test_items(), 20);
        let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(menu.handle_key(bs), MenuAction::Dismiss));
    }

    #[test]
    fn enter_selects() {
        let mut menu = InlineMenu::new(test_items(), 20);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        match menu.handle_key(enter) {
            MenuAction::Select(idx) => assert_eq!(idx, 0),
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn arrow_navigation() {
        let mut menu = InlineMenu::new(test_items(), 20);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        menu.handle_key(down);
        menu.handle_key(down);
        match menu.handle_key(enter) {
            MenuAction::Select(idx) => assert_eq!(idx, 2),
            _ => panic!("expected Select(2)"),
        }
    }

    #[test]
    fn scroll_with_small_viewport() {
        let mut menu = InlineMenu::new(test_items(), 3);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        // Move down past visible window
        for _ in 0..5 {
            menu.handle_key(down);
        }
        let (visible, cursor_in_view, has_above, has_below) = menu.visible_items();
        assert_eq!(visible.len(), 3);
        assert!(has_above);
        assert!(has_below);
        assert!(cursor_in_view < 3);
    }
}
