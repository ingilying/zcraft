//! 20 TPS world clock, daylight cycle, and vanilla Java 1.21.1 weather cycle.
use crate::protocol::{
    Clientbound,
    packets::play::{GameEvent, TimeUpdate},
};
use rand::Rng;
use std::time::Duration;

pub const TICKS_PER_SECOND: u64 = 20;
pub const TICK_INTERVAL: Duration = Duration::from_millis(50);
pub const TIME_SYNC_INTERVAL: i64 = 20;
pub const DAY_LENGTH: i64 = 24_000;
pub const INITIAL_DAY_TIME: i64 = 6_000;

// Vanilla ServerLevel weather duration ranges (in ticks, 20 ticks = 1 second).
const CLEAR_RAIN_MIN: u32 = 12_000;
const CLEAR_RAIN_MAX: u32 = 180_000;
const RAIN_MIN: u32 = 12_000;
const RAIN_MAX: u32 = 24_000;
const CLEAR_THUNDER_MIN: u32 = 12_000;
const CLEAR_THUNDER_MAX: u32 = 180_000;
const THUNDER_MIN: u32 = 3_600;
const THUNDER_MAX: u32 = 15_600;
const MAX_WEATHER_STEPS: u8 = 100;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherSnapshot {
    pub raining: bool,
    pub thundering: bool,
    pub rain_level: f32,
    pub thunder_level: f32,
}

#[derive(Debug, Clone)]
pub struct WorldClock {
    world_age: i64,
    day_time: i64,
    raining: bool,
    rain_time: u32,
    thundering: bool,
    thunder_time: u32,
    rain_steps: u8,
    thunder_steps: u8,
}

impl Default for WorldClock {
    fn default() -> Self {
        Self::new()
    }
}

