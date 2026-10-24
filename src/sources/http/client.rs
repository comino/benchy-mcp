//! REQ-17: one HTTP source's requests (docs/HTTP-SOURCE.md, "Requests").
//!
//! Every request of a source goes through one [`Client`]: one deadline for the whole source, its
//! rate limit, its retries and their waits, the redirects it may follow and the credential it may
//! carry on each, and the body, decoded and counted as it is read. Nothing here writes a secret
//! anywhere: a log line or a refusal is built from the URL's origin and path, never from a
//! request's headers or query, and is passed through [`Redact`] besides, since an API may echo a
//! key back in its errors or its next-page URLs.

use super::paginate::{self, set_param};
use super::{Auth, Compression, Spec};
use crate::exec::Fault;
use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How many redirects one request follows.
const MAX_REDIRECTS: usize = 5;
/// The most of an error body the operator log is given, as for ClickHouse's.
const ERROR_TEXT: usize = 2000;
/// Full-jitter exponential backoff: the n-th retry waits up to 250 ms × 2ⁿ, at most 10 s.
const BACKOFF_FIRST: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(10);
/// Connecting is held to this within the deadline: a host that does not answer a SYN in 10 s
/// is down, and waiting out a 300 s deadline on it would leave no time to retry.
const CONNECT: Duration = Duration::from_secs(10);

/// Every value a log line or a refusal must not carry: this source's secrets as sent — raw, as
/// percent-encoded on a URL, and as the Basic header encodes them.
#[derive(Debug, Clone, Default)]
pub struct Redact(Vec<String>);

impl Redact {
    pub fn of(spec: &Spec) -> Redact {
        let mut out: Vec<String> = Vec::new();
        for v in spec.env.values().filter(|v| !v.is_empty()) {
            out.push(v.clone());
            out.push(paginate::encode(v));
        }
        if let Auth::Basic { user, password } = &spec.auth {
            let pair = format!(
                "{}:{}",
                spec.env.get(user).map_or("", String::as_str),
                spec.env.get(password).map_or("", String::as_str)
            );
            out.push(base64(pair.as_bytes()));
        }
        // Longest first, so a secret that contains another is masked whole.
        out.sort_by_key(|s| std::cmp::Reverse(s.len()));
        out.dedup();
        Redact(out)
    }

    pub fn apply(&self, text: &str) -> String {
        let mut t = text.to_string();
        for s in &self.0 {
            if t.contains(s.as_str()) {
                t = t.replace(s.as_str(), "***");
            }
        }
        t
    }

    pub fn fault(&self, f: Fault) -> Fault {
        Fault {
            code: f.code,
            detail: self.apply(&f.detail),
            ..f
        }
    }
}

/// What a body reader saw, shared with whoever reads it, which may be another thread than the
/// one that opened it.
#[derive(Debug, Default)]
pub struct BodyState {
    /// The first transport error: whether it was the deadline, and what it said.
    pub error: Option<(bool, String)>,
    /// The first decoding error: whether the compressed stream ended early, and what it said.
    pub decode: Option<(bool, String)>,
    /// Bytes as the reader got them, after any decompression.
    pub decoded: u64,
    /// The most `decoded` may reach, and whether the body went past it.
    pub cap: u64,
    pub over: bool,
    /// gzip or zstd, when the body was compressed.
    pub codec: Option<&'static str>,
}

/// A response's body, decoded and counted as it is read, never past its cap.
pub struct Body {
    pub reader: Box<dyn Read + Send>,
    pub state: Arc<Mutex<BodyState>>,
}

/// A response that was a success.
pub struct Answer {
    /// The `Link` header's values, joined with `, `.
    pub link: Option<String>,
    /// `Content-Length`, when the body arrives as it was sent: what admission can ask for.
    pub length: Option<u64>,
    pub body: Body,
}

/// Bytes off the wire, before any decoding: counted, and the first transport error kept.
struct Wire<R> {
    inner: R,
    state: Arc<Mutex<BodyState>>,
    total: Arc<AtomicU64>,
}

impl<R: Read> Read for Wire<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => {
                self.total.fetch_add(n as u64, Ordering::Relaxed);
                Ok(n)
            }
            Err(e) => {
                let mut s = self.state.lock().unwrap();
                if s.error.is_none() {
                    s.error = Some((crate::sources::clickhouse::timed_out(&e), e.to_string()));
                }
                Err(e)
            }
        }
    }
}

