use nestweaver_schema::{Note, NoteKind};
use nestweaver_store::GraphStore;

// End-to-end correctness check on filtered string reads through `list_notes`:
// NOTE_COUNT exceeds two of Ladybug's 2,048-row value vectors, so every
// selected row's strings must survive a filtered read that spans multiple
// output vectors, repeated for determinism.
//
// This is NOT a regression detector for the upstream filtered multi-segment
// string-scan bug (LadybugDB/ladybug#737), despite what its size suggests. That
// fix retightened one bound in `StringColumn::scanFiltered` from
// `startOffsetInChunk + pos < state.metadata.numValues` (clamped against the
// whole segment) to `pos < offsetInResult + numValuesToScan` (clamped against
// the current scan batch). The two bounds only disagree when one output vector
// is filled by more than one scan call, which happens at a *segment* boundary,
// not a value-vector boundary. Segments are page-sized and far larger than
// 2,048 rows, so this fixture lives inside a single segment, the scan never
// splits, `numValuesToScan == selSize`, and buggy and fixed builds behave
// identically. (Confirmed empirically: this test passes against unpatched
// lbug 0.18.2.) Repairing it into a true #737 detector would require sizing
// against segment capacity and is out of scope.
const NOTE_COUNT: usize = 4_113;
const SELECTED_VAULT: &str = "vlt:selected";

#[test]
fn filtered_string_scan_preserves_every_selected_value_across_batches() {
    let store = GraphStore::in_memory().unwrap();
    let notes = (0..NOTE_COUNT)
        .map(|index| {
            let selected = index % 3 == 1;
            Note {
                uid: format!("note:scan:{index:05}"),
                vault_uid: if selected {
                    SELECTED_VAULT.to_string()
                } else {
                    "vlt:other".to_string()
                },
                file_path: format!("notes/scan-{index:05}.md"),
                title: format!("SCAN_TITLE_{index:05}"),
                note_kind: NoteKind::General,
                word_count: index as u32,
                content_hash: format!("hash-{index:05}"),
                frontmatter: None,
                frontmatter_raw: None,
                created_at: None,
                modified_at: None,
                pagerank_score: None,
                embedding: None,
            }
        })
        .collect::<Vec<_>>();
    store.batch_insert_notes(&notes).unwrap();

    let expected = notes
        .iter()
        .filter(|note| note.vault_uid == SELECTED_VAULT)
        .map(|note| {
            (
                note.uid.clone(),
                note.file_path.clone(),
                note.title.clone(),
                note.content_hash.clone(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();

    // Repeat the same filtered read to guard both exact values and determinism.
    for run in 0..3 {
        let actual = store
            .list_notes(Some(SELECTED_VAULT))
            .unwrap_or_else(|error| panic!("filtered scan run {run} failed: {error}"))
            .into_iter()
            .map(|note| (note.uid, note.file_path, note.title, note.content_hash))
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            actual, expected,
            "filtered scan run {run} returned missing or corrupted string values"
        );
    }
}
