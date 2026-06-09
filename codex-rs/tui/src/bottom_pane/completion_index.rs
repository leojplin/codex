use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Range;

use codex_utils_fuzzy_match::fuzzy_match;
use neo_frizbee::Config as FrizbeeConfig;
use neo_frizbee::Matcher as FrizbeeMatcher;

pub(crate) const MIN_COMPLETION_QUERY_CHARS: usize = 2;
pub(crate) const MAX_COMPLETION_RESULTS: usize = 8;
const MAX_CANDIDATE_CHARS: usize = 300;
const ENGLISH_DICTIONARY_WORDS: &str = include_str!("../../assets/completion/english_words.txt");
const ENGLISH_DICTIONARY_INDEX: &[u8] =
    include_bytes!("../../assets/completion/english_words_index.bin");
const ENGLISH_DICTIONARY_BIGRAM_INDEX: &[u8] =
    include_bytes!("../../assets/completion/english_words_bigram_index.bin");
const ENGLISH_DICTIONARY_PAIR_INDEX: &[u8] =
    include_bytes!("../../assets/completion/english_words_pair_index.bin");
const KNOWN_FILE_EXTENSIONS: &[&str] = &[
    "bash", "bazel", "c", "cc", "cfg", "conf", "cpp", "css", "csv", "cts", "go", "h", "hpp",
    "html", "java", "js", "json", "jsx", "lock", "log", "lua", "md", "mdx", "mjs", "mts", "proto",
    "py", "rb", "rs", "scss", "sh", "sql", "swift", "toml", "ts", "tsx", "txt", "xml", "yaml",
    "yml", "zsh",
];
// Fixed records: u32 word byte offset, u8 byte length, u32 lowercase ASCII letter mask.
const DICTIONARY_INDEX_RECORD_BYTES: usize = 9;
const DICTIONARY_PAIR_COUNT: usize = 26 * 26;
const DICTIONARY_PAIR_RECORD_BYTES: usize = 8;
const DICTIONARY_PAIR_HEADER_BYTES: usize = DICTIONARY_PAIR_COUNT * DICTIONARY_PAIR_RECORD_BYTES;

const _: () = assert!(
    ENGLISH_DICTIONARY_INDEX
        .len()
        .is_multiple_of(DICTIONARY_INDEX_RECORD_BYTES)
);
const _: () = assert!(ENGLISH_DICTIONARY_BIGRAM_INDEX.len() >= DICTIONARY_PAIR_HEADER_BYTES);
const _: () = assert!(ENGLISH_DICTIONARY_PAIR_INDEX.len() >= DICTIONARY_PAIR_HEADER_BYTES);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompletionKind {
    Word,
    Filename,
    Url,
    Dictionary,
}

impl CompletionKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            CompletionKind::Word => "session",
            CompletionKind::Filename => "file",
            CompletionKind::Url => "url",
            CompletionKind::Dictionary => "dict",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            CompletionKind::Url => 3,
            CompletionKind::Filename => 2,
            CompletionKind::Word => 1,
            CompletionKind::Dictionary => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionMatch {
    pub(crate) text: String,
    pub(crate) kind: CompletionKind,
    pub(crate) match_indices: Vec<usize>,
}

#[derive(Clone, Debug)]
struct CompletionCandidate {
    text: String,
    kind: CompletionKind,
    frequency: u32,
    last_seen_seq: u64,
    key_byte_len: usize,
    key_ascii_mask: [u64; 2],
    key_is_ascii: bool,
}

#[derive(Clone, Copy)]
struct CandidateView<'a> {
    text: &'a str,
    key: &'a str,
    kind: CompletionKind,
    frequency: u32,
    last_seen_seq: u64,
    key_byte_len: usize,
    key_ascii_mask: [u64; 2],
    key_is_ascii: bool,
}

impl AsRef<str> for CandidateView<'_> {
    fn as_ref(&self) -> &str {
        self.key
    }
}

