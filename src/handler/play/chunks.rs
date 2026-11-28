//! Per-session chunk window and one-in-flight batch pacing.
use crate::protocol::{
    Clientbound, invalid,
    packets::play::{
        CenterChunk, ChunkBatchFinished, ChunkBatchStart, ChunkData, ChunkViewDistance, ForgetChunk,
    },
};
use std::{
    collections::{BTreeSet, VecDeque},
    io,
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;

const STREAM_INTERVAL: Duration = Duration::from_millis(50);
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(15);
const INITIAL_BATCH: usize = 9;
const MAX_BATCH: usize = 64;
const MAX_UNACKED_AFTER_FEEDBACK: usize = 10;
const MAX_UNLOAD: usize = 32;

type Chunk = [i32; 2];

#[derive(Debug)]
pub(super) struct ChunkStream {
    center: Chunk,
    advertised_center: Chunk,
    radius: i32,
    advertised_radius: i32,
    loaded: BTreeSet<Chunk>,
    // A chunk whose subscription was removed must reload even if
    // the client moves back into range before its unload is written.
    retiring: BTreeSet<Chunk>,
    // Old chunks are drained before adding any new ones, so the set never
    // exceeds the largest possible view square, even on huge jumps.
    unload_remaining: bool,
    writing: bool,
    unacked: VecDeque<Instant>,
    max_unacked: usize,
    batch_size: usize,
    next_batch: Instant,
    next: Instant,
}

impl ChunkStream {
    #[cfg(test)]
    pub(super) fn new(radius: i32, now: Instant) -> Self {
        Self::at([0, 0], radius, now)
    }

    pub(super) fn at(center: Chunk, radius: i32, now: Instant) -> Self {
        Self {
            center,
            advertised_center: center,
            radius,
            advertised_radius: radius,
            loaded: BTreeSet::new(),
            retiring: BTreeSet::new(),
            unload_remaining: false,
            writing: false,
            unacked: VecDeque::new(),
            max_unacked: 1,
            batch_size: INITIAL_BATCH,
            next_batch: now,
            next: now,
        }
    }

    pub(super) fn follow(&mut self, x: f64, z: f64) {
        self.center = crate::world::chunk_at(x, z);
    }

    pub(super) fn set_radius(&mut self, radius: i32) {
        self.radius = radius;
    }

    pub(super) fn view(&self) -> ([i32; 2], i32) {
        (self.center, self.radius)
    }

    pub(super) fn is_loaded(&self, chunk: Chunk) -> bool {
        self.loaded.contains(&chunk) && !self.retiring.contains(&chunk)
    }

    /// Immediately shift the chunk stream to a distant teleport destination:
    /// emits `ForgetChunk` for chunks outside the new view, updates `CenterChunk`,
    /// and pumps the nearest destination chunk batch so the client and server
    /// cache both have terrain ready before `PlayerPosition` arrives.
    pub(super) fn prepare_teleport(
        &mut self,
        dest: Chunk,
        terrain: &Arc<[u8]>,
        now: Instant,
    ) -> (std::collections::HashSet<Chunk>, Vec<Clientbound>) {
        self.center = dest;
        self.advertised_center = dest;
        let [cx, cz] = dest;
        let limit = self.stream_radius();
        let unloaded: std::collections::HashSet<Chunk> = self
            .loaded
            .iter()
            .copied()
            .filter(|&[x, z]| (x - cx).abs().max((z - cz).abs()) > limit)
            .collect();
        self.retiring.clear();
        self.unload_remaining = false;
        self.next = now + STREAM_INTERVAL;

        let mut packets = Vec::new();
        for &c in &unloaded {
            packets.push(ForgetChunk(c).into());
        }
        packets.push(CenterChunk(dest).into());

        let mut missing = Vec::new();
        for x in cx - limit..=cx + limit {
            for z in cz - limit..=cz + limit {
                if !self.loaded.contains(&[x, z]) || unloaded.contains(&[x, z]) {
                    missing.push([x, z]);
                }
            }
        }
        if !missing.is_empty() && self.can_send_batch() {
            missing.sort_unstable_by_key(|&[x, z]| {
                let (dx, dz) = ((x - cx).abs(), (z - cz).abs());
                (dx.max(dz), dx * dx + dz * dz, x, z)
            });
            let count = missing.len().min(self.batch_size.max(INITIAL_BATCH));
            self.writing = true;
            packets.push(ChunkBatchStart.into());
            packets.extend(
                missing
                    .into_iter()
                    .take(count)
                    .map(|c| ChunkData(c, terrain.clone()).into()),
            );
            packets.push(
                ChunkBatchFinished {
                    count: count as i32,
                }
                .into(),
            );
        }
        (unloaded, packets)
    }

    /// Vanilla clients require a 1-chunk neighbor border around `view_distance`
    /// before meshing and rendering the outermost visible chunks.
    fn stream_radius(&self) -> i32 {
        self.radius + 1
    }

    pub(super) fn retire_outside(&mut self) -> std::collections::HashSet<Chunk> {
        let [cx, cz] = self.center;
        let limit = self.stream_radius();
        self.loaded
            .iter()
            .copied()
            .filter(|&[x, z]| (x - cx).abs().max((z - cz).abs()) > limit)
            .filter(|&chunk| self.retiring.insert(chunk))
            .collect()
    }

    fn desired_count(&self) -> usize {
        (2 * self.stream_radius() + 1).pow(2) as usize
    }

    fn can_send_batch(&self) -> bool {
        !self.writing && self.unacked.len() < self.max_unacked
    }

    pub(super) fn next_update(&self) -> Option<Instant> {
        if self.center != self.advertised_center
            || self.radius != self.advertised_radius
            || self.unload_remaining
            || !self.retiring.is_empty()
        {
            Some(self.next)
        } else if self.can_send_batch() && self.loaded.len() < self.desired_count() {
            Some(self.next.max(self.next_batch))
        } else {
            None
        }
    }

    pub(super) fn pump(&mut self, terrain: &Arc<[u8]>, now: Instant) -> Vec<Clientbound> {
        if !self.next_update().is_some_and(|at| now >= at) {
            return Vec::new();
        }
        self.next = now + STREAM_INTERVAL;
        let mut replies = Vec::new();
        if self.radius != self.advertised_radius {
            self.advertised_radius = self.radius;
            replies.push(ChunkViewDistance(self.radius).into());
        }
        if self.center != self.advertised_center {
            self.advertised_center = self.center;
            replies.push(CenterChunk(self.center).into());
        }
        let [cx, cz] = self.center;
        let limit = self.stream_radius();
        let mut outside = self.loaded.iter().filter(|&&[x, z]| {
            (x - cx).abs().max((z - cz).abs()) > limit || self.retiring.contains(&[x, z])
        });
        replies.extend(
            outside
                .by_ref()
                .take(MAX_UNLOAD)
                .map(|&c| ForgetChunk(c).into()),
        );
        self.unload_remaining = outside.next().is_some();
        if self.unload_remaining || !self.can_send_batch() || now < self.next_batch {
            return replies;
        }

        let mut missing = Vec::new();
        for x in cx - limit..=cx + limit {
            for z in cz - limit..=cz + limit {
                if !self.loaded.contains(&[x, z]) {
                    missing.push([x, z]);
                }
            }
        }
        if !missing.is_empty() {
            missing.sort_unstable_by_key(|&[x, z]| {
                let (dx, dz) = ((x - cx).abs(), (z - cz).abs());
                (dx.max(dz), dx * dx + dz * dz, x, z)
            });
            let count = missing.len().min(self.batch_size);
            self.writing = true;
            replies.push(ChunkBatchStart.into());
            replies.extend(
                missing
                    .into_iter()
                    .take(count)
                    .map(|c| ChunkData(c, terrain.clone()).into()),
            );
            replies.push(
                ChunkBatchFinished {
                    count: count as i32,
                }
                .into(),
            );
        }
        replies
    }

    pub(super) fn packet_sent(&mut self, packet: &Clientbound, now: Instant) {
        match packet {
            Clientbound::ChunkData(packet) => {
                self.loaded.insert(packet.0);
            }
            Clientbound::ForgetChunk(packet) => {
                self.loaded.remove(&packet.0);
                self.retiring.remove(&packet.0);
            }
            Clientbound::ChunkBatchFinished(_) => {
                self.writing = false;
                self.unacked.push_back(now);
                self.next = now + STREAM_INTERVAL;
            }
            _ => {}
        }
    }

    pub(super) fn feedback(&mut self, rate: f32, now: Instant) -> io::Result<()> {
        if self.unacked.is_empty() {
            return Err(invalid("unexpected chunk batch feedback"));
        }
        if self.feedback_timed_out(now) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "chunk batch feedback timed out",
            ));
        }
        self.unacked.pop_front();
        // Matches vanilla `PlayerChunkSender.onChunkBatchReceivedByClient`:
        // clamp desired chunks/tick to 0.01..=64.0 and raise max unacked to 10.
        self.max_unacked = MAX_UNACKED_AFTER_FEEDBACK;
        let rate = rate.clamp(0.01, MAX_BATCH as f32);
        self.batch_size = (rate as usize).max(1);
        self.next_batch = now
            + Duration::from_secs_f64(
                STREAM_INTERVAL.as_secs_f64() * self.batch_size as f64 / f64::from(rate),
            );
        Ok(())
    }

    pub(super) fn feedback_timed_out(&self, now: Instant) -> bool {
        self.unacked
            .front()
            .is_some_and(|&sent| now >= sent + FEEDBACK_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(stream: &mut ChunkStream, packets: &[Clientbound], now: Instant) {
        for packet in packets {
            match packet {
                Clientbound::ChunkData(packet) => assert!(!stream.loaded.contains(&packet.0)),
                Clientbound::ForgetChunk(packet) => assert!(stream.loaded.contains(&packet.0)),
                _ => {}
            }
            stream.packet_sent(packet, now);
            assert!(stream.loaded.len() <= 35 * 35);
        }
    }

    fn settle(stream: &mut ChunkStream, template: &Arc<[u8]>, mut now: Instant) -> Instant {
        for _ in 0..500 {
            let Some(next) = stream.next_update() else {
                return now;
            };
            now = now.max(next);
            let packets = stream.pump(template, now);
            assert!(packets.len() <= MAX_UNLOAD + MAX_BATCH + 4);
            for packet in &packets {
                if let Clientbound::ChunkData(data) = packet {
                    assert!(Arc::ptr_eq(&data.1, template));
                }
            }
            apply(stream, &packets, now);
            while !stream.unacked.is_empty() {
                stream.feedback(f32::MAX, now).unwrap();
            }
        }
        panic!("chunk window did not settle");
    }

    #[test]
    fn follows_negative_coordinates_and_keeps_overlap() {
        let now = Instant::now();
        let terrain = crate::world::chunk_payload();
        let mut stream = ChunkStream::new(2, now);
        let now = settle(&mut stream, &terrain, now);
        let old = stream.loaded.clone();
        stream.follow(-0.5, 15.999);
        assert_eq!(stream.center, [-1, 0]);
        let now = settle(&mut stream, &terrain, now);
        assert_eq!(stream.loaded.len(), 49);
        assert_eq!(stream.loaded.intersection(&old).count(), 42);
        stream.follow(-16.001, 16.0);
        stream.set_radius(3);
        let packets = stream.pump(&terrain, now + STREAM_INTERVAL);
        assert!(matches!(
            &packets[..2],
            [
                Clientbound::ChunkViewDistance(ChunkViewDistance(3)),
                Clientbound::CenterChunk(CenterChunk([-2, 1]))
            ]
        ));
    }

    #[test]
    fn outstanding_batch_survives_replan_and_starts_deadline_on_write() {
        let start = Instant::now();
        let terrain = Arc::from([]);
        let mut stream = ChunkStream::new(3, start);
        let packets = stream.pump(&terrain, start);
        assert!(matches!(
            &packets[1],
            Clientbound::ChunkData(ChunkData([0, 0], _))
        ));
        let sent = start + Duration::from_secs(25);
        assert!(!stream.feedback_timed_out(sent));
        apply(&mut stream, &packets, sent);
        stream.follow(1024.0, -1024.0);
        let updates = stream.pump(&terrain, sent + STREAM_INTERVAL);
        assert!(
            updates
                .iter()
                .all(|p| !matches!(p, Clientbound::ChunkData(_)))
        );
        apply(&mut stream, &updates, sent + STREAM_INTERVAL);
        let latest_valid = sent + FEEDBACK_TIMEOUT - Duration::from_millis(1);
        assert!(!stream.feedback_timed_out(latest_valid));
        assert!(stream.feedback_timed_out(sent + FEEDBACK_TIMEOUT));
        stream.feedback(0.001, latest_valid).unwrap();
        assert!(stream.pump(&terrain, latest_valid).is_empty());
        let next = stream.next_update().unwrap();
        assert!(next >= latest_valid + Duration::from_secs(5));
        let packets = stream.pump(&terrain, next);
        assert!(matches!(
            &packets[..],
            [
                Clientbound::ChunkBatchStart(_),
                Clientbound::ChunkData(ChunkData([64, -64], _)),
                Clientbound::ChunkBatchFinished(ChunkBatchFinished { count: 1 })
            ]
        ));
    }

    #[test]
    fn fractional_budget_slows_loading_without_delaying_unloads() {
        let now = Instant::now();
        let terrain = crate::world::chunk_payload();
        let mut stream = ChunkStream::new(2, now);
        let packets = stream.pump(&terrain, now);
        apply(&mut stream, &packets, now);
        stream.feedback(0.5, now).unwrap();
        assert!(stream.pump(&terrain, now + STREAM_INTERVAL).is_empty());
        stream.follow(1600.0, -1600.0);
        let packets = stream.pump(&terrain, now + STREAM_INTERVAL);
        assert!(
            packets
                .iter()
                .any(|p| matches!(p, Clientbound::ForgetChunk(_)))
        );
        assert!(
            !packets
                .iter()
                .any(|p| matches!(p, Clientbound::ChunkData(_)))
        );
        apply(&mut stream, &packets, now + STREAM_INTERVAL);
        let packets = stream.pump(&terrain, now + Duration::from_millis(100));
        assert_eq!(
            packets
                .iter()
                .filter(|p| matches!(p, Clientbound::ChunkData(_)))
                .count(),
            1
        );
    }

    #[test]
    fn distant_jumps_and_radius_changes_converge_to_bounded_windows() {
        let terrain = crate::world::chunk_payload();
        let mut now = Instant::now();
        let mut a = ChunkStream::new(16, now);
        let mut b = ChunkStream::new(2, now);
        now = settle(&mut a, &terrain, now);
        assert_eq!(a.loaded.len(), 1225);
        for i in 1..=10 {
            for j in 0..10 {
                a.follow(32000.0 * f64::from(i), -32000.0 * f64::from(j));
                a.set_radius(2 + (i + j) % 15);
                b.follow(-32000.0 * f64::from(i), 32000.0 * f64::from(j));
                if let Some(next) = a.next_update() {
                    now = now.max(next);
                    let packets = a.pump(&terrain, now);
                    apply(&mut a, &packets, now);
                    while !a.unacked.is_empty() {
                        a.feedback(8.0, now).unwrap();
                    }
                }
            }
            now = settle(&mut a, &terrain, now);
            now = settle(&mut b, &terrain, now);
            for stream in [&a, &b] {
                let [x, z] = stream.center;
                assert_eq!(stream.loaded.len(), stream.desired_count());
                assert!(
                    stream
                        .loaded
                        .iter()
                        .all(|&[cx, cz]| (cx - x).abs().max((cz - z).abs())
                            <= stream.stream_radius())
                );
            }
        }
        let reset = ChunkStream::new(a.radius, now);
        assert!(reset.loaded.is_empty() && reset.unacked.is_empty() && !reset.writing);
        assert_eq!(reset.center, [0, 0]);
    }
}
