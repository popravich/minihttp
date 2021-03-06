//! Websocket client implementation
//!
#[allow(unused_imports)]
use std::ascii::AsciiExt;
use std::fmt::Display;

use futures::{Future, Async};
use httparse::{self, Header};
use tk_bufstream::{IoBuf, ReadBuf, WriteBuf, WriteFramed, ReadFramed};
use tokio_io::{AsyncRead, AsyncWrite};

use base_serializer::{MessageState, HeaderError};
// TODO(tailhook) change the error
use websocket::{Error};
use websocket::error::ErrorEnum;
use enums::{Version, Status};
use websocket::{ClientCodec, Key};



/// Number of headers to allocate on a stack
const MIN_HEADERS: usize = 16;
/// A hard limit on the number of headers
const MAX_HEADERS: usize = 1024;

/// This a request writer that you receive in `Codec`
///
/// Methods of this structure ensure that everything you write into a buffer
/// is consistent and valid protocol
pub struct Encoder<S> {
    message: MessageState,
    buf: WriteBuf<S>,
}

/// This structure returned from `Encoder::done` and works as a continuation
/// that should be returned from the future that writes request.
pub struct EncoderDone<S> {
    buf: WriteBuf<S>,
}

/// Authorizer sends all the necessary headers and checks response headers
/// to establish websocket connection
///
/// The `SimpleAuthorizer` implementation is good enough for most cases, but
/// custom authorizer may be helpful for `Cookie` or `Authorization` header.
pub trait Authorizer<S> {
    /// The type that may be returned from a `header_received`. It should
    /// encompass everything parsed from input headers.
    type Result: Sized;
    /// Write request headers
    ///
    /// Websocket-specific headers like `Connection`, `Upgrade`, and
    /// `Sec-Websocket-Key` are written automatically. But other important
    /// things like `Host`, `Origin`, `User-Agent` must be written by
    /// this method, as well as path encoded in request-line.
    fn write_headers(&mut self, e: Encoder<S>) -> EncoderDone<S>;
    /// A handler of response headers
    ///
    /// It's called when websocket has been sucessfully connected or when
    /// server returned error, check that response code equals 101 to make
    /// sure response is established.
    ///
    /// Anyway, handler may be skipped in case of invalid response headers.
    fn headers_received(&mut self, headers: &Head)
        -> Result<Self::Result, Error>;
}

/// A borrowed structure that represents response headers
///
/// It's passed to `Authorizer::headers_received` and you are
/// free to store or discard any needed fields and headers from it.
///
#[derive(Debug)]
pub struct Head<'a> {
    version: Version,
    code: u16,
    reason: &'a str,
    headers: &'a [Header<'a>],
}

/// A future that resolves to framed streams when websocket handshake is done
pub struct HandshakeProto<S, A> {
    input: Option<ReadBuf<S>>,
    output: Option<WriteBuf<S>>,
    authorizer: A,
}

/// Default handshake handler, if you just want to get websocket connected
pub struct SimpleAuthorizer {
    host: String,
    path: String,
}

impl SimpleAuthorizer {
    /// Create a new authorizer that sends specified host and path
    pub fn new<A, B>(host: A, path: B) -> SimpleAuthorizer
        where A: Into<String>,
              B: Into<String>,
    {
        SimpleAuthorizer {
            host: host.into(),
            path: path.into()
        }
    }
}

impl<S> Authorizer<S> for SimpleAuthorizer {
    type Result = ();
    fn write_headers(&mut self, mut e: Encoder<S>) -> EncoderDone<S> {
        e.request_line(&self.path);
        e.add_header("Host", &self.host).unwrap();
        e.format_header("Origin",
            format_args!("http://{}{}", self.host, self.path))
            .unwrap();
        e.add_header("User-Agent", concat!("tk-http/",
            env!("CARGO_PKG_VERSION"))).unwrap();
        e.done()
    }
    fn headers_received(&mut self, _headers: &Head)
        -> Result<Self::Result, Error>
    {
        Ok(())
    }
}

fn check_header(name: &str) {
    if name.eq_ignore_ascii_case("Connection") ||
        name.eq_ignore_ascii_case("Upgrade") ||
        name.eq_ignore_ascii_case("Sec-Websocket-Key")
    {
        panic!("You shouldn't set websocket specific headers yourself");
    }
}

impl<S> Encoder<S> {
    /// Write request line.
    ///
    /// This puts request line into a buffer immediately. If you don't
    /// continue with request it will be sent to the network shortly.
    ///
    /// # Panics
    ///
    /// When request line is already written. It's expected that your request
    /// handler state machine will never call the method twice.
    pub fn request_line(&mut self, path: &str) {
        self.message.request_line(&mut self.buf.out_buf,
            "GET", path, Version::Http11);
    }
    /// Add a header to the websocket authenticatin data
    ///
    /// Header is written into the output buffer immediately. And is sent
    /// as soon as the next loop iteration
    ///
    /// `Content-Length` header must be send using the `add_length` method
    /// and `Transfer-Encoding: chunked` must be set with the `add_chunked`
    /// method. These two headers are important for the security of HTTP.
    ///
    /// Note that there is currently no way to use a transfer encoding other
    /// than chunked.
    ///
    /// We return Result here to make implementing proxies easier. In the
    /// application handler it's okay to unwrap the result and to get
    /// a meaningful panic (that is basically an assertion).
    ///
    /// # Panics
    ///
    /// Panics when `add_header` is called in the wrong state.
    ///
    /// When you add a special header `Connection`, `Upgrade`,
    /// `Sec-Websocket-*`, because they must be set with special methods
    pub fn add_header<V: AsRef<[u8]>>(&mut self, name: &str, value: V)
        -> Result<(), HeaderError>
    {
        check_header(name);
        self.message.add_header(&mut self.buf.out_buf, name, value.as_ref())
    }