fn candidate_view<'a>(key: &'a str, candidate: &'a CompletionCandidate) -> CandidateView<'a> {
    CandidateView {
        text: &candidate.text,
        key,
        kind: candidate.kind,
        frequency: candidate.frequency,
        last_seen_seq: candidate.last_seen_seq,
        key_byte_len: candidate.key_byte_len,
        key_ascii_mask: candidate.key_ascii_mask,
        key_is_ascii: candidate.key_is_ascii,
    }
}

#[derive(Default)]
pub(crate) struct SessionCompletionIndex {
    candidates: HashMap<String, CompletionCandidate>,
    prefix_keys: Vec<String>,
    next_seq: u64,
}

impl SessionCompletionIndex {
    pub(crate) fn ingest_text(&mut self, text: &str) {
        for (candidate, kind) in extract_candidates(text) {
            self.add_candidate(candidate, kind);
        }
    }

    #[cfg(test)]
    pub(crate) fn search(&self, query: &str) -> Vec<CompletionMatch> {
        self.search_with_dictionary(query, true)
    }

    #[cfg(test)]
    pub(crate) fn search_with_dictionary(
        &self,
        query: &str,
        include_dictionary: bool,
    ) -> Vec<CompletionMatch> {
        let never_cancelled = || false;
        self.search_with_dictionary_cancellable(query, include_dictionary, &never_cancelled)
            .unwrap_or_default()
    }

    pub(crate) fn search_with_dictionary_cancellable(
        &self,
        query: &str,
        include_dictionary: bool,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<CompletionMatch>> {
        if query.chars().count() < MIN_COMPLETION_QUERY_CHARS {
            return Some(Vec::new());
        }

        if is_cancelled() {
            return None;
        }

        let query_lower = normalize(query);
        let mut session_matches = self.session_literal_matches(
            query,
            &query_lower,
            MAX_COMPLETION_RESULTS,
            is_cancelled,
        )?;
        if session_matches.len() < MAX_COMPLETION_RESULTS {
            session_matches.extend(self.session_fuzzy_matches(
                query,
                &query_lower,
                MAX_COMPLETION_RESULTS - session_matches.len(),
                is_cancelled,
            )?);
        }

        let mut results = session_matches
            .into_iter()
            .take(MAX_COMPLETION_RESULTS)
            .map(|ranked| ranked_to_completion_match(ranked, &query_lower))
            .collect::<Vec<_>>();

        let remaining = MAX_COMPLETION_RESULTS.saturating_sub(results.len());
        if include_dictionary && remaining > 0 {
            results.extend(self.search_dictionary(query, &query_lower, remaining, is_cancelled)?);
        }

        Some(results)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.candidates.len()
    }

    fn add_candidate(&mut self, text: String, kind: CompletionKind) {
        let text = text.trim();
        if text.chars().count() < MIN_COMPLETION_QUERY_CHARS
            || text.chars().count() > MAX_CANDIDATE_CHARS
            || text.chars().all(|ch| ch.is_ascii_digit())
        {
            return;
        }

        let key = normalize(text);
        if key.is_empty() {
            return;
        }
        let key_byte_len = key.len();
        let key_ascii_mask = ascii_char_mask(&key);
        let key_is_ascii = key.is_ascii();

        self.next_seq = self.next_seq.wrapping_add(1);
        if let Some(existing) = self.candidates.get_mut(&key) {
            existing.frequency = existing.frequency.saturating_add(1);
            existing.last_seen_seq = self.next_seq;
            if kind.rank() >= existing.kind.rank() {
                existing.kind = kind;
                existing.text = text.to_string();
            }
            return;
        }

        let insert_at = self
            .prefix_keys
            .binary_search_by(|probe| probe.as_str().cmp(&key))
            .unwrap_or_else(|idx| idx);
        self.prefix_keys.insert(insert_at, key.clone());

        self.candidates.insert(
            key,
            CompletionCandidate {
                text: text.to_string(),
                kind,
                frequency: 1,
                last_seen_seq: self.next_seq,
                key_byte_len,
                key_ascii_mask,
                key_is_ascii,
            },
        );
    }

    fn session_literal_matches<'a>(
        &'a self,
        query: &str,
        query_lower: &str,
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<RankedMatch<'a>>> {
        let mut matches = Vec::new();
        let mut scanned = 0usize;
        let start = self
            .prefix_keys
            .binary_search_by(|key| key.as_str().cmp(query_lower))
            .unwrap_or_else(|idx| idx);
        for key in self.prefix_keys[start..]
            .iter()
            .take_while(|key| key.starts_with(query_lower))
        {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            let Some(candidate) = self.candidates.get(key) else {
                continue;
            };
            if let Some(ranked) = rank_candidate(
                candidate_view(key, candidate),
                query,
                query_lower,
                /*include_fuzzy*/ false,
            ) {
                push_top_ranked_match(&mut matches, ranked, limit);
            }
        }
        Some(matches)
    }

