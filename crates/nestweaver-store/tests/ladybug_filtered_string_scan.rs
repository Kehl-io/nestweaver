use nestweaver_schema::{Note, NoteKind};
use nestweaver_store::GraphStore;

// Ladybug's default value-vector capacity is 2,048 rows. Crossing two complete
// vectors makes this an end-to-end guard for filtered string reads that span
// multiple scan batches instead of a single-vector happy path.
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
