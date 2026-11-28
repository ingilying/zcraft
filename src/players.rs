//! Authenticated connection reservations and bounded Play packet fanout.
mod chat;
mod delivery;
mod index;
mod query;
#[cfg(test)]
mod tests;
use self::index::ChunkIndex;
use crate::{
    protocol::{Clientbound, invalid, packets::play_entities::*},
    world::chunk_at,
};
pub(crate) use chat::ChatRequest;
pub(crate) use delivery::{Delivery, Peer, PlayerControl};
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque, hash_map::Entry},
    io,
    ops::Bound::{Excluded, Unbounded},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::Notify;
use uuid::Uuid;

/// Queued messages per connection, in addition to at most one in-flight write.
pub const BROADCAST_QUEUE_CAPACITY: usize = 32;

#[derive(Clone)]
pub struct Players {
    max: u32,
    registry: Arc<Mutex<Registry>>,
}

type Chunk = [i32; 2];

#[derive(Default, Debug)]
struct Registry {
    players: HashMap<Uuid, Slot>,
    online: u32,
    // Both indexes only contain visible, non-lagged Play sessions.
    entities: ChunkIndex,
    watchers: ChunkIndex,
    block_watchers: ChunkIndex,
}

#[derive(Debug)]
struct Slot {
    signals: Arc<Signals>,
    online: bool,
    receiving: bool,
    chat_mode: i32,
    // A command chain is bound to one uninterrupted ready-Play lifetime.
    command_epoch: Arc<()>,
    lagged: bool,
    queue: VecDeque<Delivery>,
    visible: Option<Peer>,
    // Reconcile the latest state per peer, not a FIFO of outdated poses.
    dirty: BTreeSet<Uuid>,
    last_refreshed: Option<Uuid>,
    departed: BTreeSet<Uuid>,
    known: HashMap<Uuid, KnownPeer>,
    wire_entities: ChunkIndex,
    view: Option<(Chunk, i32)>,
    sent_chunks: HashSet<Chunk>,
}

#[derive(Debug, Clone, Copy)]
struct KnownPeer {
    id: i32,
    // Independent of Tab membership. This is the most recent pose handed to
    // the single writer, so unloads can remove entities at their wire location.
    pose: Option<EntityPose>,
}

impl Slot {
    fn enqueue(&mut self, delivery: Delivery) -> bool {
        if self.queue.len() == BROADCAST_QUEUE_CAPACITY {
            self.lagged = true;
            self.queue.clear();
            self.dirty.clear();
            self.departed.clear();
            self.signals.lagged.notify_one();
            false
        } else {
            self.queue.push_back(delivery);
            self.signals.ready.notify_one();
            true
        }
    }

    fn discard_terrain(&mut self, chunks: &HashSet<Chunk>) {
        self.queue.retain_mut(|delivery| {
            if let Delivery::Terrain { chunk, ack, .. } = delivery
                && chunks.contains(chunk)
            {
                if let Some(body) = ack.take() {
                    *delivery = Delivery::Packet(body);
                    return true;
                }
                return false;
            }
            true
        });
    }

    fn tracks(&self, chunk: Chunk) -> bool {
        self.sent_chunks.contains(&chunk) && in_view(self.view, chunk)
    }

    fn remember(&mut self, uuid: Uuid, peer: KnownPeer) -> Option<KnownPeer> {
        let previous = self.known.insert(uuid, peer);
        let old_chunk = previous.and_then(|known| known.pose.map(pose_chunk));
        let new_chunk = peer.pose.map(pose_chunk);
        if old_chunk != new_chunk {
            if let Some(chunk) = old_chunk {
                self.wire_entities.remove(chunk, uuid);
            }
            if let Some(chunk) = new_chunk {
                self.wire_entities.insert(chunk, uuid);
            }
        }
        previous
    }

    fn forget(&mut self, uuid: Uuid) -> KnownPeer {
        let known = self.known.remove(&uuid).expect("known departure");
        if let Some(pose) = known.pose {
            self.wire_entities.remove(pose_chunk(pose), uuid);
        }
        known
    }

