//! The prompt stash: Cmd+S sets the composer's draft aside (its text and
//! staged attachments) and clears it; the "Stashed" chip brings a prompt back
//! or deletes it. One stash per chat, kept on this device only:
//! `{data_dir}/prompt-stash.json`, with each attachment's bytes beside it
//! under `prompt-stash/`, so a stash outlives a restart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::attachments::StagedAttachment;

const FILE_NAME: &str = "prompt-stash.json";
const FILES_DIR: &str = "prompt-stash";

/// The app's one stash, shared by the main and side-chat composers so none
/// of them writes over another's entries. Without [`init`] (tests) it lives
/// in memory only.
#[derive(Default)]
struct PromptStashStore(Option<PromptStash>);

impl Global for PromptStashStore {}

pub(crate) fn init(data_dir: PathBuf, cx: &mut App) {
    cx.set_global(PromptStashStore(Some(PromptStash::load(Some(data_dir)))));
}

/// A chat's stashed prompts, oldest first.
pub(crate) fn entries(key: &str, cx: &App) -> Vec<StashedPrompt> {
    cx.try_global::<PromptStashStore>()
        .and_then(|store| store.0.as_ref())
        .map(|stash| stash.entries(key).to_vec())
        .unwrap_or_default()
}

pub(crate) fn update<R>(cx: &mut App, change: impl FnOnce(&mut PromptStash) -> R) -> R {
    change(
        cx.default_global::<PromptStashStore>()
            .0
            .get_or_insert_with(|| PromptStash::load(None)),
    )
}

/// A stashed attachment: its file under `prompt-stash/` and the chip number
/// the draft's text mentions it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StashedAttachment {
    pub name: String,
    /// Relative to the stash's files folder.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StashedPrompt {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<StashedAttachment>,
    pub stashed_at: DateTime<Utc>,
}

impl StashedPrompt {
    /// One line for the stash menu.
    pub(crate) fn snippet(&self) -> String {
        const MAX: usize = 80;
        let text = zeron_proto::attachment_mentions::attachment_mention_prompt(&self.text);
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            return match line.char_indices().nth(MAX) {
                Some((cut, _)) => format!("{}…", &line[..cut]),
                None => line,
            };
        }
        match self.attachments.len() {
            1 => "1 attachment".to_owned(),
            count => format!("{count} attachments"),
        }
    }
}

/// A prompt taken back out of the stash, ready for the composer.
pub(crate) struct RestoredPrompt {
    pub text: String,
    pub attachments: Vec<StagedAttachment>,
    /// Attachments whose file could not be read back.
    pub missing: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    #[serde(default)]
    chats: BTreeMap<String, Vec<StashedPrompt>>,
}

/// Every chat's stash, newest last.
#[derive(Debug, Default)]
pub(crate) struct PromptStash {
    dir: Option<PathBuf>,
    chats: BTreeMap<String, Vec<StashedPrompt>>,
}

impl PromptStash {
    /// Read the stash under `dir`; empty when there is none or it is
    /// unreadable (a stash is a convenience, never worth failing over).
    pub(crate) fn load(dir: Option<PathBuf>) -> Self {
        let chats = dir
            .as_deref()
            .and_then(|dir| std::fs::read_to_string(dir.join(FILE_NAME)).ok())
            .and_then(|text| {
                serde_json::from_str::<Saved>(&text)
                    .map_err(|error| tracing::warn!(%error, "prompt stash unreadable"))
                    .ok()
            })
            .map(|saved| saved.chats)
            .unwrap_or_default();
        Self { dir, chats }
    }

    pub(crate) fn entries(&self, key: &str) -> &[StashedPrompt] {
        self.chats.get(key).map_or(&[], Vec::as_slice)
    }

