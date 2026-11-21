//! Authenticated connection reservations and bounded Play packet fanout.
mod chat;
use crate::{
    protocol::{Clientbound, invalid, packets::play_entities::*},
    world::chunk_at,
};
pub(crate) use chat::ChatRequest;
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

pub struct Players {
    max: u32,
    registry: Arc<Mutex<Registry>>,
}

type Chunk = [i32; 2];

#[derive(Default, Debug)]
struct ChunkIndex(HashMap<Chunk, HashSet<Uuid>>);

impl ChunkIndex {
    fn insert(&mut self, chunk: Chunk, uuid: Uuid) {
        self.0.entry(chunk).or_default().insert(uuid);
    }

    fn remove(&mut self, chunk: Chunk, uuid: Uuid) {
        if let Entry::Occupied(mut bucket) = self.0.entry(chunk) {
            bucket.get_mut().remove(&uuid);
            if bucket.get().is_empty() {
                bucket.remove();
            }
        }
    }

    fn at(&self, chunk: Chunk) -> impl Iterator<Item = Uuid> + '_ {
        self.0.get(&chunk).into_iter().flatten().copied()
    }

    fn take(&mut self, chunk: Chunk) -> impl Iterator<Item = Uuid> + use<> {
        self.0.remove(&chunk).into_iter().flatten()
    }
}

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

#[derive(Debug, Clone)]
pub(crate) struct Peer {
    pub uuid: Uuid,
    pub name: Arc<str>,
    pub id: i32,
    pub pose: EntityPose,
    pub info: Arc<[u8]>,
}

#[derive(Debug)]
pub(crate) enum Delivery {
    Packet(Arc<[u8]>),
    Chat {
        body: Arc<[u8]>,
        private: bool,
    },
    Terrain {
        chunk: Chunk,
        updates: Arc<[Arc<[u8]>]>,
        ack: Option<Arc<[u8]>>,
    },
    Add {
        peer: Peer,
        spawn: bool,
    },
    Spawn(SpawnPlayer),
    Despawn(i32),
    Remove {
        uuid: Uuid,
        id: Option<i32>,
    },
    Move {
        id: i32,
        from: EntityPose,
        to: EntityPose,
    },
}

