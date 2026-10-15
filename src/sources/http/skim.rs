//! R4-3: what the pager needs of a JSON page — how many rows it holds and its cursor or next
//! URL — found without decoding its rows, so the next page can be asked for while this one is
//! decoded on another thread.
//!
//! A scan that skips every value but the two it is after: strings to their closing quote,
//! objects and arrays by their brackets, numbers and literals to the next delimiter. It checks
//! nothing it does not read; arrow-json's decode of the same page does that, and its rows and
//! cursor must be what the scan found, or the source fails (`load.rs`). Where the page is not
//! plainly what the scan expects — a duplicate key on the way to the rows or the cursor, an
//! escaped key, a row that is not an object, a cursor that is not a string, a number, a boolean
//! or null, bytes past the value — it says nothing, and the page is decoded before the next one
//! is asked for, as every page was until R4-3.
//!
//! What arrow-json 58.4 gives the decode, which the scan must give too: a number or a boolean
//! read into the cursor's text column is its text as written (`with_coerce_primitive`); a string
//! is unescaped; null and a missing key are no cursor.

/// What the scan found: the rows at the rows path, and the cursor or next URL at its path, if
/// there is one.
#[derive(Debug, PartialEq, Eq)]
pub struct Skim {
    pub rows: u64,
    pub found: Option<String>,
}

/// Scan `page` for the number of rows at `rows` (a list of objects; the page itself when the
/// path is empty) and the value at `at`, as text. `None` where the page is not plainly that.
pub fn skim(page: &[u8], rows: &[String], at: Option<&[String]>) -> Option<Skim> {
    let mut s = Scan { b: page, i: 0 };
    let out = if rows.is_empty() {
        // The page is the list; the decode reads no cursor from it.
        let n = s.rows()?;
        Skim {
            rows: n,
            found: None,
        }
    } else {
        let mut want = Want::Branch(Vec::new());
        want.put(rows, Want::Rows)?;
        if let Some(at) = at {
            want.put(at, Want::Cursor)?;
        }
        let mut got = Got::default();
        let Want::Branch(keys) = &want else {
            unreachable!()
        };
        s.object(keys, &mut got)?;
        Skim {
            rows: got.rows?,
            found: got.found,
        }
    };
    s.ws();
    (s.i == page.len()).then_some(out)
}

/// What the scan is after, by key.
enum Want {
    Rows,
    Cursor,
    Branch(Vec<(String, Want)>),
}

impl Want {
    fn put(&mut self, path: &[String], leaf: Want) -> Option<()> {
        let Want::Branch(keys) = self else {
            return None;
        };
        let (head, rest) = path.split_first()?;
        match keys.iter_mut().find(|(k, _)| k == head) {
            None if rest.is_empty() => keys.push((head.clone(), leaf)),
            None => {
                let mut b = Want::Branch(Vec::new());
                b.put(rest, leaf)?;
                keys.push((head.clone(), b));
            }
            Some((_, b @ Want::Branch(_))) if !rest.is_empty() => b.put(rest, leaf)?,
            // The paths overlap, which the decoder refuses first.
            Some(_) => return None,
        }
        Some(())
    }
}