impl WorldClock {
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        Self::with_rng(&mut rng)
    }

    pub fn with_rng(rng: &mut impl Rng) -> Self {
        Self {
            world_age: 0,
            day_time: INITIAL_DAY_TIME,
            raining: false,
            rain_time: rng.gen_range(CLEAR_RAIN_MIN..=CLEAR_RAIN_MAX),
            thundering: false,
            thunder_time: rng.gen_range(CLEAR_THUNDER_MIN..=CLEAR_THUNDER_MAX),
            rain_steps: 0,
            thunder_steps: 0,
        }
    }

    pub fn world_age(&self) -> i64 {
        self.world_age
    }

    pub fn day_time(&self) -> i64 {
        self.day_time
    }

    fn rain_level(&self) -> f32 {
        f32::from(self.rain_steps) / f32::from(MAX_WEATHER_STEPS)
    }

    fn thunder_level(&self) -> f32 {
        f32::from(self.thunder_steps) / f32::from(MAX_WEATHER_STEPS)
    }

    pub fn weather(&self) -> WeatherSnapshot {
        WeatherSnapshot {
            raining: self.raining,
            thundering: self.thundering,
            rain_level: self.rain_level(),
            thunder_level: self.thunder_level(),
        }
    }

    pub fn time_packet(&self) -> TimeUpdate {
        TimeUpdate {
            world_age: self.world_age,
            time_of_day: self.day_time,
        }
    }

    /// Packets sent to a newly entering Play client to synchronize current
    /// time and active weather.
    pub fn initial_packets(&self) -> Vec<Clientbound> {
        let mut packets = vec![self.time_packet().into()];
        if self.raining {
            packets.push(GameEvent::start_raining().into());
        }
        if self.rain_steps > 0 {
            packets.push(GameEvent::rain_level(self.rain_level()).into());
        }
        if self.thunder_steps > 0 {
            packets.push(GameEvent::thunder_level(self.thunder_level()).into());
        }
        packets
    }

    pub fn set_day_time(&mut self, day_time: i64) -> TimeUpdate {
        self.day_time = day_time.max(0);
        self.time_packet()
    }

    /// Explicitly override weather (e.g. for a future `/weather` command).
    pub fn set_weather(
        &mut self,
        raining: bool,
        thundering: bool,
        duration: u32,
    ) -> Vec<Clientbound> {
        let duration = duration.max(1);
        let was_raining = self.raining;
        self.raining = raining;
        self.thundering = raining && thundering;
        self.rain_time = duration;
        self.thunder_time = duration;
        let mut out = Vec::new();
        if was_raining != self.raining {
            out.push(
                if self.raining {
                    GameEvent::start_raining()
                } else {
                    GameEvent::stop_raining()
                }
                .into(),
            );
        }
        out
    }

    /// Advance the world by one 50ms tick (20 TPS) using the thread RNG.
    pub fn tick(&mut self) -> Vec<Clientbound> {
        let mut rng = rand::thread_rng();
        self.tick_with_rng(&mut rng)
    }

    /// Advance the world by one 50ms tick (20 TPS) with an injected RNG.
    pub fn tick_with_rng(&mut self, rng: &mut impl Rng) -> Vec<Clientbound> {
        self.world_age = self.world_age.wrapping_add(1);
        self.day_time = self.day_time.wrapping_add(1);

        let mut packets = Vec::new();
        if self.world_age % TIME_SYNC_INTERVAL == 0 {
            packets.push(self.time_packet().into());
        }

        let was_raining = self.raining;
        let prev_rain_steps = self.rain_steps;
        let prev_thunder_steps = self.thunder_steps;

        self.thunder_time = self.thunder_time.saturating_sub(1);
        if self.thunder_time == 0 {
            self.thundering = !self.thundering;
            self.thunder_time = if self.thundering {
                rng.gen_range(THUNDER_MIN..=THUNDER_MAX)
            } else {
                rng.gen_range(CLEAR_THUNDER_MIN..=CLEAR_THUNDER_MAX)
            };
        }

        self.rain_time = self.rain_time.saturating_sub(1);
        if self.rain_time == 0 {
            self.raining = !self.raining;
            self.rain_time = if self.raining {
                rng.gen_range(RAIN_MIN..=RAIN_MAX)
            } else {
                rng.gen_range(CLEAR_RAIN_MIN..=CLEAR_RAIN_MAX)
            };
        }

        self.rain_steps = if self.raining {
            self.rain_steps.saturating_add(1).min(MAX_WEATHER_STEPS)
        } else {
            self.rain_steps.saturating_sub(1)
        };
        self.thunder_steps = if self.thundering {
            self.thunder_steps.saturating_add(1).min(MAX_WEATHER_STEPS)
        } else {
            self.thunder_steps.saturating_sub(1)
        };

        if was_raining != self.raining {
            packets.push(
                if self.raining {
                    GameEvent::start_raining()
                } else {
                    GameEvent::stop_raining()
                }
                .into(),
            );
        }
        if self.rain_steps != prev_rain_steps {
            packets.push(GameEvent::rain_level(self.rain_level()).into());
        }
        if self.thunder_steps != prev_thunder_steps {
            packets.push(GameEvent::thunder_level(self.thunder_level()).into());
        }
        packets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    #[test]
    fn clock_advances_time_and_broadcasts_every_second() {
        let mut rng = StdRng::seed_from_u64(42);
        let mut clock = WorldClock::with_rng(&mut rng);
        assert_eq!((clock.world_age(), clock.day_time()), (0, INITIAL_DAY_TIME));
        for tick in 1..TIME_SYNC_INTERVAL {
            let packets = clock.tick_with_rng(&mut rng);
            assert!(packets.is_empty(), "unexpected packet on tick {tick}");
        }
        let packets = clock.tick_with_rng(&mut rng);
        assert_eq!(
            packets,
            vec![Clientbound::from(TimeUpdate {
                world_age: 20,
                time_of_day: INITIAL_DAY_TIME + 20,
            })]
        );
    }

    #[test]
    fn weather_cycle_transitions_smoothly_and_syncs_new_players() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut clock = WorldClock::with_rng(&mut rng);

        // Start a thunderstorm for 120 ticks.
        let start = clock.set_weather(true, true, 120);
        assert_eq!(start, vec![Clientbound::from(GameEvent::start_raining())]);

        for _ in 0..100 {
            let packets = clock.tick_with_rng(&mut rng);
            assert!(packets.iter().any(|p| matches!(
                p,
                Clientbound::GameEvent(e) if e.event == GameEvent::RAIN_LEVEL_CHANGE
            )));
            assert!(packets.iter().any(|p| matches!(
                p,
                Clientbound::GameEvent(e) if e.event == GameEvent::THUNDER_LEVEL_CHANGE
            )));
        }
        let snap = clock.weather();
        assert!(snap.raining && snap.thundering);
        assert_eq!(snap.rain_level, 1.0);
        assert_eq!(snap.thunder_level, 1.0);

        // Initial packets for a joining player include time + active rain & thunder levels.
        let initial = clock.initial_packets();
        assert_eq!(initial.len(), 4);
        assert!(matches!(initial[0], Clientbound::TimeUpdate(_)));
        assert_eq!(initial[1], Clientbound::from(GameEvent::start_raining()));
        assert_eq!(initial[2], Clientbound::from(GameEvent::rain_level(1.0)));
        assert_eq!(initial[3], Clientbound::from(GameEvent::thunder_level(1.0)));

        // Once settled at 1.0, ticks emit no redundant weather packets until countdown ends.
        let steady = clock.tick_with_rng(&mut rng);
        assert!(
            steady
                .iter()
                .all(|p| !matches!(p, Clientbound::GameEvent(_)))
        );

        // Finish countdown -> rain & thunder stop and fade back down to 0.0.
        let mut saw_stop = false;
        for _ in 0..150 {
            let packets = clock.tick_with_rng(&mut rng);
            if packets.contains(&Clientbound::from(GameEvent::stop_raining())) {
                saw_stop = true;
            }
        }
        assert!(saw_stop);
        let cleared = clock.weather();
        assert!(!cleared.raining && !cleared.thundering);
        assert_eq!(cleared.rain_level, 0.0);
        assert_eq!(cleared.thunder_level, 0.0);
    }
}