    /// Set a draft aside. Attachment bytes go to disk first, so a stash that
    /// is listed always has its files.
    pub(crate) fn push(&mut self, key: &str, text: String, attachments: &[StagedAttachment]) {
        let id = uuid::Uuid::new_v4().to_string();
        let mut stashed = Vec::with_capacity(attachments.len());
        for (index, attachment) in attachments.iter().enumerate() {
            let file = format!("{id}/{index}/{}", safe_name(&attachment.name));
            if let Some(dir) = self.files_dir() {
                let path = dir.join(&file);
                let written = path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&path, attachment.bytes()));
                if let Err(error) = written {
                    tracing::warn!(%error, "could not stash an attachment");
                    continue;
                }
            }
            stashed.push(StashedAttachment {
                name: attachment.name.clone(),
                file,
                mention: attachment.mention,
            });
        }
        // Without a folder (tests) the bytes stay in memory.
        if self.dir.is_none() {
            MEMORY.with(|memory| {
                let mut memory = memory.borrow_mut();
                for (attachment, entry) in attachments.iter().zip(&stashed) {
                    memory.insert(entry.file.clone(), attachment.clone());
                }
            });
        }
        self.chats.entry(key.to_owned()).or_default().push(StashedPrompt {
            id,
            text,
            attachments: stashed,
            stashed_at: Utc::now(),
        });
        self.save();
    }

    /// Take a prompt out of the stash to restore it.
    pub(crate) fn take(&mut self, key: &str, id: &str) -> Option<RestoredPrompt> {
        let entry = self.remove_entry(key, id)?;
        let mut attachments = Vec::with_capacity(entry.attachments.len());
        let mut missing = 0;
        for stashed in &entry.attachments {
            match self.read_back(stashed) {
                Some(mut attachment) => {
                    attachment.mention = stashed.mention;
                    attachments.push(attachment);
                }
                None => missing += 1,
            }
        }
        self.remove_files(&entry);
        self.save();
        Some(RestoredPrompt {
            text: entry.text,
            attachments,
            missing,
        })
    }

    /// Delete a stashed prompt and its files.
    pub(crate) fn delete(&mut self, key: &str, id: &str) -> bool {
        let Some(entry) = self.remove_entry(key, id) else {
            return false;
        };
        self.remove_files(&entry);
        self.save();
        true
    }

    /// A deleted chat's stash goes with it.
    pub(crate) fn purge(&mut self, key: &str) {
        let Some(entries) = self.chats.remove(key) else {
            return;
        };
        for entry in &entries {
            self.remove_files(entry);
        }
        self.save();
    }

    fn remove_entry(&mut self, key: &str, id: &str) -> Option<StashedPrompt> {
        let entries = self.chats.get_mut(key)?;
        let at = entries.iter().position(|entry| entry.id == id)?;
        let entry = entries.remove(at);
        if entries.is_empty() {
            self.chats.remove(key);
        }
        Some(entry)
    }

    fn files_dir(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(FILES_DIR))
    }

    fn read_back(&self, stashed: &StashedAttachment) -> Option<StagedAttachment> {
        let Some(dir) = self.files_dir() else {
            return MEMORY.with(|memory| memory.borrow_mut().remove(&stashed.file));
        };
        crate::attachments::stage_file(&dir.join(&stashed.file))
            .map_err(|error| tracing::warn!(%error, "stashed attachment unreadable"))
            .ok()
    }

    fn remove_files(&self, entry: &StashedPrompt) {
        if let Some(dir) = self.files_dir() {
            let _ = std::fs::remove_dir_all(dir.join(&entry.id));
        } else {
            MEMORY.with(|memory| {
                let mut memory = memory.borrow_mut();
                for stashed in &entry.attachments {
                    memory.remove(&stashed.file);
                }
            });
        }
    }

    /// Write atomically (temp file + rename), like the settings file.
    fn save(&self) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        if let Err(error) = write_atomically(dir, &self.chats) {
            tracing::warn!(%error, "could not save the prompt stash");
        }
    }
}

