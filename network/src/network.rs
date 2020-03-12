// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::message_processor::{MessageFuture, MessageProcessor};
use crate::net::{build_network_service, SNetworkService};
use crate::sync_messages::{DownloadMessage, SyncMessage};
use crate::{
    GetCounterMessage, NetworkMessage, PeerEvent, PeerMessage, RPCMessage, RPCRequest, RPCResponse,
    RpcRequestMessage,
};
use actix::prelude::*;
use anyhow::Result;
use bus::{Broadcast, BusActor};
use config::{NetworkConfig, NodeConfig};
use crypto::ed25519::{Ed25519PrivateKey, Ed25519PublicKey};
use crypto::hash::HashValue;
use crypto::{test_utils::KeyPair, Uniform};
use futures::{
    channel::{mpsc, oneshot},
    stream::StreamExt,
    TryFutureExt,
};
use libp2p::{
    identity,
    ping::{Ping, PingConfig, PingEvent},
    PeerId, Swarm,
};
use scs::SCSCodec;
use std::sync::Arc;
use traits::TxPoolAsyncService;
use types::{account_address::AccountAddress, peer_info::PeerInfo};
use types::{system_events::SystemEvents, transaction::SignedUserTransaction};

use actix::fut::wrap_future;
use std::time::Duration;
use tokio::runtime::Handle;

#[derive(Clone)]
pub struct NetworkAsyncService<P>
where
    P: TxPoolAsyncService,
    P: 'static,
{
    addr: Addr<NetworkActor<P>>,
    message_processor: MessageProcessor<RPCResponse>,
    tx: mpsc::UnboundedSender<NetworkMessage>,
    peer_id: PeerId,
}

impl<P> NetworkAsyncService<P>
where
    P: TxPoolAsyncService,
    P: 'static,
{
    pub async fn send_peer_message(&self, peer_id: PeerId, msg: PeerMessage) -> Result<()> {
        let data = msg.encode().unwrap();
        let network_message = NetworkMessage {
            peer_id: peer_id.into(),
            data,
        };
        self.tx.unbounded_send(network_message)?;

        Ok(())
    }

    pub async fn broadcast_system_event(&self, event: SystemEvents) -> Result<()> {
        self.addr.send(event).await;
        Ok(())
    }

    pub async fn send_request(
        &self,
        peer_id: PeerId,
        message: RPCRequest,
        _time_out: Duration,
    ) -> Result<RPCResponse> {
        let request_id = message.get_id();
        let peer_msg = PeerMessage::RPCRequest(message);
        let data = peer_msg.encode().unwrap();
        let network_message = NetworkMessage {
            peer_id: peer_id.clone().into(),
            data,
        };
        self.tx.unbounded_send(network_message)?;
        let (tx, rx) = futures::channel::mpsc::channel(1);
        let message_future = MessageFuture::new(rx);
        self.message_processor.add_future(request_id, tx).await;
        info!("send request to {}", peer_id);
        message_future.await
    }

    pub async fn response_for(
        &self,
        peer_id: PeerId,
        id: HashValue,
        mut response: RPCResponse,
    ) -> Result<()> {
        response.set_request_id(id);
        let peer_msg = PeerMessage::RPCResponse(response);
        let data = peer_msg.encode().unwrap();
        let network_message = NetworkMessage {
            peer_id: peer_id.into(),
            data,
        };
        self.tx.unbounded_send(network_message)?;
        Ok(())
    }

    pub fn identify(&self) -> &PeerId {
        &self.peer_id
    }
}

pub struct NetworkActor<P>
where
    P: TxPoolAsyncService,
    P: 'static,
{
    network_service: SNetworkService,
    tx: mpsc::UnboundedSender<NetworkMessage>,
    tx_command: mpsc::UnboundedSender<()>,
    bus: Addr<BusActor>,
    txpool: P,
    message_processor: MessageProcessor<RPCResponse>,
}

