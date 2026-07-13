/// Result of a successful fuzzy match.
pub(crate) struct FuzzyMatch {
    /// Byte positions of matched characters in the haystack.
    pub positions: Vec<usize>,
}

/// Greedy left-to-right fuzzy match (case-insensitive).
///
/// Returns `Some` if every character in `query` appears in `haystack` in order.
/// An empty query matches everything with an empty positions vec.
pub(crate) fn fuzzy_match(query: &str, haystack: &str) -> Option<FuzzyMatch> {
    if query.is_empty() {
        return Some(FuzzyMatch {
            positions: Vec::new(),
        });
    }

    let mut positions = Vec::with_capacity(query.len());
    let mut haystack_chars = haystack.char_indices().peekable();
    for qc in query.chars() {
        let qc_lower = qc.to_lowercase().next()?;
        loop {
            match haystack_chars.next() {
                Some((byte_pos, hc)) => {
                    if hc.to_lowercase().next() == Some(qc_lower) {
                        positions.push(byte_pos);
                        break;
                    }
                }
                None => return None,
            }
        }
    }

    Some(FuzzyMatch { positions })
}

/// Type-to-filter state for a selector list (host / base-branch / agent
/// pickers): the typed query plus the indices of the rows that fuzzy-match it.
/// The selection cursor lives in *filtered* row space, so the edit methods take
/// it by `&mut` and remap it — the selected row stays selected while the match
/// set changes around it, snapping to the first match when it drops out.
///
/// Every edit refilters the whole list. That is deliberate: a greedy
/// subsequence scan over even thousands of rows costs microseconds, far below
/// one frame, so incremental bookkeeping would buy nothing.
#[derive(Debug, Clone, Default)]
pub(crate) struct FuzzyFilter {
    query: String,
    /// Matching rows in list order. Only meaningful while `query` is non-empty
    /// — an empty query is the identity filter, kept implicit so `default()`
    /// needs no list length.
    matches: Vec<usize>,
}

impl FuzzyFilter {
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    /// Number of selectable rows, given the unfiltered list length.
    pub(crate) fn len(&self, total: usize) -> usize {
        if self.is_active() {
            self.matches.len()
        } else {
            total
        }
    }

    /// Map a filtered-space position to its index in the unfiltered list.
    pub(crate) fn real_index(&self, pos: usize, total: usize) -> Option<usize> {
        if self.is_active() {
            self.matches.get(pos).copied()
        } else {
            (pos < total).then_some(pos)
        }
    }

    /// The unfiltered indices of the rows to render, in list order.
    pub(crate) fn visible_indices(&self, total: usize) -> Vec<usize> {
        if self.is_active() {
            self.matches.clone()
        } else {
            (0..total).collect()
        }
    }

