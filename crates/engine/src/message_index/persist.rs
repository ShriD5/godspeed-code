//! On-disk form of the message index, so a restart loads postings instead of
//! re-reading every transcript.
//!
//! Little-endian varints throughout: header, chats (id + snapshot marker),
//! live documents renumbered densely, then the sorted dictionary with
//! delta-coded postings, and a trailing FNV-1a checksum over everything
//! before it. Any mismatch (magic, version, checksum, a truncated or
//! malformed field) rejects the whole file; the caller rebuilds.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use zeron_proto::MessageSearchRole;

use super::index::{ChatEntry, Doc, Index, Posting};

const MAGIC: &[u8; 8] = b"GSMSGIDX";
/// Bump on any format or tokenizer change: an old file then rebuilds.
const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("not a message index file")]
    Magic,
    #[error("message index version {0} is not {VERSION}")]
    Version(u32),
    #[error("message index checksum mismatch")]
    Checksum,
    #[error("message index is truncated or malformed")]
    Malformed,
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in bytes.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        hash ^= u64::from_le_bytes(word);
        hash = hash.wrapping_mul(0x0100_0000_01b3).rotate_left(23);
    }
    hash ^ bytes.len() as u64
}

struct Writer(Vec<u8>);

impl Writer {
    fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.0.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
    }

    fn signed(&mut self, value: i64) {
        self.varint(((value << 1) ^ (value >> 63)) as u64);
    }

    fn str(&mut self, value: &str) {
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value.as_bytes());
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at).ok_or(DecodeError::Malformed)?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError::Malformed)
    }

    fn signed(&mut self) -> Result<i64, DecodeError> {
        let raw = self.varint()?;
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }

    fn len(&mut self) -> Result<usize, DecodeError> {
        let len = usize::try_from(self.varint()?).map_err(|_| DecodeError::Malformed)?;
        // No count can exceed the bytes left to describe it.
        (len <= self.bytes.len() - self.at)
            .then_some(len)
            .ok_or(DecodeError::Malformed)
    }

    fn str(&mut self) -> Result<&'a str, DecodeError> {
        let len = self.len()?;
        let bytes = &self.bytes[self.at..self.at + len];
        self.at += len;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::Malformed)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.bytes.get(self.at).ok_or(DecodeError::Malformed)?;
        self.at += 1;
        Ok(byte)
    }
}

fn role_byte(role: MessageSearchRole) -> u8 {
    match role {
        MessageSearchRole::User => 0,
        MessageSearchRole::Assistant => 1,
    }
}

