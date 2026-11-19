//! Authenticated connection reservations and bounded Play packet fanout.
use crate::protocol::{Clientbound, invalid, packets::play_entities::*};
use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    io,
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

#[derive(Default, Debug)]
struct Registry {
    players: HashMap<Uuid, Slot>,
    online: u32,
}

#[derive(Debug)]
struct Slot {
    signals: Arc<Signals>,
    online: bool,
    receiving: bool,
    lagged: bool,
    queue: VecDeque<Delivery>,
    visible: Option<Peer>,
    snapshot: VecDeque<Peer>,
    // Entries handed to the single connection writer. Reconfiguration removes
    // these before switching state, including peers with a queued departure.
    known: HashMap<Uuid, i32>,
}

impl Slot {
    fn enqueue(&mut self, delivery: Delivery) -> bool {
        if self.queue.len() == BROADCAST_QUEUE_CAPACITY {
            self.lagged = true;
            self.queue.clear();
            self.snapshot.clear();
            self.signals.lagged.notify_one();
            false
        } else {
            self.queue.push_back(delivery);
            self.signals.ready.notify_one();
            true
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Peer {
    pub uuid: Uuid,
    pub id: i32,
    pub pose: EntityPose,
    pub info: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub(crate) enum Delivery {
    Packet(Arc<[u8]>),
    Spawn(Peer),
    Remove {
        uuid: Uuid,
        id: i32,
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
        match self {
            Self::Packet(body) => bodies[0] = Some(body),
            Self::Spawn(Peer {
                uuid,
                id,
                pose,
                info,
            }) => {
                bodies[0] = Some(info);
                bodies[1] = Some(encode(SpawnPlayer { id, uuid, pose }.into())?);
            }
            Self::Remove { uuid, id } => {
                bodies[0] = Some(encode(RemoveEntities(vec![id]).into())?);
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
        Ok(bodies.into_iter().flatten())
    }
}

impl Registry {
    fn fanout_entity(&mut self, source: Uuid, delivery: Delivery) {
        for (&uuid, slot) in &mut self.players {
            if uuid != source && slot.visible.is_some() && !slot.lagged {
                slot.enqueue(delivery.clone());
            }
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
                    lagged: false,
                    queue: VecDeque::new(),
                    visible: None,
                    snapshot: VecDeque::new(),
                    known: HashMap::new(),
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
        for slot in lock(&self.registry).players.values_mut() {
            if !slot.receiving || slot.lagged {
                continue;
            }
            if slot.enqueue(Delivery::Packet(body.clone())) {
                report.queued += 1;
            } else {
                report.lagged += 1;
            }
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
        let Some(slot) = self.0.slot(&mut registry) else {
            return;
        };
        if slot.lagged || slot.visible.is_some() {
            return;
        }
        // A capacity-bounded initial snapshot is separate from the 32-event
        // live FIFO. Mutations after this lock append behind it atomically.
        let snapshot = registry
            .players
            .iter()
            .filter_map(|(&other, slot)| {
                (other != uuid && !slot.lagged)
                    .then(|| slot.visible.clone())
                    .flatten()
            })
            .collect();
        let slot = &mut registry.players.get_mut(&uuid).expect("owned slot");
        slot.snapshot = snapshot;
        slot.visible = Some(peer.clone());
        slot.signals.ready.notify_one();
        registry.fanout_entity(uuid, Delivery::Spawn(peer));
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
            let id = peer.id;
            registry.fanout_entity(self.0.uuid, Delivery::Move { id, from, to });
        }
    }

    pub(crate) fn pause(&self) -> Vec<Clientbound> {
        let mut registry = lock(&self.0.registry);
        let Some(slot) = self.0.slot(&mut registry) else {
            return Vec::new();
        };
        slot.receiving = false;
        slot.queue.clear();
        slot.snapshot.clear();
        let known = std::mem::take(&mut slot.known);
        if let Some(peer) = slot.visible.take() {
            registry.fanout_entity(
                peer.uuid,
                Delivery::Remove {
                    uuid: peer.uuid,
                    id: peer.id,
                },
            );
        }
        drop(registry);
        // Fixed-size chunks bound the wire size even with a large max_players.
        let mut known = known.into_iter().peekable();
        let mut replies = Vec::new();
        while known.peek().is_some() {
            let (ids, uuids): (Vec<_>, Vec<_>) = known
                .by_ref()
                .take(256)
                .map(|(uuid, id)| (id, uuid))
                .unzip();
            replies.extend([RemoveEntities(ids).into(), RemovePlayerInfo(uuids).into()]);
        }
        replies
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut registry = lock(&self.0.registry);
        if let Some(slot) = self.0.slot(&mut registry) {
            let online = slot.online;
            let visible = slot.visible.take();
            registry.players.remove(&self.0.uuid);
            if online {
                registry.online -= 1;
            }
            if let Some(peer) = visible {
                registry.fanout_entity(
                    peer.uuid,
                    Delivery::Remove {
                        uuid: peer.uuid,
                        id: peer.id,
                    },
                );
            }
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
        registry
            .players
            .get_mut(&self.uuid)
            .filter(|slot| Arc::ptr_eq(&self.signals, &slot.signals))
    }

    fn pop(&self) -> Option<Delivery> {
        let mut registry = lock(&self.registry);
        let slot = self.slot(&mut registry)?;
        loop {
            let delivery = slot
                .snapshot
                .pop_front()
                .map(Delivery::Spawn)
                .or_else(|| slot.queue.pop_front())?;
            match &delivery {
                Delivery::Spawn(peer) => {
                    slot.known.insert(peer.uuid, peer.id);
                }
                // A lagged peer excluded from the initial snapshot can still
                // depart while the recipient is active. It was never known.
                Delivery::Remove { uuid, .. } if slot.known.remove(uuid).is_none() => continue,
                _ => {}
            }
            return Some(delivery);
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

    fn peer(i: u128) -> Peer {
        Peer {
            uuid: Uuid::from_u128(i),
            id: i as i32,
            pose: EntityPose::new([0.0; 3], 0.0, 0.0, false),
            info: Arc::from([0x3e]),
        }
    }

    #[test]
    fn reconfiguration_clears_known_players_and_rebuilds_from_latest_pose() {
        let players = Players::new(2);
        let alice = players.reserve(peer(1).uuid).unwrap();
        let bob = players.reserve(peer(2).uuid).unwrap();
        alice.enter_play();
        bob.enter_play();
        alice.show(peer(1));
        bob.show(peer(2));
        assert!(matches!(alice.inbox().pop(), Some(Delivery::Spawn(p)) if p.id == 2));
        assert!(matches!(bob.inbox().pop(), Some(Delivery::Spawn(p)) if p.id == 1));
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
        assert!(matches!(inbox.pop(), Some(Delivery::Move { id: 1, to, .. }) if to == pose));
        assert!(matches!(inbox.pop(), Some(Delivery::Remove { id: 1, .. })));
        bob.update_pose(pose);
        alice.enter_play();
        assert!(alice.inbox().pop().is_none());
        alice.show(peer(1));
        assert!(matches!(inbox.pop(), Some(Delivery::Spawn(p)) if p.id == 1));
        assert!(
            matches!(alice.inbox().pop(), Some(Delivery::Spawn(p)) if p.id == 2 && p.pose == pose)
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
                    reservation.show(peer(index as u128 + 1));
                });
            }
        });
        for (index, reservation) in reservations.iter().enumerate() {
            let mut actual = Vec::new();
            while let Some(Delivery::Spawn(p)) = reservation.inbox().pop() {
                actual.push(p.id);
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
    fn initial_snapshot_exceeds_live_queue_and_precedes_concurrent_deltas() {
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
        for _ in 0..44 {
            let Some(Delivery::Spawn(p)) = inbox.pop() else {
                panic!("incomplete snapshot")
            };
            seen.push(p.id);
        }
        seen.sort_unstable();
        assert_eq!(seen, (1..45).collect::<Vec<_>>());
        assert!(matches!(inbox.pop(), Some(Delivery::Remove { id: 44, .. })));
        assert!(inbox.pop().is_none());
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