    /// Append `c` to the query and refilter.
    pub(crate) fn push<I>(&mut self, c: char, items: I, index: &mut usize)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        self.edit(items, index, |q| q.push(c));
    }

    /// Delete the last query character and refilter (no-op when empty).
    pub(crate) fn pop<I>(&mut self, items: I, index: &mut usize)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        if self.is_active() {
            self.edit(items, index, |q| {
                q.pop();
            });
        }
    }

    /// Drop the whole query (Esc), mapping the cursor back to list space so the
    /// selected match stays selected. When the query had *no* matches there is
    /// no row under the cursor to preserve, so the cleared list selects its
    /// first row.
    pub(crate) fn clear(&mut self, index: &mut usize) {
        if !self.is_active() {
            return;
        }
        *index = self.matches.get(*index).copied().unwrap_or(0);
        self.query.clear();
        self.matches.clear();
    }

    /// Re-run the current query after the list itself changed underneath it
    /// (e.g. the branch list arriving from its background load).
    pub(crate) fn refilter<I>(&mut self, items: I, index: &mut usize)
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        self.edit(items, index, |_| {});
    }

    /// Shared edit path: note which row is selected, apply the query change,
    /// refilter, and remap `index` back onto that row (first match when gone).
    fn edit<I>(&mut self, items: I, index: &mut usize, change: impl FnOnce(&mut String))
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let kept = if self.is_active() {
            self.matches.get(*index).copied()
        } else {
            Some(*index)
        };
        change(&mut self.query);
        if !self.is_active() {
            self.matches.clear();
            *index = kept.unwrap_or(0);
            return;
        }
        self.matches = items
            .into_iter()
            .enumerate()
            .filter(|(_, item)| fuzzy_match(&self.query, item.as_ref()).is_some())
            .map(|(i, _)| i)
            .collect();
        *index = kept
            .and_then(|k| self.matches.iter().position(|&m| m == k))
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_match() {
        let m = fuzzy_match("fb", "foo-bar").unwrap();
        assert_eq!(m.positions, vec![0, 4]);
    }

    #[test]
    fn no_match() {
        assert!(fuzzy_match("xyz", "foo-bar").is_none());
    }

    #[test]
    fn case_insensitive() {
        let m = fuzzy_match("FB", "Foo-Bar").unwrap();
        assert_eq!(m.positions, vec![0, 4]);
    }

    #[test]
    fn empty_query_matches_everything() {
        let m = fuzzy_match("", "anything").unwrap();
        assert!(m.positions.is_empty());
    }

    #[test]
    fn empty_haystack_no_match() {
        assert!(fuzzy_match("a", "").is_none());
    }

    #[test]
    fn exact_match() {
        let m = fuzzy_match("abc", "abc").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2]);
    }

    #[test]
    fn partial_match_fails() {
        assert!(fuzzy_match("abz", "abc").is_none());
    }

    #[test]
    fn multibyte_characters() {
        let m = fuzzy_match("aé", "café").unwrap();
        // 'c'=0, 'a'=1, 'f'=2, 'é'=3 (byte pos 3, 2-byte char)
        assert_eq!(m.positions, vec![1, 3]);
    }

    const ROWS: [&str; 4] = ["main", "develop", "feature/map", "release/1.0"];

    #[test]
    fn filter_push_narrows_and_snaps_to_first_match() {
        let mut f = FuzzyFilter::default();
        let mut index = 1; // "develop" selected
        f.push('m', ROWS, &mut index);
        f.push('a', ROWS, &mut index);
        assert_eq!(f.visible_indices(ROWS.len()), vec![0, 2]); // main, feature/map
        assert_eq!(index, 0, "dropped row snaps to the first match");
        assert_eq!(f.real_index(index, ROWS.len()), Some(0));
        assert_eq!(f.len(ROWS.len()), 2);
    }

    #[test]
    fn filter_edit_keeps_selected_row_while_it_matches() {
        let mut f = FuzzyFilter::default();
        let mut index = 2; // "feature/map" selected
        f.push('a', ROWS, &mut index);
        // All of main / feature/map / release match "a"; the cursor follows
        // its row into the narrower list instead of resetting.
        assert_eq!(f.real_index(index, ROWS.len()), Some(2));
        f.pop(ROWS, &mut index);
        assert!(!f.is_active());
        assert_eq!(index, 2, "identity restored on the same row");
    }

    #[test]
    fn filter_clear_restores_identity_on_selected_row() {
        let mut f = FuzzyFilter::default();
        let mut index = 0;
        f.push('m', ROWS, &mut index);
        index = 1; // "feature/map" within the filtered list
        f.clear(&mut index);
        assert!(!f.is_active());
        assert_eq!(index, 2, "filtered position mapped back to list space");
        assert_eq!(f.len(ROWS.len()), ROWS.len());
    }

    #[test]
    fn filter_clear_on_zero_matches_falls_back_to_first_row() {
        let mut f = FuzzyFilter::default();
        let mut index = 2;
        f.push('z', ROWS, &mut index); // matches nothing
        assert_eq!(f.len(ROWS.len()), 0);
        f.clear(&mut index);
        assert!(!f.is_active());
        assert_eq!(index, 0, "no row under the cursor → first row of the list");
    }

    #[test]
    fn filter_no_match_yields_empty_and_inert_selection() {
        let mut f = FuzzyFilter::default();
        let mut index = 3;
        f.push('z', ROWS, &mut index);
        f.push('z', ROWS, &mut index);
        assert_eq!(f.len(ROWS.len()), 0);
        assert_eq!(index, 0);
        assert_eq!(f.real_index(0, ROWS.len()), None);
        // Backing out one character brings the matches (and the cursor) back.
        f.pop(ROWS, &mut index);
        assert_eq!(f.len(ROWS.len()), 0, "\"z\" still matches nothing");
        f.pop(ROWS, &mut index);
        assert_eq!(f.len(ROWS.len()), ROWS.len());
    }

    #[test]
    fn filter_refilter_applies_query_typed_before_list_arrived() {
        let mut f = FuzzyFilter::default();
        let mut index = 0;
        f.push('m', [] as [&str; 0], &mut index);
        assert_eq!(f.len(0), 0);
        // The branch list lands from its background load (ADR-P12).
        f.refilter(ROWS, &mut index);
        assert_eq!(f.visible_indices(ROWS.len()), vec![0, 2]);
        assert_eq!(index, 0);
    }

    #[test]
    fn filter_real_index_identity_is_bounds_checked() {
        let f = FuzzyFilter::default();
        assert_eq!(f.real_index(1, 3), Some(1));
        assert_eq!(f.real_index(3, 3), None);
    }
}