    fn untrack_wire_chunk(&mut self, chunk: Chunk, removed: &mut Vec<i32>) {
        for source in self.wire_entities.take(chunk) {
            let known = self.known.get_mut(&source).expect("wire entity is known");
            known.pose = None;
            removed.push(known.id);
            // A newer authoritative pose might be in a different loaded chunk.
            if !self.departed.contains(&source) {
                self.refresh(source);
            }
        }
    }

    fn refresh(&mut self, uuid: Uuid) {
        if self.dirty.insert(uuid) {
            self.signals.ready.notify_one();
        }
    }

    fn next_dirty(&mut self) -> Option<Uuid> {
        // Rotate by UUID so a frequently moving peer cannot starve others.
        let next = self
            .last_refreshed
            .and_then(|last| {
                self.dirty
                    .range((Excluded(last), Unbounded))
                    .next()
                    .copied()
            })
            .or_else(|| self.dirty.first().copied())?;
        self.dirty.remove(&next);
        self.last_refreshed = Some(next);
        Some(next)
    }
}

fn in_view(view: Option<(Chunk, i32)>, [cx, cz]: Chunk) -> bool {
    view.is_some_and(|([x, z], radius)| (cx - x).abs().max((cz - z).abs()) <= radius)
}

fn pose_chunk(pose: EntityPose) -> Chunk {
    chunk_at(
        f64::from_bits(pose.position[0]),
        f64::from_bits(pose.position[2]),
    )
}

impl Registry {
    fn enqueue_terrain(
        &mut self,
        uuid: Uuid,
        chunk: Chunk,
        updates: Arc<[Arc<[u8]>]>,
        ack: Option<Arc<[u8]>>,
    ) {
        if let Some(slot) = self.players.get_mut(&uuid)
            && !slot.lagged
            && !slot.enqueue(Delivery::Terrain {
                chunk,
                updates,
                ack,
            })
        {
            self.deactivate(uuid);
        }
    }

    fn withdraw(&mut self, source: Uuid) {
        for slot in self.players.values_mut() {
            slot.dirty.remove(&source);
            // Process departures before additions. Even with UUID churn the
            // known roster stays bounded by max_players.
            if slot.known.contains_key(&source) && slot.departed.insert(source) {
                slot.signals.ready.notify_one();
            }
        }
    }

    fn deactivate(&mut self, uuid: Uuid) {
        let slot = self.players.get_mut(&uuid).expect("owned slot");
        if let Some(peer) = slot.visible.take() {
            slot.command_epoch = Arc::new(());
            self.entities.remove(pose_chunk(peer.pose), uuid);
            for &chunk in &slot.sent_chunks {
                if in_view(slot.view, chunk) {
                    self.watchers.remove(chunk, uuid);
                }
            }
            self.withdraw(uuid);
        }
    }

    fn move_entity(&mut self, uuid: Uuid, from: Chunk, to: Chunk) {
        if from != to {
            self.entities.remove(from, uuid);
            self.entities.insert(to, uuid);
        }
        let affected = self.watchers.at(from).chain(
            (from != to)
                .then_some(to)
                .into_iter()
                .flat_map(|chunk| self.watchers.at(chunk)),
        );
        for other in affected.filter(|&other| other != uuid) {
            self.players
                .get_mut(&other)
                .expect("indexed watcher")
                .refresh(uuid);
        }
    }
}

