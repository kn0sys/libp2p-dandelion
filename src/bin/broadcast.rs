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


use std::{
    error::Error,
    time::Duration,
};

use futures::stream::StreamExt;
use libp2p::{gossipsub, swarm::SwarmEvent};
use tokio::{io, select, io::AsyncBufReadExt};
use libp2p_dandelion::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize logging
    env_logger::builder().init();

    tokio::spawn(async move {
        run_network().await.unwrap();
    });

    // run the network forever
    loop {}
    Ok(())
}

async fn run_network() -> Result<(), Box<dyn Error>> {

    let mut node = DandelionNode::new(
        0.3,  // fluff probability
        Duration::from_secs(30), // stem timeout
    ).await?;

    // Create a Gossipsub topic and subscribe
    let broadcast_topic = gossipsub::IdentTopic::new("dandelion");
    node.subscribe(&broadcast_topic).unwrap();

    // Read from standard input for chat
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    // Kick it off
    loop {

        // 1. Write pending message to LMDB
        node.process_pending_messages();
        // 2. Process for fluff or stem
        node.transition_to_fluff();
        // 3. Validate and propagate
        node.validate_message();

        // broadcast
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                if line.starts_with("add peer ") {
                    let address = &line.split("add peer ").collect::<Vec<&str>>().join("");
                    log::info!("adding peer: {}", address);
                    let ma = address.parse::<libp2p::Multiaddr>().unwrap();
                    node.connect(ma).await?;
                } else if line.starts_with("send ") {
                    let msg = &line.split("send ").collect::<Vec<&str>>().join("");
                    log::info!("sending message: {}", msg);
                     if let Err(e) = node.broadcast_message(msg.as_bytes().to_vec(), broadcast_topic.clone()) {
                        log::error!("Publish error: {e:?}");
                    }
                }
               
            }
            // broadcast event handling
            broadcast_event = node.swarm.select_next_some() => match broadcast_event {

                // TODO: remove automatic peer discovery

                // SwarmEvent::Behaviour(DandelionBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                //     for (peer_id, _multiaddr) in list {
                //         println!("mDNS discovered a new peer: {peer_id}");
                //         // swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                //         node.connect(_multiaddr).await?;
                //     }
                // },
                // SwarmEvent::Behaviour(DandelionBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                //     for (peer_id, _multiaddr) in list {
                //         println!("mDNS discover peer has expired: {peer_id}");
                //         //swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                //         node.disconnect(peer_id).await?;
                //     }
                // },
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::info!("Local node is listening on {address}/p2p/{}", node.swarm.local_peer_id());
                },
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    log::info!("Connected to peer: {:?}", peer_id);
            },
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                log::info!("Disconnected from peer: {:?}", peer_id);
                node.handle_peer_disconnect(peer_id).await?;
            },
            SwarmEvent::Behaviour(event) => {
                match event {
                    DandelionBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                    }) => {
                        println!(
                            "Got message: '{}' with id: {id} from peer: {peer_id}",
                            String::from_utf8_lossy(&message.data),
                        );
                        let msg_id = gossipsub::MessageId(message.data.clone());
                        // Process only if message not seen before
                        if !node.dandelion.pending_messages.contains_key(&msg_id) {
                            let pending_msg = PendingMessage {
                                content: message.data.clone(),
                                received_at: tokio::time::Instant::now(),
                                source: Some(peer_id),
                                relayed: false,
                            };
                            node.dandelion.pending_messages.insert(msg_id.clone(), pending_msg);
                        }
                    },
                    _=> {}
                };

            },
                _ => {}
            }
        }

    } // end network loop

    Ok(())
}