pub(crate) fn encode(index: &Index) -> Vec<u8> {
    let mut w = Writer(Vec::with_capacity(1 << 20));
    w.0.extend_from_slice(MAGIC);
    w.0.extend_from_slice(&VERSION.to_le_bytes());

    let chats: Vec<(&Arc<str>, &ChatEntry)> = index.chats.iter().collect();
    let chat_ix: HashMap<&str, u64> = chats
        .iter()
        .enumerate()
        .map(|(ix, (id, _))| (id.as_ref(), ix as u64))
        .collect();
    w.varint(chats.len() as u64);
    for (id, chat) in &chats {
        w.str(id);
        match chat.marker {
            Some((saved_at, len)) => {
                w.0.push(1);
                w.signed(saved_at);
                w.signed(len);
            }
            None => w.0.push(0),
        }
    }

    // Dense renumbering drops the slots of removed entries.
    let mut renumber = vec![u32::MAX; index.docs.len()];
    let live: Vec<&Doc> = index
        .docs
        .iter()
        .enumerate()
        .filter_map(|(slot, doc)| {
            let doc = doc.as_ref()?;
            renumber[slot] = 0;
            Some(doc)
        })
        .collect();
    let mut next = 0u32;
    for slot in renumber.iter_mut().filter(|slot| **slot == 0) {
        *slot = next;
        next += 1;
    }
    w.varint(live.len() as u64);
    for doc in live {
        w.varint(chat_ix[doc.chat.as_ref()]);
        w.str(&doc.message_id);
        w.0.push(role_byte(doc.role));
        w.signed(doc.created_at);
        w.str(&doc.text);
        w.str(&doc.tools);
        w.varint(u64::from(doc.len));
        w.0.extend_from_slice(&doc.hash.to_le_bytes());
    }

    w.varint(index.terms.len() as u64);
    for (term, &id) in &index.terms {
        w.str(term);
        let list = &index.postings[id as usize];
        w.varint(list.len() as u64);
        let mut previous = 0u32;
        for posting in list {
            let doc = renumber[posting.doc as usize];
            w.varint(u64::from(doc - previous));
            previous = doc;
            w.varint(u64::from(posting.text_tf));
            w.varint(u64::from(posting.tool_tf));
        }
    }
    let sum = checksum(&w.0);
    w.0.extend_from_slice(&sum.to_le_bytes());
    w.0
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Index, DecodeError> {
    if bytes.len() < MAGIC.len() + 4 + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(DecodeError::Magic);
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    let (body, sum) = bytes.split_at(bytes.len() - 8);
    if checksum(body) != u64::from_le_bytes(sum.try_into().unwrap()) {
        return Err(DecodeError::Checksum);
    }
    let mut r = Reader { bytes: body, at: 12 };

    let mut index = Index::default();
    let chat_count = r.len()?;
    let mut chat_ids: Vec<Arc<str>> = Vec::with_capacity(chat_count);
    for _ in 0..chat_count {
        let id: Arc<str> = r.str()?.into();
        let marker = match r.u8()? {
            0 => None,
            1 => Some((r.signed()?, r.signed()?)),
            _ => return Err(DecodeError::Malformed),
        };
        index.chats.insert(
            id.clone(),
            ChatEntry {
                marker,
                messages: HashMap::new(),
            },
        );
        chat_ids.push(id);
    }

    let doc_count = r.len()?;
    index.docs.reserve_exact(doc_count);
    for slot in 0..doc_count {
        let chat = chat_ids
            .get(r.len()?)
            .ok_or(DecodeError::Malformed)?
            .clone();
        let message_id: Box<str> = r.str()?.into();
        let role = match r.u8()? {
            0 => MessageSearchRole::User,
            1 => MessageSearchRole::Assistant,
            _ => return Err(DecodeError::Malformed),
        };
        let created_at = r.signed()?;
        let text: Box<str> = r.str()?.into();
        let tools: Box<str> = r.str()?.into();
        let len = u32::try_from(r.varint()?).map_err(|_| DecodeError::Malformed)?;
        let hash_bytes = body.get(r.at..r.at + 8).ok_or(DecodeError::Malformed)?;
        let hash = u64::from_le_bytes(hash_bytes.try_into().unwrap());
        r.at += 8;
        index
            .chats
            .get_mut(&chat)
            .ok_or(DecodeError::Malformed)?
            .messages
            .insert(message_id.clone(), slot as u32);
        index.live += 1;
        index.total_len += u64::from(len);
        index.docs.push(Some(Doc {
            chat,
            message_id,
            role,
            created_at,
            text,
            tools,
            len,
            hash,
        }));
    }

    let term_count = r.len()?;
    let mut terms = Vec::with_capacity(term_count);
    index.postings.reserve_exact(term_count);
    let mut last: Option<&str> = None;
    for id in 0..term_count {
        let term = r.str()?;
        // Sorted and unique, so the map below is built in one pass.
        if last.is_some_and(|last| last >= term) {
            return Err(DecodeError::Malformed);
        }
        last = Some(term);
        let count = r.len()?;
        let mut list = Vec::with_capacity(count);
        let mut doc = 0u64;
        for ix in 0..count {
            let delta = r.varint()?;
            if ix > 0 && delta == 0 {
                return Err(DecodeError::Malformed);
            }
            doc += delta;
            if doc >= doc_count as u64 {
                return Err(DecodeError::Malformed);
            }
            let text_tf = u16::try_from(r.varint()?).map_err(|_| DecodeError::Malformed)?;
            let tool_tf = u16::try_from(r.varint()?).map_err(|_| DecodeError::Malformed)?;
            list.push(Posting {
                doc: doc as u32,
                text_tf,
                tool_tf,
            });
        }
        terms.push((Box::<str>::from(term), id as u32));
        index.postings.push(list);
    }
    if r.at != body.len() {
        return Err(DecodeError::Malformed);
    }
    index.terms = BTreeMap::from_iter(terms);
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::super::index::{MessageText, SearchOptions};
    use super::*;

    fn sample() -> Index {
        let mut index = Index::default();
        let message = |id: &str, text: &str, tools: &str| MessageText {
            id: id.into(),
            role: MessageSearchRole::User,
            created_at: -5,
            text: text.into(),
            tools: tools.into(),
        };
        index.update_chat(
            "a",
            &[
                message("1", "alpha beta", ""),
                message("2", "gamma", "cargo build"),
            ],
        );
        index.update_chat("b", &[message("3", "alpha délta", "")]);
        index.set_marker("a", Some((10, 20)));
        // Leave a hole the encoder must renumber around.
        index.update_chat("a", &[message("2", "gamma", "cargo build")]);
        index
    }

    fn search(index: &Index, query: &str) -> Vec<(String, String)> {
        let options = SearchOptions {
            limit: 10,
            ..Default::default()
        };
        let mut found: Vec<_> = index
            .search(query, &options)
            .into_iter()
            .map(|hit| (hit.chat_id, hit.message_id))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn round_trip_preserves_results_markers_and_updates() {
        let index = sample();
        let mut loaded = decode(&encode(&index)).unwrap();
        assert_eq!(loaded.message_count(), 2);
        assert_eq!(loaded.marker("a"), Some((10, 20)));
        assert_eq!(loaded.marker("b"), None);
        for query in ["alpha", "car", "dé", "gamma build"] {
            assert_eq!(search(&loaded, query), search(&index, query), "{query}");
        }
        // A loaded index keeps updating incrementally.
        let changed = loaded.update_chat(
            "b",
            &[MessageText {
                id: "3".into(),
                role: MessageSearchRole::User,
                created_at: -5,
                text: "alpha délta".into(),
                tools: String::new(),
            }],
        );
        assert!(!changed, "hashes survive the round trip");
        assert!(loaded.remove_chat("a"));
        assert!(search(&loaded, "gamma").is_empty());
    }

    #[test]
    fn corruption_is_rejected() {
        let bytes = encode(&sample());
        assert!(matches!(decode(b"nope"), Err(DecodeError::Magic)));
        let mut flipped = bytes.clone();
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0x40;
        assert!(matches!(decode(&flipped), Err(DecodeError::Checksum)));
        assert!(decode(&bytes[..bytes.len() - 3]).is_err());
        let mut version = bytes.clone();
        version[8] = 99;
        assert!(matches!(decode(&version), Err(DecodeError::Version(99))));
    }
}
