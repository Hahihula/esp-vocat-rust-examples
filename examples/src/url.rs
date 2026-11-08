//! Parsing `http://` URLs for the radio stream.

/// The parts of a plain-HTTP URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Url<'a> {
    /// Host name or address.
    pub host: &'a str,
    /// TCP port, 80 unless given.
    pub port: u16,
    /// Path with query, `/` if empty.
    pub path: &'a str,
}

/// Splits `http://host[:port][/path]`. `None` for other schemes, an empty
/// host or a bad port.
#[must_use]
pub fn parse(url: &str) -> Option<Url<'_>> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, 80),
    };
    (!host.is_empty()).then_some(Url { host, port, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_url() {
        assert_eq!(
            parse("http://icecast.example.org:8000/stream.mp3?x=1"),
            Some(Url {
                host: "icecast.example.org",
                port: 8000,
                path: "/stream.mp3?x=1"
            })
        );
    }

    #[test]
    fn defaults() {
        assert_eq!(
            parse("http://radio.cz"),
            Some(Url {
                host: "radio.cz",
                port: 80,
                path: "/"
            })
        );
        assert_eq!(
            parse("http://radio.cz/a"),
            Some(Url {
                host: "radio.cz",
                port: 80,
                path: "/a"
            })
        );
    }

    #[test]
    fn rejects() {
        assert_eq!(parse("https://radio.cz/"), None);
        assert_eq!(parse("http:///path"), None);
        assert_eq!(parse("http://radio.cz:99999/"), None);
        assert_eq!(parse("http://radio.cz:port/"), None);
    }
}
