//! Owned command-query snapshots. No registry guard escapes into evaluators.
use super::{Players, lock};
use crate::{
    command::arguments::{EntityGameMode, EntitySnapshot, ResourceLocation},
    config::GameMode,
};

impl Players {
    pub(crate) fn command_entities(&self) -> Vec<EntitySnapshot> {
        lock(&self.registry)
            .players
            .values()
            .filter(|slot| slot.receiving && !slot.lagged)
            .filter_map(|slot| slot.visible.as_ref())
            .map(|peer| {
                let mode = match peer.game_mode {
                    GameMode::Survival => EntityGameMode::Survival,
                    GameMode::Creative => EntityGameMode::Creative,
                    GameMode::Spectator => EntityGameMode::Spectator,
                };
                EntitySnapshot {
                    uuid: peer.uuid,
                    name: peer.name.to_string(),
                    entity_type: ResourceLocation {
                        namespace: "minecraft".into(),
                        path: "player".into(),
                    },
                    world: "minecraft:overworld".into(),
                    position: peer.pose.position.map(f64::from_bits),
                    rotation: peer.pose.rotation.map(f32::from_bits),
                    width: 0.6,
                    height: 1.8,
                    game_mode: Some(mode),
                }
            })
            .collect()
    }

    /// Snapshot ready Survival/Creative players eligible to pick up item entities.
    pub(crate) fn pickup_candidates(&self) -> Vec<(uuid::Uuid, i32, [f64; 3])> {
        lock(&self.registry)
            .players
            .values()
            .filter(|slot| slot.receiving && !slot.lagged)
            .filter_map(|slot| slot.visible.as_ref())
            .filter(|peer| matches!(peer.game_mode, GameMode::Survival | GameMode::Creative))
            .map(|peer| (peer.uuid, peer.id, peer.pose.position.map(f64::from_bits)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{players::Peer, protocol::packets::play_entities::EntityPose};
    use uuid::Uuid;
    #[test]
    fn snapshots_are_owned_and_only_include_ready_players() {
        let players = Players::new(2);
        let alice = players.reserve(Uuid::nil()).unwrap();
        let pending = players.reserve(Uuid::from_u128(1)).unwrap();
        pending.enter_play();
        alice.enter_play();
        alice.show(Peer {
            uuid: Uuid::nil(),
            name: "Alice".into(),
            id: 1,
            game_mode: GameMode::Creative,
            pose: EntityPose::new([1.0, 64.0, 2.0], 12.3, -20.1, false),
            info: std::sync::Arc::from([]),
        });
        let snapshot = players.command_entities();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].rotation, [12.3, -20.1]);
        assert_eq!(snapshot[0].game_mode, Some(EntityGameMode::Creative));
        alice.update_game_mode(GameMode::Spectator);
        assert_eq!(
            players.command_entities()[0].game_mode,
            Some(EntityGameMode::Spectator)
        );
        alice.pause();
        assert!(players.command_entities().is_empty());
        drop(alice);
        assert_eq!(snapshot[0].name, "Alice");
    }
}
