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
    collections::{hash_map::DefaultHasher, HashSet, HashMap},
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
    sync::{Mutex, Arc},
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
use rand::{seq::SliceRandom, thread_rng, Rng};

// Derived from Derivation 15: State Distinction
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct MessageId(Vec<u8>);

/// Necessary ordering for state distinctions
#[derive(Clone)]
struct PendingMessage {
    content: Vec<u8>,
    received_at: tokio::time::Instant,
    source: Option<PeerId>,
    relayed: bool,
}

/// Gossipsub message for transitioning to fluff phase
struct FluffTransitionMessage {
    source: Option<usize>,
    data: Vec<u8>,
    sequence_number: f64,
    topic: gossipsub::TopicHash
}

/// Necessary structural relationship for depth, persistence and directions
#[derive(Clone)]
struct Dandelion {
    // State tracking for anonymous message propagation
    stem_mode: bool,
    fluff_probability: f64,
    stem_timeout: Duration,
    pending_messages: HashMap<gossipsub::MessageId, PendingMessage>,
}

/// Dandelion behavior for complete Self-Reference
#[derive(NetworkBehaviour)]
struct DandelionBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

/// Main node implementation derived
pub struct DandelionNode {
    swarm: Swarm<DandelionBehaviour>,
    ext_behaviour: DandelionBehaviour,
    handle_message_behaviour: DandelionBehaviour,
    dandelion: Dandelion,
    topics: HashSet<gossipsub::IdentTopic>,
}