impl Delivery {
    pub(crate) fn bodies(self) -> io::Result<impl Iterator<Item = Arc<[u8]>> + Send> {
        let mut bodies = [None, None];
        let encode = |packet: Clientbound| packet.encode().map(Arc::<[u8]>::from);
        let mut terrain = None;
        let mut ack = None;
        match self {
            Self::Packet(body) | Self::Chat { body, .. } => bodies[0] = Some(body),
            Self::Terrain {
                updates,
                ack: sequence,
                ..
            } => {
                terrain = Some(updates);
                ack = sequence;
            }
            Self::Add { peer, spawn } => {
                bodies[0] = Some(peer.info);
                if spawn {
                    bodies[1] = Some(encode(
                        SpawnPlayer {
                            id: peer.id,
                            uuid: peer.uuid,
                            pose: peer.pose,
                        }
                        .into(),
                    )?);
                }
            }
            Self::Spawn(packet) => bodies[0] = Some(encode(packet.into())?),
            Self::Despawn(id) => bodies[0] = Some(encode(RemoveEntities(vec![id]).into())?),
            Self::Remove { uuid, id } => {
                if let Some(id) = id {
                    bodies[0] = Some(encode(RemoveEntities(vec![id]).into())?);
                }
                bodies[1] = Some(encode(RemovePlayerInfo(vec![uuid]).into())?);
            }
            Self::Move { id, from, to } => {
                let packet = if from.position == to.position {
                    EntityLook { id, pose: to }.into()
                } else {
                    EntityTeleport { id, pose: to }.into()
                };
                bodies[0] = Some(encode(packet)?);
                if from.yaw != to.yaw {
                    bodies[1] = Some(encode(EntityHeadRotation { id, yaw: to.yaw }.into())?);
                }
            }
        }
        let mut index = 0;
        let updates = std::iter::from_fn(move || {
            let body = terrain.as_ref()?.get(index)?.clone();
            index += 1;
            Some(body)
        });
        Ok(bodies.into_iter().flatten().chain(updates).chain(ack))
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
    pub(crate) fn publish_blocks(&self, chunk: Chunk, updates: Arc<[Arc<[u8]>]>, ack: Arc<[u8]>) {
        let mut registry = lock(&self.0.registry);
        let viewers: Vec<_> = registry.block_watchers.at(chunk).collect();
        for viewer in viewers {
            let ack = (viewer == self.0.uuid).then(|| ack.clone());
            registry.enqueue_terrain(viewer, chunk, updates.clone(), ack);
        }
    }

    pub(crate) fn correct_blocks(&self, corrections: Vec<(Chunk, Arc<[u8]>)>, ack: Arc<[u8]>) {
        let mut registry = lock(&self.0.registry);
        let uuid = self.0.uuid;
        for (chunk, body) in corrections {
            registry.enqueue_terrain(uuid, chunk, Arc::from([body]), None);
        }
        if let Some(slot) = self.0.slot(&mut registry)
            && !slot.lagged
            && !slot.enqueue(Delivery::Packet(ack))
        {
            registry.deactivate(uuid);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::packets::play::{KeepAlive, TimeUpdate, WorldBorder};
    use std::sync::Barrier;

    #[test]
    fn admission_is_atomic_for_both_capacity_and_identity() {
        for duplicate in [false, true] {
            let players = Players::new(if duplicate { 16 } else { 1 });
            let barrier = Barrier::new(16);
            let reservations = std::thread::scope(|scope| {
                let mut attempts = Vec::new();
                for i in 0..16 {
                    let players = &players;
                    let barrier = &barrier;
                    attempts.push(scope.spawn(move || {
                        barrier.wait();
                        players.reserve(Uuid::from_u128(if duplicate { 0 } else { i }))
                    }));
                }
                attempts
                    .into_iter()
                    .map(|attempt| attempt.join().unwrap())
                    .collect::<Vec<_>>()
            });
            assert_eq!(
                reservations.iter().filter(|result| result.is_ok()).count(),
                1
            );
            let expected = if duplicate {
                AdmissionError::Duplicate
            } else {
                AdmissionError::Full
            };
            assert!(
                reservations
                    .iter()
                    .filter_map(|result| result.as_ref().err())
                    .all(|err| err == &expected)
            );
            assert_eq!(players.online_count(), 0);
            drop(reservations);
            assert!(players.reserve(Uuid::nil()).is_ok());
        }
        assert_eq!(
            Players::new(0).reserve(Uuid::nil()).unwrap_err(),
            AdmissionError::Full
        );
    }

    #[test]
    fn broadcast_allowlist_has_fixed_bounded_encodings() {
        use crate::protocol::packets::play::DefaultSpawnPosition;

        let players = Players::new(0);
        for (packet, size) in [
            (Clientbound::from(TimeUpdate), 17),
            (Clientbound::from(WorldBorder), 40),
            (Clientbound::from(DefaultSpawnPosition), 13),
        ] {
            assert_eq!(packet.encode().unwrap().len(), size);
            assert!(players.broadcast(&packet).is_ok());
        }
    }

    fn ready_view(reservation: &Reservation) {
        reservation.set_view([0, 0], 2);
        reservation.chunk_sent([0, 0]);
    }

    fn peer(i: u128) -> Peer {
        Peer {
            uuid: Uuid::from_u128(i),
            name: Arc::from(format!("Player{i}")),
            id: i as i32,
            pose: EntityPose::new([0.0; 3], 0.0, 0.0, false),
            info: Arc::from([0x3e]),
        }
    }

    fn pose_in([x, z]: Chunk, yaw: f32) -> EntityPose {
        EntityPose::new(
            [f64::from(x) * 16.0 + 0.5, 65.0, f64::from(z) * 16.0 + 0.5],
            yaw,
            0.0,
            false,
        )
    }

    fn assert_visibility_matches_scan(players: &Players, reservations: &[Option<Reservation>]) {
        for reservation in reservations.iter().flatten() {
            while reservation.inbox().pop().is_some() {}
        }
        let registry = lock(&players.registry);
        let mut expected_entities = ChunkIndex::default();
        let mut expected_watchers = ChunkIndex::default();
        for (&uuid, slot) in &registry.players {
            let mut expected_wire = ChunkIndex::default();
            if let Some(peer) = &slot.visible {
                expected_entities.insert(pose_chunk(peer.pose), uuid);
                for &chunk in &slot.sent_chunks {
                    if in_view(slot.view, chunk) {
                        expected_watchers.insert(chunk, uuid);
                    }
                }
                let mut tab_count = 0;
                for (&other, remote) in &registry.players {
                    let Some(source) = remote.visible.as_ref().filter(|_| other != uuid) else {
                        continue;
                    };
                    tab_count += 1;
                    let chunk = pose_chunk(source.pose);
                    let known = slot.known.get(&other).expect("online peer in Tab");
                    assert_eq!(known.id, source.id);
                    assert_eq!(known.pose, slot.tracks(chunk).then_some(source.pose));
                    if known.pose.is_some() {
                        expected_wire.insert(chunk, other);
                    }
                }
                assert_eq!(slot.known.len(), tab_count);
            } else if !slot.lagged {
                assert!(slot.known.is_empty());
            }
            if !slot.lagged {
                assert_eq!(slot.wire_entities.0, expected_wire.0);
            }
        }
        assert_eq!(registry.entities.0, expected_entities.0);
        assert_eq!(registry.watchers.0, expected_watchers.0);
    }

    #[test]
    fn spatial_indexes_match_scan_across_batched_lifecycle_changes() {
        let players = Players::new(8);
        let mut reservations: Vec<_> = (1..=8)
            .map(|i| Some(players.reserve(Uuid::from_u128(i)).unwrap()))
            .collect();
        let mut serial = 8;
        for round in 0..96i32 {
            for (index, slot) in reservations.iter_mut().enumerate() {
                let i = index as i32;
                let x = (round + 2 * i).rem_euclid(7) - 4;
                let z = (round / 2 + i).rem_euclid(5) - 3;
                let chunk = [x, z];
                if (round + i) % 11 == 0 {
                    drop(slot.take());
                    *slot = Some(players.reserve(Uuid::from_u128(index as u128 + 1)).unwrap());
                }
                let reservation = slot.as_ref().unwrap();
                if (round + i) % 7 == 0 {
                    reservation.pause();
                }
                reservation.set_view(chunk, 2 + (round + i) % 3);
                reservation.before_unload(&HashSet::from([[x - 2, z], [x + 2, z + 1]]));
                for offset in [-1, 0, 1] {
                    reservation.chunk_sent([x + offset, z]);
                    reservation.chunk_sent([x, z + offset]);
                }
                // Also exercise a later view expansion over a sent but out-of-view chunk.
                reservation.chunk_sent([x + 4, z]);
                serial += 1;
                let mut source = peer(index as u128 + 1);
                source.id = serial;
                source.pose = pose_in(chunk, round as f32);
                reservation.show(source);
                reservation.update_pose(pose_in([x - 1, z], 30.0));
                reservation.update_pose(pose_in(chunk, 60.0));
            }
            assert_visibility_matches_scan(&players, &reservations);
        }
        reservations.clear();
        let registry = lock(&players.registry);
        assert!(registry.players.is_empty());
        assert!(registry.entities.0.is_empty());
        assert!(registry.watchers.0.is_empty());
    }

    #[test]
    fn sparse_grid_notifies_only_old_and_new_chunk_watchers() {
        let players = Players::new(68);
        let reservations: Vec<_> = (1..=68)
            .map(|i| players.reserve(Uuid::from_u128(i)).unwrap())
            .collect();
        for (index, reservation) in reservations.iter().enumerate() {
            let chunk = match index {
                0 | 1 => [-1, 0],
                2 => [0, 0],
                3 => [1, 0],
                _ => [100 * index as i32, -100],
            };
            reservation.set_view(chunk, 2);
            reservation.chunk_sent(chunk);
            let mut source = peer(index as u128 + 1);
            source.pose = pose_in(chunk, 0.0);
            reservation.show(source);
        }
        for reservation in &reservations {
            while reservation.inbox().pop().is_some() {}
        }
        let source = Uuid::from_u128(1);
        reservations[0].update_pose(pose_in([-1, 0], 90.0));
        let dirty = || {
            let registry = lock(&players.registry);
            registry
                .players
                .iter()
                .filter_map(|(&uuid, slot)| slot.dirty.contains(&source).then_some(uuid))
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(dirty(), BTreeSet::from([Uuid::from_u128(2)]));
        assert!(matches!(
            reservations[1].inbox().pop(),
            Some(Delivery::Move { .. })
        ));
        reservations[0].update_pose(pose_in([0, 0], 180.0));
        assert_eq!(
            dirty(),
            BTreeSet::from([Uuid::from_u128(2), Uuid::from_u128(3)])
        );
        assert!(matches!(
            reservations[1].inbox().pop(),
            Some(Delivery::Despawn(1))
        ));
        assert!(matches!(
            reservations[2].inbox().pop(),
            Some(Delivery::Spawn(_))
        ));
        reservations[3].chunk_sent([0, 0]);
        assert!(matches!(
            reservations[3].inbox().pop(),
            Some(Delivery::Spawn(_))
        ));
        reservations[3].set_view([5, 0], 2);
        assert!(matches!(
            reservations[3].inbox().pop(),
            Some(Delivery::Despawn(1))
        ));
        assert!(dirty().is_empty());
    }

    #[test]
    fn tab_is_global_but_spawn_requires_a_sent_in_range_chunk() {
        let players = Players::new(2);
        let alice = players.reserve(peer(1).uuid).unwrap();
        let bob = players.reserve(peer(2).uuid).unwrap();
        let pose = |x, yaw| EntityPose::new([x, 65.0, 0.0], yaw, 0.0, false);
        ready_view(&alice);
        bob.set_view([10, 0], 2);
        bob.chunk_sent([10, 0]);
        alice.show(peer(1));
        bob.show(Peer {
            pose: pose(160.0, 0.0),
            ..peer(2)
        });
        let view = alice.inbox();
        assert!(matches!(view.pop(), Some(Delivery::Add { peer, spawn: false }) if peer.id == 2));
        assert!(
            matches!(bob.inbox().pop(), Some(Delivery::Add { peer, spawn: false }) if peer.id == 1)
        );
        bob.update_pose(pose(168.0, 180.0));
        assert!(view.pop().is_none());

        let latest = pose(48.5, 90.0);
        bob.update_pose(latest);
        alice.set_view([0, 0], 3);
        assert!(view.pop().is_none(), "desired is not yet sent");
        alice.chunk_sent([3, 0]);
        assert!(matches!(view.pop(), Some(Delivery::Spawn(p)) if p.id == 2 && p.pose == latest));
        assert!(bob.inbox().pop().is_none(), "visibility is directional");

        // Shrinking the window invalidates pending pose updates immediately.
        bob.update_pose(pose(49.0, 0.0));
        alice.set_view([0, 0], 2);
        assert_eq!(
            alice.before_unload(&HashSet::from([[3, 0]])),
            [RemoveEntities(vec![2]).into()]
        );
        assert!(view.pop().is_none(), "no duplicate despawn or stale move");
        alice.set_view([0, 0], 3);
        assert!(
            view.pop().is_none(),
            "resizing cannot restore unloaded chunks"
        );
        alice.chunk_sent([3, 0]);
        assert!(matches!(view.pop(), Some(Delivery::Spawn(p)) if p.id == 2));
        bob.update_pose(pose(160.0, 0.0));
        assert!(matches!(view.pop(), Some(Delivery::Despawn(2))));
        drop(bob);
        assert!(matches!(
            view.pop(),
            Some(Delivery::Remove { id: None, .. })
        ));
    }

    #[test]
    fn reconfiguration_clears_known_players_and_rebuilds_from_latest_pose() {
        let players = Players::new(2);
        let alice = players.reserve(peer(1).uuid).unwrap();
        let bob = players.reserve(peer(2).uuid).unwrap();
        alice.enter_play();
        bob.enter_play();
        ready_view(&alice);
        ready_view(&bob);
        alice.show(peer(1));
        bob.show(peer(2));
        assert!(
            matches!(alice.inbox().pop(), Some(Delivery::Add { peer, spawn: true }) if peer.id == 2)
        );
        assert!(
            matches!(bob.inbox().pop(), Some(Delivery::Add { peer, spawn: true }) if peer.id == 1)
        );
        let pose = EntityPose::new([7.0; 3], 90.0, 0.0, false);
        alice.update_pose(pose);
        let replies = alice.pause();
        assert_eq!(
            replies,
            [
                RemoveEntities(vec![2]).into(),
                RemovePlayerInfo(vec![peer(2).uuid]).into()
            ]
        );
        assert_eq!(players.online_count(), 2);
        let inbox = bob.inbox();
        assert!(matches!(
            inbox.pop(),
            Some(Delivery::Remove { id: Some(1), .. })
        ));
        assert!(inbox.pop().is_none());
        bob.update_pose(pose);
        alice.enter_play();
        assert!(alice.inbox().pop().is_none());
        ready_view(&alice);
        alice.show(peer(1));
        assert!(matches!(inbox.pop(), Some(Delivery::Add { peer, spawn: true }) if peer.id == 1));
        assert!(
            matches!(alice.inbox().pop(), Some(Delivery::Add { peer, spawn: true }) if peer.id == 2 && peer.pose == pose)
        );
        drop(bob);
        // Pausing before flushing a departure still removes the known entry.
        assert_eq!(
            alice.pause(),
            [
                RemoveEntities(vec![2]).into(),
                RemovePlayerInfo(vec![peer(2).uuid]).into()
            ]
        );
        assert_eq!(players.online_count(), 1);
    }

    #[test]
    fn simultaneous_entry_delivers_each_peer_once() {
        let players = Players::new(8);
        let reservations: Vec<_> = (1..=8)
            .map(|i| players.reserve(peer(i).uuid).unwrap())
            .collect();
        let barrier = Barrier::new(reservations.len());
        std::thread::scope(|scope| {
            for (index, reservation) in reservations.iter().enumerate() {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    ready_view(reservation);
                    reservation.show(peer(index as u128 + 1));
                });
            }
        });
        for (index, reservation) in reservations.iter().enumerate() {
            let mut actual = Vec::new();
            while let Some(Delivery::Add { peer, spawn: true }) = reservation.inbox().pop() {
                actual.push(peer.id);
            }
            actual.sort_unstable();
            assert_eq!(
                actual,
                (1..=8)
                    .filter(|&id| id != index as i32 + 1)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn initial_roster_exceeds_live_queue_and_skips_unseen_departures() {
        let players = Players::new(50);
        let mut peers: Vec<Reservation> = Vec::new();
        for i in 1..45 {
            let reservation = players.reserve(peer(i).uuid).unwrap();
            reservation.show(peer(i));
            peers.push(reservation);
            for peer in &peers {
                while peer.inbox().pop().is_some() {}
            }
        }
        let newcomer = players.reserve(peer(50).uuid).unwrap();
        newcomer.show(peer(50));
        drop(peers.pop().unwrap());
        let inbox = newcomer.inbox();
        let mut seen = Vec::new();
        while let Some(delivery) = inbox.pop() {
            let Delivery::Add { peer, spawn: false } = delivery else {
                panic!("unexpected entity or departure")
            };
            seen.push(peer.id);
        }
        seen.sort_unstable();
        assert_eq!(seen, (1..44).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn overflow_isolates_the_slow_recipient_and_preserves_healthy_order() {
        let players = Players::new(3);
        let slow = players.reserve(Uuid::nil()).unwrap();
        let healthy = players.reserve(Uuid::from_u128(1)).unwrap();
        let _pending = players.reserve(Uuid::from_u128(2)).unwrap();
        slow.enter_play();
        healthy.enter_play();
        let healthy_inbox = healthy.inbox();
        for index in 0..BROADCAST_QUEUE_CAPACITY {
            let packet = if index % 2 == 0 {
                TimeUpdate.into()
            } else {
                WorldBorder.into()
            };
            assert_eq!(
                players.broadcast(&packet).unwrap(),
                BroadcastReport {
                    queued: 2,
                    lagged: 0
                }
            );
            assert_eq!(
                &*healthy_inbox.next().await.bodies().unwrap().next().unwrap(),
                packet.encode().unwrap()
            );
        }
        let packet = TimeUpdate.into();
        assert_eq!(
            players.broadcast(&packet).unwrap(),
            BroadcastReport {
                queued: 1,
                lagged: 1
            }
        );
        slow.inbox().cancelled().await;
        assert!(slow.inbox().pop().is_none());
        assert_eq!(
            players.broadcast(&WorldBorder.into()).unwrap(),
            BroadcastReport {
                queued: 1,
                lagged: 0
            }
        );
        for id in [0x64, 0x25] {
            assert_eq!(
                healthy_inbox.next().await.bodies().unwrap().next().unwrap()[0],
                id
            );
        }
        assert!(players.broadcast(&KeepAlive { id: 0 }.into()).is_err());
        drop(slow);
        assert_eq!(players.online_ids(), vec![Uuid::from_u128(1)]);
    }

    #[test]
    fn pausing_discards_packets_and_stale_inbox_cannot_read_a_new_reservation() {
        let players = Players::new(1);
        let first = players.reserve(Uuid::nil()).unwrap();
        let stale = first.inbox();
        first.enter_play();
        let packet = TimeUpdate.into();
        assert_eq!(players.broadcast(&packet).unwrap().queued, 1);
        first.pause();
        assert_eq!(players.online_count(), 1);
        assert!(stale.pop().is_none());
        assert_eq!(players.broadcast(&packet).unwrap().queued, 0);
        first.enter_play();
        first.enter_play();
        assert_eq!(players.online_count(), 1);
        assert_eq!(players.broadcast(&packet).unwrap().queued, 1);
        drop(first);
        let second = players.reserve(Uuid::nil()).unwrap();
        second.enter_play();
        assert_eq!(players.broadcast(&packet).unwrap().queued, 1);
        assert!(stale.pop().is_none());
        assert!(second.inbox().pop().is_some());
    }
}
