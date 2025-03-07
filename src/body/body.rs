use std::fmt;

use bytes::Bytes;
use futures_channel::mpsc;
use futures_channel::oneshot;
use futures_core::Stream; // for mpsc::Receiver
use http::HeaderMap;
use http_body::{Body, SizeHint};

use super::DecodedLength;
use crate::common::Future;
use crate::common::{task, watch, Pin, Poll};
#[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
use crate::proto::h2::ping;

type BodySender = mpsc::Sender<Result<Bytes, crate::Error>>;
type TrailersSender = oneshot::Sender<HeaderMap>;

/// A stream of `Bytes`, used when receiving bodies.
///
/// A good default [`Body`](crate::body::Body) to use in many
/// applications.
///
/// Note: To read the full body, use [`body::to_bytes`](crate::body::to_bytes)
/// or [`body::aggregate`](crate::body::aggregate).
#[must_use = "streams do nothing unless polled"]
pub struct Recv {
    kind: Kind,
}

enum Kind {
    #[allow(dead_code)]
    Empty,
    Chan {
        content_length: DecodedLength,
        want_tx: watch::Sender,
        data_rx: mpsc::Receiver<Result<Bytes, crate::Error>>,
        trailers_rx: oneshot::Receiver<HeaderMap>,
    },
    #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
    H2 {
        ping: ping::Recorder,
        content_length: DecodedLength,
        recv: h2::RecvStream,
    },
    #[cfg(feature = "ffi")]
    Ffi(crate::ffi::UserBody),
}

/// A sender half created through [`Body::channel()`].
///
/// Useful when wanting to stream chunks from another thread.
///
/// ## Body Closing
///
/// Note that the request body will always be closed normally when the sender is dropped (meaning
/// that the empty terminating chunk will be sent to the remote). If you desire to close the
/// connection with an incomplete response (e.g. in the case of an error during asynchronous
/// processing), call the [`Sender::abort()`] method to abort the body in an abnormal fashion.
///
/// [`Body::channel()`]: struct.Body.html#method.channel
/// [`Sender::abort()`]: struct.Sender.html#method.abort
#[must_use = "Sender does nothing unless sent on"]
pub(crate) struct Sender {
    want_rx: watch::Receiver,
    data_tx: BodySender,
    trailers_tx: Option<TrailersSender>,
}

const WANT_PENDING: usize = 1;
const WANT_READY: usize = 2;

impl Recv {
    /// Create a `Body` stream with an associated sender half.
    ///
    /// Useful when wanting to stream chunks from another thread.
    #[inline]
    #[allow(unused)]
    pub(crate) fn channel() -> (Sender, Recv) {
        Self::new_channel(DecodedLength::CHUNKED, /*wanter =*/ false)
    }

    pub(crate) fn new_channel(content_length: DecodedLength, wanter: bool) -> (Sender, Recv) {
        let (data_tx, data_rx) = mpsc::channel(0);
        let (trailers_tx, trailers_rx) = oneshot::channel();

        // If wanter is true, `Sender::poll_ready()` won't becoming ready
        // until the `Body` has been polled for data once.
        let want = if wanter { WANT_PENDING } else { WANT_READY };

        let (want_tx, want_rx) = watch::channel(want);

        let tx = Sender {
            want_rx,
            data_tx,
            trailers_tx: Some(trailers_tx),
        };
        let rx = Recv::new(Kind::Chan {
            content_length,
            want_tx,
            data_rx,
            trailers_rx,
        });

        (tx, rx)
    }

    fn new(kind: Kind) -> Recv {
        Recv { kind }
    }

    #[allow(dead_code)]
    pub(crate) fn empty() -> Recv {
        Recv::new(Kind::Empty)
    }

    #[cfg(feature = "ffi")]
    pub(crate) fn ffi() -> Recv {
        Recv::new(Kind::Ffi(crate::ffi::UserBody::new()))
    }

