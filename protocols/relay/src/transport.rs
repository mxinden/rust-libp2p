use crate::behaviour::BehaviourToTransportMsg;

use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::channel::mpsc;
use futures::future::{BoxFuture, Future, FutureExt, Ready};
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::Stream;
use futures::channel::oneshot;
use futures::sink::SinkExt;
use libp2p_core::{
    either::{EitherError, EitherFuture, EitherListenStream, EitherOutput},
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    Transport,
};

pub enum TransportToBehaviourMsg {
    DialRequest(Multiaddr, oneshot::Sender<RelayConnection>),
    ListenRequest(Multiaddr),
}

#[derive(Clone)]
pub struct RelayTransportWrapper<T: Clone> {
    to_behaviour: mpsc::Sender<TransportToBehaviourMsg>,
    // TODO: Can we get around the arc mutex?
    from_behaviour: Arc<Mutex<mpsc::Receiver<BehaviourToTransportMsg>>>,

    inner_transport: T,
}

impl<T: Clone> RelayTransportWrapper<T> {
    pub fn new(
        t: T,
    ) -> (
        Self,
        (
            mpsc::Sender<BehaviourToTransportMsg>,
            mpsc::Receiver<TransportToBehaviourMsg>,
        ),
    ) {
        let (to_behaviour, from_transport) = mpsc::channel(0);
        let (to_transport, from_behaviour) = mpsc::channel(0);

        let transport = RelayTransportWrapper {
            to_behaviour,
            from_behaviour: Arc::new(Mutex::new(from_behaviour)),

            inner_transport: t,
        };

        (transport, (to_transport, from_transport))
    }
}

impl<T: Transport + Clone> Transport for RelayTransportWrapper<T> {
    type Output = EitherOutput<<T as Transport>::Output, RelayConnection>;
    type Error = EitherError<<T as Transport>::Error, RelayError>;
    type Listener = RelayListener<T>;
    type ListenerUpgrade = RelayedListenerUpgrade<T>;
    type Dial = EitherFuture<<T as Transport>::Dial, RelayedDial>;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        if !is_relay_address(&addr) {
            let inner_listener = match self.inner_transport.listen_on(addr) {
                Ok(listener) => listener,
                Err(TransportError::MultiaddrNotSupported(addr)) => {
                    return Err(TransportError::MultiaddrNotSupported(addr))
                }
                Err(TransportError::Other(err)) => {
                    return Err(TransportError::Other(EitherError::A(err)))
                }
            };
            return Ok(RelayListener {
                inner_listener: Some(inner_listener),
                // TODO: Do we want a listener for inner incoming connections to also yield relayed
                // connections?
                from_behaviour: self.from_behaviour.clone(),
                msg_to_behaviour: None,
            });
        }


        let mut to_behaviour = self.to_behaviour.clone();
        let msg_to_behaviour = Some(async move {
            to_behaviour.send(TransportToBehaviourMsg::ListenRequest(addr)).await.unwrap();
        }.boxed());

        Ok(RelayListener {
            inner_listener: None,
            from_behaviour: self.from_behaviour.clone(),
            msg_to_behaviour,
        })
    }

    fn dial(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        if !is_relay_address(&addr) {
            match self.inner_transport.dial(addr) {
                Ok(dialer) => return Ok(EitherFuture::First(dialer)),
                Err(TransportError::MultiaddrNotSupported(addr)) => {
                    return Err(TransportError::MultiaddrNotSupported(addr))
                }
                Err(TransportError::Other(err)) => {
                    return Err(TransportError::Other(EitherError::A(err)))
                }
            };
        }

        let mut to_behaviour = self.to_behaviour.clone();
        Ok(EitherFuture::Second(async move {
            let (tx, rx) = oneshot::channel();
            to_behaviour.send(TransportToBehaviourMsg::DialRequest(addr, tx)).await.unwrap();
            Ok(rx.await.unwrap())
        }.boxed()))
    }
}

fn is_relay_address(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

pub struct RelayListener<T: Transport> {
    inner_listener: Option<<T as Transport>::Listener>,
    from_behaviour: Arc<Mutex<mpsc::Receiver<BehaviourToTransportMsg>>>,

    msg_to_behaviour: Option<BoxFuture<'static, ()>>,
}

impl<T: Transport> Stream for RelayListener<T> {
    type Item = Result<
        ListenerEvent<RelayedListenerUpgrade<T>, EitherError<<T as Transport>::Error, RelayError>>,
        EitherError<<T as Transport>::Error, RelayError>,
    >;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        unimplemented!();
    }
}

pub type RelayedDial = BoxFuture<'static, Result<RelayConnection, RelayError>>;

pub struct RelayedListenerUpgrade<T> {
    marker: PhantomData<T>,
}

impl<T: Transport> Future for RelayedListenerUpgrade<T> {
    type Output = Result<
        EitherOutput<<T as Transport>::Output, RelayConnection>,
        EitherError<<T as Transport>::Error, RelayError>,
    >;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unimplemented!();
    }
}

#[derive(Debug)]
pub struct RelayError {
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unimplemented!();
    }
}

impl std::error::Error for RelayError {}

pub struct RelayConnection {}

impl AsyncRead for RelayConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unimplemented!();
    }
}

impl AsyncWrite for RelayConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        unimplemented!();
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        unimplemented!();
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        unimplemented!();
    }
}
