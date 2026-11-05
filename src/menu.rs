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

/// Where the cursor can be in `q`: where each grapheme starts, as a row draws them, and its end.
fn stops(q: &str) -> Vec<usize> {
    let span = Span::raw(q);
    let starts = span.styled_graphemes(Style::new());
    let starts = starts.map(|g| g.symbol.as_ptr() as usize - q.as_ptr() as usize);
    starts.chain([q.len()]).collect()
}

/// A word to ^w, as in readline: back to a space. The other word keys go by letters and digits.
fn spaced(c: char) -> bool {
    !c.is_whitespace()
}

/// Where the word before `at` starts: back over what's between words, then over the word.
fn word_before(q: &str, at: usize, word: fn(char) -> bool) -> usize {
    let stops = stops(q);
    let is_word = |s: &&usize| q[**s..].chars().next().is_some_and(word);
    let back = stops.iter().filter(|s| **s < at).rev();
    let start = back.skip_while(|s| !is_word(s)).take_while(is_word).last();
    start.map_or(0, |s| *s)
}

fn word_start(q: &str, at: usize) -> usize {
    word_before(q, at, char::is_alphanumeric)
}

/// Where the word at or after `at` ends.
fn word_end(q: &str, at: usize) -> usize {
    let stops = stops(q);
    let is_word = |s: &&usize| q[**s..].chars().next().is_some_and(char::is_alphanumeric);
    let on = stops.iter().filter(|s| **s >= at);
    let end = on.skip_while(|s| !is_word(s)).find(|s| !is_word(s));
    end.map_or(q.len(), |s| *s)
}

pub enum Act {
    Accept,
    Back,
}

pub struct Menu {
    pub items: Vec<Item>,
    pub query: String,
    /// where typing goes in `query`, in bytes
    pub at: usize,
    /// (item index, matched char positions), best match first
    pub matches: Vec<(usize, Vec<u32>)>,
    pub marked: BTreeSet<usize>,
    pub multi: bool,
    /// a question has no prompt: what's typed is out of sight, so no key acts on it
    pub hidden: bool,
    pub state: ListState,
    matcher: Matcher,
}

