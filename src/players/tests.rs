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
    use crate::protocol::packets::play::{DefaultSpawnPosition, GameEvent};

    let players = Players::new(0);
    for (packet, size) in [
        (Clientbound::from(TimeUpdate::default()), 17),
        (Clientbound::from(GameEvent::start_raining()), 6),
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
        game_mode: crate::config::GameMode::Spectator,
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
    assert!(matches!(bob.inbox().pop(), Some(Delivery::Add { peer, spawn: true }) if peer.id == 1));
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
            TimeUpdate::default().into()
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
    let packet = TimeUpdate::default().into();
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
    let packet = TimeUpdate::default().into();
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