    /// Same as `add_header` but allows value to be formatted directly into
    /// the buffer
    ///
    /// Useful for dates and numeric headers, as well as some strongly typed
    /// wrappers
    pub fn format_header<D: Display>(&mut self, name: &str, value: D)
        -> Result<(), HeaderError>
    {
        check_header(name);
        self.message.format_header(&mut self.buf.out_buf, name, value)
    }
    /// Finish writing headers and return `EncoderDone` which can be moved to
    ///
    /// # Panics
    ///
    /// Panics when the request is in a wrong state.
    pub fn done(mut self) -> EncoderDone<S> {
        self.message.add_header(&mut self.buf.out_buf,
            "Connection", b"upgrade").unwrap();
        self.message.add_header(&mut self.buf.out_buf,
            "Upgrade", b"websocket").unwrap();
        // TODO(tailhook) generate real random key
        self.message.format_header(&mut self.buf.out_buf,
            "Sec-WebSocket-Key", Key::new()).unwrap();
        self.message.add_header(&mut self.buf.out_buf,
            "Sec-WebSocket-Version", b"13").unwrap();
        self.message.done_headers(&mut self.buf.out_buf)
            .map(|ignore_body| assert!(ignore_body)).unwrap();
        self.message.done(&mut self.buf.out_buf);
        EncoderDone { buf: self.buf }
    }
}

fn encoder<S>(io: WriteBuf<S>) -> Encoder<S> {
    Encoder {
        message: MessageState::RequestStart,
        buf: io,
    }
}

impl<S, A: Authorizer<S>> HandshakeProto<S, A> {
    /// Create an instance of future from already connected socket
    pub fn new(transport: S, mut authorizer: A) -> HandshakeProto<S, A>
        where S: AsyncRead + AsyncWrite
    {
        let (tx, rx) = IoBuf::new(transport).split();
        let out = authorizer.write_headers(encoder(tx)).buf;
        HandshakeProto {
            authorizer: authorizer,
            input: Some(rx),
            output: Some(out),
        }
    }
    fn parse_headers(&mut self) -> Result<Option<A::Result>, Error> {
        let ref mut buf = self.input.as_mut()
            .expect("buffer still exists")
            .in_buf;
        let (res, bytes) = {
            let mut vec;
            let mut headers = [httparse::EMPTY_HEADER; MIN_HEADERS];
            let (code, reason, headers, bytes) = {
                let mut raw = httparse::Response::new(&mut headers);
                let mut result = raw.parse(&buf[..]);
                if matches!(result, Err(httparse::Error::TooManyHeaders)) {
                    vec = vec![httparse::EMPTY_HEADER; MAX_HEADERS];
                    raw = httparse::Response::new(&mut vec);
                    result = raw.parse(&buf[..]);
                }
                match result.map_err(ErrorEnum::HeaderError)? {
                    httparse::Status::Complete(bytes) => {
                        let ver = raw.version.unwrap();
                        if ver != 1 {
                            //return Error::VersionTooOld;
                            unimplemented!();
                        }
                        let code = raw.code.unwrap();
                        (code, raw.reason.unwrap(), raw.headers, bytes)
                    }
                    _ => return Ok(None),
                }
            };
            let head = Head {
                version: Version::Http11,
                code: code,
                reason: reason,
                headers: headers,
            };
            let data = self.authorizer.headers_received(&head)?;
            (data, bytes)
        };
        buf.consume(bytes);
        return Ok(Some(res));
    }
}

impl<S, A> Future for HandshakeProto<S, A>
    where A: Authorizer<S>,
          S: AsyncRead + AsyncWrite
{
    type Item = (WriteFramed<S, ClientCodec>, ReadFramed<S, ClientCodec>,
                 A::Result);
    type Error = Error;
    fn poll(&mut self) -> Result<Async<Self::Item>, Error> {
        self.output.as_mut().expect("poll after complete")
            .flush().map_err(ErrorEnum::Io)?;
        self.input.as_mut().expect("poll after complete")
            .read().map_err(ErrorEnum::Io)?;
        if self.input.as_mut().expect("poll after complete").done() {
            return Err(ErrorEnum::PrematureResponseHeaders.into());
        }
        match self.parse_headers()? {
            Some(x) => {
                let inp = self.input.take()
                    .expect("input still here")
                    .framed(ClientCodec);
                let out = self.output.take()
                    .expect("input still here")
                    .framed(ClientCodec);
                Ok(Async::Ready((out, inp, x)))
            }
            None => Ok(Async::NotReady),
        }
    }
}

impl<'a> Head<'a> {
    /// Returns status if it is one of the supported statuses otherwise None
    ///
    /// Note: this method does not consider "reason" string at all just
    /// status code. Which is fine as specification states.
    pub fn status(&self) -> Option<Status> {
        Status::from(self.code)
    }
    /// Returns raw status code and reason as received even
    ///
    /// This returns something even if `status()` returned `None`.
    ///
    /// Note: the reason string may not match the status code or may even be
    /// an empty string.
    pub fn raw_status(&self) -> (u16, &'a str) {
        (self.code, self.reason)
    }
    /// All headers of HTTP request
    ///
    /// Unlike `self.headers()` this does include hop-by-hop headers. This
    /// method is here just for completeness, you shouldn't need it.
    pub fn all_headers(&self) -> &'a [Header<'a>] {
        self.headers
    }
}
