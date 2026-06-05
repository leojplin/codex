use super::*;

#[test]
fn indexes_words_paths_filenames_and_urls() {
    let mut index = SessionCompletionIndex::default();
    index.ingest_text(
        "Open src/bottom_pane/chat_composer.rs and Cargo.toml, then visit https://example.com/docs?q=codex.",
    );

    assert!(index.len() >= 6);

    let path_results = index.search("src/bot");
    assert_eq!(path_results[0].text, "src/bottom_pane/chat_composer.rs");
    assert_eq!(path_results[0].kind, CompletionKind::Path);

    let file_results = index.search("chat");
    assert!(
        file_results
            .iter()
            .any(|result| result.text == "chat_composer.rs"
                && result.kind == CompletionKind::Filename)
    );

    let url_results = index.search("http");
    assert_eq!(url_results[0].text, "https://example.com/docs?q=codex");
    assert_eq!(url_results[0].kind, CompletionKind::Url);
}

#[test]
fn ranks_prefix_matches_before_fuzzy_matches() {
    let mut index = SessionCompletionIndex::default();
    index.ingest_text("application a-p-p-l-e");

    let results = index.search("app");
    assert_eq!(results[0].text, "application");
    assert!(
        results
            .iter()
            .position(|result| result.text == "application")
            .unwrap()
            < results
                .iter()
                .position(|result| result.text == "a-p-p-l-e")
                .unwrap()
    );
}

#[test]
fn fuzzy_matches_non_contiguous_queries() {
    let mut index = SessionCompletionIndex::default();
    index.ingest_text("bottom_pane_completion_popup");

    let results = index.search("bpcp");
    assert_eq!(results[0].text, "bottom_pane_completion_popup");
    assert!(!results[0].match_indices.is_empty());
}

#[test]
fn does_not_return_identical_candidate() {
    let mut index = SessionCompletionIndex::default();
    index.ingest_text("hello");

    assert!(
        !index
            .search("hello")
            .iter()
            .any(|result| result.text == "hello")
    );
}

#[test]
fn returns_dictionary_words_as_fallback() {
    let index = SessionCompletionIndex::default();

    let results = index.search("dictio");
    assert!(
        results
            .iter()
            .any(|result| result.text == "dictionary" && result.kind == CompletionKind::Dictionary)
    );
}

#[test]
fn ranks_session_words_before_dictionary_words() {
    let mut index = SessionCompletionIndex::default();
    index.ingest_text("dictionary-like");

    let results = index.search("dict");
    assert_eq!(results[0].text, "dictionary-like");
    assert_eq!(results[0].kind, CompletionKind::Word);
}

#[test]
fn fuzzy_matches_dictionary_words() {
    let index = SessionCompletionIndex::default();

    let results = index.search("aknwlg");
    assert!(results.iter().any(|result| result.text == "acknowledgment"
        && result.kind == CompletionKind::Dictionary
        && !result.match_indices.is_empty()));
}

#[test]
fn dictionary_does_not_duplicate_session_words() {
    let mut index = SessionCompletionIndex::default();
    index.ingest_text("completion");

    let results = index.search("comp");
    assert_eq!(
        results
            .iter()
            .filter(|result| result.text == "completion")
            .count(),
        1
    );
    let completion = results
        .iter()
        .find(|result| result.text == "completion")
        .unwrap();
    assert_eq!(completion.kind, CompletionKind::Word);
}
