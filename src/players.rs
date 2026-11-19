//! Authenticated connection reservations and bounded Play packet fanout.
use crate::protocol::{Clientbound, invalid};
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
    queue: VecDeque<Arc<[u8]>>,
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
            if slot.queue.len() == BROADCAST_QUEUE_CAPACITY {
                slot.lagged = true;
                slot.queue.clear();
                slot.signals.lagged.notify_one();
                report.lagged += 1;
            } else {
                slot.queue.push_back(body.clone());
                slot.signals.ready.notify_one();
                report.queued += 1;
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

    pub(crate) fn pause(&self) {
        if let Some(slot) = self.0.slot(&mut lock(&self.0.registry)) {
            slot.receiving = false;
            slot.queue.clear();
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut registry = lock(&self.0.registry);
        if let Some(slot) = self.0.slot(&mut registry) {
            if slot.online {
                registry.online -= 1;
            }
            registry.players.remove(&self.0.uuid);
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

    fn pop(&self) -> Option<Arc<[u8]>> {
        self.slot(&mut lock(&self.registry))?.queue.pop_front()
    }

    pub(crate) async fn next(&self) -> Arc<[u8]> {
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
            assert_eq!(&*healthy_inbox.next().await, packet.encode().unwrap());
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
        assert_eq!(healthy_inbox.next().await[0], 0x64);
        assert_eq!(healthy_inbox.next().await[0], 0x25);
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
