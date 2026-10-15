//! REQ-17: where the next page of a paginated JSON API is, or that there is none
//! ([docs/HTTP-SOURCE.md](../../../docs/HTTP-SOURCE.md), "Paginated JSON").
//!
//! Pure: no I/O, no clock. The fetch loop reads a page, tells a [`Pager`] what it saw, and is
//! told the next URL to ask for, that the source has ended, or why it must fail. A page that
//! cannot be followed fails the whole source, never ends it early: a table that stops at page 7
//! of 9 is a wrong answer with nothing to say so, which is worse than no table.
//!
//! Every next request is one of the operator's URL with a parameter set, or a URL the API handed
//! back on the operator's own origin. A next URL on another origin is refused: the request
//! carries this source's credentials, and an API, or anyone who can write into its pages, could
//! otherwise send them anywhere.

use std::collections::HashSet;

/// The longest cursor or next URL followed. It goes into a URL, a log line and the loop guard's
/// set; a longer one is a broken or hostile API, not a cursor.
pub const MAX_TOKEN: usize = 8 * 1024;

/// How an API says where its next page is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Style {
    /// One request; the response is the whole source.
    Single,
    /// The page holds a cursor, sent back as the parameter `param`. A missing, null or empty
    /// cursor ends the source.
    Cursor { param: String },
    /// `offset_param` advances by the rows each page held — not by the size asked for, since an
    /// API may cap a page below it — and `limit_param` asks for `page_size`.
    Offset {
        offset_param: String,
        limit_param: String,
        page_size: u64,
    },
    /// `page_param` counts pages from `first`; `size` asks for a page size when the API takes
    /// one.
    Page {
        page_param: String,
        first: u64,
        size: Option<(String, u64)>,
    },
    /// RFC 8288: the response's `Link` header names the next page with `rel="next"`.
    Link,
    /// The page holds the next page's URL.
    NextUrl,
}

/// What one page told the fetch loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct Seen<'a> {
    /// The rows the page held.
    pub rows: u64,
    /// For [`Style::Cursor`] and [`Style::NextUrl`]: the value at the operator's path, as text,
    /// or `None` when it is missing or null.
    pub found: Option<&'a str>,
    /// For [`Style::Link`]: the response's `Link` header values, joined with `, `.
    pub link: Option<&'a str>,
    /// Whether the body was byte for byte the previous page's. The fetch loop compares the two
    /// buffers it reads pages into in turn, which costs a `memcmp` and only when the lengths
    /// agree; an API that ignores its offset or page parameter answers every request alike, and
    /// nothing else would stop that before `max_pages`.
    pub repeats_previous: bool,
}

/// What to do after a page.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Fetch(String),
    End,
}

/// Why paging cannot go on. Each fails the source; none ends it.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// A cursor or URL already followed, or a page the same as the one before it.
    Loop(String),
    /// More pages than `max_pages`.
    TooManyPages(u64),
    /// A next URL on another scheme, host or port than the source's.
    OtherOrigin(String),
    /// A next URL that is not one, or a cursor or URL past [`MAX_TOKEN`].
    Unusable(String),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stop::Loop(what) => write!(
                f,
                "the API sent {what} again, so paging would never end. Refused, not truncated: \
                 nothing of it was loaded."
            ),
            Stop::TooManyPages(n) => write!(
                f,
                "the API has more than {n} page(s), this source's `max_pages`. Refused, not \
                 truncated: narrow it, or raise `max_pages`."
            ),
            Stop::OtherOrigin(url) => write!(
                f,
                "the API's next page is on another origin ({url}), and the request would carry \
                 this source's credentials there. Refused."
            ),
            Stop::Unusable(why) => write!(f, "the API's next page cannot be followed: {why}"),
        }
    }
}

/// Follows one source's pages.
#[derive(Debug)]
pub struct Pager {
    style: Style,
    /// The operator's URL, its own parameters already on it.
    base: String,
    origin: Origin,
    /// The URL of the page just read, which a relative next URL resolves against.
    current: String,
    /// Every cursor or URL followed so far: the loop guard. Bounded by `max_pages`.
    followed: HashSet<String>,
    pages: u64,
    max_pages: u64,
    offset: u64,
    page: u64,
}