/// The decoded body, held to its cap as it is read: one byte past it is read, to know the body
/// is longer, and refused. A decompression bomb stops here, at the cap, never decoded whole.
struct Decoded<R> {
    inner: R,
    state: Arc<Mutex<BodyState>>,
}

impl<R: Read> Read for Decoded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let room = {
            let s = self.state.lock().unwrap();
            if s.over {
                return Err(std::io::Error::other("byte cap reached"));
            }
            s.cap.saturating_add(1).saturating_sub(s.decoded)
        };
        let room = room.min(buf.len() as u64) as usize;
        if room == 0 && !buf.is_empty() {
            self.state.lock().unwrap().over = true;
            return Err(std::io::Error::other("byte cap reached"));
        }
        match self.inner.read(&mut buf[..room]) {
            Ok(n) => {
                let mut s = self.state.lock().unwrap();
                s.decoded += n as u64;
                if s.decoded > s.cap {
                    s.over = true;
                    return Err(std::io::Error::other("byte cap reached"));
                }
                Ok(n)
            }
            Err(e) => {
                let mut s = self.state.lock().unwrap();
                if s.error.is_none() && s.decode.is_none() {
                    s.decode = Some((e.kind() == std::io::ErrorKind::UnexpectedEof, e.to_string()));
                }
                Err(e)
            }
        }
    }
}

/// How one attempt ended, when it did not succeed.
pub enum Try {
    /// Worth another try, after the wait the server asked for if it asked.
    Again(Fault, Option<Duration>),
    Fail(Fault),
}

impl From<Fault> for Try {
    fn from(f: Fault) -> Self {
        Try::Fail(f)
    }
}

/// One source's requests. Shared by reference with the thread that fetches a paged source's
/// pages ahead of its table (R4-3), so it holds its counts in atomics.
pub struct Client<'a> {
    spec: &'a Spec,
    deadline: Instant,
    pub redact: Redact,
    /// When the last request started, for `rate_limit`.
    last: Mutex<Option<Instant>>,
    seed: AtomicU64,
    /// Bytes that crossed the network, every response included.
    pub wire: Arc<AtomicU64>,
    pub requests: AtomicU64,
    pub retried: AtomicU64,
}

