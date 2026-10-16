//! The bot token: `{data_dir}/telegram/bot-token`, owner-only (0600 in a
//! 0700 directory), like the agent sign-in homes. It never leaves the
//! engine: not in RPC replies, not in logs (its `Debug` is redacted and
//! every error text that could echo it goes through [`BotToken::redact`]).

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "bot-token";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BotToken(String);

impl fmt::Debug for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BotToken(***)")
    }
}

impl BotToken {
    /// `123456:ABC-DEF…` as @BotFather hands it out.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let raw = raw.trim();
        let Some((id, secret)) = raw.split_once(':') else {
            return Err("That doesn't look like a bot token (expected 123456:ABC…)");
        };
        let id_ok = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
        let secret_ok = secret.len() >= 20
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if id_ok && secret_ok {
            Ok(Self(raw.to_string()))
        } else {
            Err("That doesn't look like a bot token (expected 123456:ABC…)")
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every copy of the token (and its secret half) masked.
    pub fn redact(&self, text: &str) -> String {
        let masked = text.replace(&self.0, "***");
        match self.0.split_once(':') {
            Some((_, secret)) if !secret.is_empty() => masked.replace(secret, "***"),
            _ => masked,
        }
    }
}

pub(crate) struct TokenFile {
    dir: PathBuf,
}

impl TokenFile {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("telegram"),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    pub fn load(&self) -> Option<BotToken> {
        let raw = std::fs::read_to_string(self.path()).ok()?;
        BotToken::parse(&raw).ok()
    }

    pub fn save(&self, token: &BotToken) -> std::io::Result<()> {
        private_dir(&self.dir)?;
        let tmp = self.dir.join(format!("{FILE_NAME}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(token.expose().as_bytes())?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, self.path())
    }

    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.path()) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "123456789:AAH-sEcReT_tokenValue0123456789";

    #[test]
    fn parses_botfather_tokens_only() {
        assert!(BotToken::parse(TOKEN).is_ok());
        assert!(BotToken::parse(&format!("  {TOKEN}\n")).is_ok());
        assert!(BotToken::parse("hello").is_err());
        assert!(BotToken::parse("abc:AAH-sEcReT_tokenValue0123456789").is_err());
        assert!(BotToken::parse("123:short").is_err());
    }

    #[test]
    fn debug_and_redact_hide_the_secret() {
        let token = BotToken::parse(TOKEN).unwrap();
        assert_eq!(format!("{token:?}"), "BotToken(***)");
        let leaked = format!("GET https://x/bot{TOKEN}/getMe and AAH-sEcReT_tokenValue0123456789");
        let redacted = token.redact(&leaked);
        assert!(!redacted.contains("sEcReT"), "{redacted}");
    }

    #[test]
    fn file_is_owner_only_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let file = TokenFile::new(tmp.path());
        assert!(file.load().is_none());
        let token = BotToken::parse(TOKEN).unwrap();
        file.save(&token).unwrap();
        assert_eq!(file.load(), Some(token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir = std::fs::metadata(&file.dir).unwrap().permissions().mode();
            assert_eq!(dir & 0o777, 0o700);
        }
        file.clear().unwrap();
        file.clear().unwrap();
        assert!(file.load().is_none());
    }
}
