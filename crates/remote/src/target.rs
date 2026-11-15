//! `user@host[:port]` parsing. The result is passed to `ssh` as separate
//! arguments, never through a shell, but it is still validated strictly: a
//! value that starts with `-` would be read as an ssh option.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SshTarget {
    /// Login name; `None` lets ssh config (or the local user name) decide.
    pub user: Option<String>,
    /// Host name, ssh config alias, IPv4 or IPv6 address (no brackets).
    pub host: String,
    /// `None` lets ssh config (or 22) decide.
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    Empty,
    /// Whitespace, quotes, shell or control characters.
    InvalidCharacter(char),
    /// Would be parsed by ssh as an option.
    LeadingDash,
    InvalidUser,
    InvalidHost,
    InvalidPort,
}

impl fmt::Display for TargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "Enter a server as user@host"),
            Self::InvalidCharacter(c) if c.is_whitespace() => {
                write!(f, "Remove the spaces: enter the server as user@host")
            }
            Self::InvalidCharacter(c) => write!(f, "“{c}” can't appear in a server address"),
            Self::LeadingDash => write!(f, "A server address can't start with “-”"),
            Self::InvalidUser => write!(f, "That user name isn't valid"),
            Self::InvalidHost => write!(f, "That host name isn't valid"),
            Self::InvalidPort => write!(f, "The port must be a number from 1 to 65535"),
        }
    }
}

impl std::error::Error for TargetError {}

impl SshTarget {
    /// Accepts `host`, `user@host`, `user@host:2222`, `ssh://user@host:2222`,
    /// `user@[2001:db8::1]:2222` and a bare IPv6 address.
    pub fn parse(input: &str) -> Result<Self, TargetError> {
        let mut rest = input.trim();
        if let Some(stripped) = rest.strip_prefix("ssh://") {
            rest = stripped.strip_suffix('/').unwrap_or(stripped);
        }
        if rest.is_empty() {
            return Err(TargetError::Empty);
        }
        if let Some(c) = rest
            .chars()
            .find(|c| c.is_whitespace() || c.is_control() || "'\"`$;&|<>(){}\\!*?#~,".contains(*c))
        {
            return Err(TargetError::InvalidCharacter(c));
        }
        if rest.starts_with('-') {
            return Err(TargetError::LeadingDash);
        }
        let (user, hostport) = match rest.split_once('@') {
            Some((user, hostport)) => (Some(user), hostport),
            None => (None, rest),
        };
        if let Some(user) = user
            && (user.is_empty()
                || user.starts_with('-')
                || !user
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Err(TargetError::InvalidUser);
        }
        if hostport.starts_with('-') {
            return Err(TargetError::LeadingDash);
        }
        let (host, port) = split_host_port(hostport)?;
        Ok(Self {
            user: user.map(str::to_owned),
            host,
            port,
        })
    }

    /// The ssh destination argument: `user@host` (IPv6 without brackets, as
    /// ssh expects with `-p`).
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// Port arguments for ssh (`-p 2222`), empty when ssh config decides.
    pub fn port_args(&self) -> Vec<String> {
        self.port
            .map(|port| vec!["-p".to_owned(), port.to_string()])
            .unwrap_or_default()
    }
}

impl fmt::Display for SshTarget {
    /// The canonical form, which also round-trips through [`SshTarget::parse`]
    /// and keys saved servers.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        match (self.port, self.host.contains(':')) {
            (Some(port), true) => write!(f, "[{}]:{port}", self.host),
            (Some(port), false) => write!(f, "{}:{port}", self.host),
            (None, _) => write!(f, "{}", self.host),
        }
    }
}

fn split_host_port(hostport: &str) -> Result<(String, Option<u16>), TargetError> {
    if let Some(inner) = hostport.strip_prefix('[') {
        let (addr, after) = inner.split_once(']').ok_or(TargetError::InvalidHost)?;
        if !valid_ipv6(addr) {
            return Err(TargetError::InvalidHost);
        }
        let port = match after {
            "" => None,
            _ => Some(parse_port(
                after.strip_prefix(':').ok_or(TargetError::InvalidHost)?,
            )?),
        };
        return Ok((addr.to_owned(), port));
    }
    match hostport.matches(':').count() {
        0 => valid_host(hostport)
            .then(|| (hostport.to_owned(), None))
            .ok_or(TargetError::InvalidHost),
        1 => {
            let (host, port) = hostport.split_once(':').expect("one colon");
            if !valid_host(host) {
                return Err(TargetError::InvalidHost);
            }
            Ok((host.to_owned(), Some(parse_port(port)?)))
        }
        _ => valid_ipv6(hostport)
            .then(|| (hostport.to_owned(), None))
            .ok_or(TargetError::InvalidHost),
    }
}

fn parse_port(port: &str) -> Result<u16, TargetError> {
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return Err(TargetError::InvalidPort);
    }
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(TargetError::InvalidPort),
        Ok(port) => Ok(port),
    }
}