impl<'a> Client<'a> {
    pub fn new(spec: &'a Spec) -> Client<'a> {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Client {
            spec,
            deadline: Instant::now() + spec.limits.deadline,
            redact: Redact::of(spec),
            last: Mutex::new(None),
            seed: AtomicU64::new(nanos ^ (u64::from(std::process::id()) << 32) | 1),
            wire: Arc::new(AtomicU64::new(0)),
            requests: AtomicU64::new(0),
            retried: AtomicU64::new(0),
        }
    }

    /// A URL as the operator log may show it: origin and path, no query, which is where a
    /// cursor or an echoed key would be.
    pub fn shown(&self, url: &str) -> String {
        let at = url.find(['?', '#']).unwrap_or(url.len());
        self.redact.apply(&url[..at])
    }

    fn timeout(&self, why: &str) -> Fault {
        Fault::host(
            "E_TIMEOUT",
            format!(
                "{}: no whole answer within the source's deadline of {}s: {why}",
                self.shown(&self.spec.url),
                self.spec.limits.deadline.as_secs()
            ),
        )
    }

    fn left(&self) -> Result<Duration, Fault> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(self.timeout("it passed before the next request"));
        }
        Ok(left)
    }

    /// The bulk body: retried until its headers arrive, never once its bytes start going into the
    /// table (decision 7).
    pub fn open(&self, url: &str, cap: u64) -> Result<Answer, Fault> {
        self.with_retries(|| self.exchange(url, cap))
    }

    /// One JSON page, read whole into `buf` before it is decoded, so a page that breaks off is
    /// retried like a request that failed. What each attempt read is spent on `budget`.
    pub fn page(
        &self,
        url: &str,
        buf: &mut Vec<u8>,
        cap: u64,
        budget: &crate::admission::Budget,
    ) -> Result<Option<String>, Fault> {
        self.with_retries(|| {
            let mut answer = self.exchange(url, cap)?;
            buf.clear();
            let read = answer.body.reader.read_to_end(buf);
            budget.spend(0, buf.len() as u64);
            match read {
                Ok(_) => Ok(answer.link.take()),
                Err(e) => {
                    let s = answer.body.state.lock().unwrap();
                    let f = self
                        .body_fault(&s)
                        .unwrap_or_else(|| Fault::host("E_UNAVAILABLE", e.to_string()));
                    // Past the cap, or not what it says it is: the same again would be the same.
                    if s.over || (s.decode.is_some() && f.code == "E_CONFIG") {
                        Err(Try::Fail(f))
                    } else {
                        Err(Try::Again(f, None))
                    }
                }
            }
        })
    }

    fn with_retries<T>(&self, mut once: impl FnMut() -> Result<T, Try>) -> Result<T, Fault> {
        let mut attempt = 0u32;
        loop {
            match once() {
                Ok(t) => return Ok(t),
                Err(Try::Fail(f)) => return Err(f),
                Err(Try::Again(f, wait)) => {
                    if attempt >= self.spec.retries {
                        return Err(f);
                    }
                    let wait = wait.unwrap_or_else(|| self.backoff(attempt));
                    if Instant::now() + wait >= self.deadline {
                        return Err(Fault::host(
                            "E_TIMEOUT",
                            format!(
                                "{} The wait before trying again, {:.1}s, would pass the source's \
                                 deadline of {}s, so it was not waited for.",
                                f.detail,
                                wait.as_secs_f64(),
                                self.spec.limits.deadline.as_secs()
                            ),
                        ));
                    }
                    eprintln!(
                        "benchy: source `{}`: [{}] {} — trying again in {:.1}s",
                        self.spec.name,
                        f.code,
                        f.detail,
                        wait.as_secs_f64()
                    );
                    std::thread::sleep(wait);
                    attempt += 1;
                    self.retried.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Full jitter: anywhere between nothing and the attempt's ceiling, so clients that failed
    /// together do not come back together.
    fn backoff(&self, attempt: u32) -> Duration {
        let ceiling = BACKOFF_FIRST
            .saturating_mul(1u32 << attempt.min(16))
            .min(BACKOFF_CAP);
        // xorshift64*: a jitter, not a secret.
        let mut x = self.seed.load(Ordering::Relaxed);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed.store(x, Ordering::Relaxed);
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        ceiling.mul_f64(r as f64 / (1u64 << 53) as f64)
    }

    /// Wait out the rate limit before a request starts.
    fn space(&self) -> Result<(), Fault> {
        if let Some(rl) = self.spec.rate_limit {
            let gap = Duration::from_secs(rl.per_secs) / rl.requests;
            let last = *self.last.lock().unwrap();
            if let Some(last) = last {
                let at = last + gap;
                let now = Instant::now();
                if at > now {
                    if at >= self.deadline {
                        return Err(self.timeout("the rate limit spaces the next request past it"));
                    }
                    std::thread::sleep(at - now);
                }
            }
        }
        *self.last.lock().unwrap() = Some(Instant::now());
        Ok(())
    }

    fn agent(&self, within: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            // A status is the API's answer, read and classified here.
            .http_status_as_error(false)
            // Where the config says and nowhere else: no proxy from the environment (a loader's
            // is empty anyway), and no redirect ureq follows itself. It strips only
            // `Authorization` on a cross-host redirect, and would carry an `X-Api-Key` header or
            // an `auth` parameter wherever a redirect pointed; benchy follows them itself.
            .proxy(None)
            .max_redirects(0)
            .timeout_connect(Some(within.min(CONNECT)))
            .timeout_global(Some(within))
            .user_agent(format!("benchy-mcp/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent()
    }

    /// `url` without any query parameter whose value is one of this source's secrets.
    fn without_secrets(&self, url: &str) -> String {
        let Some((path, query)) = url.split_once('?') else {
            return url.to_string();
        };
        let kept: Vec<&str> = query
            .split('&')
            .filter(|kv| {
                let v = kv.split_once('=').map_or("", |(_, v)| v);
                !kv.is_empty() && !self.redact.0.iter().any(|s| s == v)
            })
            .collect();
        if kept.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", kept.join("&"))
        }
    }

    /// The query parameter the credential travels in, when it is one.
    fn auth_param(&self) -> Option<(&str, &str)> {
        match &self.spec.auth {
            Auth::Param { name, env } => Some((name, self.spec.env.get(env)?)),
            _ => None,
        }
    }

    /// One exchange: the request, the redirects it may follow, and the status read.
    fn exchange(&self, url: &str, cap: u64) -> Result<Answer, Try> {
        let mut url = url.to_string();
        let mut credential = true;
        let mut post = self.spec.post.as_deref();
        for _ in 0..=MAX_REDIRECTS {
            let left = self.left()?;
            self.space()?;
            self.requests.fetch_add(1, Ordering::Relaxed);
            let target = match self.auth_param() {
                Some((name, value)) if credential => set_param(&url, name, value),
                // Off the origin, nothing of the secret: not the `auth` parameter, and no
                // parameter holding a secret's value, which an API may have echoed into the URL
                // it redirected to.
                Some((name, _)) => self.without_secrets(&without_param(&url, name)),
                None if !credential => self.without_secrets(&url),
                None => url.clone(),
            };
            let agent = self.agent(left);
            let sent = match post {
                Some(body) => {
                    let req = self.headers(agent.post(&target), credential);
                    req.send(body.as_bytes())
                }
                None => self.headers(agent.get(&target), credential).call(),
            };
            let resp = match sent {
                Ok(r) => r,
                Err(ureq::Error::Timeout(t)) => {
                    if Instant::now() >= self.deadline {
                        return Err(Try::Fail(self.timeout(&t.to_string())));
                    }
                    return Err(Try::Again(
                        Fault::host(
                            "E_UNAVAILABLE",
                            format!("{}: no answer: {t}", self.shown(&url)),
                        ),
                        None,
                    ));
                }
                Err(e) => {
                    return Err(Try::Again(
                        Fault::host(
                            "E_UNAVAILABLE",
                            format!(
                                "{}: could not be reached: {}",
                                self.shown(&url),
                                self.redact.apply(&e.to_string())
                            ),
                        ),
                        None,
                    ))
                }
            };
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                return self.answer(resp, cap);
            }
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let location = resp
                    .headers()
                    .get("Location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        Fault::host(
                            "E_HOST_PROTOCOL",
                            format!(
                                "{}: HTTP {status} without a Location to go to",
                                self.shown(&url)
                            ),
                        )
                    })?;
                let next = paginate::resolve(&url, location.trim()).ok_or_else(|| {
                    Fault::host(
                        "E_HOST_PROTOCOL",
                        format!(
                            "{}: HTTP {status} to a Location that is not a URL",
                            self.shown(&url)
                        ),
                    )
                })?;
                credential = credential && self.follow(&next)?;
                if status == 303 {
                    post = None;
                }
                url = next;
                continue;
            }
            return Err(self.refusal(resp, status, &url));
        }
        Err(Try::Fail(Fault::host(
            "E_CONFIG",
            format!(
                "{}: more than {MAX_REDIRECTS} redirects for one request",
                self.shown(&self.spec.url)
            ),
        )))
    }

    /// Whether a redirect to `next` may be followed, and with the credential: on the source's
    /// own origin, yes and yes; to a host in `redirect_hosts`, yes, without it, and never from
    /// https down to http (`a_redirect_off_the_origin_carries_no_credential`). Anywhere else is
    /// refused, naming where it pointed and nothing of its path or query.
    fn follow(&self, next: &str) -> Result<bool, Fault> {
        if paginate::same_origin(next, &self.spec.url) {
            return Ok(true);
        }
        if self.spec.redirect_listed(next) {
            return Ok(false);
        }
        Err(Fault::host(
            "E_CONFIG",
            format!(
                "{}: redirected to {}, which is not this source's origin and not in its \
                 `redirect_hosts`. Refused: the request carries this source's credential.",
                self.shown(&self.spec.url),
                self.redact.apply(&paginate::origin_only(next))
            ),
        ))
    }

    fn headers<B>(
        &self,
        mut req: ureq::RequestBuilder<B>,
        credential: bool,
    ) -> ureq::RequestBuilder<B> {
        for (k, v) in &self.spec.headers {
            req = req.header(k, v);
        }
        // Asked for under `auto` and decoded here, bounded; otherwise the body as it is.
        req = req.header(
            "Accept-Encoding",
            if self.spec.compression == Compression::Auto {
                "zstd, gzip"
            } else {
                "identity"
            },
        );
        if !credential {
            return req;
        }
        let env = |k: &str| self.spec.env.get(k).map_or("", String::as_str);
        match &self.spec.auth {
            Auth::Bearer { env: k } => req.header("Authorization", format!("Bearer {}", env(k))),
            Auth::Header { name, env: k } => req.header(name, env(k)),
            Auth::Basic { user, password } => req.header(
                "Authorization",
                format!(
                    "Basic {}",
                    base64(format!("{}:{}", env(user), env(password)).as_bytes())
                ),
            ),
            Auth::Param { .. } | Auth::None => req,
        }
    }

    /// A status that is neither a success nor a redirect, mapped to the closed set of codes. The
    /// API's own words go to the operator log, redacted and cut to 2,000 characters.
    fn refusal(&self, resp: ureq::http::Response<ureq::Body>, status: u16, url: &str) -> Try {
        let wait = resp
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| retry_after(v, SystemTime::now()));
        let mut text = Vec::new();
        let _ = resp
            .into_body()
            .into_reader()
            .take(64 * 1024)
            .read_to_end(&mut text);
        let text: String = String::from_utf8_lossy(&text)
            .trim()
            .chars()
            .take(ERROR_TEXT)
            .collect();
        let said = |code: &'static str, more: &str| {
            Fault::host(
                code,
                format!(
                    "{}: HTTP {status}{more}{}",
                    self.shown(url),
                    if text.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", self.redact.apply(&text))
                    }
                ),
            )
        };
        let pushed = self.spec.pushed();
        match status {
            408 | 429 | 500 | 502 | 503 | 504 => Try::Again(said("E_UNAVAILABLE", ""), wait),
            401 | 403 => Try::Fail(said("E_AUTH", "")),
            // Decision 8: no retry without the parameters. Correctness never rests on pushdown —
            // the narrowing is re-applied after ingest — but fetching everything instead can be
            // a thousand-fold more bytes, and a 400 does not say which parameter it disliked.
            400 if !pushed.is_empty() => {
                let sent: Vec<String> = pushed.iter().map(|(k, v)| format!("{k}={v}")).collect();
                Try::Fail(said(
                    "E_CONFIG",
                    &format!(
                        " to a request carrying the pushdown parameters {}; not retried without \
                         them. Check [source.pushdown] against what the API takes; `benchy-mcp \
                         check` shows the request",
                        sent.join(", ")
                    ),
                ))
            }
            500..=599 => Try::Fail(said("E_UNAVAILABLE", "")),
            400..=499 => Try::Fail(said("E_CONFIG", "")),
            _ => Try::Fail(said(
                "E_HOST_PROTOCOL",
                ", which is not an answer benchy reads",
            )),
        }
    }

    /// A success: its body, decoded as `Content-Encoding` and `compression` say, counted, and
    /// held to `cap` decoded bytes.
    fn answer(&self, resp: ureq::http::Response<ureq::Body>, cap: u64) -> Result<Answer, Try> {
        let header = |k: &str| -> Vec<String> {
            resp.headers()
                .get_all(k)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(|v| v.trim().to_string())
                .collect()
        };
        let encoding = header("Content-Encoding").join(",").to_ascii_lowercase();
        let link = header("Link");
        let link = (!link.is_empty()).then(|| link.join(", "));
        let length = header("Content-Length")
            .first()
            .and_then(|v| v.parse().ok());
        let codec = match (self.spec.compression, encoding.as_str()) {
            (Compression::Auto | Compression::None, "" | "identity") => None,
            (Compression::Auto | Compression::Gzip, "gzip" | "x-gzip") => Some("gzip"),
            (Compression::Auto | Compression::Zstd, "zstd") => Some("zstd"),
            // The body is a compressed file whatever the header says: `readings.csv.gz`.
            (Compression::Gzip, "" | "identity") => Some("gzip"),
            (Compression::Zstd, "" | "identity") => Some("zstd"),
            (_, other) => {
                return Err(Try::Fail(Fault::host(
                    "E_CONFIG",
                    format!(
                        "{}: the response is `{other}`-encoded, which is not what this source \
                         asked for or reads: gzip and zstd are read, with `compression` \"auto\"",
                        self.shown(&self.spec.url)
                    ),
                )))
            }
        };
        let state = Arc::new(Mutex::new(BodyState {
            cap,
            codec,
            ..BodyState::default()
        }));
        let wire = Wire {
            inner: resp.into_body().into_reader(),
            state: state.clone(),
            total: self.wire.clone(),
        };
        let decoded = decompress(codec, wire).map_err(|e| {
            Fault::host(
                "E_INTERNAL",
                format!("could not start the body's decoder: {e}"),
            )
        })?;
        Ok(Answer {
            link,
            length: if codec.is_none() { length } else { None },
            body: Body {
                reader: Box::new(Decoded {
                    inner: decoded,
                    state: state.clone(),
                }),
                state,
            },
        })
    }

    /// Why a body could not be read whole, from what its reader saw: past the cap, the deadline,
    /// cut short, or not the compression it said. `None` when the reader saw nothing wrong.
    pub fn body_fault(&self, s: &BodyState) -> Option<Fault> {
        let at = self.shown(&self.spec.url);
        if s.over {
            return Some(Fault::host(
                "E_TOO_LARGE",
                format!(
                    "{at}: the body decodes to more than {} bytes, what this source's `max_bytes` \
                     and the session's budget allow. Refused, not truncated: nothing of it was \
                     loaded.",
                    s.cap
                ),
            ));
        }
        if let Some((timeout, why)) = &s.error {
            let why = self.redact.apply(why);
            return Some(if *timeout {
                self.timeout(&why)
            } else {
                Fault::host(
                    "E_HOST_TRUNCATED",
                    format!(
                        "{at}: the body broke off after {} byte(s): {why}. Not retried: what it \
                         sent was already going into the table.",
                        s.decoded
                    ),
                )
            });
        }
        if let Some((eof, why)) = &s.decode {
            let codec = s.codec.unwrap_or("body");
            return Some(if *eof {
                Fault::host(
                    "E_HOST_TRUNCATED",
                    format!("{at}: the {codec} stream ends before it is complete: {why}"),
                )
            } else {
                Fault::host(
                    "E_CONFIG",
                    format!("{at}: the body is not the {codec} it was said to be: {why}"),
                )
            });
        }
        None
    }
}

