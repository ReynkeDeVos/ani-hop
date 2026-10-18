//! fzf-style list: type to filter (fzf syntax via nucleo), tab to mark, enter to accept.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;
use std::collections::BTreeSet;

pub struct Item {
    pub label: String,
    pub hint: String,
    pub key: String,
    /// already watched: shown with a check and dimmed
    pub done: bool,
}

impl Item {
    pub fn new(key: impl Into<String>, label: impl Into<String>, hint: impl Into<String>) -> Self {
        Item {
            key: key.into(),
            label: label.into(),
            hint: hint.into(),
            done: false,
        }
    }
}

pub enum Act {
    Accept,
    Back,
}

pub struct Menu {
    pub items: Vec<Item>,
    pub query: String,
    /// (item index, matched char positions), best match first
    pub matches: Vec<(usize, Vec<u32>)>,
    pub marked: BTreeSet<usize>,
    pub multi: bool,
    pub state: ListState,
    matcher: Matcher,
}

impl Menu {
    pub fn new(items: Vec<Item>, multi: bool) -> Self {
        let mut m = Menu {
            items: Vec::new(),
            query: String::new(),
            matches: Vec::new(),
            marked: BTreeSet::new(),
            multi,
            state: ListState::default(),
            matcher: Matcher::new(Config::DEFAULT),
        };
        m.set_items(items);
        m
    }

    /// Replaces the items, keeping query and cursor position where possible.
    pub fn set_items(&mut self, items: Vec<Item>) {
        let keep = self.current().map(|i| i.key.clone());
        self.items = items;
        self.marked.retain(|&i| i < self.items.len());
        self.filter();
        if let Some(k) = keep {
            self.select_key(&k);
        }
    }

    pub fn select_key(&mut self, key: &str) {
        if let Some(pos) = self
            .matches
            .iter()
            .position(|(i, _)| self.items[*i].key == key)
        {
            self.state.select(Some(pos));
        }
    }

    fn filter(&mut self) {
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32, Vec<u32>)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let mut idx = Vec::new();
            let hay = Utf32Str::new(&item.label, &mut buf);
            if let Some(score) = pattern.indices(hay, &mut self.matcher, &mut idx) {
                idx.sort_unstable();
                idx.dedup();
                scored.push((i, score, idx));
            }
        }
        scored.sort_by_key(|s| std::cmp::Reverse(s.1)); // stable: ties keep list order
        self.matches = scored.into_iter().map(|(i, _, idx)| (i, idx)).collect();
        self.state.select((!self.matches.is_empty()).then_some(0));
    }

    pub fn current_index(&self) -> Option<usize> {
        self.matches.get(self.state.selected()?).map(|m| m.0)
    }

    pub fn current(&self) -> Option<&Item> {
        self.current_index().map(|i| &self.items[i])
    }

    /// Marked items, or the one under the cursor if nothing is marked.
    pub fn selection(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            self.current_index().into_iter().collect()
        } else {
            self.marked.iter().copied().collect()
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.matches.len() as isize;
        if n == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as isize;
        // wrap around at the ends like `fzf --cycle`, but not when paging
        let next = if delta.abs() == 1 {
            (cur + delta).rem_euclid(n)
        } else {
            (cur + delta).clamp(0, n - 1)
        };
        self.state.select(Some(next as usize));
    }

    fn toggle(&mut self) {
        if let (true, Some(i)) = (self.multi, self.current_index())
            && !self.marked.remove(&i)
        {
            self.marked.insert(i);
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Option<Act> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter => return Some(Act::Accept),
            KeyCode::Esc if !self.query.is_empty() => self.edit(String::clear),
            KeyCode::Esc => return Some(Act::Back),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('p' | 'k') if ctrl => self.step(-1),
            KeyCode::Char('n' | 'j') if ctrl => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Home => self.state.select(Some(0)),
            KeyCode::End => self.step(isize::MAX / 2),
            KeyCode::Tab => {
                self.toggle();
                self.step(1);
            }
            KeyCode::BackTab => {
                self.toggle();
                self.step(-1);
            }
            KeyCode::Char('a') if ctrl && self.multi => {
                let visible: Vec<usize> = self.matches.iter().map(|m| m.0).collect();
                if visible.iter().all(|i| self.marked.contains(i)) {
                    visible.iter().for_each(|i| _ = self.marked.remove(i));
                } else {
                    self.marked.extend(visible);
                }
            }
            KeyCode::Char('u') if ctrl => self.edit(String::clear),
            KeyCode::Char('w') if ctrl => self.edit(|q| {
                let trimmed = q.trim_end().len();
                q.truncate(q[..trimmed].rfind(' ').map_or(0, |p| p + 1));
            }),
            KeyCode::Backspace => self.edit(|q| _ = q.pop()),
            KeyCode::Char(c) if !ctrl => self.edit(|q| q.push(c)),
            _ => {}
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        self.edit(|q| q.push_str(text.trim()));
    }

    pub fn edit(&mut self, f: impl FnOnce(&mut String)) {
        f(&mut self.query);
        self.filter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn menu() -> Menu {
        let items = [
            "Frieren",
            "Attack on Titan",
            "Fire Force",
            "Frieren Season 2",
        ];
        Menu::new(items.iter().map(|l| Item::new(*l, *l, "")).collect(), true)
    }

    fn press(m: &mut Menu, code: KeyCode) {
        m.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn filters_with_fzf_syntax() {
        let mut m = menu();
        "frn".chars().for_each(|c| press(&mut m, KeyCode::Char(c)));
        let labels: Vec<_> = m
            .matches
            .iter()
            .map(|(i, _)| m.items[*i].label.as_str())
            .collect();
        assert_eq!(labels, ["Frieren", "Frieren Season 2"]);
        m.query = "!season fri".into();
        m.filter();
        assert_eq!(m.matches.len(), 1);
    }

    #[test]
    fn tab_marks_and_selection_falls_back_to_cursor() {
        let mut m = menu();
        assert_eq!(m.selection(), [0]);
        press(&mut m, KeyCode::Tab);
        press(&mut m, KeyCode::Down);
        press(&mut m, KeyCode::Tab);
        assert_eq!(m.selection(), [0, 2]);
        m.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(m.selection(), [0, 1, 2, 3]);
        press(&mut m, KeyCode::Up);
        press(&mut m, KeyCode::Up);
        press(&mut m, KeyCode::Up);
        press(&mut m, KeyCode::Up);
        assert_eq!(m.current_index(), Some(3)); // wrapped around
    }
}