fn write_atomically(
    dir: &Path,
    chats: &BTreeMap<String, Vec<StashedPrompt>>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&Saved {
        chats: chats.clone(),
    })
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// A file name safe to join under the stash folder.
fn safe_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base.is_empty() || base == "." || base == ".." {
        "attachment".to_owned()
    } else {
        base
    }
}

thread_local! {
    /// Attachment bytes for stashes without a folder (tests).
    static MEMORY: std::cell::RefCell<std::collections::HashMap<String, StagedAttachment>> =
        Default::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(name: &str, mention: Option<u32>) -> StagedAttachment {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let mut staged = crate::attachments::stage_clipboard_image(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            bytes.into_inner(),
        ));
        staged.name = name.into();
        staged.mention = mention;
        staged
    }

    #[test]
    fn a_stash_survives_a_restart_and_restores_its_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let chip = zeron_proto::attachment_mentions::attachment_mention_link(1, None);
        let mut stash = PromptStash::load(Some(dir.path().into()));
        stash.push("chat-a", format!("look at {chip}"), &[png("shot.png", Some(1))]);
        stash.push("chat-a", "second".into(), &[]);
        stash.push("chat-b", "other chat".into(), &[]);

        let mut reopened = PromptStash::load(Some(dir.path().into()));
        assert_eq!(reopened.entries("chat-a").len(), 2);
        assert_eq!(reopened.entries("chat-b").len(), 1);
        assert!(reopened.entries("new").is_empty());
        let first = reopened.entries("chat-a")[0].clone();
        assert_eq!(first.snippet(), "look at Image 1");
        let restored = reopened.take("chat-a", &first.id).unwrap();
        assert_eq!(restored.text, format!("look at {chip}"));
        assert_eq!(restored.missing, 0);
        assert_eq!(restored.attachments.len(), 1);
        assert_eq!(restored.attachments[0].name, "shot.png");
        assert_eq!(restored.attachments[0].mention, Some(1));
        assert!(restored.attachments[0].image().is_some());
        assert!(
            !dir.path().join(FILES_DIR).join(&first.id).exists(),
            "restoring removes the stashed files"
        );

        let again = PromptStash::load(Some(dir.path().into()));
        assert_eq!(again.entries("chat-a").len(), 1);
        assert_eq!(again.entries("chat-a")[0].text, "second");
    }

    #[test]
    fn delete_and_purge_remove_entries_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut stash = PromptStash::load(Some(dir.path().into()));
        stash.push("chat", "keep".into(), &[]);
        stash.push("chat", "drop".into(), &[png("a.png", None)]);
        let drop_id = stash.entries("chat")[1].id.clone();
        assert!(stash.delete("chat", &drop_id));
        assert!(!stash.delete("chat", &drop_id), "already gone");
        assert!(!dir.path().join(FILES_DIR).join(&drop_id).exists());
        assert_eq!(stash.entries("chat").len(), 1);
        stash.purge("chat");
        assert!(stash.entries("chat").is_empty());
        assert!(PromptStash::load(Some(dir.path().into())).entries("chat").is_empty());
    }

    #[test]
    fn a_missing_file_restores_the_rest_and_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut stash = PromptStash::load(Some(dir.path().into()));
        stash.push("chat", String::new(), &[png("a.png", None), png("b.png", None)]);
        let entry = stash.entries("chat")[0].clone();
        assert_eq!(entry.snippet(), "2 attachments");
        std::fs::remove_file(dir.path().join(FILES_DIR).join(&entry.attachments[0].file))
            .unwrap();
        let restored = stash.take("chat", &entry.id).unwrap();
        assert_eq!(restored.attachments.len(), 1);
        assert_eq!(restored.missing, 1);
    }

    #[test]
    fn a_corrupt_file_reads_as_an_empty_stash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "{not json").unwrap();
        assert!(PromptStash::load(Some(dir.path().into())).entries("x").is_empty());
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(".."), "attachment");
    }
}
