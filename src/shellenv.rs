//! The environment that points pip and uv at a gcs-pypi server, as a sourceable shell snippet.

use std::net::SocketAddr;

pub fn index_url(addr: SocketAddr) -> String {
    format!("http://{addr}/simple/")
}

/// POSIX shell lines for `eval "$(...)"`. Values are quoted: an IPv6 `[::1]` would otherwise glob.
pub fn snippet(url: &str, pid: Option<u32>) -> String {
    let url = quote(url);
    let mut s = format!(
        "export PIP_INDEX_URL={url}\n\
         export UV_DEFAULT_INDEX={url}\n\
         unset UV_INDEX_URL  # deprecated alias; would conflict with UV_DEFAULT_INDEX\n"
    );
    if let Some(pid) = pid {
        s.push_str(&format!("export GCS_PYPI_PID={pid}\n"));
    }
    s
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("127.0.0.1:8765", "http://127.0.0.1:8765/simple/")]
    #[case("[::1]:8765", "http://[::1]:8765/simple/")]
    fn index_urls(#[case] addr: SocketAddr, #[case] expected: &str) {
        assert_eq!(index_url(addr), expected);
    }

    #[test]
    fn snippet_without_pid() {
        assert_eq!(
            snippet("http://[::1]:1/simple/", None),
            "export PIP_INDEX_URL='http://[::1]:1/simple/'\n\
             export UV_DEFAULT_INDEX='http://[::1]:1/simple/'\n\
             unset UV_INDEX_URL  # deprecated alias; would conflict with UV_DEFAULT_INDEX\n"
        );
    }

    #[test]
    fn snippet_with_pid() {
        assert!(snippet("u", Some(42)).ends_with("\nexport GCS_PYPI_PID=42\n"));
    }

    #[rstest]
    #[case("plain", "'plain'")]
    #[case("it's", r"'it'\''s'")]
    fn quoting(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(quote(input), expected);
    }
}