/// Host names, ssh config aliases (may contain `_`) and IPv4 addresses.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['-', '.'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn valid_ipv6(addr: &str) -> bool {
    let (addr, zone) = match addr.split_once('%') {
        Some((addr, zone)) => (addr, Some(zone)),
        None => (addr, None),
    };
    addr.contains(':')
        && addr.parse::<std::net::Ipv6Addr>().is_ok()
        && zone
            .is_none_or(|zone| !zone.is_empty() && zone.chars().all(|c| c.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(user: Option<&str>, host: &str, port: Option<u16>) -> SshTarget {
        SshTarget {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn parses_the_usual_forms() {
        assert_eq!(SshTarget::parse("box").unwrap(), t(None, "box", None));
        assert_eq!(
            SshTarget::parse("  root@203.0.113.7 ").unwrap(),
            t(Some("root"), "203.0.113.7", None)
        );
        assert_eq!(
            SshTarget::parse("deploy@vps.example.com:2222").unwrap(),
            t(Some("deploy"), "vps.example.com", Some(2222))
        );
        assert_eq!(
            SshTarget::parse("ssh://me@box.tail1234.ts.net:22/").unwrap(),
            t(Some("me"), "box.tail1234.ts.net", Some(22))
        );
        assert_eq!(
            SshTarget::parse("my_alias").unwrap(),
            t(None, "my_alias", None)
        );
    }

    #[test]
    fn parses_ipv6() {
        assert_eq!(
            SshTarget::parse("root@2001:db8::1").unwrap(),
            t(Some("root"), "2001:db8::1", None)
        );
        assert_eq!(
            SshTarget::parse("root@[2001:db8::1]:2222").unwrap(),
            t(Some("root"), "2001:db8::1", Some(2222))
        );
        assert_eq!(
            SshTarget::parse("[fe80::1%en0]").unwrap(),
            t(None, "fe80::1%en0", None)
        );
        assert_eq!(
            SshTarget::parse("root@[2001:db8::1]:2222")
                .unwrap()
                .destination(),
            "root@2001:db8::1"
        );
    }

    #[test]
    fn display_round_trips() {
        for input in [
            "box",
            "root@203.0.113.7",
            "deploy@vps.example.com:2222",
            "root@[2001:db8::1]:2222",
            "root@2001:db8::1",
        ] {
            let parsed = SshTarget::parse(input).unwrap();
            assert_eq!(parsed.to_string(), input);
            assert_eq!(SshTarget::parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn port_args_only_when_given() {
        assert!(SshTarget::parse("a@b").unwrap().port_args().is_empty());
        assert_eq!(
            SshTarget::parse("a@b:2200").unwrap().port_args(),
            vec!["-p", "2200"]
        );
    }

    #[test]
    fn rejects_option_injection_and_shell_characters() {
        assert_eq!(
            SshTarget::parse("-oProxyCommand=touch /tmp/x"),
            Err(TargetError::InvalidCharacter(' '))
        );
        assert_eq!(
            SshTarget::parse("-oProxyCommand=x"),
            Err(TargetError::LeadingDash)
        );
        assert_eq!(SshTarget::parse("me@-host"), Err(TargetError::LeadingDash));
        assert_eq!(SshTarget::parse("-me@host"), Err(TargetError::LeadingDash));
        assert_eq!(
            SshTarget::parse("me@host;reboot"),
            Err(TargetError::InvalidCharacter(';'))
        );
        assert_eq!(
            SshTarget::parse("me@$(id)"),
            Err(TargetError::InvalidCharacter('$'))
        );
        assert_eq!(
            SshTarget::parse("me@ho st"),
            Err(TargetError::InvalidCharacter(' '))
        );
    }

    #[test]
    fn rejects_malformed_values() {
        assert_eq!(SshTarget::parse("   "), Err(TargetError::Empty));
        assert_eq!(SshTarget::parse("@host"), Err(TargetError::InvalidUser));
        assert_eq!(SshTarget::parse("a@b@c"), Err(TargetError::InvalidHost));
        assert_eq!(SshTarget::parse("me@"), Err(TargetError::InvalidHost));
        assert_eq!(SshTarget::parse("me@host:"), Err(TargetError::InvalidPort));
        assert_eq!(SshTarget::parse("me@host:0"), Err(TargetError::InvalidPort));
        assert_eq!(
            SshTarget::parse("me@host:70000"),
            Err(TargetError::InvalidPort)
        );
        assert_eq!(
            SshTarget::parse("me@host:22x"),
            Err(TargetError::InvalidPort)
        );
        assert_eq!(SshTarget::parse("me@.host"), Err(TargetError::InvalidHost));
        assert_eq!(SshTarget::parse("me@[::1"), Err(TargetError::InvalidHost));
        assert_eq!(
            SshTarget::parse("me@[nothex]:22"),
            Err(TargetError::InvalidHost)
        );
        assert_eq!(SshTarget::parse("me@1:2:zz"), Err(TargetError::InvalidHost));
    }

    #[test]
    fn errors_read_as_guidance() {
        assert_eq!(
            TargetError::Empty.to_string(),
            "Enter a server as user@host"
        );
        assert!(
            TargetError::InvalidCharacter(' ')
                .to_string()
                .contains("spaces")
        );
    }
}