/// The body's decoder: `crate::decompress`, shared with ClickHouse's source.
fn decompress(
    codec: Option<&'static str>,
    wire: impl Read + Send + 'static,
) -> std::io::Result<Box<dyn Read + Send>> {
    let encoding = match codec {
        None => return Ok(Box::new(wire)),
        Some("gzip") => crate::decompress::Encoding::Gzip,
        Some(_) => crate::decompress::Encoding::Zstd,
    };
    crate::decompress::decoder(encoding, BufReader::with_capacity(64 * 1024, wire))
}

/// `url` without its query parameter `key`: what a redirect off the origin is sent to, when its
/// URL carries the key back.
fn without_param(url: &str, key: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let key = paginate::encode(key);
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| !kv.is_empty() && kv.split('=').next() != Some(key.as_str()))
        .collect();
    if kept.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

/// `Retry-After`: delay-seconds, or an IMF-fixdate (RFC 9110 §10.2.3), as a wait from `now`.
pub fn retry_after(v: &str, now: SystemTime) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // Sun, 06 Nov 1994 08:49:37 GMT
    let f: Vec<&str> = v.split_whitespace().collect();
    if f.len() != 6 || f[5] != "GMT" {
        return None;
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let day: i64 = f[1].parse().ok()?;
    let month = MONTHS.iter().position(|m| *m == f[2])? as i64 + 1;
    let year: i64 = f[3].parse().ok()?;
    let hms: Vec<i64> = f[4]
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    let [h, m, s] = hms.as_slice() else {
        return None;
    };
    // Days from civil, after Howard Hinnant.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let at = (era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + m * 60 + s;
    let now = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(Duration::from_secs((at - now).max(0) as u64))
}

/// Standard base64 with padding, for HTTP Basic.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_seconds_or_an_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777); // 08:49:37 GMT
        assert_eq!(retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:40 GMT", now),
            Some(Duration::from_secs(3))
        );
        // In the past: now.
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after("soon", now), None);
        assert_eq!(retry_after("Sunday, 06-Nov-94 08:49:37 GMT", now), None);
    }

    #[test]
    fn basic_credentials_are_base64() {
        // RFC 7617's example.
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    #[test]
    fn a_parameter_is_taken_off_a_url() {
        assert_eq!(
            without_param("https://h/o?sig=1&api_key=k&x=2", "api_key"),
            "https://h/o?sig=1&x=2"
        );
        assert_eq!(
            without_param("https://h/o?api_key=k", "api_key"),
            "https://h/o"
        );
        assert_eq!(without_param("https://h/o", "api_key"), "https://h/o");
    }
}
