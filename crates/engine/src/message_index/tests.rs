use std::time::{Duration, Instant};

use zeron_doc::{MessageStatus, SegmentWriter};

use super::index::{Index, MessageText, SearchOptions};
use super::*;

fn reads(index: &MessageIndex) -> usize {
    index.inner.snapshot_reads.load(Ordering::Relaxed)
}

fn full_reads(index: &MessageIndex) -> usize {
    index.inner.full_reads.load(Ordering::Relaxed)
}

const FLUSH: Duration = Duration::from_secs(20);

fn entry(id: &str, role: MessageRole, text: &str, created_at: i64) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role,
        parts: vec![MessagePart::Text {
            id: format!("{id}-text"),
            text: text.into(),
        }],
        created_at,
        device_id: "host".into(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
        duration_ms: None,
        turn_stats: None,
    }
}

fn save_chat(store: &DocsStore, chat_id: &str, entries: &[SessionMessageEntry]) {
    let doc = SessionDoc::init(chat_id).unwrap();
    for entry in entries {
        doc.push_message(entry).unwrap();
    }
    store
        .save_snapshot(chat_id, &doc.export_snapshot().unwrap())
        .unwrap();
}

fn found(index: &MessageIndex, query: &str) -> Vec<(String, String)> {
    index
        .search(query, 20, None, None)
        .into_iter()
        .map(|hit| (hit.chat_id, hit.message_id))
        .collect()
}

#[test]
fn indexes_stored_chats_and_tools_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let mut tool = entry("a2", MessageRole::Assistant, "Running the suite.", 2);
    tool.parts.push(MessagePart::Tool {
        id: "t".into(),
        call: ToolCall::Exec {
            command: "cargo nextest run".into(),
        },
        is_error: false,
        resolved: true,
        output: Some("huge output that is never indexed".into()),
        diff: None,
        output_ref: None,
        output_bytes: None,
        diff_ref: None,
        diff_stats: None,
        subagent_ref: None,
        subagent_status: None,
        subagent_tail: None,
    });
    save_chat(
        &store,
        "chat-a",
        &[entry("a1", MessageRole::User, "Please fix the flaky deploy", 1), tool],
    );
    save_chat(
        &store,
        "chat-b",
        &[entry("b1", MessageRole::User, "Deploy notes", 3)],
    );
    let index = MessageIndex::open(store, Some(dir.path()), None);
    assert!(index.flush(FLUSH));
    assert!(index.is_ready());
    let mut deploy = found(&index, "dep");
    deploy.sort();
    assert_eq!(
        deploy,
        [
            ("chat-a".to_owned(), "a1".to_owned()),
            ("chat-b".to_owned(), "b1".to_owned())
        ]
    );
    let hits = index.search("nextest", 5, None, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].field, zeron_proto::MessageSearchField::Tool);
    assert!(found(&index, "huge").is_empty(), "tool output is not indexed");
    index.shutdown();
}

#[test]
fn filter_skips_documents_that_are_not_chats() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    save_chat(&store, "chat", &[entry("m", MessageRole::User, "kept", 1)]);
    save_chat(&store, "chat.pre-chat2", &[entry("m", MessageRole::User, "kept", 1)]);
    let filter: ChatFilter = Arc::new(|id: &str| !id.contains('.'));
    let index = MessageIndex::open(store, None, Some(filter));
    assert!(index.flush(FLUSH));
    assert_eq!(found(&index, "kept"), [("chat".to_owned(), "m".to_owned())]);
    index.shutdown();
}

#[test]
fn stored_index_restarts_without_rereading_and_rebuilds_when_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    for chat in 0..5 {
        save_chat(
            &store,
            &format!("chat-{chat}"),
            &[entry("m", MessageRole::Assistant, &format!("persisted reply {chat}"), 1)],
        );
    }
    let first = MessageIndex::open(store.clone(), Some(dir.path()), None);
    assert!(first.flush(FLUSH));
    assert_eq!(reads(&first), 5);
    first.shutdown();
    drop(first);
    let path = dir.path().join(INDEX_FILE);
    assert!(path.exists());

    // A restart loads the file and reads no snapshot; a changed chat alone
    // is re-read.
    save_chat(
        &store,
        "chat-3",
        &[entry("m", MessageRole::Assistant, "rewritten answer", 9)],
    );
    let second = MessageIndex::open(store.clone(), Some(dir.path()), None);
    assert!(second.flush(FLUSH));
    assert_eq!(reads(&second), 1);
    assert_eq!(second.stats().messages, 5);
    assert_eq!(found(&second, "rewritten").len(), 1);
    assert_eq!(found(&second, "persisted").len(), 4);
    second.shutdown();
    drop(second);

    // Garbage on disk: rebuild from the store.
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    std::fs::write(&path, b"definitely not an index").unwrap();
    let third = MessageIndex::open(store, Some(dir.path()), None);
    assert!(third.flush(FLUSH));
    assert_eq!(reads(&third), 5);
    assert_eq!(found(&third, "persisted").len(), 4);
    third.shutdown();
}

