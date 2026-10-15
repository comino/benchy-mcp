//! A scripted HTTP/1.1 server for the HTTP source's tests (docs/HTTP-SOURCE.md, "Tests"): what
//! a real API sends, and what a broken or hostile one could — a status, a `Retry-After`, a
//! `Link` header, a chunked or compressed body, a body cut short, a reply that never comes.
//!
//! Each request is answered by a closure of the test's and recorded, so a test can say what
//! benchy asked for: the parameters it pushed down, the credential it sent, and that it sent
//! nothing past a refusal. One connection per request, closed after the reply.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A request as the server read it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The path and query, as sent.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    /// Query parameter `key`, percent-decoded; the last one when it is repeated.
    pub fn param(&self, key: &str) -> Option<String> {
        let query = self.target.split_once('?')?.1;
        query
            .split('&')
            .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
            .filter(|(k, _)| decode(k) == key)
            .map(|(_, v)| decode(v))
            .next_back()
    }
}

/// How to answer one request.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Sent with `Transfer-Encoding: chunked`, in chunks of this many bytes.
    pub chunked: Option<usize>,
    /// Wait this long before answering.
    pub delay: Duration,
    /// Close the connection after this many bytes of the body, having promised them all.
    pub cut_after: Option<usize>,
    /// Wait this long after this many bytes of the body, then send the rest.
    pub stall: Option<(usize, Duration)>,
}

impl Reply {
    pub fn ok(body: impl Into<Vec<u8>>) -> Reply {
        Reply::status(200).body(body)
    }

    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            chunked: None,
            delay: Duration::ZERO,
            cut_after: None,
            stall: None,
        }
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Reply {
        self.body = body.into();
        self
    }

    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn chunked(mut self, size: usize) -> Reply {
        self.chunked = Some(size.max(1));
        self
    }

    pub fn delay(mut self, d: Duration) -> Reply {
        self.delay = d;
        self
    }

    pub fn cut_after(mut self, n: usize) -> Reply {
        self.cut_after = Some(n);
        self
    }

    pub fn stall_after(mut self, n: usize, d: Duration) -> Reply {
        self.stall = Some((n, d));
        self
    }

    /// `body` written, waiting once `stall` says to.
    fn send(&self, w: &mut impl Write, body: &[u8], sent: &mut usize) -> std::io::Result<()> {
        if let Some((at, d)) = self.stall {
            if *sent <= at && at < *sent + body.len() {
                let (head, rest) = body.split_at(at - *sent);
                w.write_all(head)?;
                w.flush()?;
                std::thread::sleep(d);
                *sent += head.len();
                return self.send_rest(w, rest, sent);
            }
        }
        self.send_rest(w, body, sent)
    }

    fn send_rest(&self, w: &mut impl Write, body: &[u8], sent: &mut usize) -> std::io::Result<()> {
        *sent += body.len();
        w.write_all(body)
    }

    fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        let mut head = format!("HTTP/1.1 {} X\r\n", self.status);
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        let body = match self.cut_after {
            Some(n) => &self.body[..n.min(self.body.len())],
            None => &self.body[..],
        };
        match self.chunked {
            None => {
                head.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    self.body.len()
                ));
                w.write_all(head.as_bytes())?;
                self.send(w, body, &mut 0)?;
            }
            Some(size) => {
                head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                w.write_all(head.as_bytes())?;
                let mut sent = 0;
                for chunk in body.chunks(size) {
                    write!(w, "{:x}\r\n", chunk.len())?;
                    self.send(w, chunk, &mut sent)?;
                    w.write_all(b"\r\n")?;
                }
                if self.cut_after.is_none() {
                    w.write_all(b"0\r\n\r\n")?;
                }
            }
        }
        w.flush()
    }
}

/// A server on a free local port. It serves until the test ends.
pub struct FakeHttp {
    /// `http://127.0.0.1:<port>`, no trailing slash.
    pub url: String,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl FakeHttp {
    pub fn start(answer: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> FakeHttp {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let answer = Arc::new(answer);
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let (answer, log) = (answer.clone(), log.clone());
                std::thread::spawn(move || {
                    let Some(req) = read_request(&conn) else {
                        return;
                    };
                    log.lock().unwrap().push(req.clone());
                    let reply = answer(&req);
                    std::thread::sleep(reply.delay);
                    let mut w = &conn;
                    let _ = reply.write(&mut w);
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                });
            }
        });
        FakeHttp { url, seen }
    }

    /// Every request so far, in the order they arrived.
    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

fn read_request(conn: &std::net::TcpStream) -> Option<Request> {
    let mut r = BufReader::new(conn);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut len = 0;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 || line == "\r\n" {
            break;
        }
        let (k, v) = line.trim_end().split_once(':')?;
        if k.eq_ignore_ascii_case("content-length") {
            len = v.trim().parse().ok()?;
        }
        headers.push((k.to_string(), v.trim().to_string()));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