    fn session_fuzzy_matches<'a>(
        &'a self,
        query: &str,
        query_lower: &str,
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<RankedMatch<'a>>> {
        if limit == 0 {
            return Some(Vec::new());
        }

        if !query_lower.is_ascii() {
            return self.session_legacy_fuzzy_matches(
                query,
                query_lower,
                false,
                limit,
                is_cancelled,
            );
        }

        let max_typos = completion_max_typos(query_lower);
        let min_len = query_lower.len().saturating_sub(max_typos as usize);
        let query_mask = ascii_char_mask(query_lower);
        let allowed_missing_chars = u32::from(max_typos);
        let mut candidates = Vec::new();
        let mut scanned = 0usize;
        for (key, candidate) in self.candidates.iter() {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            let view = candidate_view(key, candidate);
            if !view.key_is_ascii
                || has_literal_match(view.key, query_lower)
                || view.key_byte_len < min_len
                || missing_ascii_chars(view.key_ascii_mask, query_mask) > allowed_missing_chars
            {
                continue;
            }
            candidates.push(view);
        }

        let config = FrizbeeConfig {
            max_typos: Some(max_typos),
            sort: false,
            ..Default::default()
        };

        let mut matches = Vec::new();
        let mut matcher = FrizbeeMatcher::new(query_lower, &config);
        let mut scanned = 0usize;
        for matched in matcher.match_iter(candidates.as_slice()) {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            push_top_ranked_match(
                &mut matches,
                RankedMatch {
                    candidate: candidates[matched.index as usize],
                    match_class: MatchClass::Fuzzy,
                    fuzzy_score: -(matched.score as i32),
                    match_indices: RankedMatchIndices::FrizbeeFuzzy { max_typos },
                },
                limit,
            );
        }
        for ranked in
            self.session_legacy_fuzzy_matches(query, query_lower, true, limit, is_cancelled)?
        {
            push_top_ranked_match(&mut matches, ranked, limit);
        }
        Some(matches)
    }

    fn session_legacy_fuzzy_matches<'a>(
        &'a self,
        query: &str,
        query_lower: &str,
        only_non_ascii: bool,
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<RankedMatch<'a>>> {
        let mut matches = Vec::new();
        let mut scanned = 0usize;
        for (key, candidate) in self.candidates.iter() {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            let view = candidate_view(key, candidate);
            if has_literal_match(view.key, query_lower) || (only_non_ascii && view.key_is_ascii) {
                continue;
            }
            if let Some(ranked) =
                rank_candidate(view, query, query_lower, /*include_fuzzy*/ true)
                    .filter(|ranked| ranked.match_class == MatchClass::Fuzzy)
            {
                push_top_ranked_match(&mut matches, ranked, limit);
            }
        }
        Some(matches)
    }

    fn search_dictionary(
        &self,
        query: &str,
        query_lower: &str,
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<CompletionMatch>> {
        if !query_lower.is_ascii() {
            return Some(Vec::new());
        }

        let mut matches =
            self.dictionary_literal_matches(query, query_lower, limit, is_cancelled)?;
        if matches.len() < limit {
            for ranked in
                self.dictionary_fuzzy_matches(query_lower, limit - matches.len(), is_cancelled)?
            {
                push_top_dictionary_match(&mut matches, ranked, limit);
            }
        }
        Some(
            matches
                .into_iter()
                .take(limit)
                .map(|ranked| dictionary_match_to_completion_match(ranked, query_lower))
                .collect(),
        )
    }

    fn dictionary_literal_matches(
        &self,
        query: &str,
        query_lower: &str,
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<DictionaryRankedMatch>> {
        let Some(pair) = rarest_query_pair(query_lower, ENGLISH_DICTIONARY_BIGRAM_INDEX) else {
            return Some(Vec::new());
        };

        let mut matches = Vec::new();
        let mut scanned = 0usize;
        for word_id in dictionary_pair_word_ids(ENGLISH_DICTIONARY_BIGRAM_INDEX, pair) {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            let record = dictionary_record_at(word_id);
            if self.candidates.contains_key(record.text) {
                continue;
            }

            if record.text == query_lower && record.text == query {
                continue;
            }

            if record.text == query_lower {
                push_top_dictionary_match(
                    &mut matches,
                    DictionaryRankedMatch {
                        text: record.text,
                        match_class: MatchClass::Exact,
                        fuzzy_score: -1_000,
                        match_indices: DictionaryMatchIndices::All,
                    },
                    limit,
                );
                continue;
            }

            if record.text.starts_with(query_lower) {
                push_top_dictionary_match(
                    &mut matches,
                    DictionaryRankedMatch {
                        text: record.text,
                        match_class: MatchClass::Prefix,
                        fuzzy_score: -500,
                        match_indices: DictionaryMatchIndices::Prefix {
                            char_count: query_lower.chars().count(),
                        },
                    },
                    limit,
                );
                continue;
            }

            if let Some(start) = record.text.find(query_lower) {
                let end = start + query_lower.len();
                push_top_dictionary_match(
                    &mut matches,
                    DictionaryRankedMatch {
                        text: record.text,
                        match_class: MatchClass::Substring,
                        fuzzy_score: 0,
                        match_indices: DictionaryMatchIndices::ByteRange(start..end),
                    },
                    limit,
                );
            }
        }
        Some(matches)
    }

    fn dictionary_fuzzy_matches(
        &self,
        query_lower: &str,
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<DictionaryRankedMatch>> {
        if limit == 0 || query_lower.len() < 4 {
            return Some(Vec::new());
        }

        let Some(pair) = rarest_query_pair(query_lower, ENGLISH_DICTIONARY_PAIR_INDEX) else {
            return Some(Vec::new());
        };

        let max_typos = dictionary_max_typos(query_lower);
        let min_len = query_lower.len().saturating_sub(max_typos as usize);
        let query_mask = ascii_letter_mask(query_lower);
        let allowed_missing_letters = u32::from(max_typos);
        let mut candidates = Vec::new();
        let mut scanned = 0usize;
        for record in
            dictionary_pair_word_ids(ENGLISH_DICTIONARY_PAIR_INDEX, pair).map(dictionary_record_at)
        {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            if self.candidates.contains_key(record.text)
                || dictionary_has_literal_match(record.text, query_lower)
                || record.byte_len < min_len
                || missing_ascii_letters(record.letter_mask, query_mask) > allowed_missing_letters
            {
                continue;
            }
            candidates.push(record.text);
        }

        if candidates.is_empty() {
            return Some(Vec::new());
        }

        let config = FrizbeeConfig {
            max_typos: Some(max_typos),
            sort: false,
            ..Default::default()
        };

        let mut matches = Vec::new();
        let mut matcher = FrizbeeMatcher::new(query_lower, &config);
        let mut scanned = 0usize;
        for matched in matcher.match_iter(candidates.as_slice()) {
            if should_cancel_scan(&mut scanned, is_cancelled) {
                return None;
            }
            push_top_dictionary_match(
                &mut matches,
                DictionaryRankedMatch {
                    text: candidates[matched.index as usize],
                    match_class: MatchClass::Fuzzy,
                    fuzzy_score: -(matched.score as i32),
                    match_indices: DictionaryMatchIndices::Fuzzy { max_typos },
                },
                limit,
            );
        }
        Some(matches)
    }
}

fn should_cancel_scan(scanned: &mut usize, is_cancelled: &dyn Fn() -> bool) -> bool {
    *scanned = scanned.saturating_add(1);
    (*scanned).is_multiple_of(128) && is_cancelled()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchClass {
    Exact,
    Prefix,
    Substring,
    Fuzzy,
}

struct RankedMatch<'a> {
    candidate: CandidateView<'a>,
    match_class: MatchClass,
    fuzzy_score: i32,
    match_indices: RankedMatchIndices,
}

enum RankedMatchIndices {
    Resolved(Vec<usize>),
    All,
    Prefix { char_count: usize },
    ByteRange(Range<usize>),
    FrizbeeFuzzy { max_typos: u16 },
}

struct DictionaryRankedMatch {
    text: &'static str,
    match_class: MatchClass,
    fuzzy_score: i32,
    match_indices: DictionaryMatchIndices,
}

enum DictionaryMatchIndices {
    All,
    Prefix { char_count: usize },
    ByteRange(Range<usize>),
    Fuzzy { max_typos: u16 },
}

fn rank_candidate<'a>(
    candidate: CandidateView<'a>,
    query: &str,
    query_lower: &str,
    include_fuzzy: bool,
) -> Option<RankedMatch<'a>> {
    if candidate.key == query_lower && candidate.text == query {
        return None;
    }

    if candidate.key == query_lower {
        return Some(RankedMatch {
            candidate,
            match_class: MatchClass::Exact,
            fuzzy_score: -1_000,
            match_indices: RankedMatchIndices::All,
        });
    }

    if candidate.key.starts_with(query_lower) {
        return Some(RankedMatch {
            candidate,
            match_class: MatchClass::Prefix,
            fuzzy_score: -500,
            match_indices: RankedMatchIndices::Prefix {
                char_count: query.chars().count(),
            },
        });
    }

    if let Some(start) = candidate.key.find(query_lower) {
        let end = start + query_lower.len();
        return Some(RankedMatch {
            candidate,
            match_class: MatchClass::Substring,
            fuzzy_score: 0,
            match_indices: RankedMatchIndices::ByteRange(start..end),
        });
    }

    if !include_fuzzy {
        return None;
    }

    let (indices, score) = fuzzy_match(candidate.text, query)?;
    Some(RankedMatch {
        candidate,
        match_class: MatchClass::Fuzzy,
        fuzzy_score: score,
        match_indices: RankedMatchIndices::Resolved(indices),
    })
}

