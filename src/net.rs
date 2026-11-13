//! The HTTP transport shared by the model client, single-file HTTP tools and MCP: one client policy, one capped body
//! reader, one server-sent-events parser, and one classification of how far a failed request got.
use crate::policy::Class;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// How far a failed request got, which decides whether the remote side may have acted on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing reached the server (unknown server, failed spawn or handshake, refused connection, failed write), or
    /// the server rejected it before processing: it certainly did not act.
    NotSent,
    /// The server answered with a definite failure (a JSON-RPC error, a 4xx or a 503): it did not act.
    Answered,
    /// The request was sent and its answer lost or inconclusive (a dropped connection, a timeout, an unreadable or
    /// oversized response, a 3xx, a 5xx other than 503): the server may have acted.
    Lost,
}

impl Delivery {
    /// The classification of a status other than 2xx. Only a 4xx or a 503 refuses before processing. A 3xx is often
    /// the answer to work done ("303 See Other" after a POST), which hx does not follow; other 5xx can follow work the
    /// server did (a gateway that lost its upstream's answer, say). Neither settles anything.
    pub fn of_status(status: reqwest::StatusCode) -> Delivery {
        if status.is_client_error() || status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            Delivery::Answered
        } else {
            Delivery::Lost
        }
    }

    /// The classification of a failed `send`: only a connection that was never established certainly sent nothing.
    pub fn of_send(error: &reqwest::Error) -> Delivery {
        if error.is_connect() || error.is_builder() {
            Delivery::NotSent
        } else {
            Delivery::Lost
        }
    }
}

/// A failed request with its classification. The message never contains the request URL.
#[derive(Debug)]
pub struct Error {
    pub delivery: Delivery,
    pub message: String,
}

impl Error {
    pub fn new(delivery: Delivery, message: impl Into<String>) -> Error {
        Error {
            delivery,
            message: message.into(),
        }
    }