#[derive(Debug, Default)]
struct Signals {
    ready: Notify,
    lagged: Notify,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    Duplicate,
    Full,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BroadcastReport {
    pub queued: usize,
    /// Recipients newly marked for disconnect. The network aborts their I/O.
    pub lagged: usize,
}

impl Players {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            registry: Arc::default(),
        }
    }

    pub fn max_players(&self) -> u32 {
        self.max
    }

    /// Includes players who finished their first Play initialization and are
    /// now reconfiguring. Pending logins occupy slots but don't count as online.
    pub fn online_count(&self) -> u32 {
        lock(&self.registry).online
    }

    pub fn online_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<_> = lock(&self.registry)
            .players
            .iter()
            .filter_map(|(id, slot)| slot.online.then_some(*id))
            .collect();
        ids.sort_unstable();
        ids
    }

    pub(crate) fn reserve(&self, uuid: Uuid) -> Result<Reservation, AdmissionError> {
        let mut registry = lock(&self.registry);
        let full = registry.players.len() >= self.max as usize;
        match registry.players.entry(uuid) {
            Entry::Occupied(_) => Err(AdmissionError::Duplicate),
            Entry::Vacant(_) if full => Err(AdmissionError::Full),
            Entry::Vacant(entry) => {
                let signals = Arc::new(Signals::default());
                entry.insert(Slot {
                    signals: signals.clone(),
                    online: false,
                    receiving: false,
                    chat_mode: 0,
                    command_epoch: Arc::new(()),
                    lagged: false,
                    queue: VecDeque::new(),
                    visible: None,
                    dirty: BTreeSet::new(),
                    last_refreshed: None,
                    departed: BTreeSet::new(),
                    known: HashMap::new(),
                    wire_entities: ChunkIndex::default(),
                    view: None,
                    sent_chunks: HashSet::new(),
                });
                Ok(Reservation(Inbox {
                    uuid,
                    signals,
                    registry: self.registry.clone(),
                }))
            }
        }
    }

    /// Fan out a supported Play data packet without waiting for socket writes.
    /// Protocol control, handshake and connection-specific packets cannot be
    /// queued. Newly lagging recipients are disconnected; healthy ones proceed.
    pub fn broadcast(&self, packet: &Clientbound) -> io::Result<BroadcastReport> {
        // These fieldless packets have fixed encodings (at most 40 bytes).
        // Add new data packets only with a bounded encoder/input size.
        if !matches!(
            packet,
            Clientbound::TimeUpdate(_)
                | Clientbound::GameEvent(_)
                | Clientbound::WorldBorder(_)
                | Clientbound::DefaultSpawnPosition(_)
        ) {
            return Err(invalid("packet is not supported for Play broadcast"));
        }
        let body: Arc<[u8]> = packet.encode()?.into();
        let mut report = BroadcastReport::default();
        let mut registry = lock(&self.registry);
        let mut lagged = Vec::new();
        for (&uuid, slot) in &mut registry.players {
            if !slot.receiving || slot.lagged {
                continue;
            }
            if slot.enqueue(Delivery::Packet(body.clone())) {
                report.queued += 1;
            } else {
                lagged.push(uuid);
            }
        }
        report.lagged = lagged.len();
        for uuid in lagged {
            registry.deactivate(uuid);
        }
        Ok(report)
    }

    /// Broadcast an entity lifecycle packet (`SpawnEntity`, `EntityMetadata`,
    /// `EntityVelocity`, `EntityTeleport`, `CollectItem`, `RemoveEntities`,
    /// `HurtAnimation`, `BlockDestruction`) to all ready players in Play.
    pub(crate) fn broadcast_entity_packet(&self, packet: &Clientbound) -> io::Result<()> {
        if !matches!(
            packet,
            Clientbound::SpawnEntity(_)
                | Clientbound::EntityMetadata(_)
                | Clientbound::EntityVelocity(_)
                | Clientbound::EntityTeleport(_)
                | Clientbound::CollectItem(_)
                | Clientbound::RemoveEntities(_)
                | Clientbound::HurtAnimation(_)
                | Clientbound::BlockDestruction(_)
        ) {
            return Err(invalid("packet is not a supported entity broadcast"));
        }
        let body: Arc<[u8]> = packet.encode()?.into();
        let mut registry = lock(&self.registry);
        let mut lagged = Vec::new();
        for (&uuid, slot) in &mut registry.players {
            if !slot.receiving || slot.lagged {
                continue;
            }
            if !slot.enqueue(Delivery::Packet(body.clone())) {
                lagged.push(uuid);
            }
        }
        for uuid in lagged {
            registry.deactivate(uuid);
        }
        Ok(())
    }

    /// Enqueue a server-initiated control action for a ready Play player.
    pub(crate) fn send_control(&self, uuid: Uuid, control: PlayerControl) -> bool {
        let mut registry = lock(&self.registry);
        let Some(slot) = registry.players.get_mut(&uuid) else {
            return false;
        };
        if !slot.receiving || slot.lagged {
            return false;
        }
        slot.enqueue(Delivery::Control(control))
    }

    /// Enqueue a player-targeted Play packet into that player's outbound delivery queue.
    pub(crate) fn send_packet(&self, uuid: Uuid, packet: &Clientbound) -> io::Result<bool> {
        let body: Arc<[u8]> = packet.encode()?.into();
        let mut registry = lock(&self.registry);
        let Some(slot) = registry.players.get_mut(&uuid) else {
            return Ok(false);
        };
        if !slot.receiving || slot.lagged {
            return Ok(false);
        }
        if !slot.enqueue(Delivery::Packet(body)) {
            registry.deactivate(uuid);
            return Ok(false);
        }
        Ok(true)
    }

    /// Update a ready player's authoritative pose in the spatial index by UUID.
    pub(crate) fn update_pose_for(&self, uuid: Uuid, to: EntityPose) {
        let mut registry = lock(&self.registry);
        let Some(peer) = registry
            .players
            .get_mut(&uuid)
            .filter(|slot| !slot.lagged)
            .and_then(|slot| slot.visible.as_mut())
        else {
            return;
        };
        if peer.pose != to {
            let from = std::mem::replace(&mut peer.pose, to);
            registry.move_entity(uuid, pose_chunk(from), pose_chunk(to));
        }
    }

    /// Update a ready player's cached game mode in the peer roster by UUID.
    pub(crate) fn update_game_mode_for(&self, uuid: Uuid, mode: crate::config::GameMode) {
        let mut registry = lock(&self.registry);
        if let Some(slot) = registry.players.get_mut(&uuid)
            && let Some(peer) = slot.visible.as_mut()
        {
            peer.game_mode = mode;
        }
    }

    /// Broadcast an entity-scoped packet to all other ready players watching `uuid`'s chunk.
    pub(crate) fn broadcast_to_viewers_of(
        &self,
        uuid: Uuid,
        packet: &Clientbound,
    ) -> io::Result<()> {
        let body: Arc<[u8]> = packet.encode()?.into();
        let mut registry = lock(&self.registry);
        let chunk = match registry
            .players
            .get(&uuid)
            .filter(|slot| !slot.lagged)
            .and_then(|slot| slot.visible.as_ref())
        {
            Some(peer) => pose_chunk(peer.pose),
            None => return Ok(()),
        };
        let viewers: Vec<Uuid> = registry
            .watchers
            .at(chunk)
            .filter(|&viewer| viewer != uuid)
            .collect();
        for viewer in viewers {
            if let Some(slot) = registry.players.get_mut(&viewer)
                && !slot.lagged
                && !slot.enqueue(Delivery::Packet(body.clone()))
            {
                registry.deactivate(viewer);
            }
        }
        Ok(())
    }

    /// Publish block updates with an ordered sequence acknowledgement for `actor`.
    pub(crate) fn publish_blocks_for(
        &self,
        actor: Uuid,
        chunk: Chunk,
        updates: Arc<[Arc<[u8]>]>,
        ack: Arc<[u8]>,
    ) {
        let mut registry = lock(&self.registry);
        let viewers: Vec<_> = registry.block_watchers.at(chunk).collect();
        for viewer in viewers {
            let ack = (viewer == actor).then(|| ack.clone());
            registry.enqueue_terrain(viewer, chunk, updates.clone(), ack);
        }
    }

    /// Publish a single chunk light update to all block watchers of `chunk`.
    pub(crate) fn publish_light_chunk(&self, chunk: Chunk, light: Arc<[u8]>) {
        let mut registry = lock(&self.registry);
        let viewers: Vec<_> = registry.block_watchers.at(chunk).collect();
        let updates: Arc<[Arc<[u8]>]> = Arc::from([light]);
        for viewer in viewers {
            registry.enqueue_terrain(viewer, chunk, updates.clone(), None);
        }
    }

    /// Enqueue block corrections and a sequence acknowledgement for `uuid`.
    pub(crate) fn correct_blocks_for(
        &self,
        uuid: Uuid,
        corrections: Vec<(Chunk, Arc<[u8]>)>,
        ack: Arc<[u8]>,
    ) {
        let mut registry = lock(&self.registry);
        for (chunk, body) in corrections {
            registry.enqueue_terrain(uuid, chunk, Arc::from([body]), None);
        }
        if let Some(slot) = registry.players.get_mut(&uuid)
            && !slot.lagged
            && !slot.enqueue(Delivery::Packet(ack))
        {
            registry.deactivate(uuid);
        }
    }

    /// Publish world block/light updates to all subscribers of `chunk`.
    pub(crate) fn publish_world_updates(&self, chunk: Chunk, updates: Arc<[Arc<[u8]>]>) {
        if updates.is_empty() {
            return;
        }
        let mut registry = lock(&self.registry);
        let viewers: Vec<_> = registry.block_watchers.at(chunk).collect();
        for viewer in viewers {
            registry.enqueue_terrain(viewer, chunk, updates.clone(), None);
        }
    }

    /// Find an online ready player by exact username or unique case-insensitive match.
    pub(crate) fn find_player_by_name(&self, name: &str) -> Option<(Uuid, String)> {
        let registry = lock(&self.registry);
        let mut case_match: Option<(Uuid, String)> = None;
        let mut ambiguous = false;
        for (&uuid, slot) in &registry.players {
            let Some(peer) = &slot.visible else {
                continue;
            };
            if !slot.receiving || slot.lagged {
                continue;
            }
            if peer.name.as_ref() == name {
                return Some((uuid, peer.name.to_string()));
            }
            if peer.name.eq_ignore_ascii_case(name) {
                if case_match.is_some() {
                    ambiguous = true;
                } else {
                    case_match = Some((uuid, peer.name.to_string()));
                }
            }
        }
        (!ambiguous).then_some(case_match).flatten()
    }
}

