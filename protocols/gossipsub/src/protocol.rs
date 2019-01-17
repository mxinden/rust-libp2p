// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::rpc_proto;
use bytes::{BufMut, BytesMut};
use futures::future;
use libp2p_core::{InboundUpgrade, OutboundUpgrade, PeerId, UpgradeInfo};
use libp2p_floodsub::TopicHash;
use protobuf::Message as ProtobufMessage;
use std::{io, iter};
use tokio_codec::{Decoder, Encoder, Framed};
use tokio_io::{AsyncRead, AsyncWrite};
use unsigned_varint::codec;

/// Implementation of the `ConnectionUpgrade` for the Gossipsub protocol.
#[derive(Debug, Clone)]
pub struct ProtocolConfig {}

impl ProtocolConfig {
    /// Builds a new `ProtocolConfig`.
    #[inline]
    pub fn new() -> ProtocolConfig {
        ProtocolConfig {}
    }
}

impl UpgradeInfo for ProtocolConfig {
    type Info = &'static [u8];
    type InfoIter = iter::Once<Self::Info>;

    #[inline]
    fn protocol_info(&self) -> Self::InfoIter {
        iter::once(b"/meshsub/1.0.0")
    }
}

impl<TSocket> InboundUpgrade<TSocket> for ProtocolConfig
where
    TSocket: AsyncRead + AsyncWrite,
{
    type Output = Framed<TSocket, GossipsubCodec>;
    type Error = io::Error;
    type Future = future::FutureResult<Self::Output, Self::Error>;

    #[inline]
    fn upgrade_inbound(self, socket: TSocket, _: Self::Info) -> Self::Future {
        future::ok(Framed::new(
            socket,
            GossipsubCodec {
                length_prefix: Default::default(),
            },
        ))
    }
}

impl<TSocket> OutboundUpgrade<TSocket> for ProtocolConfig
where
    TSocket: AsyncRead + AsyncWrite,
{
    type Output = Framed<TSocket, GossipsubCodec>;
    type Error = io::Error;
    type Future = future::FutureResult<Self::Output, Self::Error>;

    #[inline]
    fn upgrade_outbound(self, socket: TSocket, _: Self::Info) -> Self::Future {
        future::ok(Framed::new(
            socket,
            GossipsubCodec {
                length_prefix: Default::default(),
            },
        ))
    }
}

/// Implementation of `tokio_codec::Codec`.
pub struct GossipsubCodec {
    /// The codec for encoding/decoding the length prefix of messages.
    length_prefix: codec::UviBytes,
}

impl Encoder for GossipsubCodec {
    type Item = GossipsubRpc;
    type Error = io::Error;

    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut proto = rpc_proto::RPC::new();

        for message in item.messages.into_iter() {
            let mut msg = rpc_proto::Message::new();
            msg.set_from(message.source.into_bytes());
            msg.set_data(message.data);
            msg.set_seqno(message.sequence_number);
            msg.set_topicIDs(
                message
                    .topics
                    .into_iter()
                    .map(TopicHash::into_string)
                    .collect(),
            );
            proto.mut_publish().push(msg);
        }

        for topic in item.subscriptions.into_iter() {
            let mut subscription = rpc_proto::RPC_SubOpts::new();
            subscription.set_subscribe(topic.action == GossipsubSubscriptionAction::Subscribe);
            subscription.set_topicid(topic.topic.into_string());
            proto.mut_subscriptions().push(subscription);
        }

        let msg_size = proto.compute_size();
        // Reserve enough space for the data and the length. The length has a maximum of 32 bits,
        // which means that 5 bytes is enough for the variable-length integer.
        dst.reserve(msg_size as usize + 5);

        proto
            .write_length_delimited_to_writer(&mut dst.by_ref().writer())
            .expect(
                "there is no situation in which the protobuf message can be invalid, and \
                 writing to a BytesMut never fails as we reserved enough space beforehand",
            );
        Ok(())
    }
}

impl Decoder for GossipsubCodec {
    type Item = GossipsubRpc;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let packet = match self.length_prefix.decode(src)? {
            Some(p) => p,
            None => return Ok(None),
        };

        let mut rpc: rpc_proto::RPC = protobuf::parse_from_bytes(&packet)?;

        let mut messages = Vec::with_capacity(rpc.get_publish().len());
        for mut publish in rpc.take_publish().into_iter() {
            messages.push(GossipsubMessage {
                source: PeerId::from_bytes(publish.take_from()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid peer ID in message")
                })?,
                data: publish.take_data(),
                sequence_number: publish.take_seqno(),
                topics: publish
                    .take_topicIDs()
                    .into_iter()
                    .map(|topic| TopicHash::from_raw(topic))
                    .collect(),
            });
        }

        Ok(Some(GossipsubRpc {
            messages,
            subscriptions: rpc
                .take_subscriptions()
                .into_iter()
                .map(|mut sub| GossipsubSubscription {
                    action: if sub.get_subscribe() {
                        GossipsubSubscriptionAction::Subscribe
                    } else {
                        GossipsubSubscriptionAction::Unsubscribe
                    },
                    topic: TopicHash::from_raw(sub.take_topicid()),
                })
                .collect(),
        }))
    }
}

/// An RPC received by the gossipsub system.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GossipsubRpc {
    /// List of messages that were part of this RPC query.
    pub messages: Vec<GossipsubMessage>,
    /// List of subscriptions.
    pub subscriptions: Vec<GossipsubSubscription>,
}

/// A message received by the gossipsub system.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GossipsubMessage {
    /// Id of the peer that published this message.
    pub source: PeerId,

    /// Content of the message. Its meaning is out of scope of this library.
    pub data: Vec<u8>,

    /// An incrementing sequence number.
    pub sequence_number: Vec<u8>,

    /// List of topics this message belongs to.
    ///
    /// Each message can belong to multiple topics at once.
    pub topics: Vec<TopicHash>,
}

/// A subscription received by the gossipsub system.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GossipsubSubscription {
    /// Action to perform.
    pub action: GossipsubSubscriptionAction,
    /// The topic from which to subscribe or unsubscribe.
    pub topic: TopicHash,
}

/// Action that a subscription wants to perform.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GossipsubSubscriptionAction {
    /// The remote wants to subscribe to the given topic.
    Subscribe,
    /// The remote wants to unsubscribe from the given topic.
    Unsubscribe,
    /*
    /// The remote has data
    iHave,
    /// iWant control action
    iWant,
    /// Graft control action
    graft,
    /// Prune control action
    prune,
    */
}