impl<P> NetworkActor<P>
where
    P: TxPoolAsyncService,
{
    pub fn launch(
        node_config: Arc<NodeConfig>,
        bus: Addr<BusActor>,
        txpool: P,
        handle: Handle,
    ) -> NetworkAsyncService<P> {
        let (service, tx, rx, event_rx, tx_command) =
            build_network_service(&node_config.network, handle);
        info!(
            "network started at {} with seed {},network address is {}",
            &node_config.network.listen,
            &node_config
                .network
                .seeds
                .iter()
                .fold(String::new(), |acc, arg| acc + arg.as_str()),
            service.identify()
        );
        let message_processor = MessageProcessor::new();
        let message_processor_clone = message_processor.clone();
        let tx_clone = tx.clone();
        let peer_id = service.identify().clone();
        let addr = NetworkActor::create(move |ctx: &mut Context<NetworkActor<P>>| {
            ctx.add_stream(rx.fuse());
            ctx.add_stream(event_rx.fuse());
            NetworkActor {
                network_service: service,
                tx: tx_clone,
                tx_command,
                bus,
                txpool,
                message_processor: message_processor_clone,
            }
        });
        NetworkAsyncService {
            addr,
            message_processor,
            tx,
            peer_id,
        }
    }
}

impl<P> Actor for NetworkActor<P>
where
    P: TxPoolAsyncService,
{
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!("Network actor started ",);
    }
}

impl<P> StreamHandler<NetworkMessage> for NetworkActor<P>
where
    P: TxPoolAsyncService,
{
    fn handle(&mut self, network_msg: NetworkMessage, ctx: &mut Self::Context) {
        info!("receive network_message {:?}", network_msg);
        let message = PeerMessage::decode(&network_msg.data);
        match message {
            Ok(msg) => {
                self.handle_network_message(network_msg.peer_id.into(), msg, ctx);
            }
            Err(e) => {
                warn!("get error {:?}", e);
            }
        }
    }
}

impl<P> StreamHandler<PeerEvent> for NetworkActor<P>
where
    P: TxPoolAsyncService,
{
    fn handle(&mut self, event: PeerEvent, ctx: &mut Self::Context) {
        info!("event is {:?}", event);
        let bus = self.bus.clone();
        let f = async move {
            bus.send(Broadcast { msg: event }).await;
            info!("already broadcast event");
        };
        let f = actix::fut::wrap_future(f);
        ctx.spawn(Box::new(f));
    }
}

impl<P> NetworkActor<P>
where
    P: TxPoolAsyncService,
{
    fn handle_network_message(&self, peer_id: PeerId, msg: PeerMessage, ctx: &mut Context<Self>) {
        match msg {
            PeerMessage::UserTransaction(txn) => {
                let txpool = self.txpool.clone();
                let f = async move {
                    let new_txn = txpool.add(txn).await.unwrap();
                    info!("add tx success, is new tx: {}", new_txn);
                };
                let f = actix::fut::wrap_future(f);
                ctx.spawn(Box::new(f));
            }
            PeerMessage::Block(block) => {
                let bus = self.bus.clone();
                let peer_info = PeerInfo::new(peer_id.into());
                let f = async move {
                    bus.send(Broadcast {
                        msg: SyncMessage::DownloadMessage(DownloadMessage::NewHeadBlock(
                            peer_info, block,
                        )),
                    })
                    .await;
                };
                let f = actix::fut::wrap_future(f);
                ctx.spawn(Box::new(f));
            }
            PeerMessage::LatestStateMsg(state) => {
                info!("broadcast LatestStateMsg.");
                let bus = self.bus.clone();
                let peer_info = PeerInfo::new(peer_id.into());
                let f = async move {
                    bus.send(Broadcast {
                        msg: SyncMessage::DownloadMessage(DownloadMessage::LatestStateMsg(
                            peer_info, state,
                        )),
                    })
                    .await;
                };
                let f = actix::fut::wrap_future(f);
                ctx.spawn(Box::new(f));
            }
            PeerMessage::RPCRequest(request) => {
                info!("do request.");
                let bus = self.bus.clone();
                let f = async move {
                    bus.send(Broadcast {
                        msg: RpcRequestMessage {
                            peer_id: peer_id.into(),
                            request,
                        },
                    })
                    .await;
                    info!("receive rpc request");
                };
                let f = actix::fut::wrap_future(f);
                ctx.spawn(Box::new(f));
            }
            PeerMessage::RPCResponse(response) => {
                info!("do response.");
                let message_processor = self.message_processor.clone();
                let f = async move {
                    let id = response.get_id();
                    message_processor.send_response(id, response).await.unwrap();
                };
                let f = actix::fut::wrap_future(f);
                ctx.spawn(Box::new(f));
            }
        }
    }
}