impl DandelionNode {
    pub async fn new(
        fluff_probability: f64,
        stem_timeout: Duration,
    ) -> Result<Self, Box<dyn Error>> {
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
        .build();

            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn) 
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;
            let g_behaviour: gossipsub::Behaviour = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(local_key.clone()),
                gossipsub_config.clone(),
                )?;

            let gh_behaviour: gossipsub::Behaviour = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(local_key.clone()),
                gossipsub_config,
            )?;

            let m_behaviour =
                mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;

            let mh_behaviour = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;

            let ext_behaviour = DandelionBehaviour {
                gossipsub: g_behaviour,
                mdns: m_behaviour,
            };

            let handle_message_behaviour = DandelionBehaviour {
                gossipsub: gh_behaviour,
                mdns: mh_behaviour,
            };
        // Framework-derived listener configuration
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
        log::debug!("swarm new listners: {}", swarm.listeners().count());
        let dandelion = Dandelion {
            stem_mode: false,
            fluff_probability,
            stem_timeout,
            pending_messages: HashMap::new()
        };
        Ok(Self {
            swarm,
            ext_behaviour,
            handle_message_behaviour,
            dandelion,
            topics: HashSet::new(),
        })
    }

    /// Integration Requirement - should be placed in an infinite loop
    async fn process_network_events(
        &mut self, mut s: Swarm<DandelionBehaviour>,
        d: Dandelion,
        t: HashSet<gossipsub::IdentTopic>,
        ext: DandelionBehaviour,
        hm_behaviour: DandelionBehaviour
    ) -> Result<(), Box<dyn Error>> {
        self.process_pending_messages(ext, d);
        match s.next().await.expect("Swarm not terminated") {
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                log::info!("Connected to peer: {:?}", peer_id);
                for topic in &t {
                    if let Err(e) = s.behaviour_mut().gossipsub.subscribe(topic) {
                        log::error!("Failed to subscribe to topic: {:?}", e);
                    }
                }
            },
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                log::info!("Disconnected from peer: {:?}", peer_id);
                self.handle_peer_disconnect(peer_id).await?;
            },
            SwarmEvent::NewListenAddr { address, .. } => {
                log::info!("Listening on: {:?}", address);
            },
            SwarmEvent::Behaviour(event) => {
                match event {
                    DandelionBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                    }) => {
                        let msg_id = gossipsub::MessageId(message.data.clone());
                        // Process only if message not seen before
                        if !self.dandelion.pending_messages.contains_key(&msg_id) {
                            let pending_msg = PendingMessage {
                                content: message.data.clone(),
                                received_at: tokio::time::Instant::now(),
                                source: Some(peer_id),
                                relayed: false,
                            };
                            self.dandelion.pending_messages.insert(msg_id.clone(), pending_msg);
                            // Process based on current phase
                            if self.dandelion.stem_mode {
                                self.handle_stem_message(msg_id, message, hm_behaviour.gossipsub).await?;
                            } else {
                                self.handle_fluff_message(msg_id, message, hm_behaviour.gossipsub).await?;
                            }
                        }
                    },
                    _=> {}
                };
            },
            _=> {}
        }
        Ok(())
    }

    /// Handle stem messages with probabilistic forwarding
    async fn handle_stem_message(
        &mut self,
        msg_id: gossipsub::MessageId,
        message: gossipsub::Message,
        mut g: gossipsub::Behaviour,
    ) -> Result<(), Box<dyn Error>> {
        let mut rng = thread_rng();
        // Probabilistic forwarding
        if rng.gen::<f64>() <= self.dandelion.fluff_probability {
            // Get available peers for forwarding
            if let peers = self.swarm.behaviour_mut().gossipsub.all_peers() {
                let available_peers = peers.into_iter().collect::<Vec<_>>();
                if let Some(&ref next_peer) = available_peers.choose(&mut rng) {
                    // Forward message in stem phase
                    if let Err(e) = g.report_message_validation_result(
                        &msg_id,
                        &next_peer.0,
                        libp2p::gossipsub::MessageAcceptance::Accept
                    ) {
                        log::error!("Failed to forward stem message: {:?}", e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Fluff phase propagation for messages
    async fn handle_fluff_message(
        &mut self,
        msg_id: gossipsub::MessageId,
        message: gossipsub::Message,
        mut g: gossipsub::Behaviour,
    ) -> Result<(), Box<dyn Error>> {
        // Implement fluff phase propagation
        if let peers = self.swarm.behaviour().gossipsub.all_peers() {
            let v_peers = peers.collect::<Vec<_>>();
            let spread_factor = (v_peers.len() as f64).sqrt().ceil() as usize;
            for peer in v_peers.into_iter().take(spread_factor) {
                if let Err(e) = g.report_message_validation_result(
                    &msg_id,
                    &peer.0,
                    libp2p::gossipsub::MessageAcceptance::Accept
                ) {
                    log::error!("Failed to propagate fluff message: {:?}", e);
                }
            }
        }
        Ok(())
    }
    /// Boundary Dynamics
    async fn handle_peer_disconnect(&mut self, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
        // Clean up any pending messages related to this peer
        self.dandelion.pending_messages.retain(|_, msg| {
            msg.source.map_or(true, |source| source != peer_id)
        });
        Ok(())
    }

    pub fn listen_addresses(&self) -> impl Iterator<Item = &Multiaddr> {
        // Get all active listening addresses from the swarm
        self.swarm.listeners()
    }

    pub fn subscribe(&mut self, topic: &gossipsub::IdentTopic) -> Result<bool, Box<dyn Error>> {

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

    pub async fn connect(&mut self, addr: Multiaddr) -> Result<(), Box<dyn Error>> {
        let peer_id = match addr.iter().find_map(|p| match p {
            libp2p::multiaddr::Protocol::P2p(hash) => Some(PeerId::from_multihash(hash.into()).expect("Valid hash")),
            _ => None,
        }) {
            Some(peer_id) => peer_id,
            None => return Err("Address must contain peer ID".into()),
        };

        if self.is_connected(&peer_id) {
            log::info!("Already connected to peer: {:?}", peer_id);
            return Ok(());
        }

        match self.swarm.dial(addr.clone()) {
            Ok(_) => {
                log::info!("Dialing peer {:?} at {}", peer_id, addr);
                
                self.wait_for_connection(peer_id).await?;
                
                Ok(())
            },
            Err(e) => {
                log::error!("Failed to dial peer: {:?}", e);
                Err(Box::new(e))
            }
        }
    }

    async fn wait_for_connection(&mut self, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
        let timeout = Duration::from_secs(30);
        let start = tokio::time::Instant::now();

        while !self.is_connected(&peer_id) {
            if start.elapsed() > timeout {
                return Err("Connection timeout".into());
            }

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


    pub fn is_connected(&self, peer_id: &PeerId) -> bool {
        self.swarm.is_connected(peer_id)
    }

    pub fn connected_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.swarm.connected_peers()
    }

    pub async fn disconnect(&mut self, peer_id: PeerId) -> Result<(), Box<dyn Error>> {
        if self.is_connected(&peer_id) {
            // Disconnect peer
            self.swarm.disconnect_peer_id(peer_id).unwrap();
            
            log::info!("Disconnected from peer: {:?}", peer_id);
        }

        Ok(())
    }

        /// Self-modeling frame for state transition influence
    pub fn broadcast_message(&mut self, data: Vec<u8>, topic: gossipsub::IdentTopic) -> Result<(), Box<dyn Error>> {
        let message_id = gossipsub::MessageId(data.clone());
        if !self.dandelion.pending_messages.contains_key(&message_id) {
            self.dandelion.pending_messages.insert(
                message_id,
                PendingMessage {
                    content: data.clone(),
                    received_at: tokio::time::Instant::now(),
                    source: None,
                    relayed: false,
                },
            );
            if let Err(e) = self.handle_message_behaviour.gossipsub.publish(topic, data) {
                log::error!("Failed to publish message: {:?}", e);
            }
        }
        Ok(())
    }
    /// Structural Feedback
    fn process_pending_messages(&mut self, behaviour: DandelionBehaviour, dandelion: Dandelion) {
        let now = tokio::time::Instant::now();
        let mut completed_messages = Vec::new();
        let a_behaviour = &mut Arc::new(Mutex::new(behaviour));
        let copy_dandelion = &mut Dandelion {
            stem_mode: dandelion.stem_mode,
            fluff_probability: dandelion.fluff_probability,
            stem_timeout: dandelion.stem_timeout,
            pending_messages: dandelion.pending_messages.clone()
        };
        for (id, message) in dandelion.pending_messages.iter() {
            if now.duration_since(message.received_at) >= dandelion.stem_timeout {
                completed_messages.push(id.clone());
            }
            if !message.relayed {
                let pm = PendingMessage {
                    content: message.content.clone(),
                    received_at: message.received_at,
                    source: message.source,
                    relayed: message.relayed,
            };
            self.transition_to_fluff(&id, pm, (&mut a_behaviour.clone(), copy_dandelion))
            }
        }
    }

    /// Interactive feedback for stable structures
    fn transition_to_fluff(&mut self, id: &gossipsub::MessageId, message: PendingMessage, cs: (&mut Arc<Mutex<DandelionBehaviour>>, &mut Dandelion)) {
        let mut rng = thread_rng();
        // Dynamic Stability through probabilistic transition
        if rng.gen::<f64>() <= cs.1.fluff_probability {
            // Get all peers from gossipsub
            let cs_copy_all_peers = Arc::clone(cs.0);
            let all_peers = cs_copy_all_peers.lock().unwrap();
            let u_all_peers = all_peers.gossipsub.all_peers();
            if let peers = u_all_peers {
                let mut available_peers: Vec<(&PeerId, Vec<&gossipsub::TopicHash>)> = peers
                    .into_iter()
                    .filter(|peer| peer.0 != &message.source.unwrap())
                    .collect();
                // Calculate optimal spread factor based on network size
                let spread_factor = (available_peers.len() as f64).sqrt().ceil() as usize;
                let spread_factor = spread_factor.max(3).min(available_peers.len());
                // Shuffle peers for randomized selection
                available_peers.shuffle(&mut rng);
                // Select subset of peers for message propagation
                let selected_peers = available_peers.iter().take(spread_factor);
                // Prepare fluff phase message
                let fluff_message = FluffTransitionMessage {
                    source: None, // Anonymize source
                    data: message.content,
                    sequence_number: rng.gen(), // Random sequence for unlinkability
                    topic: gossipsub::IdentTopic::new("dandelion-fluff").hash(),
                };
                // Propagate to selected peers
                for peer in selected_peers {
                     if let Err(e) = cs_copy_all_peers.lock().unwrap().gossipsub.report_message_validation_result(
                         &gossipsub::MessageId(fluff_message.data.clone()),
                         &peer.0,
                         libp2p::gossipsub::MessageAcceptance::Accept
                     ) {
                         log::error!("Failed to propagate fluff message to peer: {:?}", e);
                     }
                 }
                log::info!(
                    "Transitioned message to fluff phase, propagated to {}/{} peers",
                    spread_factor,
                    available_peers.len()
                );
            }
        } else {
            // If not transitioning to fluff, implement stem phase extension
            self.extend_stem_phase(id, message, cs);
        }
    }

    /// Extend stem phase for state influence
    fn extend_stem_phase(&mut self, id: &gossipsub::MessageId, message: PendingMessage, cs: (&mut Arc<Mutex<DandelionBehaviour>>, &mut Dandelion)) {
        let mut rng = thread_rng();
        // Get available peers excluding message source
        let cs_copy_all_peers = Arc::clone(cs.0);
        let all_peers_lock = cs_copy_all_peers.lock().unwrap();
        let u_all_peers = all_peers_lock.gossipsub.all_peers();
        if let peers = u_all_peers {
            let available_peers: Vec<_>= peers
                .into_iter()
                .filter(|peer| peer.0 != &message.source.unwrap())
                .collect();
            // Select single next hop for stem phase
            if let &next_peer = available_peers.choose(&mut rng).unwrap().0 {
                // Prepare stem phase message
                let stem_message = FluffTransitionMessage {
                    source: None,
                    data: message.content,
                    sequence_number: rng.gen(),
                    topic: gossipsub::IdentTopic::new("dandelion-stem").hash(),
                };
                // Propagate to next peer
                if let Err(e) = cs_copy_all_peers.lock().unwrap().gossipsub.report_message_validation_result(
                    &gossipsub::MessageId(stem_message.data.clone()),
                    &next_peer,
                    libp2p::gossipsub::MessageAcceptance::Accept
                ) {
                    log::error!("Failed to extend stem phase: {:?}", e);
                }
                log::info!("Extended stem phase with new relay peer");
            }
        }
    }
  }