#[derive(Default)]
struct Got {
    rows: Option<u64>,
    found: Option<String>,
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }

    /// An object, its keys in `keys` read into `got`, every other value skipped. A key wanted
    /// twice, or written with an escape, is not plain: arrow-json would take the last one, and
    /// match a key by its unescaped text.
    fn object(&mut self, keys: &[(String, Want)], got: &mut Got) -> Option<()> {
        self.eat(b'{')?;
        let mut seen = vec![false; keys.len()];
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(());
        }
        loop {
            let (from, to, escaped) = self.string()?;
            let key = &self.b[from..to];
            // An escaped key is matched by its text, as arrow-json matches it: one that names a
            // wanted key is not plain.
            let wanted = if escaped {
                let key = unescape(key)?;
                keys.iter().position(|(k, _)| *k == key).map(|_| usize::MAX)
            } else {
                keys.iter().position(|(k, _)| k.as_bytes() == key)
            };
            self.eat(b':')?;
            match wanted {
                Some(usize::MAX) => return None,
                Some(k) if seen[k] => return None,
                Some(k) => {
                    seen[k] = true;
                    match &keys[k].1 {
                        Want::Rows => got.rows = Some(self.rows()?),
                        Want::Cursor => got.found = self.cursor()?,
                        Want::Branch(inner) => match self.peek()? {
                            b'{' => self.object(inner, got)?,
                            // A null parent: nothing under it, as the decode reads it.
                            b'n' => self.literal(b"null")?,
                            _ => return None,
                        },
                    }
                }
                None => self.skip()?,
            }
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// A list of objects: how many.
    fn rows(&mut self) -> Option<u64> {
        self.eat(b'[')?;
        let mut n = 0;
        if self.peek()? == b']' {
            self.i += 1;
            return Some(0);
        }
        loop {
            if self.peek()? != b'{' {
                return None;
            }
            self.skip()?;
            n += 1;
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(n);
                }
                _ => return None,
            }
        }
    }

    /// The cursor's value as the decode reads it into text: `Some(None)` for null.
    fn cursor(&mut self) -> Option<Option<String>> {
        match self.peek()? {
            b'"' => {
                let (from, to, escaped) = self.string()?;
                let raw = &self.b[from..to];
                let text = if escaped {
                    unescape(raw)?
                } else {
                    std::str::from_utf8(raw).ok()?.to_string()
                };
                Some(Some(text))
            }
            b'n' => {
                self.literal(b"null")?;
                Some(None)
            }
            b't' => {
                self.literal(b"true")?;
                Some(Some("true".into()))
            }
            b'f' => {
                self.literal(b"false")?;
                Some(Some("false".into()))
            }
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                while let Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') = self.b.get(self.i)
                {
                    self.i += 1;
                }
                let text = std::str::from_utf8(&self.b[start..self.i]).ok()?;
                // A number as JSON writes one; anything else the decode refuses.
                text.parse::<f64>().ok()?;
                Some(Some(text.to_string()))
            }
            _ => None,
        }
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        self.ws();
        let end = self.i + word.len();
        (self.b.get(self.i..end)? == word).then(|| self.i = end)
    }

    /// A string: where its raw bytes between the quotes are, and whether it holds an escape.
    fn string(&mut self) -> Option<(usize, usize, bool)> {
        self.eat(b'"')?;
        let start = self.i;
        let (end, escaped) = string_end(self.b, start)?;
        self.i = end + 1;
        Some((start, end, escaped))
    }

    /// Any one value, unread: a string to its closing quote, an object or a list to its closing
    /// bracket, anything else to the next delimiter.
    fn skip(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => self.string().map(|_| ()),
            b'{' | b'[' => {
                // Most of a page's bytes go through here, a row at a time, so a table lookup a
                // byte: the harness's 1,000,000 rows of pages (63 MB) in 601 M instructions and
                // 248 M cycles, against 742 M and 281 M with a `match` a byte (`perf stat`).
                let (b, mut i, mut depth) = (self.b, self.i, 0usize);
                loop {
                    while CLASS[*b.get(i)? as usize] == 0 {
                        i += 1;
                    }
                    match CLASS[b[i] as usize] {
                        QUOTE => i = string_end(b, i + 1)?.0,
                        OPEN => depth += 1,
                        _ => {
                            depth -= 1;
                            if depth == 0 {
                                self.i = i + 1;
                                return Some(());
                            }
                        }
                    }
                    i += 1;
                }
            }
            _ => {
                let start = self.i;
                while let Some(&c) = self.b.get(self.i) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.i += 1;
                }
                (self.i > start).then_some(())
            }
        }
    }
}

const QUOTE: u8 = 1;
const OPEN: u8 = 2;
const CLOSE: u8 = 3;
const ESCAPE: u8 = 4;

/// What [`Scan::skip`] stops at in a container: a quote, an opening or a closing bracket.
static CLASS: [u8; 256] = {
    let mut t = [0; 256];
    t[b'"' as usize] = QUOTE;
    t[b'{' as usize] = OPEN;
    t[b'[' as usize] = OPEN;
    t[b'}' as usize] = CLOSE;
    t[b']' as usize] = CLOSE;
    t
};

/// What a string's scan stops at: its closing quote, or an escape.
static IN_STRING: [u8; 256] = {
    let mut t = [0; 256];
    t[b'"' as usize] = QUOTE;
    t[b'\\' as usize] = ESCAPE;
    t
};

/// Where the string whose text starts at `i` closes, and whether it holds an escape.
fn string_end(b: &[u8], mut i: usize) -> Option<(usize, bool)> {
    let mut escaped = false;
    loop {
        while IN_STRING[*b.get(i)? as usize] == 0 {
            i += 1;
        }
        if b[i] == b'"' {
            return Some((i, escaped));
        }
        escaped = true;
        i += 2;
    }
}