/// First launch after the in-memory index: searches answer from the old file
/// while the database builds in the background, then the old file goes.
#[test]
fn a_first_build_answers_from_the_old_index_then_deletes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let mut old = Index::default();
    for chat in 0..3 {
        let id = format!("chat-{chat}");
        let entries = [entry(
            "m",
            MessageRole::Assistant,
            &format!("migrated reply {chat}"),
            1,
        )];
        save_chat(&store, &id, &entries);
        old.update_chat(&id, &message_texts(entries.to_vec()));
    }
    let legacy = dir.path().join(LEGACY_INDEX_FILE);
    std::fs::write(&legacy, persist::encode(&old)).unwrap();

    let index = MessageIndex::open_with(store, Some(dir.path()), None, true);
    let deadline = Instant::now() + FLUSH;
    while read(&index.inner.legacy).is_none() {
        assert!(Instant::now() < deadline, "old index never loaded");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!index.is_ready(), "still building");
    assert_eq!(
        found(&index, "migrated").len(),
        3,
        "the old index answers meanwhile"
    );
    assert_eq!(reads(&index), 0);

    index.inner.hold_build.store(false, Ordering::Release);
    assert!(index.flush(FLUSH));
    assert!(index.is_ready());
    assert!(read(&index.inner.legacy).is_none(), "old index dropped");
    assert!(!legacy.exists(), "old file deleted");
    assert_eq!(reads(&index), 3);
    assert_eq!(found(&index, "migrated").len(), 3);
    index.shutdown();
}

#[test]
fn streaming_reply_updates_incrementally() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let index = MessageIndex::open(store, None, None);
    assert!(index.flush(FLUSH));
    let doc = Arc::new(SessionDoc::init("live").unwrap());
    doc.push_message(&entry("q", MessageRole::User, "how do lanterns work", 1))
        .unwrap();
    let mut writer = SegmentWriter::begin(&doc, "reply", "host", 2).unwrap();
    let mut text = String::from("Lanterns");
    for word in [" burn", " oil", " through", " a", " wick"] {
        text.push_str(word);
        writer
            .sync(&[MessagePart::Text {
                id: "body".into(),
                text: text.clone(),
            }])
            .unwrap();
        index.note_changed("live", &doc);
    }
    assert!(index.flush(FLUSH));
    assert_eq!(found(&index, "wick"), [("live".to_owned(), "reply".to_owned())]);
    assert_eq!(
        found(&index, "how work"),
        [("live".to_owned(), "q".to_owned())]
    );
    assert_eq!(index.stats().messages, 2);

    // Keep streaming: searches see the new words, nothing is written and
    // the transcript is never read whole while the turn is in flight.
    text.push_str(" slowly");
    writer
        .sync(&[MessagePart::Text {
            id: "body".into(),
            text: text.clone(),
        }])
        .unwrap();
    index.note_changed("live", &doc);
    assert!(index.flush(FLUSH));
    assert_eq!(found(&index, "slow").len(), 1);
    assert_eq!(index.stats().messages, 2);
    assert_eq!(full_reads(&index), 0, "no whole-transcript read mid-turn");
    assert_eq!(
        index.inner.with_db(|db| db.totals()).docs,
        0,
        "nothing stored mid-turn"
    );

    // The turn ends: one full read stores it and the overlay empties.
    writer
        .finish(
            &[MessagePart::Text {
                id: "body".into(),
                text: text.clone(),
            }],
            MessageStatus::Complete,
        )
        .unwrap();
    index.note_changed("live", &doc);
    assert!(index.flush(FLUSH));
    assert_eq!(full_reads(&index), 1);
    assert!(read(&index.inner.overlay).is_empty());
    assert_eq!(index.inner.with_db(|db| db.totals()).docs, 2);
    assert_eq!(found(&index, "slow").len(), 1);
    assert_eq!(index.stats().messages, 2);

    index.remove_chat("live");
    assert!(index.flush(FLUSH));
    assert!(found(&index, "lantern").is_empty());
    index.shutdown();
}