fn compare_ranked_matches(left: &RankedMatch<'_>, right: &RankedMatch<'_>) -> std::cmp::Ordering {
    left.match_class
        .cmp(&right.match_class)
        .then_with(|| left.fuzzy_score.cmp(&right.fuzzy_score))
        .then_with(|| right.candidate.kind.rank().cmp(&left.candidate.kind.rank()))
        .then_with(|| {
            right
                .candidate
                .last_seen_seq
                .cmp(&left.candidate.last_seen_seq)
        })
        .then_with(|| right.candidate.frequency.cmp(&left.candidate.frequency))
        .then_with(|| left.candidate.text.len().cmp(&right.candidate.text.len()))
        .then_with(|| left.candidate.text.cmp(right.candidate.text))
}

fn push_top_ranked_match<'a>(
    matches: &mut Vec<RankedMatch<'a>>,
    ranked: RankedMatch<'a>,
    limit: usize,
) {
    push_top_match(matches, ranked, limit, compare_ranked_matches);
}

fn ranked_to_completion_match(ranked: RankedMatch<'_>, query_lower: &str) -> CompletionMatch {
    let match_indices = match ranked.match_indices {
        RankedMatchIndices::Resolved(indices) => indices,
        RankedMatchIndices::All => (0..ranked.candidate.text.chars().count()).collect(),
        RankedMatchIndices::Prefix { char_count } => (0..char_count).collect(),
        RankedMatchIndices::ByteRange(range) => {
            char_indices_for_byte_range(ranked.candidate.text, range)
        }
        RankedMatchIndices::FrizbeeFuzzy { max_typos } => {
            frizbee_match_indices(query_lower, ranked.candidate.key, max_typos)
        }
    };

    CompletionMatch {
        text: ranked.candidate.text.to_string(),
        kind: ranked.candidate.kind,
        match_indices,
    }
}

