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

#![allow(unreachable_code)]

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

    panic!("network error");
    Ok(())
}

async fn run_network() -> Result<(), Box<dyn Error>> {

    let mut node = DandelionNode::new(
        0.3,  // fluff probability
        Duration::from_secs(30), // stem timeout
    ).await?;

    // Create a Gossipsub topic and subscribe to our own topic
    let broadcast_topic = gossipsub::IdentTopic::new(format!("stem-{}", node.swarm.local_peer_id()));
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
        // 4. Re-initialize peers
        node.set_all_peers();

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
                    // TODO: randomize peer on initial broadcast
                    let topic = node.random_topic();
                    if let Err(e) = node.broadcast_message(msg.as_bytes().to_vec(), topic) {
                        log::error!("Publish error: {e:?}");
                    }
                }
            }
            // broadcast event handling
            broadcast_event = node.swarm.select_next_some() => match broadcast_event {
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
                        log::info!(
                            "Got message: '{}' with id: {id} from peer: {peer_id}",
                            String::from_utf8_lossy(&message.data),
                        );
                        // Process only if message not seen before
                        if !node.dandelion.pending_messages.contains_key(&id.clone()) {
                            let pending_msg = PendingMessage {
                                content: message.data.clone(),
                                received_at: tokio::time::Instant::now(),
                                source: Some(peer_id),
                                relayed: false,
                            };
                            node.dandelion.pending_messages.insert(id.clone(), pending_msg);
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