    #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
    pub(crate) fn h2(
        recv: h2::RecvStream,
        mut content_length: DecodedLength,
        ping: ping::Recorder,
    ) -> Self {
        // If the stream is already EOS, then the "unknown length" is clearly
        // actually ZERO.
        if !content_length.is_exact() && recv.is_end_stream() {
            content_length = DecodedLength::ZERO;
        }
        let body = Recv::new(Kind::H2 {
            ping,
            content_length,
            recv,
        });

        body
    }

    #[cfg(feature = "ffi")]
    pub(crate) fn as_ffi_mut(&mut self) -> &mut crate::ffi::UserBody {
        match self.kind {
            Kind::Ffi(ref mut body) => return body,
            _ => {
                self.kind = Kind::Ffi(crate::ffi::UserBody::new());
            }
        }

        match self.kind {
            Kind::Ffi(ref mut body) => body,
            _ => unreachable!(),
        }
    }

    fn poll_inner(&mut self, cx: &mut task::Context<'_>) -> Poll<Option<crate::Result<Bytes>>> {
        match self.kind {
            Kind::Empty => Poll::Ready(None),
            Kind::Chan {
                content_length: ref mut len,
                ref mut data_rx,
                ref mut want_tx,
                ..
            } => {
                want_tx.send(WANT_READY);

                match ready!(Pin::new(data_rx).poll_next(cx)?) {
                    Some(chunk) => {
                        len.sub_if(chunk.len() as u64);
                        Poll::Ready(Some(Ok(chunk)))
                    }
                    None => Poll::Ready(None),
                }
            }
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 {
                ref ping,
                recv: ref mut h2,
                content_length: ref mut len,
            } => match ready!(h2.poll_data(cx)) {
                Some(Ok(bytes)) => {
                    let _ = h2.flow_control().release_capacity(bytes.len());
                    len.sub_if(bytes.len() as u64);
                    ping.record_data(bytes.len());
                    Poll::Ready(Some(Ok(bytes)))
                }
                Some(Err(e)) => Poll::Ready(Some(Err(crate::Error::new_body(e)))),
                None => Poll::Ready(None),
            },

            #[cfg(feature = "ffi")]
            Kind::Ffi(ref mut body) => body.poll_data(cx),
        }
    }
}

impl Body for Recv {
    type Data = Bytes;
    type Error = crate::Error;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        self.poll_inner(cx)
    }

    fn poll_trailers(
        #[cfg_attr(not(feature = "http2"), allow(unused_mut))] mut self: Pin<&mut Self>,
        #[cfg_attr(not(feature = "http2"), allow(unused))] cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        match self.kind {
            Kind::Empty => Poll::Ready(Ok(None)),
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 {
                recv: ref mut h2,
                ref ping,
                ..
            } => match ready!(h2.poll_trailers(cx)) {
                Ok(t) => {
                    ping.record_non_data();
                    Poll::Ready(Ok(t))
                }
                Err(e) => Poll::Ready(Err(crate::Error::new_h2(e))),
            },
            Kind::Chan {
                ref mut trailers_rx,
                ..
            } => match ready!(Pin::new(trailers_rx).poll(cx)) {
                Ok(t) => Poll::Ready(Ok(Some(t))),
                Err(_) => Poll::Ready(Ok(None)),
            },
            #[cfg(feature = "ffi")]
            Kind::Ffi(ref mut body) => body.poll_trailers(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self.kind {
            Kind::Empty => true,
            Kind::Chan { content_length, .. } => content_length == DecodedLength::ZERO,
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 { recv: ref h2, .. } => h2.is_end_stream(),
            #[cfg(feature = "ffi")]
            Kind::Ffi(..) => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        macro_rules! opt_len {
            ($content_length:expr) => {{
                let mut hint = SizeHint::default();

                if let Some(content_length) = $content_length.into_opt() {
                    hint.set_exact(content_length);
                }

                hint
            }};
        }

        match self.kind {
            Kind::Empty => SizeHint::with_exact(0),
            Kind::Chan { content_length, .. } => opt_len!(content_length),
            #[cfg(all(feature = "http2", any(feature = "client", feature = "server")))]
            Kind::H2 { content_length, .. } => opt_len!(content_length),
            #[cfg(feature = "ffi")]
            Kind::Ffi(..) => SizeHint::default(),
        }
    }
}

impl fmt::Debug for Recv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[derive(Debug)]
        struct Streaming;
        #[derive(Debug)]
        struct Empty;

        let mut builder = f.debug_tuple("Body");
        match self.kind {
            Kind::Empty => builder.field(&Empty),
            _ => builder.field(&Streaming),
        };

        builder.finish()
    }
}