fn compare_dictionary_matches(
    left: &DictionaryRankedMatch,
    right: &DictionaryRankedMatch,
) -> std::cmp::Ordering {
    left.match_class
        .cmp(&right.match_class)
        .then_with(|| left.fuzzy_score.cmp(&right.fuzzy_score))
        .then_with(|| left.text.len().cmp(&right.text.len()))
        .then_with(|| left.text.cmp(right.text))
}

fn push_top_dictionary_match(
    matches: &mut Vec<DictionaryRankedMatch>,
    ranked: DictionaryRankedMatch,
    limit: usize,
) {
    push_top_match(matches, ranked, limit, compare_dictionary_matches);
}

fn push_top_match<T>(
    matches: &mut Vec<T>,
    ranked: T,
    limit: usize,
    compare: fn(&T, &T) -> Ordering,
) {
    if limit == 0 {
        return;
    }

    let insert_at = matches
        .binary_search_by(|probe| compare(probe, &ranked))
        .unwrap_or_else(|idx| idx);
    if insert_at >= limit {
        return;
    }

    matches.insert(insert_at, ranked);
    if matches.len() > limit {
        matches.pop();
    }
}

fn dictionary_match_to_completion_match(
    ranked: DictionaryRankedMatch,
    query_lower: &str,
) -> CompletionMatch {
    let match_indices = match ranked.match_indices {
        DictionaryMatchIndices::All => (0..ranked.text.chars().count()).collect(),
        DictionaryMatchIndices::Prefix { char_count } => (0..char_count).collect(),
        DictionaryMatchIndices::ByteRange(range) => char_indices_for_byte_range(ranked.text, range),
        DictionaryMatchIndices::Fuzzy { max_typos } => {
            frizbee_match_indices(query_lower, ranked.text, max_typos)
        }
    };

    CompletionMatch {
        text: ranked.text.to_string(),
        kind: CompletionKind::Dictionary,
        match_indices,
    }
}

