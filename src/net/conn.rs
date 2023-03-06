use crate::awareness::Awareness;
use crate::sync::{DefaultProtocol, Error, Message, MessageReader, Protocol, SyncMessage};
use futures_util::sink::SinkExt;
use futures_util::StreamExt;
use lib0::decoding::Cursor;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::spawn;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::Update;

/// Connection handler over a pair of message streams, which implements a Yjs/Yrs awareness and
/// update exchange protocol.
///
/// This connection implements Future pattern and can be awaited upon in order for a caller to
/// recognize whether underlying websocket connection has been finished gracefully or abruptly.
#[derive(Debug)]
pub struct ConnHandler<I, O> {
    processing_loop: JoinHandle<Result<(), Error>>,
    inbox: Arc<Mutex<O>>,
    _stream: PhantomData<I>,
}

impl<I, O, E> ConnHandler<I, O>
where
    I: StreamExt<Item = Result<Vec<u8>, E>> + Send + Sync + Unpin + 'static,
    O: SinkExt<Vec<u8>, Error = E> + Send + Sync + Unpin + 'static,
    E: Into<Error> + Send + Sync,
{
    /// Wraps incoming [WebSocket] connection and supplied [Awareness] accessor into a new
    /// connection handler capable of exchanging Yrs/Yjs messages.
    ///
    /// While creation of new [WarpConn] always succeeds, a connection itself can possibly fail
    /// while processing incoming input/output. This can be detected by awaiting for returned
    /// [WarpConn] and handling the awaited result.
    pub fn new(awareness: Arc<RwLock<Awareness>>, socket: (O, I)) -> Self {
        Self::with_protocol(awareness, socket, DefaultProtocol)
    }

    /// Wraps incoming [WebSocket] connection and supplied [Awareness] accessor into a new
    /// connection handler capable of exchanging Yrs/Yjs messages.
    ///
    /// While creation of new [WarpConn] always succeeds, a connection itself can possibly fail
    /// while processing incoming input/output. This can be detected by awaiting for returned
    /// [WarpConn] and handling the awaited result.
    pub fn with_protocol<P>(awareness: Arc<RwLock<Awareness>>, socket: (O, I), protocol: P) -> Self
    where
        P: Protocol + Send + Sync + 'static,
    {
        let (sink, mut source) = socket;
        let mut sink = Arc::new(Mutex::new(sink));
        let inbox = sink.clone();
        let processing_loop: JoinHandle<Result<(), Error>> = spawn(async move {
            // at the beginning send SyncStep1 and AwarenessUpdate
            let payload = {
                let mut encoder = EncoderV1::new();
                let awareness = awareness.read().await;
                protocol.start(&awareness, &mut encoder)?;
                encoder.to_vec()
            };
            if !payload.is_empty() {
                let mut s = sink.lock().await;
                if let Err(e) = s.send(payload).await {
                    return Err(e.into());
                }
            }

            while let Some(input) = source.next().await {
                match input {
                    Ok(data) => {
                        match Self::process(&protocol, &awareness, &mut sink, data).await {
                            Ok(()) => { /* continue */ }
                            Err(e) => {
                                return Err(e);
                            }
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            Ok(())
        });
        ConnHandler {
            processing_loop,
            inbox,
            _stream: PhantomData::default(),
        }
    }

    /// Returns a reference to a [ConnInbox] that can be used to send data through an underlying
    /// websocket connection.
    pub fn inbox(&self) -> &Arc<Mutex<O>> {
        &self.inbox
    }

    async fn process<P: Protocol>(
        protocol: &P,
        awareness: &Arc<RwLock<Awareness>>,
        sink: &mut Arc<Mutex<O>>,
        input: Vec<u8>,
    ) -> Result<(), Error> {
        let mut decoder = DecoderV1::new(Cursor::new(&input));
        let reader = MessageReader::new(&mut decoder);
        for r in reader {
            let msg = r?;
            if let Some(reply) = handle_msg(protocol, &awareness, msg).await? {
                let mut sender = sink.lock().await;
                if let Err(e) = sender.send(reply.encode_v1()).await {
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

impl<I, O> Unpin for ConnHandler<I, O> {}

impl<I, O> core::future::Future for ConnHandler<I, O> {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.processing_loop).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Ready(Ok(r)) => Poll::Ready(r),
        }
    }
}

async fn handle_msg<P: Protocol>(
    protocol: &P,
    a: &Arc<RwLock<Awareness>>,
    msg: Message,
) -> Result<Option<Message>, Error> {
    match msg {
        Message::Sync(msg) => match msg {
            SyncMessage::SyncStep1(sv) => {
                let awareness = a.read().await;
                protocol.handle_sync_step1(&awareness, sv)
            }
            SyncMessage::SyncStep2(update) => {
                let mut awareness = a.write().await;
                protocol.handle_sync_step2(&mut awareness, Update::decode_v1(&update)?)
            }
            SyncMessage::Update(update) => {
                let mut awareness = a.write().await;
                protocol.handle_update(&mut awareness, Update::decode_v1(&update)?)
            }
        },
        Message::Auth(reason) => {
            let awareness = a.read().await;
            protocol.handle_auth(&awareness, reason)
        }
        Message::AwarenessQuery => {
            let awareness = a.read().await;
            protocol.handle_awareness_query(&awareness)
        }
        Message::Awareness(update) => {
            let mut awareness = a.write().await;
            protocol.handle_awareness_update(&mut awareness, update)
        }
        Message::Custom(tag, data) => {
            let mut awareness = a.write().await;
            protocol.missing_handle(&mut awareness, tag, data)
        }
    }
}
