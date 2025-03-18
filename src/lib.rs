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

//! libp2p_dandelion is a modified Dandelion implementation for enhanced privacy
//! of p2p message broadcasting.
//!
//! ## Example
//!
//! see https://github.com/kn0sys/libp2p-dandelion/blob/main/src/bin/broadcast.rs
use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
};

use futures::stream::StreamExt;
use libp2p::{
    Multiaddr,
    identity,
    PeerId,
    Swarm,
    gossipsub, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use tokio::io;
use rand::{seq::*, rng, Rng};
use serde::{Deserialize, Serialize};
mod database;
use crate::database::*;
use bincode::{Encode, Decode, config};

/// Pending Message Cache serializtion for passing messages to fluff propagation
#[derive(Deserialize, Serialize, Debug)]
struct PendingMessageCache {
    /// bytes representation of the message to broadcast 
    content: Vec<u8>,
    /// unix timestamp of message reception in bytes
    received_at: Vec<u8>,
    /// bytes of the `PeerId` of the message
    source: Vec<u8>,
    /// status of relay
    relayed: bool,
}

/// Bincode V2 PendingMessageCache
#[derive(Encode, Decode, Debug)]
struct PrePendingMessageCache {
    /// Bincode V1 PendingMessageCache
    #[bincode(with_serde)]
    pub serde: PendingMessageCache,
}

/// Necessary ordering for state distinctions
#[derive(Clone)]
pub struct PendingMessage {
    /// bytes representation of the message to broadcast
    pub content: Vec<u8>,
    /// time of receipt
    pub received_at: tokio::time::Instant,
    /// source of message
    pub source: Option<PeerId>,
    /// status of relay
    pub relayed: bool,
}

/// Peer queue of peers for fluff propagation
#[derive(Debug, Default, Deserialize, Serialize)]
struct MessageCache {
    /// Vector of PeerIds in their respective bytes representation
    peers: Vec<Vec<u8>>,
    /// Message Id as bytes
    msg_id: Vec<u8>,
    /// see `struct FluffTransitionMessage`
    fluff_msg: FluffTransitionMessage,
    /// flag for messages selected for fluff transition
    is_fluff: bool,
}

/// Bincode V2 MessageCache
#[derive(Encode, Decode, Debug)]
pub struct PreMessageCache {
    /// Bincode V1 MessageCache
    #[bincode(with_serde)]
    serde: MessageCache,
}


/// Gossipsub message for transitioning to fluff phase
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FluffTransitionMessage {
    /// masked source of the message
    source: Option<usize>,
    /// message content in bytes
    data: Vec<u8>,
    /// randomized sequence number for the message
    sequence_number: f64,
}

/// Necessary structural relationship for depth, persistence and directions
#[derive(Clone)]
pub struct Dandelion {
    /// probabilistic parameter for fluff activation
    fluff_probability: f64,
    /// TODO: randomized stem timeout values
    /// amount of time to delay before allowing stem activation
    stem_timeout: Duration,
    /// data structure containing messages to propagate
    pub pending_messages: HashMap<gossipsub::MessageId, PendingMessage>,
}

/// Dandelion behavior uses gossipsub.
#[derive(NetworkBehaviour)]
pub struct DandelionBehaviour {
    /// libp2p gossipsub
    gossipsub: gossipsub::Behaviour,
    /// see libp2p mdns, TODO: remove this if not needed
    mdns: mdns::tokio::Behaviour,
}

/// Main node implementation
pub struct DandelionNode {
    /// see libp2p swarm
    pub swarm: Swarm<DandelionBehaviour>,
    /// see `struct Dandelion`
    pub dandelion: Dandelion,
    /// list of currently connected peers
    peers: Vec<PeerId>
}

impl DandelionNode {
    /// Create a new Dandelion instance.
    pub async fn new(
        fluff_probability: f64,
        stem_timeout: Duration,
    ) -> Result<Self, Box<dyn Error>> {
        log::info!("creating new dandelion node with probability: {:?}", fluff_probability);
        let local_key = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(local_key.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
             // To content-address message, we can take the hash of message and use it as an ID.
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };
            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validate_messages()
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn)
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;
            let g_behaviour = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
                )?;
            let m_behaviour =
                mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
            Ok( DandelionBehaviour { gossipsub: g_behaviour, mdns: m_behaviour })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();
        // Framework-derived listener configuration
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
        let dandelion = Dandelion {
            fluff_probability,
            stem_timeout,
            pending_messages: HashMap::new()
        };
        let peers = Vec::new();
        Ok(Self {
            swarm,
            dandelion,
            peers,
        })
    }
    /// Report message validation for propagation
    pub fn validate_message(&mut self) {
        let msg_key: Vec<u8> = Vec::from(VALIDATE_MSG.as_bytes());
        let db: &DatabaseEnvironment = &DATABASE_LOCK;
        let p_msg_bytes = DatabaseEnvironment::read(&db.env, &db.handle, &msg_key).unwrap_or_default();
        let result: Vec<PreMessageCache> =
            bincode::decode_from_slice(&p_msg_bytes[..], config::standard()).unwrap_or_default().0; 
        log::info!("validating {} messages for propagation", result.len());
        log::debug!("processing cache: {:?}", result);
        for m in result {
            for p in m.serde.peers {
                let msg_id = gossipsub::MessageId::new(&m.serde.msg_id);
                if !self.swarm.behaviour_mut().gossipsub.report_message_validation_result(
                    &msg_id,
                    &PeerId::from_bytes(&p).unwrap(),
                    libp2p::gossipsub::MessageAcceptance::Accept) {
                        log::error!("Failed to validate message");
                } else {
                    let topic = gossipsub::IdentTopic::new(format!("stem-{}", PeerId::from_bytes(&p).unwrap()));
                    self.broadcast_message(m.serde.fluff_msg.data.clone(), topic).unwrap();
                }
            }
        }
    }
    /// Clean up for messages related to a disconnected peer
    pub async fn handle_peer_disconnect(&mut self, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
        log::info!("handle_peer_disconnect for {:?}", &peer_id);
        // Clean up any pending messages related to this peer
        self.dandelion.pending_messages.retain(|_, msg| {
            msg.source != Some(peer_id)
        });
        Ok(())
    }
    /// Same as swarm.listeners()
    pub fn listen_addresses(&self) -> impl Iterator<Item = &Multiaddr> {
        // Get all active listening addresses from the swarm
        self.swarm.listeners()
    }
    /// Helper method for fetching all peers
    pub fn set_all_peers(&mut self) {
        let all_peers = self.swarm.behaviour_mut().gossipsub.all_peers().collect::<Vec<_>>();
        self.peers = all_peers.into_iter()
            .map(|x| PeerId::from_bytes(&x.0.to_bytes()).unwrap()).collect::<Vec<_>>();
    }
    /// Helper method for selecting a random peer based on peer subscription
    pub fn random_topic(&mut self) -> gossipsub::IdentTopic {
        log::info!("selecting random topic");
        // choose a random topic hash get all peer and match it
        let topics = self.swarm.behaviour_mut().gossipsub.topics().collect::<Vec<_>>();
        let r_topic = *topics.choose(&mut rand::rng()).unwrap();
        let peers = &self.peers; 
        for p in peers {
            let topic = gossipsub::IdentTopic::new(format!("stem-{}", p));
            if *r_topic == topic.hash() {
                log::debug!("found topic hash match for random topic");
                return topic;
            }
        }
        gossipsub::IdentTopic::new("dandelion")
    }
    /// see swarm.behaviour_mut().gossipsub.subscribe(topic);
    pub fn subscribe(&mut self, topic: &gossipsub::IdentTopic) -> Result<bool, Box<dyn Error>> {
        // Subscribe to topic in gossipsub
        match self.swarm.behaviour_mut().gossipsub.subscribe(topic) {
            Ok(_) => {
                log::info!(
                    "Subscribed to topic: [{:?}]",
                    topic.hash()
                );
                Ok(true)
            },
            Err(e) => {
                log::error!("Failed to subscribe to topic: {}", e);
                // Remove from local tracking on failure
                Err(Box::new(e))
            }
        }
    }
    /// Calls swarm.dial() and subscribes to `stem-{PEER_ID}` on successful connection
    pub async fn connect(&mut self, addr: Multiaddr) -> Result<(), Box<dyn Error>> {
        // Extract peer ID from address
        let peer_id = match addr.iter().find_map(|p| match p {
            libp2p::multiaddr::Protocol::P2p(hash) => Some(PeerId::from_multihash(hash.into()).expect("Valid hash")),
            _ => None,
        }) {
            Some(peer_id) => peer_id,
            None => return Err("Address must contain peer ID".into()),
        };
        // Check if already connected
        if self.is_connected(&peer_id) {
            log::info!("Already connected to peer: {:?}", peer_id);
            return Ok(());
        }
        // Attempt connection
        let topic = gossipsub::IdentTopic::new(format!("stem-{}", &peer_id));
        match self.swarm.dial(addr.clone()) {
            Ok(_) => {
                log::info!("Dialing peer {:?} at {}", peer_id, addr);
                // create peer subscription for stem and fluff
                self.subscribe(&topic).unwrap();
                // Wait for connection establishment
                self.wait_for_connection(peer_id).await?;
                Ok(())
            },
            Err(e) => {
                log::error!("Failed to dial peer: {:?}", e);
                Err(Box::new(e))
            }
        }
    }
    /// Helper for processing events on peer connection
    async fn wait_for_connection(&mut self, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
        let timeout = Duration::from_secs(30);
        let start = tokio::time::Instant::now();
        while !self.is_connected(&peer_id) {
            if start.elapsed() > timeout {
                return Err("Connection timeout".into());
            }
            // Process events while waiting
            match self.swarm.next().await {
                Some(SwarmEvent::ConnectionEstablished { peer_id: connected_peer, .. }) => {
                    if connected_peer == peer_id {
                        return Ok(());
                    }
                }
                Some(SwarmEvent::OutgoingConnectionError { peer_id: failed_peer, error, .. }) => {
                    if failed_peer == Some(peer_id) {
                        return Err(format!("Connection failed: {:?}", error).into());
                    }
                }
                _ => continue,
            }
        }
        Ok(())
    }
    /// see swarm.is_connected()
    pub fn is_connected(&self, peer_id: &PeerId) -> bool {
        self.swarm.is_connected(peer_id)
    }
    /// see swarm.connected_peers()
    pub fn connected_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.swarm.connected_peers()
    }
    /// see swarm.disconnected()
    pub async fn disconnect(&mut self, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
        if self.is_connected(&peer_id) {
            self.swarm.disconnect_peer_id(peer_id).unwrap();
            log::info!("Disconnected from peer: {:?}", peer_id);
        }
        Ok(())
    }
    /// Self-modeling frame for state transition influence
    pub fn broadcast_message(&mut self, data: Vec<u8>, topic: gossipsub::IdentTopic) -> Result<(), Box<dyn Error>> {
        // TODO: write broadcasted messages to db
        if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
            log::error!("Failed to publish message: {:?}", e);
        }
        Ok(())
    }
    /// Write messages pending fluff propagation or stem extension to LMDB for processing
    pub fn process_pending_messages(&mut self) {
        let now = tokio::time::Instant::now();
        let mut completed_messages = HashMap::new();
        for (id, message) in self.dandelion.pending_messages.iter() {
            log::debug!("process_pending_messages: {:?}", id);
            if now.duration_since(message.received_at) >= self.dandelion.stem_timeout {
                let nanos = message.received_at.elapsed().as_nanos() as u64;
                let received_at = nanos.to_be_bytes().to_vec();
                let p_cache = PendingMessageCache {
                    content: message.content.clone(),
                    received_at,
                    source: message.source.unwrap().to_bytes(),
                    relayed: message.relayed
                };
                let pre_p_cache = PrePendingMessageCache { serde: p_cache };
                completed_messages.insert(id.clone().0, pre_p_cache);
            }
        } 
        log::info!("processing {} pending messages", &completed_messages.len());
        let b_cache = bincode::encode_to_vec(&completed_messages, config::standard()).unwrap_or_default();
        let b_key = Vec::from(PENDING_FLUFF_MSG.as_bytes());
        let db: &DatabaseEnvironment = &DATABASE_LOCK;
        write_chunks(&db.env, &db.handle, &b_key, &b_cache).unwrap();
    }
    /// Pull pending message cache from LMDB and transition to fluff if needed
    pub fn transition_to_fluff(&mut self) {
        let key: Vec<u8> = Vec::from(PENDING_FLUFF_MSG.as_bytes());
        let db: &DatabaseEnvironment = &DATABASE_LOCK;
        let p_msg_bytes = DatabaseEnvironment::read(&db.env, &db.handle, &key).unwrap_or_default();
        let result: HashMap<Vec<u8>, PrePendingMessageCache> =
            bincode::decode_from_slice(&p_msg_bytes[..], config::standard()).unwrap_or_default().0; 
        let mut rng = rng();
        // Dynamic Stability through probabilistic transition
        if rng.random::<f64>() <= self.dandelion.fluff_probability {
            log::info!("attempting fluff transition");
            // Initialize Message cache
            let mut new_cache: Vec<PreMessageCache> = Vec::new();
            // Get all peers from gossipsub
            for (id, cache) in result {
                let all_peers = self.swarm.behaviour().gossipsub.all_peers();
                let mut available_peers: Vec<(&PeerId, Vec<&gossipsub::TopicHash>)> = all_peers
                    .into_iter()
                    .filter(|peer| *peer.0 != PeerId::from_bytes(&cache.serde.source).unwrap())
                    .collect();
                log::debug!("available peers: {:?}", &available_peers);
                let a_len = available_peers.len();
                // Calculate optimal spread factor based on network size
                let spread_factor = (a_len as f64).sqrt().ceil() as usize;
                let spread_factor = spread_factor.max(3).min(a_len);
                // Shuffle peers for randomized selection
                available_peers.shuffle(&mut rng);
                // Select subset of peers for message propagation
                let selected_peers = available_peers.iter().take(spread_factor);
                // Prepare fluff phase message
                let fluff_message = FluffTransitionMessage {
                    source: None, // Anonymize source
                    data: cache.serde.content.clone(),
                    sequence_number: rng.random(), // Random sequence for unlinkability
                };
                let c_msg_id: gossipsub::MessageId = gossipsub::MessageId(id);
                log::debug!("transition_to_fluff {:?}", c_msg_id);
                let v_peers = selected_peers.map(|x| x.0.to_bytes()).collect::<Vec<Vec<u8>>>();
                let cache = MessageCache {
                    peers: v_peers,
                    msg_id: c_msg_id.0,
                    fluff_msg: fluff_message,
                    is_fluff: true
                };
                let pre_cache = PreMessageCache { serde: cache };
                new_cache.push(pre_cache);
            }
                let b_cache = bincode::encode_to_vec(&new_cache, config::standard()).unwrap_or_default();
                let b_key = Vec::from(VALIDATE_MSG.as_bytes());
                let db: &DatabaseEnvironment = &DATABASE_LOCK;
                write_chunks(&db.env, &db.handle, &b_key, &b_cache).unwrap();
                log::info!("Transitioned message to fluff phase");
        } else {
            // If not transitioning to fluff, implement stem phase extension
            log::info!("activate stem phase extension");
            self.extend_stem_phase();
        }
    }
    /// Extend stem phase for state influence
    pub fn extend_stem_phase(&mut self) {
        let mut rng = rng();
        // Get available peers excluding message source
        let key: Vec<u8> = Vec::from(PENDING_FLUFF_MSG.as_bytes());
        let db: &DatabaseEnvironment = &DATABASE_LOCK;
        let p_msg_bytes = DatabaseEnvironment::read(&db.env, &db.handle, &key).unwrap_or_default();
        let result: HashMap<Vec<u8>, PrePendingMessageCache> =
            bincode::decode_from_slice(&p_msg_bytes[..], config::standard()).unwrap_or_default().0; 
        // initialize message cache
        let mut new_cache: Vec<PreMessageCache> = Vec::new();
        for (id, cache) in result {
            let all_peers = self.swarm.behaviour().gossipsub.all_peers();
            let available_peers: Vec<_>= all_peers
                .into_iter()
                .filter(|peer| *peer.0 != PeerId::from_bytes(&cache.serde.source).unwrap())
                .collect();
            log::debug!("log debug: {:?}", &available_peers);
            // Select single next hop for stem phase
            let u_next_peer = available_peers.choose(&mut rng);
            let next_peer = if u_next_peer.is_some() { u_next_peer.unwrap().0 } else { available_peers[0].0 };
            log::info!("selected {:?} for stem phase", &next_peer);
            // Prepare stem phase message
            let stem_message = FluffTransitionMessage {
                source: None,
                data: cache.serde.content.clone(),
                sequence_number: rng.random(),
            };
            let c_msg_id: gossipsub::MessageId = gossipsub::MessageId(id);
            let v_peers: Vec<Vec<u8>> = vec![next_peer.to_bytes()];
            let cache = MessageCache {
                peers: v_peers,
                msg_id: c_msg_id.0,
                fluff_msg: stem_message,
                is_fluff: false
            };
            let pre_cache = PreMessageCache { serde: cache };
            new_cache.push(pre_cache);
        }
            let b_cache = bincode::encode_to_vec(&new_cache, config::standard()).unwrap_or_default();
            let b_key = Vec::from(VALIDATE_MSG.as_bytes());
            let db: &DatabaseEnvironment = &DATABASE_LOCK;
            write_chunks(&db.env, &db.handle, &b_key, &b_cache).unwrap();
            log::info!("Extended stem phase with new relay peer");
    }
}