fn frizbee_match_indices(query_lower: &str, text: &str, max_typos: u16) -> Vec<usize> {
    let config = FrizbeeConfig {
        max_typos: Some(max_typos),
        sort: false,
        ..Default::default()
    };
    let Some(mut matched) = neo_frizbee::match_list_indices(query_lower, &[text], &config).pop()
    else {
        return Vec::new();
    };
    matched.indices.sort_unstable();
    matched.indices.dedup();
    matched.indices
}

fn char_indices_for_byte_range(text: &str, range: Range<usize>) -> Vec<usize> {
    text.char_indices()
        .enumerate()
        .filter_map(|(char_idx, (byte_idx, _))| {
            (byte_idx >= range.start && byte_idx < range.end).then_some(char_idx)
        })
        .collect()
}

fn extract_candidates(text: &str) -> Vec<(String, CompletionKind)> {
    let mut candidates = Vec::new();

    for raw in text.split_whitespace() {
        extract_from_token(raw, &mut candidates);
    }

    extract_words(text, &mut candidates);
    candidates
}

fn extract_from_token(raw: &str, candidates: &mut Vec<(String, CompletionKind)>) {
    let raw = trim_token(raw);
    if raw.is_empty() {
        return;
    }

    if let Some(url_start) = raw.find("https://").or_else(|| raw.find("http://")) {
        let url = trim_trailing_token_punctuation(&raw[url_start..]);
        if !url.is_empty() {
            candidates.push((url.to_string(), CompletionKind::Url));
        }
        return;
    }

    if looks_like_path(raw) {
        if let Some(file_name) = basename(raw).filter(|name| looks_like_filename(name)) {
            candidates.push((file_name.to_string(), CompletionKind::Filename));
        }
        return;
    }

    if looks_like_filename(raw) {
        candidates.push((raw.to_string(), CompletionKind::Filename));
    }
}