/// handler system events.
impl<P> Handler<SystemEvents> for NetworkActor<P>
where
    P: TxPoolAsyncService,
{
    type Result = ();

    fn handle(&mut self, msg: SystemEvents, _ctx: &mut Self::Context) -> Self::Result {
        let peer_msg = match msg {
            SystemEvents::NewUserTransaction(txn) => {
                info!("new user transaction {:?}", txn);
                Some(PeerMessage::UserTransaction(txn))
            }
            SystemEvents::NewHeadBlock(block) => {
                info!("broadcast a new block {:?}", block.header().id());
                Some(PeerMessage::Block(block))
            }
            _ => None,
        };

        if let Some(msg) = peer_msg {
            let bytes = msg.encode().unwrap();
            self.network_service.broadcast_message(bytes);
        };

        ()
    }
}

// /// Handler for receive broadcast from other peer.
// impl<P> Handler<PeerMessage> for NetworkActor<P>
// where
//     P: TxPoolAsyncService,
// {
//     type Result = ResponseActFuture<Self, Result<()>>;
//
//     fn handle(&mut self, msg: PeerMessage, _ctx: &mut Self::Context) -> Self::Result {
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RpcRequestMessage, TestRequest, TestResponse};
    use bus::Subscription;
    use futures::future::IntoFuture;
    use futures_timer::Delay;
    use log::{Level, Metadata, Record};
    use log::{LevelFilter, SetLoggerError};
    use logger::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    use tokio::runtime::{Handle, Runtime};
    use traits::mock::MockTxPoolService;
    use types::account_address::AccountAddress;

    // #[actix_rt::test]
    // async fn test_network() {
    //     let node_config = NodeConfig::default();
    //     let bus = BusActor::launch();
    //     let txpool = TxPoolActor::launch(&node_config, bus.clone()).unwrap();
    //     let network = NetworkActor::launch(&node_config, bus, txpool.clone()).unwrap();
    //     network
    //         .send(PeerMessage::UserTransaction(SignedUserTransaction::mock()))
    //         .await
    //         .unwrap();
    //
    //     let txns = txpool.get_pending_txns().await.unwrap();
    //     assert_eq!(1, txns.len());
    // }

    #[test]
    fn test_network_with_mock() {
        use std::thread;
        use std::time::Duration;

        ::logger::init_for_test();
        let mut system = System::new("test");
        let mut rt = Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let mut node_config1 = NodeConfig::random_for_test();
        node_config1.network.listen =
            format!("/ip4/127.0.0.1/tcp/{}", config::get_available_port());
        let node_config1 = Arc::new(node_config1);

        let (txpool1, network1, addr1, bus1) = build_network(node_config1.clone(), handle.clone());

        thread::sleep(Duration::from_secs(1));

        let mut node_config2 = NodeConfig::random_for_test();
        let addr1_hex = network1.peer_id.to_base58();
        let seed = format!("{}/p2p/{}", &node_config1.network.listen, addr1_hex);
        node_config2.network.listen =
            format!("/ip4/127.0.0.1/tcp/{}", config::get_available_port());
        node_config2.network.seeds = vec![seed];
        let node_config2 = Arc::new(node_config2);

        let (txpool2, network2, addr2, bus2) = build_network(node_config2.clone(), handle.clone());

        thread::sleep(Duration::from_secs(1));

        Arbiter::spawn(async move {
            network1
                .broadcast_system_event(SystemEvents::NewUserTransaction(
                    SignedUserTransaction::mock(),
                ))
                .await;
            network2
                .broadcast_system_event(SystemEvents::NewUserTransaction(
                    SignedUserTransaction::mock(),
                ))
                .await;

            let network_clone2 = network2.clone();

            let response_actor = TestResponseActor::create(network_clone2);
            let addr = response_actor.start();

            let recipient = addr.clone().recipient::<RpcRequestMessage>();
            bus2.send(Subscription { recipient }).await;

            let recipient = addr.recipient::<PeerEvent>();
            bus2.send(Subscription { recipient }).await;

            _delay(Duration::from_millis(100)).await;

            let txns = txpool1.get_pending_txns(None).await.unwrap();
            //assert_eq!(1, txns.len());

            let txns = txpool2.get_pending_txns(None).await.unwrap();
            //assert_eq!(1, txns.len());

            let request = RPCRequest::TestRequest(TestRequest {
                data: HashValue::random(),
            });
            info!("req :{:?}", request);
            let resp = network1
                .send_request(network2.identify().clone(), request, Duration::from_secs(1))
                .await;
            info!("resp :{:?}", resp);

            _delay(Duration::from_millis(100)).await;

            System::current().stop();
            ()
        });

        system.run();
    }

    async fn _delay(duration: Duration) {
        Delay::new(duration).await;
    }

    fn build_network(
        node_config: Arc<NodeConfig>,
        handle: Handle,
    ) -> (
        MockTxPoolService,
        NetworkAsyncService<MockTxPoolService>,
        AccountAddress,
        Addr<BusActor>,
    ) {
        let bus = BusActor::launch();
        let addr =
            AccountAddress::from_public_key(&node_config.network.network_keypair().public_key);
        let txpool = traits::mock::MockTxPoolService::new();
        let network =
            NetworkActor::launch(node_config.clone(), bus.clone(), txpool.clone(), handle);
        (txpool, network, addr, bus)
    }

    struct TestResponseActor {
        network_service: NetworkAsyncService<MockTxPoolService>,
    }

    impl TestResponseActor {
        fn create(network_service: NetworkAsyncService<MockTxPoolService>) -> TestResponseActor {
            let instance = Self { network_service };
            instance
        }
    }

    impl Actor for TestResponseActor {
        type Context = Context<Self>;

        fn started(&mut self, _ctx: &mut Self::Context) {
            info!("Test actor started ",);
        }
    }

    impl Handler<RpcRequestMessage> for TestResponseActor {
        type Result = Result<()>;

        fn handle(&mut self, msg: RpcRequestMessage, ctx: &mut Self::Context) -> Self::Result {
            let id = (&msg.request).get_id();
            let peer_id = (&msg).peer_id.clone();
            match msg.request {
                RPCRequest::TestRequest(_r) => {
                    info!("request is {:?}", _r);
                    let response = TestResponse {
                        len: 1,
                        id: id.clone(),
                    };
                    let network_service = self.network_service.clone();
                    let f = async move {
                        network_service
                            .response_for(peer_id.into(), id, RPCResponse::TestResponse(response))
                            .await;
                    };
                    let f = actix::fut::wrap_future(f);
                    ctx.spawn(Box::new(f));
                    Ok(())
                }
                _ => Ok(()),
            }
        }
    }

    impl Handler<PeerEvent> for TestResponseActor {
        type Result = Result<()>;

        fn handle(&mut self, msg: PeerEvent, ctx: &mut Self::Context) -> Self::Result {
            info!("Event is {:?}", msg);
            Ok(())
        }
    }
}