impl Pager {
    /// Paging from `url` under `style`, at most `max_pages` pages.
    pub fn new(style: Style, url: &str, max_pages: u64) -> Result<Pager, String> {
        let origin = Origin::of(url).ok_or_else(|| format!("`{url}` is not an http(s) URL"))?;
        if max_pages == 0 {
            return Err("`max_pages = 0` would fetch nothing".into());
        }
        let page = match &style {
            Style::Page { first, .. } => *first,
            _ => 0,
        };
        Ok(Pager {
            style,
            base: url.to_string(),
            origin,
            current: url.to_string(),
            followed: HashSet::new(),
            pages: 0,
            max_pages,
            offset: 0,
            page,
        })
    }

    /// The first request.
    pub fn first(&mut self) -> String {
        self.pages = 1;
        let url = match &self.style {
            Style::Offset {
                offset_param,
                limit_param,
                page_size,
            } => {
                let u = set_param(&self.base, offset_param, "0");
                set_param(&u, limit_param, &page_size.to_string())
            }
            Style::Page { .. } => self.page_url(),
            _ => self.base.clone(),
        };
        self.followed.insert(url.clone());
        self.current = url.clone();
        url
    }

    fn page_url(&self) -> String {
        let Style::Page {
            page_param, size, ..
        } = &self.style
        else {
            unreachable!()
        };
        let u = set_param(&self.base, page_param, &self.page.to_string());
        match size {
            Some((p, n)) => set_param(&u, p, &n.to_string()),
            None => u,
        }
    }

    /// After a page: the next request, or the end.
    pub fn next(&mut self, seen: &Seen) -> Result<Next, Stop> {
        if seen.repeats_previous && seen.rows > 0 {
            return Err(Stop::Loop("the same page".into()));
        }
        let url = match &self.style {
            Style::Single => return Ok(Next::End),
            Style::Cursor { param } => {
                let Some(cursor) = seen.found.filter(|c| !c.is_empty()) else {
                    return Ok(Next::End);
                };
                if cursor.len() > MAX_TOKEN {
                    return Err(Stop::Unusable(format!(
                        "a cursor of {} bytes, past {MAX_TOKEN}",
                        cursor.len()
                    )));
                }
                // The cursor itself is the guard's key: the same cursor is the same page,
                // whatever else is on the URL.
                if !self.followed.insert(format!("cursor:{cursor}")) {
                    return Err(Stop::Loop("a cursor it had sent before".into()));
                }
                set_param(&self.base, param, cursor)
            }
            Style::Offset {
                offset_param,
                limit_param,
                page_size,
            } => {
                // Only an empty page ends: a short one may be the API's own cap on page size,
                // and stopping there would drop every row after it.
                if seen.rows == 0 {
                    return Ok(Next::End);
                }
                self.offset += seen.rows;
                let u = set_param(&self.base, offset_param, &self.offset.to_string());
                set_param(&u, limit_param, &page_size.to_string())
            }
            Style::Page { .. } => {
                if seen.rows == 0 {
                    return Ok(Next::End);
                }
                self.page += 1;
                self.page_url()
            }
            Style::Link => match seen.link.and_then(link_next) {
                None => return Ok(Next::End),
                Some(next) => self.follow(&next)?,
            },
            Style::NextUrl => match seen.found.filter(|u| !u.is_empty()) {
                None => return Ok(Next::End),
                Some(next) => self.follow(next)?,
            },
        };
        if self.pages >= self.max_pages {
            return Err(Stop::TooManyPages(self.max_pages));
        }
        self.pages += 1;
        self.current = url.clone();
        Ok(Next::Fetch(url))
    }

    /// A next URL the API handed back: resolved against the page it came from, held to the
    /// source's origin, and never followed twice.
    fn follow(&mut self, next: &str) -> Result<String, Stop> {
        if next.len() > MAX_TOKEN {
            return Err(Stop::Unusable(format!(
                "a next URL of {} bytes, past {MAX_TOKEN}",
                next.len()
            )));
        }
        if next.chars().any(|c| c.is_control() || c == ' ') {
            return Err(Stop::Unusable(
                "a next URL with a space or control character".into(),
            ));
        }
        let url = resolve(&self.current, next)
            .ok_or_else(|| Stop::Unusable(format!("`{next}` is not a URL")))?;
        match Origin::of(&url) {
            Some(o) if o == self.origin => {}
            _ => return Err(Stop::OtherOrigin(origin_only(&url))),
        }
        if !self.followed.insert(url.clone()) {
            return Err(Stop::Loop("a next URL it had sent before".into()));
        }
        Ok(url)
    }
}