/// A JSON string's text between its quotes, unescaped. `None` for an escape JSON does not have,
/// a lone surrogate, or bytes that are not UTF-8.
fn unescape(raw: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    let hex = |i: usize| -> Option<u32> {
        let h = std::str::from_utf8(raw.get(i..i + 4)?).ok()?;
        u32::from_str_radix(h, 16).ok()
    };
    while i < raw.len() {
        if raw[i] != b'\\' {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        let c = match *raw.get(i + 1)? {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let hi = hex(i + 2)?;
                if (0xD800..0xDC00).contains(&hi) {
                    if raw.get(i + 6..i + 8)? != b"\\u" {
                        return None;
                    }
                    let lo = hex(i + 8)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return None;
                    }
                    i += 6;
                    char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))?
                } else {
                    char::from_u32(hi)?
                }
            }
            _ => return None,
        };
        let skip = if raw[i + 1] == b'u' { 6 } else { 2 };
        let mut b = [0; 4];
        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        i += skip;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> Vec<String> {
        if s.is_empty() {
            Vec::new()
        } else {
            s.split('.').map(String::from).collect()
        }
    }

    fn sk(page: &str, rows: &str, at: Option<&str>) -> Option<Skim> {
        let at = at.map(path);
        skim(page.as_bytes(), &path(rows), at.as_deref())
    }

    fn found(rows: u64, c: Option<&str>) -> Option<Skim> {
        Some(Skim {
            rows,
            found: c.map(String::from),
        })
    }

    #[test]
    fn a_pages_rows_and_cursor_are_found_without_decoding_them() {
        let page = r#" {"data": [{"a": "x]}\"", "b": [1, {"c": 2}]}, {"a": null}],
                        "meta": {"total": 2, "next_cursor": "c2", "x": [1,2]}} "#;
        assert_eq!(
            sk(page, "data", Some("meta.next_cursor")),
            found(2, Some("c2"))
        );
        // The same, the cursor before the rows; and no cursor path at all.
        let page = r#"{"meta":{"next_cursor":"c2"},"data":[{"a":1}]}"#;
        assert_eq!(
            sk(page, "data", Some("meta.next_cursor")),
            found(1, Some("c2"))
        );
        assert_eq!(sk(page, "data", None), found(1, None));
        // The page as the list.
        assert_eq!(sk(r#"[{"n":1},{"n":2},{"n":3}]"#, "", None), found(3, None));
        assert_eq!(sk("[]", "", None), found(0, None));
        assert_eq!(sk(r#"{"items": []}"#, "items", None), found(0, None));
    }

    #[test]
    fn a_cursor_reads_as_the_decode_reads_it() {
        for (value, want) in [
            (r#""abc""#, Some("abc")),
            (
                r#""a\/b\"c\\d\u00e9\ud83d\ude00\n""#,
                Some("a/b\"c\\d\u{e9}\u{1f600}\n"),
            ),
            ("17", Some("17")),
            ("-1.5e3", Some("-1.5e3")),
            ("true", Some("true")),
            ("false", Some("false")),
            ("null", None),
            (r#""""#, Some("")),
        ] {
            let page = format!(r#"{{"rows": [{{"n": 1}}], "c": {value}}}"#);
            assert_eq!(sk(&page, "rows", Some("c")), found(1, want), "{value}");
        }
        // Missing, or under a null parent: no cursor.
        assert_eq!(sk(r#"{"rows": []}"#, "rows", Some("m.c")), found(0, None));
        assert_eq!(
            sk(r#"{"rows": [], "m": null}"#, "rows", Some("m.c")),
            found(0, None)
        );
    }

    #[test]
    fn a_page_that_is_not_plain_is_left_to_the_decode() {
        for (page, rows, at) in [
            // The rows missing, null, or not a list of objects.
            (r#"{"meta": {}}"#, "data", None),
            (r#"{"data": null}"#, "data", None),
            (r#"{"data": [{"n": 1}, null]}"#, "data", None),
            (r#"{"data": [1, 2]}"#, "data", None),
            (r#"{"data": {"n": 1}}"#, "data", None),
            // The rows or the cursor twice: arrow-json takes the last.
            (r#"{"data": [], "data": [{"n": 1}]}"#, "data", None),
            (r#"{"data": [], "c": "1", "c": "2"}"#, "data", Some("c")),
            // A cursor that is not text, a number, a boolean or null.
            (r#"{"data": [], "c": {"x": 1}}"#, "data", Some("c")),
            (r#"{"data": [], "c": [1]}"#, "data", Some("c")),
            (r#"{"data": [], "c": nope}"#, "data", Some("c")),
            (r#"{"data": [], "c": "\ud800"}"#, "data", Some("c")),
            // A wanted key written with an escape.
            (r#"{"d\u0061ta": [{"n": 1}]}"#, "data", None),
            // Not one JSON value, or cut short.
            (r#"{"data": []} {"data": []}"#, "data", None),
            (r#"{"data": [{"n": 1}"#, "data", None),
            ("", "data", None),
            (r#"[{"n": 1}"#, "", None),
            // A parent that is not an object.
            (r#"{"data": [], "m": 3}"#, "data", Some("m.c")),
        ] {
            assert_eq!(sk(page, rows, at), None, "{page}");
        }
    }
}