impl Sender {
    /// Check to see if this `Sender` can send more data.
    pub(crate) fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        // Check if the receiver end has tried polling for the body yet
        ready!(self.poll_want(cx)?);
        self.data_tx
            .poll_ready(cx)
            .map_err(|_| crate::Error::new_closed())
    }

    fn poll_want(&mut self, cx: &mut task::Context<'_>) -> Poll<crate::Result<()>> {
        match self.want_rx.load(cx) {
            WANT_READY => Poll::Ready(Ok(())),
            WANT_PENDING => Poll::Pending,
            watch::CLOSED => Poll::Ready(Err(crate::Error::new_closed())),
            unexpected => unreachable!("want_rx value: {}", unexpected),
        }
    }

    async fn ready(&mut self) -> crate::Result<()> {
        futures_util::future::poll_fn(|cx| self.poll_ready(cx)).await
    }

    /// Send data on data channel when it is ready.
    #[allow(unused)]
    pub(crate) async fn send_data(&mut self, chunk: Bytes) -> crate::Result<()> {
        self.ready().await?;
        self.data_tx
            .try_send(Ok(chunk))
            .map_err(|_| crate::Error::new_closed())
    }

    /// Send trailers on trailers channel.
    #[allow(unused)]
    pub(crate) async fn send_trailers(&mut self, trailers: HeaderMap) -> crate::Result<()> {
        let tx = match self.trailers_tx.take() {
            Some(tx) => tx,
            None => return Err(crate::Error::new_closed()),
        };
        tx.send(trailers).map_err(|_| crate::Error::new_closed())
    }

    /// Try to send data on this channel.
    ///
    /// # Errors
    ///
    /// Returns `Err(Bytes)` if the channel could not (currently) accept
    /// another `Bytes`.
    ///
    /// # Note
    ///
    /// This is mostly useful for when trying to send from some other thread
    /// that doesn't have an async context. If in an async context, prefer
    /// `send_data()` instead.
    pub(crate) fn try_send_data(&mut self, chunk: Bytes) -> Result<(), Bytes> {
        self.data_tx
            .try_send(Ok(chunk))
            .map_err(|err| err.into_inner().expect("just sent Ok"))
    }

    /// Aborts the body in an abnormal fashion.
    #[allow(unused)]
    pub(crate) fn abort(self) {
        let _ = self
            .data_tx
            // clone so the send works even if buffer is full
            .clone()
            .try_send(Err(crate::Error::new_body_write_aborted()));
    }

    #[cfg(feature = "http1")]
    pub(crate) fn send_error(&mut self, err: crate::Error) {
        let _ = self.data_tx.try_send(Err(err));
    }
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[derive(Debug)]
        struct Open;
        #[derive(Debug)]
        struct Closed;

        let mut builder = f.debug_tuple("Sender");
        match self.want_rx.peek() {
            watch::CLOSED => builder.field(&Closed),
            _ => builder.field(&Open),
        };

        builder.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::mem;
    use std::task::Poll;

    use super::{Body, DecodedLength, Recv, Sender, SizeHint};

    #[test]
    fn test_size_of() {
        // These are mostly to help catch *accidentally* increasing
        // the size by too much.

        let body_size = mem::size_of::<Recv>();
        let body_expected_size = mem::size_of::<u64>() * 6;
        assert!(
            body_size <= body_expected_size,
            "Body size = {} <= {}",
            body_size,
            body_expected_size,
        );

        assert_eq!(body_size, mem::size_of::<Option<Recv>>(), "Option<Recv>");

        assert_eq!(
            mem::size_of::<Sender>(),
            mem::size_of::<usize>() * 5,
            "Sender"
        );

        assert_eq!(
            mem::size_of::<Sender>(),
            mem::size_of::<Option<Sender>>(),
            "Option<Sender>"
        );
    }

    #[test]
    fn size_hint() {
        fn eq(body: Recv, b: SizeHint, note: &str) {
            let a = body.size_hint();
            assert_eq!(a.lower(), b.lower(), "lower for {:?}", note);
            assert_eq!(a.upper(), b.upper(), "upper for {:?}", note);
        }

        eq(Recv::empty(), SizeHint::with_exact(0), "empty");

        eq(Recv::channel().1, SizeHint::new(), "channel");

        eq(
            Recv::new_channel(DecodedLength::new(4), /*wanter =*/ false).1,
            SizeHint::with_exact(4),
            "channel with length",
        );
    }

    #[cfg(not(miri))]
    #[tokio::test]
    async fn channel_abort() {
        let (tx, mut rx) = Recv::channel();

        tx.abort();

        let err = rx.data().await.unwrap().unwrap_err();
        assert!(err.is_body_write_aborted(), "{:?}", err);
    }

    #[cfg(not(miri))]
    #[tokio::test]
    async fn channel_abort_when_buffer_is_full() {
        let (mut tx, mut rx) = Recv::channel();

        tx.try_send_data("chunk 1".into()).expect("send 1");
        // buffer is full, but can still send abort
        tx.abort();

        let chunk1 = rx.data().await.expect("item 1").expect("chunk 1");
        assert_eq!(chunk1, "chunk 1");

        let err = rx.data().await.unwrap().unwrap_err();
        assert!(err.is_body_write_aborted(), "{:?}", err);
    }

    #[test]
    fn channel_buffers_one() {
        let (mut tx, _rx) = Recv::channel();

        tx.try_send_data("chunk 1".into()).expect("send 1");

        // buffer is now full
        let chunk2 = tx.try_send_data("chunk 2".into()).expect_err("send 2");
        assert_eq!(chunk2, "chunk 2");
    }

    #[cfg(not(miri))]
    #[tokio::test]
    async fn channel_empty() {
        let (_, mut rx) = Recv::channel();

        assert!(rx.data().await.is_none());
    }

    #[test]
    fn channel_ready() {
        let (mut tx, _rx) = Recv::new_channel(DecodedLength::CHUNKED, /*wanter = */ false);

        let mut tx_ready = tokio_test::task::spawn(tx.ready());

        assert!(tx_ready.poll().is_ready(), "tx is ready immediately");
    }

    #[test]
    fn channel_wanter() {
        let (mut tx, mut rx) = Recv::new_channel(DecodedLength::CHUNKED, /*wanter = */ true);

        let mut tx_ready = tokio_test::task::spawn(tx.ready());
        let mut rx_data = tokio_test::task::spawn(rx.data());

        assert!(
            tx_ready.poll().is_pending(),
            "tx isn't ready before rx has been polled"
        );

        assert!(rx_data.poll().is_pending(), "poll rx.data");
        assert!(tx_ready.is_woken(), "rx poll wakes tx");

        assert!(
            tx_ready.poll().is_ready(),
            "tx is ready after rx has been polled"
        );
    }

    #[test]
    fn channel_notices_closure() {
        let (mut tx, rx) = Recv::new_channel(DecodedLength::CHUNKED, /*wanter = */ true);

        let mut tx_ready = tokio_test::task::spawn(tx.ready());

        assert!(
            tx_ready.poll().is_pending(),
            "tx isn't ready before rx has been polled"
        );

        drop(rx);
        assert!(tx_ready.is_woken(), "dropping rx wakes tx");

        match tx_ready.poll() {
            Poll::Ready(Err(ref e)) if e.is_closed() => (),
            unexpected => panic!("tx poll ready unexpected: {:?}", unexpected),
        }
    }
}