fn extract_words(text: &str, candidates: &mut Vec<(String, CompletionKind)>) {
    let mut start: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if is_word_char(ch) {
            if start.is_none() {
                start = Some(idx);
            }
            continue;
        }

        if let Some(word_start) = start.take() {
            push_word_candidate(&text[word_start..idx], candidates);
        }
    }

    if let Some(word_start) = start {
        push_word_candidate(&text[word_start..], candidates);
    }
}

fn push_word_candidate(word: &str, candidates: &mut Vec<(String, CompletionKind)>) {
    if word.chars().count() >= MIN_COMPLETION_QUERY_CHARS
        && !word.chars().all(|ch| ch.is_ascii_digit())
    {
        candidates.push((word.to_string(), CompletionKind::Word));
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-')
}

fn looks_like_path(value: &str) -> bool {
    if value.starts_with("http://") || value.starts_with("https://") {
        return false;
    }
    let has_separator = value.contains('/') || value.contains('\\');
    has_separator && value.chars().any(char::is_alphanumeric)
}

fn looks_like_filename(value: &str) -> bool {
    if value.contains('/') || value.contains('\\') || value.starts_with('.') {
        return false;
    }

    let Some((stem, extension)) = value.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem.chars().any(char::is_alphanumeric)
        && KNOWN_FILE_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
}

fn basename(path: &str) -> Option<&str> {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
}

fn trim_token(token: &str) -> &str {
    trim_trailing_token_punctuation(token.trim_matches(is_outer_token_punctuation))
}

fn trim_trailing_token_punctuation(token: &str) -> &str {
    token.trim_end_matches(is_trailing_token_punctuation)
}

fn is_outer_token_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
    )
}

fn is_trailing_token_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ',' | ';' | ':' | ')' | ']' | '}' | '>' | '"' | '\''
    )
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
}

#[derive(Clone, Copy)]
struct DictionaryRecord {
    text: &'static str,
    byte_len: usize,
    letter_mask: u32,
}

fn dictionary_record_count() -> usize {
    ENGLISH_DICTIONARY_INDEX.len() / DICTIONARY_INDEX_RECORD_BYTES
}

fn dictionary_record_at(word_id: usize) -> DictionaryRecord {
    debug_assert!(word_id < dictionary_record_count());
    let start = word_id * DICTIONARY_INDEX_RECORD_BYTES;
    let record = &ENGLISH_DICTIONARY_INDEX[start..start + DICTIONARY_INDEX_RECORD_BYTES];
    let offset = u32::from_le_bytes([record[0], record[1], record[2], record[3]]) as usize;
    let byte_len = record[4] as usize;
    let letter_mask = u32::from_le_bytes([record[5], record[6], record[7], record[8]]);
    let end = offset + byte_len;
    debug_assert!(end <= ENGLISH_DICTIONARY_WORDS.len());
    DictionaryRecord {
        text: &ENGLISH_DICTIONARY_WORDS[offset..end],
        byte_len,
        letter_mask,
    }
}