#[test]
fn live_changes_are_debounced() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(DocsStore::open(dir.path()).unwrap());
    let index = MessageIndex::open(store, None, None);
    assert!(index.flush(FLUSH));
    let doc = Arc::new(SessionDoc::init("chat").unwrap());
    doc.push_message(&entry("m", MessageRole::User, "debounced words", 1))
        .unwrap();
    index.note_changed("chat", &doc);
    // Not yet: the change waits out the quiet period.
    assert!(found(&index, "debounced").is_empty());
    let deadline = Instant::now() + Duration::from_secs(10);
    while found(&index, "debounced").is_empty() {
        assert!(Instant::now() < deadline, "live change never indexed");
        std::thread::sleep(Duration::from_millis(20));
    }
    index.shutdown();
}

// ---------------------------------------------------------------------------
// Synthetic scale: ~360 chats / ~43k messages, the owner's imported history.
// ---------------------------------------------------------------------------

/// Deterministic xorshift, so the corpus and its timings are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Zipf-ish rank in `0..n`: common words dominate like real prose.
    fn zipf(&mut self, n: usize) -> usize {
        let u = (self.next() % 1_000_000) as f64 / 1_000_000.0;
        ((n as f64).powf(u) as usize).saturating_sub(1).min(n - 1)
    }
}

pub(super) fn synthetic_corpus(chats: usize, per_chat: usize) -> Index {
    let mut index = Index::default();
    for (chat, messages) in synthetic_chats(chats, per_chat) {
        index.update_chat(&chat, &messages);
    }
    index
}

/// The synthetic corpus as `(chat id, transcript)` in index order.
pub(super) fn synthetic_chats(chats: usize, per_chat: usize) -> Vec<(String, Vec<MessageText>)> {
    const SYLLABLES: [&str; 24] = [
        "ka", "lo", "mi", "ren", "sto", "va", "qui", "dex", "tor", "pa", "ze", "lin", "ar",
        "bo", "cu", "fen", "gri", "hal", "jo", "nu", "or", "pe", "sy", "wex",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    // A 40k-word vocabulary plus real coding terms near the top.
    let mut vocabulary: Vec<String> = [
        "the", "a", "to", "and", "fix", "test", "error", "build", "cargo", "function", "file",
        "search", "index", "deploy", "worker", "message", "query", "rust", "async", "thread",
    ]
    .iter()
    .map(|w| (*w).to_owned())
    .collect();
    while vocabulary.len() < 40_000 {
        let parts = 1 + rng.next() % 3;
        let word: String = (0..parts)
            .map(|_| SYLLABLES[(rng.next() % SYLLABLES.len() as u64) as usize])
            .collect();
        vocabulary.push(word);
    }
    let mut corpus = Vec::with_capacity(chats);
    let day = 86_400_000i64;
    for chat in 0..chats {
        let messages: Vec<MessageText> = (0..per_chat)
            .map(|ix| {
                let words = 20 + rng.next() % 120;
                let mut text = String::new();
                for w in 0..words {
                    if w > 0 {
                        text.push(if rng.next() % 12 == 0 { '\n' } else { ' ' });
                    }
                    text.push_str(&vocabulary[rng.zipf(vocabulary.len())]);
                    if rng.next() % 40 == 0 {
                        // Identifiers and hashes inflate real vocabularies.
                        text.push_str(&format!("_{:x}", rng.next() % 0xffff_ffff));
                    }
                }
                let tools = if ix % 2 == 1 {
                    format!("cargo test -p crate_{}", rng.next() % 50)
                } else {
                    String::new()
                };
                MessageText {
                    id: format!("m{ix}"),
                    role: if ix % 2 == 0 {
                        MessageSearchRole::User
                    } else {
                        MessageSearchRole::Assistant
                    },
                    created_at: (chat as i64) * day / 4 + ix as i64 * 60_000,
                    text,
                    tools,
                }
            })
            .collect();
        corpus.push((format!("chat-{chat}"), messages));
    }
    corpus
}

/// Per-query bound: 50 ms is the release target; unoptimized test builds of
/// this crate run several times slower, so they get a generous ceiling.
fn query_bound() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(50)
    }
}

