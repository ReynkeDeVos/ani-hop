//! fzf-style list: type to filter (fzf syntax via nucleo), tab to mark, enter to accept.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::ListState;
use std::collections::BTreeSet;

pub struct Item {
    pub label: String,
    pub hint: String,
    pub key: String,
    /// already watched: shown with a check and dimmed
    pub done: bool,
    /// found by the filter too, but not shown (e.g. what an episode is about)
    pub about: String,
    /// a command like "More…", not something to pick: can't be marked, isn't counted
    pub action: bool,
    /// rows of another group start under a faint rule naming it (a sequel's episodes, a
    /// TheTVDB season)
    pub group: String,
    /// the hint said shorter, for a row too narrow for it ("subtitled")
    pub short: String,
}

impl Item {
    pub fn new(key: impl Into<String>, label: impl Into<String>, hint: impl Into<String>) -> Self {
        Item {
            key: key.into(),
            label: label.into(),
            hint: hint.into(),
            done: false,
            about: String::new(),
            action: false,
            group: String::new(),
            short: String::new(),
        }
    }
}

/// `s` as the matcher reads it for an atom of `kind`: one char per grapheme, its first, so
/// where it matched counts graphemes as a row draws them. nucleo's own `Utf32Str::new` falls
/// back to counting bytes when every grapheme starts with an ASCII char, as in a decomposed
/// "é". Around three bugs in nucleo-matcher 0.3.1: ASCII text stays bytes, as chars score a
/// one-letter atom by the wrong neighbour, but is chars for ^, $ and ^…$, which compare all of
/// the bytes when case-sensitive; and chars get one more that nothing matches for ' and !,
/// which never find a substring that ends them.
fn haystack<'a>(s: &'a str, kind: AtomKind, buf: &'a mut Vec<char>) -> Utf32Str<'a> {
    let exact = matches!(kind, AtomKind::Exact | AtomKind::Prefix | AtomKind::Postfix);
    if s.is_ascii() && !exact {
        return Utf32Str::Ascii(s.as_bytes());
    }
    buf.clear();
    match s.is_ascii() {
        true => buf.extend(s.chars()),
        false => {
            let span = Span::raw(s);
            let graphemes = span.styled_graphemes(Style::new());
            buf.extend(graphemes.filter_map(|g| g.symbol.chars().next()));
        }
    }
    if matches!(kind, AtomKind::Substring) {
        buf.push(END);
    }
    Utf32Str::Unicode(buf)
}