fn rarest_query_pair(query_lower: &str, index: &'static [u8]) -> Option<usize> {
    query_letter_pair_keys(query_lower)
        .into_iter()
        .filter(|key| dictionary_pair_count(index, *key) > 0)
        .min_by_key(|key| dictionary_pair_count(index, *key))
}

fn query_letter_pair_keys(query_lower: &str) -> Vec<usize> {
    let mut keys = Vec::new();
    let mut prev: Option<u8> = None;
    for byte in query_lower.bytes() {
        if !byte.is_ascii_lowercase() {
            prev = None;
            continue;
        }
        let current = byte - b'a';
        if let Some(previous) = prev {
            keys.push(pair_key(previous, current));
        }
        prev = Some(current);
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

fn pair_key(left: u8, right: u8) -> usize {
    left as usize * 26 + right as usize
}

fn dictionary_pair_word_ids(index: &'static [u8], key: usize) -> impl Iterator<Item = usize> {
    dictionary_pair_posting(index, key)
        .chunks_exact(4)
        .map(|record| u32::from_le_bytes([record[0], record[1], record[2], record[3]]) as usize)
}

fn dictionary_pair_posting(index: &'static [u8], key: usize) -> &'static [u8] {
    debug_assert!(key < DICTIONARY_PAIR_COUNT);
    let header_offset = key * DICTIONARY_PAIR_RECORD_BYTES;
    let offset = u32::from_le_bytes([
        index[header_offset],
        index[header_offset + 1],
        index[header_offset + 2],
        index[header_offset + 3],
    ]) as usize;
    let count = u32::from_le_bytes([
        index[header_offset + 4],
        index[header_offset + 5],
        index[header_offset + 6],
        index[header_offset + 7],
    ]) as usize;
    let end = offset + count * 4;
    debug_assert!(end <= index.len());
    &index[offset..end]
}

fn dictionary_pair_count(index: &'static [u8], key: usize) -> usize {
    debug_assert!(key < DICTIONARY_PAIR_COUNT);
    let header_offset = key * DICTIONARY_PAIR_RECORD_BYTES + 4;
    u32::from_le_bytes([
        index[header_offset],
        index[header_offset + 1],
        index[header_offset + 2],
        index[header_offset + 3],
    ]) as usize
}

fn dictionary_has_literal_match(word: &str, query_lower: &str) -> bool {
    has_literal_match(word, query_lower)
}

fn dictionary_max_typos(query_lower: &str) -> u16 {
    completion_max_typos(query_lower)
}

fn completion_max_typos(query_lower: &str) -> u16 {
    if query_lower.len() >= 4 { 1 } else { 0 }
}

fn has_literal_match(candidate_key: &str, query_lower: &str) -> bool {
    candidate_key == query_lower || candidate_key.contains(query_lower)
}

fn ascii_char_mask(text: &str) -> [u64; 2] {
    let mut mask = [0u64; 2];
    for byte in text.bytes() {
        if byte.is_ascii() {
            let idx = (byte / 64) as usize;
            let bit = byte % 64;
            mask[idx] |= 1 << bit;
        }
    }
    mask
}

fn missing_ascii_chars(candidate_mask: [u64; 2], query_mask: [u64; 2]) -> u32 {
    (query_mask[0] & !candidate_mask[0]).count_ones()
        + (query_mask[1] & !candidate_mask[1]).count_ones()
}

fn ascii_letter_mask(text: &str) -> u32 {
    let mut mask = 0u32;
    for byte in text.bytes() {
        if byte.is_ascii_alphabetic() {
            mask |= 1 << (byte.to_ascii_lowercase() - b'a');
        }
    }
    mask
}

fn missing_ascii_letters(candidate_mask: u32, query_mask: u32) -> u32 {
    (query_mask & !candidate_mask).count_ones()
}

#[cfg(test)]
#[path = "completion_index_tests.rs"]
mod completion_index_tests;