impl Menu {
    pub fn new(items: Vec<Item>, multi: bool) -> Self {
        let mut m = Menu {
            items: Vec::new(),
            query: String::new(),
            at: 0,
            matches: Vec::new(),
            marked: BTreeSet::new(),
            multi,
            hidden: false,
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

    /// The cursor one row down, if there's one: no wrapping round to the first.
    pub fn down(&mut self) {
        if let Some(i) = (self.state.selected()).filter(|i| i + 1 < self.matches.len()) {
            self.state.select(Some(i + 1));
        }
    }

    fn toggle(&mut self) {
        if let (true, Some(i)) = (self.multi, self.current_index())
            && !self.items[i].action
            && !self.marked.remove(&i)
        {
            self.marked.insert(i);
        }
    }

    fn back_one(&self) -> usize {
        let stops = stops(&self.query);
        stops.into_iter().rfind(|&s| s < self.at).unwrap_or(0)
    }

    fn on_one(&self) -> usize {
        let stops = stops(&self.query);
        let next = stops.into_iter().find(|&s| s > self.at);
        next.unwrap_or(self.query.len())
    }

    /// Whether there's text on screen for the keys to edit.
    pub fn typed(&self) -> bool {
        !self.hidden && !self.query.is_empty()
    }

    pub fn key(&mut self, k: KeyEvent) -> Option<Act> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let (at, end, typed) = (self.at, self.query.len(), self.typed());
        // The word keys as readline and fzf have them. While there's text the keys a list uses
        // too only edit it, so holding one down can't run into the list. Ghostty on a Mac sends
        // cmd+← → ⌫ as ^a ^e ^u and option+← → as alt+b alt+f, and everywhere ctrl+⌫ as ^h
        match k.code {
            KeyCode::Enter => return Some(Act::Accept),
            KeyCode::Char('l') if ctrl => return Some(Act::Accept),
            KeyCode::Char('h') if ctrl && typed => self.cut(word_start(&self.query, at), at),
            KeyCode::Char('h') if ctrl => return Some(Act::Back),
            KeyCode::Left if !typed => return Some(Act::Back),
            KeyCode::Left if ctrl || alt => self.at = word_start(&self.query, at),
            KeyCode::Char('b') if alt => self.at = word_start(&self.query, at),
            KeyCode::Left => self.at = self.back_one(),
            KeyCode::Char('b') if ctrl => self.at = self.back_one(),
            KeyCode::Right if !typed => return Some(Act::Accept),
            KeyCode::Right if ctrl || alt => self.at = word_end(&self.query, at),
            KeyCode::Char('f') if alt => self.at = word_end(&self.query, at),
            KeyCode::Right => self.at = self.on_one(),
            KeyCode::Char('f') if ctrl => self.at = self.on_one(),
            KeyCode::Esc if end > 0 => self.edit(String::clear),
            KeyCode::Esc => return Some(Act::Back),
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            // at the end of the text a shell's ^k has nothing to do, so it stays fzf's up
            KeyCode::Char('k') if ctrl && at < end => self.cut(at, end),
            KeyCode::Char('p' | 'k') if ctrl => self.step(-1),
            KeyCode::Char('n' | 'j') if ctrl => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Home if typed => self.at = 0,
            KeyCode::Home => self.state.select(Some(0)),
            KeyCode::End if typed => self.at = end,
            KeyCode::End => self.step(isize::MAX / 2),
            KeyCode::Tab => {
                self.toggle();
                self.step(1);
            }
            KeyCode::BackTab => {
                self.toggle();
                self.step(-1);
            }
            KeyCode::Char('a') if ctrl => self.at = 0,
            KeyCode::Char('e') if ctrl => self.at = end,
            KeyCode::Char('t') if ctrl && self.multi => {
                let visible: Vec<usize> = (self.matches.iter().map(|m| m.0))
                    .filter(|&i| !self.items[i].action)
                    .collect();
                if visible.iter().all(|i| self.marked.contains(i)) {
                    visible.iter().for_each(|i| _ = self.marked.remove(i));
                } else {
                    self.marked.extend(visible);
                }
            }
            KeyCode::Char('u') if ctrl => self.cut(0, at),
            KeyCode::Char('w') if ctrl => self.cut(word_before(&self.query, at, spaced), at),
            KeyCode::Backspace if ctrl || alt => self.cut(word_start(&self.query, at), at),
            KeyCode::Backspace => self.cut(self.back_one(), at),
            KeyCode::Delete if ctrl || alt => self.cut(at, word_end(&self.query, at)),
            KeyCode::Char('d') if alt => self.cut(at, word_end(&self.query, at)),
            KeyCode::Delete => self.cut(at, self.on_one()),
            KeyCode::Char('d') if ctrl => self.cut(at, self.on_one()),
            KeyCode::Char(c) if !ctrl => self.put(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        // a cursor nobody can see stays after what's typed
        if self.hidden {
            self.at = self.query.len();
        }
        None
    }

    /// On one line, as a prompt is. Its spaces stay, and what's submitted gets trimmed.
    pub fn paste(&mut self, text: &str) {
        self.put(&text.replace(char::is_control, " "));
    }

    /// Replaces the text, with the cursor after it.
    pub fn edit(&mut self, f: impl FnOnce(&mut String)) {
        f(&mut self.query);
        self.at = self.query.len();
        self.filter();
    }

    /// `text` where the cursor is, with the cursor after it.
    fn put(&mut self, text: &str) {
        self.query.insert_str(self.at, text);
        self.changed(self.at + text.len());
    }

    /// Takes `from..to` out of the text. Nothing to take leaves the list where it is too.
    fn cut(&mut self, from: usize, to: usize) {
        if from < to {
            self.query.replace_range(from..to, "");
            self.changed(from);
        }
    }

    /// The text changed, with the cursor at `at`, or after what that's now inside of: two
    /// halves of a flag join into one.
    fn changed(&mut self, at: usize) {
        let stops = stops(&self.query);
        self.at = stops
            .into_iter()
            .find(|&s| s >= at)
            .unwrap_or(self.query.len());
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
        m.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
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
        m.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
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

    #[test]
    fn every_way_of_deleting_a_word_deletes_one() {
        let (ctrl, alt) = (KeyModifiers::CONTROL, KeyModifiers::ALT);
        let keys = [
            (KeyCode::Char('w'), ctrl),
            (KeyCode::Char('h'), ctrl), // how most terminals send ctrl+backspace
            (KeyCode::Backspace, ctrl),
            (KeyCode::Backspace, alt),
        ];
        for (code, mods) in keys {
            let mut m = Menu::new(Vec::new(), false);
            m.edit(|q| *q = "attack on titan".into());
            assert!(m.key(KeyEvent::new(code, mods)).is_none());
            assert_eq!(m.query, "attack on ", "{code:?}");
            m.key(KeyEvent::new(code, mods));
            assert_eq!(m.query, "attack ", "{code:?}");
        }
        // a path goes a folder at a time, down to nothing, and all at once with ^w
        for (code, mods) in &keys[1..] {
            let mut m = typed("~/Videos/Anime");
            for left in ["~/Videos/", "~/", ""] {
                m.key(KeyEvent::new(*code, *mods));
                assert_eq!(m.query, left, "{code:?}");
            }
        }
        let mut m = typed("~/Videos/Anime");
        m.key(KeyEvent::new(KeyCode::Char('w'), ctrl));
        assert_eq!(m.query, "");
        // ^w goes back to a space, the others stop at anything that isn't a letter
        let mut m = Menu::new(Vec::new(), false);
        m.edit(|q| *q = "kaguya-sama".into());
        m.key(KeyEvent::new(KeyCode::Backspace, alt));
        assert_eq!(m.query, "kaguya-");
        m.edit(|q| *q = "kaguya-sama".into());
        m.key(KeyEvent::new(KeyCode::Char('w'), ctrl));
        assert_eq!(m.query, "");
        // text that isn't ASCII is cut between characters
        m.edit(|q| *q = "葬送の フリーレン".into());
        m.key(KeyEvent::new(KeyCode::Char('h'), ctrl));
        assert_eq!(m.query, "葬送の ");
        // with nothing typed there's no word to delete, and they go back
        m.edit(String::clear);
        for (code, mods) in [
            (KeyCode::Char('h'), ctrl),
            (KeyCode::Left, ctrl),
            (KeyCode::Left, KeyModifiers::NONE),
        ] {
            let act = m.key(KeyEvent::new(code, mods));
            assert!(matches!(act, Some(Act::Back)), "{code:?}");
        }
    }

    /// `text` with `|` where the cursor is.
    fn at(m: &Menu) -> String {
        format!("{}|{}", &m.query[..m.at], &m.query[m.at..])
    }

    fn typed(text: &str) -> Menu {
        let mut m = Menu::new(Vec::new(), false);
        m.edit(|q| *q = text.into());
        m
    }

    /// The keys a shell's prompt has, as the terminal sends them: each moves or deletes from
    /// the middle of the text and nothing else on the screen happens.
    #[test]
    fn a_prompt_edits_text_as_a_shell_does() {
        let (none, ctrl, alt) = (KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT);
        let keys: &[(KeyCode, KeyModifiers, &str)] = &[
            // moving a character
            (KeyCode::Left, none, "attack o|n titan"),
            (KeyCode::Char('b'), ctrl, "attack o|n titan"),
            (KeyCode::Right, none, "attack on |titan"),
            (KeyCode::Char('f'), ctrl, "attack on |titan"),
            // moving a word: ctrl+arrows on Linux, option+arrows on a Mac, which is alt+b, alt+f
            (KeyCode::Left, ctrl, "attack |on titan"),
            (KeyCode::Left, alt, "attack |on titan"),
            (KeyCode::Char('b'), alt, "attack |on titan"),
            (KeyCode::Right, ctrl, "attack on titan|"),
            (KeyCode::Right, alt, "attack on titan|"),
            (KeyCode::Char('f'), alt, "attack on titan|"),
            // to either end: cmd+arrows on a Mac are ^a, ^e
            (KeyCode::Home, none, "|attack on titan"),
            (KeyCode::Char('a'), ctrl, "|attack on titan"),
            (KeyCode::End, none, "attack on titan|"),
            (KeyCode::Char('e'), ctrl, "attack on titan|"),
            // deleting a character
            (KeyCode::Backspace, none, "attack o| titan"),
            (KeyCode::Delete, none, "attack on|titan"),
            (KeyCode::Char('d'), ctrl, "attack on|titan"),
            // deleting a word
            (KeyCode::Char('w'), ctrl, "attack | titan"),
            (KeyCode::Backspace, alt, "attack | titan"),
            (KeyCode::Char('h'), ctrl, "attack | titan"),
            (KeyCode::Char('d'), alt, "attack on|"),
            (KeyCode::Delete, ctrl, "attack on|"),
            (KeyCode::Delete, alt, "attack on|"),
            // deleting to either end: cmd+backspace on a Mac is ^u
            (KeyCode::Char('u'), ctrl, "| titan"),
            (KeyCode::Char('k'), ctrl, "attack on|"),
        ];
        for (code, mods, want) in keys {
            let mut m = typed("attack on titan");
            m.at = "attack on".len();
            let act = m.key(KeyEvent::new(*code, *mods));
            assert!(act.is_none(), "{code:?} {mods:?} left the prompt");
            assert_eq!(at(&m), *want, "{code:?} {mods:?}");
        }
        // typing and pasting go where the cursor is
        let mut m = typed("attack titan");
        m.at = "attack ".len();
        press(&mut m, KeyCode::Char('o'));
        m.paste("n ");
        assert_eq!(at(&m), "attack on |titan");
        // on one line, with its spaces
        let mut m = typed("ab");
        m.at = 1;
        m.paste(" x\ny ");
        assert_eq!(at(&m), "a x y |b");
        // a whole grapheme at a time, and no further than the text goes
        let mut m = typed("e\u{301}葬");
        press(&mut m, KeyCode::Left);
        assert_eq!(at(&m), "e\u{301}|葬");
        press(&mut m, KeyCode::Left);
        assert_eq!(at(&m), "|e\u{301}葬");
        press(&mut m, KeyCode::Delete);
        assert_eq!(at(&m), "|葬");
        m.key(KeyEvent::new(KeyCode::Char('b'), ctrl));
        assert_eq!(at(&m), "|葬");
        // what joins up around the cursor leaves it after the whole of it
        let mut m = typed("🇷");
        press(&mut m, KeyCode::Home);
        m.paste("🇫");
        assert_eq!(at(&m), "🇫🇷|");
        let mut m = typed("🇫x🇷");
        m.at = "🇫x".len();
        press(&mut m, KeyCode::Backspace);
        assert_eq!(at(&m), "🇫🇷|");
    }

    #[test]
    fn while_theres_text_the_keys_a_list_uses_too_only_edit_it() {
        let (ctrl, alt) = (KeyModifiers::CONTROL, KeyModifiers::ALT);
        let mut m = menu();
        m.edit(|q| *q = "fr".into());
        press(&mut m, KeyCode::Down);
        let row = m.state.selected();
        assert_eq!(row, Some(1));
        // held down they stop at the text's ends, and the list stays where it is
        for code in [KeyCode::Right, KeyCode::End, KeyCode::Delete] {
            for mods in [KeyModifiers::NONE, ctrl, alt] {
                for _ in 0..3 {
                    assert!(m.key(KeyEvent::new(code, mods)).is_none(), "{code:?}");
                    assert_eq!(at(&m), "fr|", "{code:?}");
                    assert_eq!(m.state.selected(), row, "{code:?}");
                }
            }
        }
        m.key(KeyEvent::new(KeyCode::Char('f'), ctrl));
        assert_eq!(at(&m), "fr|");
        for code in [KeyCode::Left, KeyCode::Home, KeyCode::Backspace] {
            for mods in [KeyModifiers::NONE, ctrl, alt] {
                m.at = 0;
                for _ in 0..3 {
                    assert!(m.key(KeyEvent::new(code, mods)).is_none(), "{code:?}");
                    assert_eq!(at(&m), "|fr", "{code:?}");
                    assert_eq!(m.state.selected(), row, "{code:?}");
                }
            }
        }
        // ^a only moves, so it can't mark what a cmd+← didn't mean to, or lose marks made by hand
        press(&mut m, KeyCode::Tab);
        assert_eq!(m.marked.len(), 1);
        for _ in 0..3 {
            m.key(KeyEvent::new(KeyCode::Char('a'), ctrl));
            assert_eq!((at(&m).as_str(), m.marked.len()), ("|fr", 1));
        }
        // ^t marks what the filter left, whatever the cursor is doing, and again unmarks it
        m.key(KeyEvent::new(KeyCode::Char('t'), ctrl));
        assert_eq!(m.marked.len(), m.matches.len());
        m.key(KeyEvent::new(KeyCode::Char('t'), ctrl));
        assert!(m.marked.is_empty());
        // ^k deletes what's after the cursor, and with nothing after it it's up, as in fzf
        m.at = 1;
        m.state.select(Some(1));
        m.key(KeyEvent::new(KeyCode::Char('k'), ctrl));
        assert_eq!(at(&m), "f|");
        m.state.select(Some(1));
        m.key(KeyEvent::new(KeyCode::Char('k'), ctrl));
        assert_eq!((at(&m).as_str(), m.state.selected()), ("f|", Some(0)));
        // enter and ^l select from anywhere in it
        m.at = 0;
        assert!(matches!(
            m.key(KeyEvent::from(KeyCode::Enter)),
            Some(Act::Accept)
        ));
        assert!(matches!(
            m.key(KeyEvent::new(KeyCode::Char('l'), ctrl)),
            Some(Act::Accept)
        ));
    }

    #[test]
    fn with_no_text_on_screen_the_keys_act_on_the_list() {
        let ctrl = KeyModifiers::CONTROL;
        for hidden in [false, true] {
            let mut m = menu();
            m.hidden = hidden;
            if hidden {
                // what a question has typed is out of sight, wherever its cursor is
                m.edit(|q| *q = "f".into());
                m.at = 0;
            }
            let last = Some(m.matches.len() - 1);
            press(&mut m, KeyCode::End);
            assert_eq!(m.state.selected(), last, "{hidden}");
            press(&mut m, KeyCode::Home);
            assert_eq!(m.state.selected(), Some(0), "{hidden}");
            let keys = [
                (KeyCode::Left, KeyModifiers::NONE, false),
                (KeyCode::Left, ctrl, false),
                (KeyCode::Char('h'), ctrl, false),
                (KeyCode::Right, KeyModifiers::NONE, true),
                (KeyCode::Right, ctrl, true),
            ];
            for (code, mods, accepts) in keys {
                match m.key(KeyEvent::new(code, mods)) {
                    Some(Act::Accept) => assert!(accepts, "{code:?}"),
                    Some(Act::Back) => assert!(!accepts, "{code:?}"),
                    None => panic!("{code:?} did nothing with {hidden}"),
                }
            }
        }
    }

    /// Whatever's pressed in whatever order, the cursor stays between graphemes.
    #[test]
    fn no_key_leaves_the_cursor_inside_a_grapheme() {
        let (none, ctrl, alt) = (KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT);
        let mut keys = Vec::new();
        for mods in [none, ctrl, alt] {
            let codes = [
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Backspace,
                KeyCode::Delete,
                KeyCode::Char('a'),
                KeyCode::Char('b'),
                KeyCode::Char('d'),
                KeyCode::Char('e'),
                KeyCode::Char('f'),
                KeyCode::Char('h'),
                KeyCode::Char('k'),
                KeyCode::Char('u'),
                KeyCode::Char('w'),
                KeyCode::Char('é'),
            ];
            keys.extend(codes.map(|code| KeyEvent::new(code, mods)));
        }
        // a mark on its own, an emoji of several chars, flags
        let text = "\u{301}葬 e\u{301}/ab 👨‍👩‍👧 🇩🇪🇫🇷";
        for start in [0, text.find('/').unwrap(), text.len()] {
            for first in &keys {
                for second in &keys {
                    let mut m = typed(text);
                    m.at = start;
                    for k in [first, second] {
                        m.key(*k);
                        assert!(stops(&m.query).contains(&m.at), "{k:?} in {:?}", m.query);
                    }
                }
            }
        }
    }
}