    /// A failed `send`, classified, with the URL stripped from reqwest's message.
    pub fn send(context: &str, error: reqwest::Error) -> Error {
        Error::new(
            Delivery::of_send(&error),
            format!("{context}: {}", error.without_url()),
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The Luau error for a failed network effect. Only a mutation whose answer was lost is uncertain: its intent stays
/// open for owner reconciliation. Everything else is a definite failure the model can act on.
pub fn effect_error(class: Class, delivery: Delivery, message: impl AsRef<str>) -> mlua::Error {
    if class != Class::Read && delivery == Delivery::Lost {
        crate::effects::unknown(format!(
            "{} (the request was sent; the server may have acted)",
            message.as_ref()
        ))
    } else {
        mlua::Error::runtime(message.as_ref())
    }
}

/// localhost or a loopback address.
pub fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The client policy for every request hx makes; callers add their timeouts.
///
/// No redirects: reqwest would forward a custom credential header (`x-api-key`) to another host. No automatic retries:
/// a request may be a billable generation or a mutation, and only the caller knows whether a second one is safe. No
/// proxy for a loopback URL: a proxy cannot reach this machine's loopback, and on `http://` it would read the
/// credential in cleartext.
pub fn client(url: &reqwest::Url) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .user_agent(concat!("hx/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never());
    if is_loopback(url) {
        builder.no_proxy()
    } else {
        builder
    }
}

/// Why a response body could not be read whole.
#[derive(Debug)]
pub enum BodyError {
    /// The response exceeds the caller's allowance.
    TooLarge { limit: usize },
    /// The stream failed after the response started.
    Read(String),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::TooLarge { limit } => {
                write!(f, "response too large (limit {} MiB)", limit >> 20)
            }
            BodyError::Read(message) => write!(f, "response read failed: {message}"),
        }
    }
}

impl std::error::Error for BodyError {}

/// One byte allowance for a whole response, including frames the caller skips and error bodies.
/// Check before appending; neither a missing Content-Length nor draining parsed events resets the allowance.
pub struct ResponseBudget {
    limit: usize,
    remaining: usize,
}

impl ResponseBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            remaining: limit,
        }
    }

    /// Charge `bytes` received bytes.
    pub fn consume(&mut self, bytes: usize) -> Result<(), BodyError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or(BodyError::TooLarge { limit: self.limit })?;
        Ok(())
    }

    /// One `\n`-terminated frame (or the rest of the stream at EOF; empty at EOF).
    pub async fn line(
        &mut self,
        reader: &mut (impl AsyncBufRead + Unpin),
    ) -> Result<Vec<u8>, BodyError> {
        let mut bytes = Vec::new();
        // One sentinel byte distinguishes an exact-sized frame from an oversized, newline-free stream.
        reader
            .take(self.remaining as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|e| BodyError::Read(e.to_string()))?;
        self.consume(bytes.len())?;
        Ok(bytes)
    }

    /// The whole body of `response`.
    pub async fn body(&mut self, response: &mut reqwest::Response) -> Result<Vec<u8>, BodyError> {
        if response
            .content_length()
            .is_some_and(|n| n > self.remaining as u64)
        {
            return Err(BodyError::TooLarge { limit: self.limit });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| BodyError::Read(e.without_url().to_string()))?
        {
            self.consume(chunk.len())?;
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// Server-sent events: bytes in, the `data` of each complete event out. Linear in the bytes pushed: each byte is
/// searched once for an event end, and consumed events are dropped once per push.
#[derive(Default)]
pub struct Sse {
    buf: Vec<u8>,
    /// bytes of `buf` already searched for an event end
    scanned: usize,
}

impl Sse {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        // A line ends with \n or \r\n; JSON data never holds a raw \r, so dropping every \r leaves \n endings only.
        self.buf.extend(bytes.iter().filter(|&&b| b != b'\r'));
        let mut out = Vec::new();
        let mut start = 0;
        // Resume one byte early: the previous push may have ended between the two newlines of an event end.
        let mut from = self.scanned.saturating_sub(1);
        while let Some(at) = self.buf[from..].windows(2).position(|w| w == b"\n\n") {
            let end = from + at;
            let event = String::from_utf8_lossy(&self.buf[start..end]);
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
            start = end + 2;
            from = start;
        }
        self.buf.drain(..start);
        self.scanned = self.buf.len();
        out
    }

    /// The event left when the stream ends without its final blank line, if any.
    pub fn finish(&mut self) -> Vec<String> {
        self.push(b"\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_split_across_chunks_and_line_endings() {
        let mut sse = Sse::default();
        assert!(
            sse.push(b"event: response.created\ndata: {\"a\"")
                .is_empty()
        );
        assert_eq!(
            sse.push(b":1}\n\n: comment\n\ndata: x\r\n\r\ndata: one\ndata: two\n"),
            vec!["{\"a\":1}", "x"]
        );
        assert_eq!(sse.push(b"\n"), vec!["one\ntwo"]);
        assert!(sse.push(b"data: partial\r\n\r").is_empty());
        assert_eq!(sse.push(b"\n"), vec!["partial"]);
        assert!(sse.push(b"data: tail").is_empty());
        assert_eq!(sse.finish(), vec!["tail"]);
        assert!(sse.finish().is_empty());
    }

    #[test]
    fn sse_parsing_is_linear_in_the_stream() {
        // 16 MiB in 64-byte chunks: a parser that rescans its buffer per chunk does ~2^37 byte comparisons here.
        let data = "x".repeat(16 << 20);
        let stream = format!("data: {data}\n\n");
        let started = std::time::Instant::now();
        let mut sse = Sse::default();
        let mut events = Vec::new();
        for chunk in stream.as_bytes().chunks(64) {
            events.extend(sse.push(chunk));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].len(), data.len());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn loopback_hosts_bypass_the_proxy() {
        for url in [
            "http://localhost:1/x",
            "http://LOCALHOST/x",
            "http://127.0.0.1/x",
            "http://127.1.2.3/x",
            "http://[::1]:8/x",
            "https://localhost/x",
        ] {
            assert!(is_loopback(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "http://example.com/x",
            "http://10.0.0.1/x",
            "http://localhost.example.com/x",
            "http://[::2]/x",
        ] {
            assert!(!is_loopback(&reqwest::Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn status_classification() {
        use reqwest::StatusCode;
        for (status, delivery) in [
            (400, Delivery::Answered),
            (404, Delivery::Answered),
            (429, Delivery::Answered),
            (503, Delivery::Answered),
            (500, Delivery::Lost),
            (502, Delivery::Lost),
            (504, Delivery::Lost),
            (301, Delivery::Lost),
            (302, Delivery::Lost),
            (303, Delivery::Lost),
            (307, Delivery::Lost),
            (304, Delivery::Lost),
        ] {
            assert_eq!(
                Delivery::of_status(StatusCode::from_u16(status).unwrap()),
                delivery,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn line_budget_covers_newline_free_output_and_skipped_frames() {
        let bytes = vec![b'x'; 65];
        let error = ResponseBudget::new(64)
            .line(&mut bytes.as_slice())
            .await
            .unwrap_err();
        assert!(matches!(error, BodyError::TooLarge { limit: 64 }));

        let mut budget = ResponseBudget::new(8);
        let mut reader = &b"ping\npong\n"[..];
        assert_eq!(budget.line(&mut reader).await.unwrap(), b"ping\n");
        assert!(matches!(
            budget.line(&mut reader).await,
            Err(BodyError::TooLarge { .. })
        ));
        let mut budget = ResponseBudget::new(5);
        assert_eq!(budget.line(&mut &b"done\n"[..]).await.unwrap(), b"done\n");
    }

    #[tokio::test]
    async fn refused_connection_sent_nothing() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url =
            reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let error = client(&url)
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap_err();
        assert_eq!(Delivery::of_send(&error), Delivery::NotSent);
    }
}