/// Scheme, host and port: what a request's credentials are bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    fn of(url: &str) -> Option<Origin> {
        let (scheme, rest) = url.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let default = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            // An IPv6 literal without a port has colons inside its brackets.
            Some((h, p)) if !p.contains(']') => (h, p.parse().ok()?),
            _ => (authority, default),
        };
        Some(Origin {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }
}

/// Whether two URLs share scheme, host and port: what a request's credentials are bound to. A
/// redirect is held to the same rule as a next page (`a_redirect_off_the_origin_carries_no_credential`).
pub fn same_origin(a: &str, b: &str) -> bool {
    matches!((Origin::of(a), Origin::of(b)), (Some(x), Some(y)) if x == y)
}

/// A URL's scheme and host, lowercased, for `redirect_hosts`.
pub fn host_of(url: &str) -> Option<(String, String)> {
    Origin::of(url).map(|o| (o.scheme, o.host))
}

/// What a refusal may say of a URL it would not follow: where it pointed, not its path or query,
/// which may carry a token.
pub fn origin_only(url: &str) -> String {
    match url.split_once("://") {
        Some((s, rest)) => {
            let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
            // Whatever precedes an `@` is a user and password.
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{s}://{host}")
        }
        None => "a URL that is not one".into(),
    }
}

/// `url` with its query parameter `key` set to `value`: every earlier `key` removed, the new one
/// appended, both percent-encoded. A fragment is dropped; it is never sent.
pub fn set_param(url: &str, key: &str, value: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url, ""),
    };
    let key_enc = encode(key);
    let mut kept: Vec<&str> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter(|kv| kv.split('=').next() != Some(key_enc.as_str()))
        .collect();
    let pair = format!("{key_enc}={}", encode(value));
    kept.push(&pair);
    format!("{path}?{}", kept.join("&"))
}

/// RFC 3986 percent-encoding of everything but the unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The `rel="next"` target of a `Link` header value (RFC 8288), several links comma-separated.
pub fn link_next(header: &str) -> Option<String> {
    let mut rest = header;
    loop {
        let open = rest.find('<')?;
        let close = open + rest[open..].find('>')?;
        let target = &rest[open + 1..close];
        let after = &rest[close + 1..];
        // This link's parameters run to the next link, which starts with `<`; a comma inside a
        // quoted parameter is not a separator, and `<` cannot appear outside the target.
        let end = after.find('<').unwrap_or(after.len());
        let params = &after[..end];
        let is_next = params.split(';').any(|p| {
            let Some((k, v)) = p.split_once('=') else {
                return false;
            };
            k.trim().eq_ignore_ascii_case("rel")
                && v.trim()
                    .trim_end_matches(',')
                    .trim()
                    .trim_matches('"')
                    .split_ascii_whitespace()
                    .any(|r| r.eq_ignore_ascii_case("next"))
        });
        if is_next {
            return Some(target.trim().to_string());
        }
        rest = &after[end..];
    }
}