/// What ends a char haystack for ' and !: a noncharacter, taken out of every query.
const END: char = '\u{FFFF}';

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
        // marks stay with their items, wherever those move to
        let marked: Vec<String> = self
            .marked
            .iter()
            .map(|&i| self.items[i].key.clone())
            .collect();
        self.items = items;
        self.marked = (0..self.items.len())
            .filter(|&i| marked.contains(&self.items[i].key))
            .collect();
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
        let query = self.query.replace(END, "");
        let pattern = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
        // as fzf: terms around a lone | are one of them ("core go$ | rb$" is core and either)
        let mut groups: Vec<Vec<(Atom, Atom)>> = Vec::new();
        let mut or = false;
        for atom in pattern.atoms {
            // a plain one: '|, ^|, |$ and !| look for one
            let plain = !atom.negative && atom.kind == AtomKind::Fuzzy;
            if plain && matches!(atom.needle_text(), Utf32Str::Ascii(b"|")) {
                or = !groups.is_empty();
                continue;
            }
            // hidden text is long: fuzzy would match almost anything, so the term as typed
            let needle: String = atom.needle_text().chars().collect();
            let word = Atom::new(
                &needle,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Substring,
                false,
            );
            match groups.last_mut() {
                Some(g) if or => g.push((atom, word)),
                _ => groups.push(vec![(atom, word)]),
            }
            or = false;
        }
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32, Vec<u32>)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let mut idx = Vec::new();
            // each term in the label, as `Pattern::indices` with each atom's own haystack, else
            // in the hidden text, which ranks it after every label match; a ! term hides what
            // shows it
            let mut term = |(atom, word): &(Atom, Atom), idx: &mut Vec<u32>| {
                let hay = haystack(&item.label, atom.kind, &mut buf);
                let label = atom.indices(hay, &mut self.matcher, idx);
                if atom.negative || label.is_some() {
                    return label.map(u32::from);
                }
                let hay = haystack(&item.about, AtomKind::Substring, &mut buf);
                word.score(hay, &mut self.matcher).map(|_| 0)
            };
            let score = groups.iter().try_fold(0, |score, group| {
                let best = group.iter().filter_map(|t| {
                    let mut at = Vec::new();
                    term(t, &mut at).map(|s| (s, at))
                });
                let (s, at) = best.max_by_key(|(s, _)| *s)?;
                idx.extend(at);
                Some(score + s)
            });
            if let Some(score) = score {
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
            && !self.items[i].action
            && !self.marked.remove(&i)
        {
            self.marked.insert(i);
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Option<Act> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter | KeyCode::Right => return Some(Act::Accept),
            KeyCode::Char('l') if ctrl => return Some(Act::Accept),
            KeyCode::Left => return Some(Act::Back),
            KeyCode::Char('h') if ctrl => return Some(Act::Back),
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
                let visible: Vec<usize> = (self.matches.iter().map(|m| m.0))
                    .filter(|&i| !self.items[i].action)
                    .collect();
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
    fn marks_follow_items_and_skip_actions() {
        let mut m = menu();
        m.items[3].action = true;
        m.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(m.selection(), [0, 1, 2], "the action isn't marked");
        // a new item in front: the marks move along with theirs
        let mut items = vec![Item::new("new", "New", "")];
        items.extend(["Frieren", "Attack on Titan", "Fire Force"].map(|l| Item::new(l, l, "")));
        m.set_items(items);
        assert_eq!(m.selection(), [1, 2, 3]);
    }

    #[test]
    fn matches_count_graphemes_as_rows_draw_them() {
        // a decomposed "é" (e and an accent) is one grapheme
        let label = "Cafe\u{301} Axx";
        let mut m = Menu::new(vec![Item::new("k", label, "")], false);
        m.edit(|q| *q = "A".into());
        assert_eq!(m.matches[0].1, [5]);
        // and a word that ends it, as typed
        m.edit(|q| *q = "'Axx".into());
        assert_eq!(m.matches[0].1, [5, 6, 7]);
        m.edit(|q| *q = "!Axx".into());
        assert!(m.matches.is_empty());
    }

    #[test]
    fn starts_ends_and_last_words_match() {
        let items = ["Plain Start end", "Plain Axx", "Other", "Pokémon Horizons"];
        let mut m = Menu::new(items.iter().map(|l| Item::new(*l, *l, "")).collect(), false);
        let found = |m: &mut Menu, q: &str| {
            m.edit(|query| *query = q.into());
            let found = m.matches.iter().map(|(i, _)| m.items[*i].label.clone());
            found.collect::<Vec<_>>()
        };
        assert_eq!(found(&mut m, "^Plain"), ["Plain Start end", "Plain Axx"]);
        assert_eq!(found(&mut m, "Axx$"), ["Plain Axx"]);
        assert_eq!(found(&mut m, "^Other$"), ["Other"]);
        assert_eq!(found(&mut m, "'Horizons"), ["Pokémon Horizons"]);
        assert_eq!(found(&mut m, "!Horizons").len(), 3);
        // what an episode is about, by its last word
        m.items[2].about = "A town where everyone sleeps".into();
        assert_eq!(found(&mut m, "sleeps"), ["Other"]);
    }

    #[test]
    fn plain_titles_score_as_nucleo_scores_them() {
        let labels = ["Plain Axx", "ba a"];
        let mut m = Menu::new(
            labels.iter().map(|l| Item::new(*l, *l, "")).collect(),
            false,
        );
        m.edit(|q| *q = "a".into());
        let found: Vec<(&str, &[u32])> = (m.matches.iter())
            .map(|(i, idx)| (m.items[*i].label.as_str(), idx.as_slice()))
            .collect();
        let ascii = |s: &str| {
            let pattern = Pattern::parse("a", CaseMatching::Smart, Normalization::Smart);
            let hay = Utf32Str::Ascii(s.as_bytes());
            pattern
                .score(hay, &mut Matcher::new(Config::DEFAULT))
                .unwrap()
        };
        let mut order = labels;
        order.sort_by_key(|l| std::cmp::Reverse(ascii(l)));
        assert_eq!(found.iter().map(|f| f.0).collect::<Vec<_>>(), order);
        // the a after a space lights up, not the one inside a word
        assert!(found.contains(&("ba a", &[3][..])));
    }

    #[test]
    fn nothing_matches_past_the_end() {
        let labels = ["Solo", "Sólo"];
        let mut m = Menu::new(
            labels.iter().map(|l| Item::new(*l, *l, "")).collect(),
            false,
        );
        for q in ["Solo\\ ", "Solo\\ $", "'Solo\\ ", "'Sólo\\ ", "Sólo\\ $"] {
            m.edit(|query| *query = q.into());
            assert!(m.matches.is_empty(), "{q}");
        }
        // nor what ends them, pasted: it's as if it wasn't there, in what rows show or hide
        m.items[1].about = "Un día más".into();
        for q in ["'\u{FFFF}", "!\u{FFFF}", "Sólo\u{FFFF}", "más\u{FFFF}"] {
            m.edit(|query| *query = q.into());
            let without = q.replace('\u{FFFF}', "");
            let mut plain = Menu::new(
                labels.iter().map(|l| Item::new(*l, *l, "")).collect(),
                false,
            );
            plain.items[1].about = m.items[1].about.clone();
            plain.edit(|query| *query = without);
            assert_eq!(m.matches, plain.matches, "{q}");
        }
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
        let shown = |m: &mut Menu, q: &str| {
            m.edit(|query| *query = q.into());
            let mut labels: Vec<String> = (m.matches.iter())
                .map(|(i, _)| m.items[*i].label.clone())
                .collect();
            labels.sort();
            labels
        };
        assert_eq!(shown(&mut m, "frieren | titan").len(), 3);
        // either term, but with the others: f and (2$ or force$)
        assert_eq!(
            shown(&mut m, "f 2$ | force$"),
            ["Fire Force", "Frieren Season 2"]
        );
        assert_eq!(shown(&mut m, "| titan |"), ["Attack on Titan"]);
        m.items[0].label = "A | B |".into();
        for q in ["'|", "^A\\ |", "|$"] {
            assert_eq!(shown(&mut m, q), ["A | B |"], "{q}");
        }
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

    #[test]
    fn finds_hidden_text_word_by_word_after_labels() {
        let mut m = menu();
        m.items[3].about = "A town where everyone sleeps.".into();
        m.items[1].about = "The walls are on fire.".into();
        m.edit(|q| *q = "sleeps town".into());
        assert_eq!(m.current_index(), Some(3));
        assert_eq!(m.matches.len(), 1); // no fuzzy hits all over the text
        m.edit(|q| *q = "fire".into());
        let order: Vec<usize> = m.matches.iter().map(|(i, _)| *i).collect();
        assert_eq!(order.first(), Some(&2));
        assert_eq!(order.last(), Some(&1));
        // term by term: one in the label, one in the hidden text, as typed with fzf's syntax
        m.edit(|q| *q = "season sleeps".into());
        assert_eq!(m.current_index(), Some(3));
        m.edit(|q| *q = "'sleeps".into());
        assert_eq!(m.current_index(), Some(3));
        m.edit(|q| *q = "titan | sleeps".into());
        assert_eq!(m.matches.len(), 2);
        // what ! hides in a label, hidden text that has it doesn't bring back
        m.items[2].about = "A wall with !force painted on it.".into();
        m.edit(|q| *q = "!force".into());
        assert!(m.matches.iter().all(|(i, _)| *i != 2));
    }
}