#[test]
fn synthetic_owner_scale_queries_stay_fast() {
    let started = Instant::now();
    let index = synthetic_corpus(360, 120);
    let built = started.elapsed();
    assert_eq!(index.message_count(), 360 * 120);
    let options = SearchOptions {
        limit: 50,
        per_chat: Some(1),
        chat_filter: None,
        now_ms: 400 * 86_400_000,
    };
    let mut worst = (Duration::ZERO, "");
    for query in [
        "t",
        "e",
        "ka",
        "the",
        "fix test",
        "search index deploy",
        "cargo",
        "zzzz",
        "wexgri",
    ] {
        // Best of three, so a scheduler hiccup on a shared CI box is not a failure.
        let elapsed = (0..3)
            .map(|_| {
                let at = Instant::now();
                let _ = index.search(query, &options);
                at.elapsed()
            })
            .min()
            .unwrap();
        if elapsed > worst.0 {
            worst = (elapsed, query);
        }
        assert!(
            elapsed < query_bound(),
            "query {query:?} took {elapsed:?} over {} messages",
            index.message_count()
        );
    }
    let encoded = persist::encode(&index);
    let decode_started = Instant::now();
    let decoded = persist::decode(&encoded).unwrap();
    let decoded_in = decode_started.elapsed();
    assert_eq!(decoded.message_count(), index.message_count());
    println!(
        "synthetic: {} msgs, {} terms, built {built:?}, worst query {:?} ({:?}), file {} KiB, load {decoded_in:?}",
        index.message_count(),
        index.term_count(),
        worst.0,
        worst.1,
        encoded.len() / 1024,
    );
}

/// The database ranks exactly like the in-memory index it replaces: same
/// hits, order, snippets and scores, on the owner-scale corpus, from single
/// letters to multi-word queries, with and without the per-chat cap.
#[test]
fn the_database_ranks_like_the_old_index() {
    let corpus = synthetic_chats(360, 120);
    let mut old = Index::default();
    for (chat, messages) in &corpus {
        old.update_chat(chat, messages);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut db = db::Db::open(Some(&dir.path().join(INDEX_FILE))).unwrap();
    let started = Instant::now();
    for batch in corpus.chunks(32) {
        db.batch(|db| {
            for (chat, messages) in batch {
                db.update_chat(chat, messages)?;
            }
            Ok(())
        })
        .unwrap();
    }
    db.optimize().unwrap();
    let built = started.elapsed();
    assert_eq!(db.totals().unwrap().docs as usize, old.message_count());
    assert_eq!(db.term_count().unwrap(), old.term_count());

    let options = |per_chat| SearchOptions {
        limit: 50,
        per_chat,
        chat_filter: None,
        now_ms: 400 * 86_400_000,
    };
    let queries = [
        "t",
        "e",
        "a",
        "k",
        "s",
        "ka",
        "the",
        "fix test",
        "search index deploy",
        "cargo",
        "zzzz",
        "wexgri",
        "lo mi",
        "crate_1",
        "deploy worker",
        "tor",
        "error build",
        "qui dex",
    ];
    let mut timings = Vec::new();
    for query in queries {
        for per_chat in [None, Some(1)] {
            let old_at = Instant::now();
            let want = old.search(query, &options(per_chat));
            let old_took = old_at.elapsed();
            let terms = tokens::query_terms(query);
            let at = Instant::now();
            let got = db.search(&terms, &options(per_chat), &[]).unwrap();
            timings.push((query, per_chat, old_took, at.elapsed()));
            assert_eq!(got.len(), want.len(), "{query:?} {per_chat:?}");
            for (got, want) in got.iter().zip(&want) {
                assert_eq!(
                    (
                        &got.chat_id,
                        &got.message_id,
                        &got.snippet,
                        &got.ranges,
                        got.field
                    ),
                    (
                        &want.chat_id,
                        &want.message_id,
                        &want.snippet,
                        &want.ranges,
                        want.field
                    ),
                    "{query:?} {per_chat:?}"
                );
                assert_eq!(
                    got.score.to_bits(),
                    want.score.to_bits(),
                    "{query:?} {per_chat:?}"
                );
            }
        }
    }
    timings.sort_by_key(|t| std::cmp::Reverse(t.3));
    println!(
        "database: {} msgs, built {built:?}; slowest (query, per_chat, old, new): {:?}",
        old.message_count(),
        &timings[..6]
    );
}