/// `next` resolved against `base` (RFC 3986 §5.2, for the forms APIs send: absolute,
/// scheme-relative, absolute-path, query-only and path-relative).
pub fn resolve(base: &str, next: &str) -> Option<String> {
    let lower = next.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(next.to_string());
    }
    if next.contains("://") {
        return None;
    }
    let (scheme, rest) = base.split_once("://")?;
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let path_query = rest[auth_end..].split('#').next().unwrap_or("");
    let path = path_query.split('?').next().unwrap_or("");
    let root = format!("{scheme}://{authority}");
    Some(if let Some(r) = next.strip_prefix("//") {
        format!("{scheme}://{r}")
    } else if next.starts_with('/') {
        format!("{root}{next}")
    } else if next.starts_with('?') {
        format!("{root}{path}{next}")
    } else {
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        let dir = if dir.is_empty() { "/" } else { dir };
        format!("{root}{dir}{next}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = "https://api.example.com/v1/readings?site=north";

    fn seen(rows: u64) -> Seen<'static> {
        Seen {
            rows,
            ..Seen::default()
        }
    }

    fn found(rows: u64, v: &'static str) -> Seen<'static> {
        Seen {
            rows,
            found: Some(v),
            ..Seen::default()
        }
    }

    #[test]
    fn a_cursor_is_followed_until_the_api_stops_sending_one() {
        let mut p = Pager::new(
            Style::Cursor {
                param: "cursor".into(),
            },
            API,
            100,
        )
        .unwrap();
        assert_eq!(p.first(), API);
        assert_eq!(
            p.next(&found(10, "c/2=")).unwrap(),
            Next::Fetch(format!("{API}&cursor=c%2F2%3D"))
        );
        // A page may be empty and still say there is more.
        assert_eq!(
            p.next(&found(0, "c3")).unwrap(),
            Next::Fetch(format!("{API}&cursor=c3"))
        );
        // Missing, null and empty all end it.
        assert_eq!(p.next(&seen(4)).unwrap(), Next::End);
        let mut p = Pager::new(Style::Cursor { param: "c".into() }, API, 100).unwrap();
        p.first();
        assert_eq!(p.next(&found(1, "")).unwrap(), Next::End);
    }

    #[test]
    fn a_repeated_cursor_fails_the_source() {
        let mut p = Pager::new(Style::Cursor { param: "c".into() }, API, 100).unwrap();
        p.first();
        p.next(&found(5, "a")).unwrap();
        p.next(&found(5, "b")).unwrap();
        // Not only the last one: a cycle of two is a loop too.
        let e = p.next(&found(5, "a")).unwrap_err();
        assert!(matches!(e, Stop::Loop(_)), "{e:?}");
        assert!(e.to_string().contains("not truncated"), "{e}");
    }

    #[test]
    fn offset_paging_advances_by_the_rows_received_and_ends_only_at_an_empty_page() {
        let style = Style::Offset {
            offset_param: "offset".into(),
            limit_param: "limit".into(),
            page_size: 1000,
        };
        let mut p = Pager::new(style, API, 100).unwrap();
        assert_eq!(p.first(), format!("{API}&offset=0&limit=1000"));
        // The API capped the page at 100: the next one starts at 100, and a short page does not
        // end the source.
        assert_eq!(
            p.next(&seen(100)).unwrap(),
            Next::Fetch(format!("{API}&offset=100&limit=1000"))
        );
        assert_eq!(
            p.next(&seen(37)).unwrap(),
            Next::Fetch(format!("{API}&offset=137&limit=1000"))
        );
        assert_eq!(p.next(&seen(0)).unwrap(), Next::End);
    }

    #[test]
    fn page_numbers_count_from_the_first_page_and_end_at_an_empty_page() {
        let style = Style::Page {
            page_param: "page".into(),
            first: 1,
            size: Some(("per_page".into(), 50)),
        };
        let mut p = Pager::new(style, API, 100).unwrap();
        assert_eq!(p.first(), format!("{API}&page=1&per_page=50"));
        assert_eq!(
            p.next(&seen(50)).unwrap(),
            Next::Fetch(format!("{API}&page=2&per_page=50"))
        );
        assert_eq!(p.next(&seen(0)).unwrap(), Next::End);
    }

    #[test]
    fn an_api_that_ignores_its_page_parameter_fails_the_source() {
        let style = Style::Page {
            page_param: "page".into(),
            first: 0,
            size: None,
        };
        let mut p = Pager::new(style, API, 100).unwrap();
        p.first();
        let mut again = seen(50);
        again.repeats_previous = true;
        assert!(matches!(p.next(&again), Err(Stop::Loop(_))));
    }

    #[test]
    fn more_pages_than_max_pages_fails_rather_than_truncates() {
        let mut p = Pager::new(Style::Cursor { param: "c".into() }, API, 2).unwrap();
        p.first();
        assert!(matches!(p.next(&found(1, "a")), Ok(Next::Fetch(_))));
        assert_eq!(p.next(&found(1, "b")), Err(Stop::TooManyPages(2)));
        // The last page allowed may still end the source.
        let mut p = Pager::new(Style::Cursor { param: "c".into() }, API, 1).unwrap();
        p.first();
        assert_eq!(p.next(&seen(1)).unwrap(), Next::End);
        assert!(Pager::new(Style::Single, API, 0).is_err());
    }

    #[test]
    fn link_header_next_is_followed_on_the_same_origin() {
        let mut p = Pager::new(Style::Link, API, 100).unwrap();
        p.first();
        let link = "<https://api.example.com/v1/readings?page=1>; rel=\"first\", \
                    </v1/readings?page=2&site=north>; rel=\"next\"";
        let s = Seen {
            rows: 3,
            link: Some(link),
            ..Seen::default()
        };
        assert_eq!(
            p.next(&s).unwrap(),
            Next::Fetch("https://api.example.com/v1/readings?page=2&site=north".into())
        );
        let s = Seen {
            rows: 3,
            link: Some("<https://api.example.com/v1/readings?page=1>; rel=\"prev\""),
            ..Seen::default()
        };
        assert_eq!(p.next(&s).unwrap(), Next::End);
    }

    #[test]
    fn a_next_url_on_another_origin_is_refused() {
        for next in [
            "https://elsewhere.example.net/v1/readings?page=2",
            "http://api.example.com/v1/readings?page=2",
            "https://api.example.com:8443/v1/readings?page=2",
            "//elsewhere.example.net/steal",
            "https://api.example.com@elsewhere.example.net/x",
        ] {
            let mut p = Pager::new(Style::NextUrl, API, 100).unwrap();
            p.first();
            let e = p.next(&found(1, next)).unwrap_err();
            assert!(matches!(e, Stop::OtherOrigin(_)), "{next}: {e:?}");
            // The refusal names where it pointed, not the path or query, which may hold a token.
            assert!(
                !e.to_string().contains("/v1/") && !e.to_string().contains("steal"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_next_url_is_resolved_against_the_page_it_came_from_and_never_followed_twice() {
        let mut p = Pager::new(Style::NextUrl, API, 100).unwrap();
        p.first();
        assert_eq!(
            p.next(&found(1, "?site=north&after=9")).unwrap(),
            Next::Fetch("https://api.example.com/v1/readings?site=north&after=9".into())
        );
        assert_eq!(
            p.next(&found(1, "readings?after=12")).unwrap(),
            Next::Fetch("https://api.example.com/v1/readings?after=12".into())
        );
        assert!(matches!(
            p.next(&found(1, "/v1/readings?site=north&after=9")),
            Err(Stop::Loop(_))
        ));
        // The first URL counts as followed.
        let mut p = Pager::new(Style::NextUrl, API, 100).unwrap();
        p.first();
        assert!(matches!(p.next(&found(1, API)), Err(Stop::Loop(_))));
    }

    #[test]
    fn an_oversized_or_malformed_token_is_refused() {
        let long: &'static str = Box::leak("x".repeat(MAX_TOKEN + 1).into_boxed_str());
        let mut p = Pager::new(Style::Cursor { param: "c".into() }, API, 100).unwrap();
        p.first();
        assert!(matches!(p.next(&found(1, long)), Err(Stop::Unusable(_))));
        let mut p = Pager::new(Style::NextUrl, API, 100).unwrap();
        p.first();
        assert!(matches!(
            p.next(&found(1, "/a\r\nX-Injected: 1")),
            Err(Stop::Unusable(_))
        ));
        assert!(matches!(
            p.next(&found(1, "ftp://api.example.com/x")),
            Err(Stop::Unusable(_))
        ));
    }

    #[test]
    fn set_param_replaces_and_encodes() {
        assert_eq!(
            set_param("http://h/p", "a b", "x&y"),
            "http://h/p?a%20b=x%26y"
        );
        assert_eq!(
            set_param("http://h/p?a=1&b=2&a=3", "a", "9"),
            "http://h/p?b=2&a=9"
        );
        assert_eq!(
            set_param("http://h/p?x=1#frag", "y", "2"),
            "http://h/p?x=1&y=2"
        );
    }

    #[test]
    fn a_single_response_has_no_next_page() {
        let mut p = Pager::new(Style::Single, API, 1).unwrap();
        assert_eq!(p.first(), API);
        assert_eq!(p.next(&found(10, "ignored")).unwrap(), Next::End);
    }
}