fn lock(registry: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
    registry.lock().expect("player registry poisoned")
}

/// The connection's unique ownership token. Drop releases both UUID and slot.
#[derive(Debug)]
pub(crate) struct Reservation(Inbox);

impl Reservation {
    pub(crate) fn inbox(&self) -> Inbox {
        self.0.clone()
    }

    pub(crate) fn enter_play(&self) {
        let mut registry = lock(&self.0.registry);
        if let Some(slot) = self.0.slot(&mut registry)
            && !slot.lagged
        {
            slot.receiving = true;
            if !std::mem::replace(&mut slot.online, true) {
                registry.online += 1;
            }
        }
    }

    pub(crate) fn show(&self, peer: Peer) {
        let mut registry = lock(&self.0.registry);
        let uuid = self.0.uuid;
        let Registry {
            players,
            watchers,
            entities,
            ..
        } = &mut *registry;
        let Some(slot) = self.0.slot_in(players) else {
            return;
        };
        if slot.lagged || slot.visible.is_some() {
            return;
        }
        entities.insert(pose_chunk(peer.pose), uuid);
        slot.visible = Some(peer);
        for &chunk in &slot.sent_chunks {
            if in_view(slot.view, chunk) {
                watchers.insert(chunk, uuid);
            }
        }
        let mut peers = BTreeSet::new();
        for (&other, slot) in players.iter_mut() {
            if other != uuid && slot.visible.is_some() && !slot.lagged {
                peers.insert(other);
                slot.refresh(uuid);
            }
        }
        let slot = players.get_mut(&uuid).expect("owned slot");
        slot.dirty = peers;
        slot.signals.ready.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn update_game_mode(&self, mode: crate::config::GameMode) {
        let mut registry = lock(&self.0.registry);
        if let Some(slot) = self.0.slot(&mut registry)
            && let Some(peer) = slot.visible.as_mut()
        {
            peer.game_mode = mode;
        }
    }

    #[cfg(test)]
    pub(crate) fn update_pose(&self, to: EntityPose) {
        let mut registry = lock(&self.0.registry);
        let Some(peer) = self
            .0
            .slot(&mut registry)
            .filter(|slot| !slot.lagged)
            .and_then(|slot| slot.visible.as_mut())
        else {
            return;
        };
        if peer.pose != to {
            let from = std::mem::replace(&mut peer.pose, to);
            registry.move_entity(self.0.uuid, pose_chunk(from), pose_chunk(to));
        }
    }

    pub(crate) fn set_view(&self, center: [i32; 2], radius: i32) {
        let mut registry = lock(&self.0.registry);
        let Registry {
            players,
            watchers,
            entities,
            ..
        } = &mut *registry;
        let Some(slot) = self.0.slot_in(players) else {
            return;
        };
        let uuid = self.0.uuid;
        let view = Some((center, radius));
        if slot.view == view {
            return;
        }
        let previous = std::mem::replace(&mut slot.view, view);
        if slot.visible.is_none() {
            return;
        }
        let mut refresh = Vec::new();
        for &chunk in &slot.sent_chunks {
            match (in_view(previous, chunk), in_view(view, chunk)) {
                (false, true) => {
                    watchers.insert(chunk, uuid);
                    refresh.extend(entities.at(chunk).filter(|&other| other != uuid));
                }
                (true, false) => {
                    watchers.remove(chunk, uuid);
                    // A coalesced pending move can make the authoritative and
                    // last written chunks differ. Reconcile the wire location.
                    refresh.extend(
                        slot.wire_entities
                            .at(chunk)
                            .filter(|other| !slot.departed.contains(other)),
                    );
                }
                _ => {}
            }
        }
        for other in refresh {
            slot.refresh(other);
        }
    }

    pub(crate) fn chunk_sent(&self, chunk: Chunk) {
        let mut registry = lock(&self.0.registry);
        let Registry {
            players,
            watchers,
            entities,
            ..
        } = &mut *registry;
        if let Some(slot) = self.0.slot_in(players)
            && slot.view.is_some()
            && slot.sent_chunks.insert(chunk)
            && slot.visible.is_some()
            && in_view(slot.view, chunk)
        {
            let uuid = self.0.uuid;
            watchers.insert(chunk, uuid);
            for other in entities.at(chunk).filter(|&other| other != uuid) {
                slot.refresh(other);
            }
        }
    }

    /// Called under the shared world edit lock, after the chunk write. Even
    /// before teleport confirmation terrain needs updates; entity visibility
    /// is activated separately by `show`.
    pub(crate) fn watch_blocks(&self, chunk: Chunk, catchup: Arc<[Arc<[u8]>]>) {
        let mut registry = lock(&self.0.registry);
        let uuid = self.0.uuid;
        registry.block_watchers.insert(chunk, uuid);
        if !catchup.is_empty() {
            registry.enqueue_terrain(uuid, chunk, catchup, None);
        }
    }

    /// The world lock orders edits, snapshot reconciliation and queued updates.
    #[cfg(test)]
    pub(crate) fn publish_blocks(&self, chunk: Chunk, updates: Arc<[Arc<[u8]>]>, ack: Arc<[u8]>) {
        let mut registry = lock(&self.0.registry);
        let viewers: Vec<_> = registry.block_watchers.at(chunk).collect();
        for viewer in viewers {
            let ack = (viewer == self.0.uuid).then(|| ack.clone());
            registry.enqueue_terrain(viewer, chunk, updates.clone(), ack);
        }
    }

    pub(crate) fn retire_blocks(&self, chunks: &HashSet<Chunk>) {
        if chunks.is_empty() {
            return;
        }
        let mut registry = lock(&self.0.registry);
        for &chunk in chunks {
            registry.block_watchers.remove(chunk, self.0.uuid);
        }
        if let Some(slot) = self.0.slot(&mut registry) {
            slot.discard_terrain(chunks);
        }
    }

    pub(crate) fn before_unload(&self, chunks: &HashSet<Chunk>) -> Vec<Clientbound> {
        let mut registry = lock(&self.0.registry);
        let Registry {
            players, watchers, ..
        } = &mut *registry;
        let Some(slot) = self.0.slot_in(players) else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        for &chunk in chunks {
            if slot.sent_chunks.remove(&chunk)
                && slot.visible.is_some()
                && in_view(slot.view, chunk)
            {
                watchers.remove(chunk, self.0.uuid);
            }
            slot.untrack_wire_chunk(chunk, &mut removed);
        }
        removed
            .chunks(256)
            .map(|batch| RemoveEntities(batch.to_vec()).into())
            .collect()
    }

    pub(crate) fn pause(&self) -> Vec<Clientbound> {
        let mut registry = lock(&self.0.registry);
        registry.deactivate(self.0.uuid);
        let slot = registry.players.get_mut(&self.0.uuid).expect("owned slot");
        slot.receiving = false;
        slot.queue.clear();
        slot.dirty.clear();
        slot.departed.clear();
        slot.view = None;
        let chunks = std::mem::take(&mut slot.sent_chunks);
        slot.wire_entities.0.clear();
        let known = std::mem::take(&mut slot.known);
        for chunk in chunks {
            registry.block_watchers.remove(chunk, self.0.uuid);
        }
        drop(registry);
        // Fixed-size chunks bound the wire size even with a large max_players.
        let mut known = known.into_iter().peekable();
        let mut replies = Vec::new();
        while known.peek().is_some() {
            let mut ids = Vec::new();
            let mut uuids = Vec::new();
            for (uuid, peer) in known.by_ref().take(256) {
                uuids.push(uuid);
                if peer.pose.is_some() {
                    ids.push(peer.id);
                }
            }
            if !ids.is_empty() {
                replies.push(RemoveEntities(ids).into());
            }
            replies.push(RemovePlayerInfo(uuids).into());
        }
        replies
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut registry = lock(&self.0.registry);
        registry.deactivate(self.0.uuid);
        let slot = registry.players.remove(&self.0.uuid).expect("owned slot");
        for chunk in slot.sent_chunks {
            registry.block_watchers.remove(chunk, self.0.uuid);
        }
        if slot.online {
            registry.online -= 1;
        }
    }
}

/// A receive handle, not an owner. Only the connection task may poll it.
#[derive(Clone, Debug)]
pub(crate) struct Inbox {
    uuid: Uuid,
    signals: Arc<Signals>,
    registry: Arc<Mutex<Registry>>,
}

impl Inbox {
    fn slot<'a>(&self, registry: &'a mut Registry) -> Option<&'a mut Slot> {
        self.slot_in(&mut registry.players)
    }

    fn slot_in<'a>(&self, players: &'a mut HashMap<Uuid, Slot>) -> Option<&'a mut Slot> {
        players
            .get_mut(&self.uuid)
            .filter(|slot| Arc::ptr_eq(&self.signals, &slot.signals))
    }

    fn pop(&self) -> Option<Delivery> {
        let mut registry = lock(&self.registry);
        let slot = self.slot(&mut registry)?;
        if let Some(delivery) = slot.queue.pop_front() {
            return Some(delivery);
        }
        if slot.lagged || slot.visible.is_none() {
            return None;
        }
        if let Some(uuid) = slot.departed.pop_first() {
            let known = slot.forget(uuid);
            return Some(Delivery::Remove {
                uuid,
                id: known.pose.map(|_| known.id),
            });
        }
        loop {
            let slot = registry.players.get_mut(&self.uuid).expect("owned slot");
            let uuid = slot.next_dirty()?;
            let Some(peer) = registry
                .players
                .get(&uuid)
                .filter(|source| !source.lagged)
                .and_then(|source| source.visible.clone())
            else {
                continue;
            };
            let slot = registry.players.get_mut(&self.uuid).expect("owned slot");
            let pose = slot.tracks(pose_chunk(peer.pose)).then_some(peer.pose);
            let Some(previous) = slot.remember(uuid, KnownPeer { id: peer.id, pose }) else {
                return Some(Delivery::Add {
                    peer,
                    spawn: pose.is_some(),
                });
            };
            match (previous.pose, pose) {
                (None, Some(pose)) => {
                    return Some(Delivery::Spawn(SpawnPlayer {
                        id: peer.id,
                        uuid,
                        pose,
                    }));
                }
                (Some(_), None) => return Some(Delivery::Despawn(peer.id)),
                (Some(from), Some(to)) if from != to => {
                    return Some(Delivery::Move {
                        id: peer.id,
                        from,
                        to,
                    });
                }
                _ => {}
            }
        }
    }

    pub(crate) async fn next(&self) -> Delivery {
        loop {
            if let Some(body) = self.pop() {
                return body;
            }
            self.signals.ready.notified().await;
        }
    }

    pub(crate) async fn cancelled(&self) {
        let already_lagged = self
            .slot(&mut lock(&self.registry))
            .is_some_and(|slot| slot.lagged);
        if !already_lagged {
            // notify_one retains a permit if overflow raced this check.
            self.signals.lagged.notified().await;
        }
    }
}
