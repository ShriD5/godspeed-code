use std::time::{Duration, Instant};

use zeron_doc::{MessageStatus, SegmentWriter};

use super::index::{Index, MessageText, SearchOptions};
use super::*;

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
fn persisted_index_restarts_without_rereading_and_rebuilds_when_corrupt() {
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
    assert_eq!(first.inner.snapshot_reads.load(Ordering::Relaxed), 5);
    first.shutdown();
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
    assert_eq!(second.inner.snapshot_reads.load(Ordering::Relaxed), 1);
    assert_eq!(second.stats().messages, 5);
    assert_eq!(found(&second, "rewritten").len(), 1);
    assert_eq!(found(&second, "persisted").len(), 4);
    second.shutdown();

    // Garbage on disk: rebuild from the store and write a valid file.
    std::fs::write(&path, b"definitely not an index").unwrap();
    let third = MessageIndex::open(store, Some(dir.path()), None);
    assert!(third.flush(FLUSH));
    assert_eq!(third.inner.snapshot_reads.load(Ordering::Relaxed), 5);
    assert_eq!(found(&third, "persisted").len(), 4);
    third.shutdown();
    assert!(persist::decode(&std::fs::read(&path).unwrap()).is_ok());
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
    assert_eq!(index.stats().messages, 2);

    // Keep streaming: the reply re-indexes in place.
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
    assert_eq!(index.inner.read().docs.len(), 2, "no slot churn while streaming");

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
    let mut index = Index::default();
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
        index.update_chat(&format!("chat-{chat}"), &messages);
    }
    index
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
    for query in ["t", "e", "ka", "the", "fix test", "search index deploy", "cargo", "zzzz", "wexgri"] {
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
