//! 20 TPS world clock, daylight cycle, and vanilla Java 1.21.1 weather cycle.
//! Pure domain model: produces protocol-agnostic time and weather events.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSnapshot {
    pub world_age: i64,
    pub day_time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherSnapshot {
    pub raining: bool,
    pub thundering: bool,
    pub rain_level: f32,
    pub thunder_level: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WeatherEvent {
    StartRaining,
    StopRaining,
    RainLevel(f32),
    ThunderLevel(f32),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickEvents {
    pub time_sync: Option<TimeSnapshot>,
    pub weather: Vec<WeatherEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedClock {
    pub world_age: i64,
    pub day_time: i64,
    pub raining: bool,
    pub rain_time: u32,
    pub thundering: bool,
    pub thunder_time: u32,
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

    pub fn from_persisted(state: PersistedClock) -> Self {
        Self {
            world_age: state.world_age.max(0),
            day_time: state.day_time.max(0),
            raining: state.raining,
            rain_time: state.rain_time.max(1),
            thundering: state.raining && state.thundering,
            thunder_time: state.thunder_time.max(1),
            rain_steps: if state.raining { MAX_WEATHER_STEPS } else { 0 },
            thunder_steps: if state.raining && state.thundering {
                MAX_WEATHER_STEPS
            } else {
                0
            },
        }
    }

    pub fn persisted_state(&self) -> PersistedClock {
        PersistedClock {
            world_age: self.world_age,
            day_time: self.day_time,
            raining: self.raining,
            rain_time: self.rain_time,
            thundering: self.thundering,
            thunder_time: self.thunder_time,
        }
    }

    pub fn world_age(&self) -> i64 {
        self.world_age
    }

    pub fn day_time(&self) -> i64 {
        self.day_time
    }

    pub fn time(&self) -> TimeSnapshot {
        TimeSnapshot {
            world_age: self.world_age,
            day_time: self.day_time,
        }
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

    /// Initial domain state to synchronize a newly entering player.
    pub fn initial_state(&self) -> (TimeSnapshot, Vec<WeatherEvent>) {
        let mut weather = Vec::new();
        if self.raining {
            weather.push(WeatherEvent::StartRaining);
        }
        if self.rain_steps > 0 {
            weather.push(WeatherEvent::RainLevel(self.rain_level()));
        }
        if self.thunder_steps > 0 {
            weather.push(WeatherEvent::ThunderLevel(self.thunder_level()));
        }
        (self.time(), weather)
    }

    pub fn set_day_time(&mut self, day_time: i64) -> TimeSnapshot {
        self.day_time = day_time.max(0);
        self.time()
    }

    pub fn set_weather(
        &mut self,
        raining: bool,
        thundering: bool,
        duration: u32,
    ) -> Vec<WeatherEvent> {
        let duration = duration.max(1);
        let was_raining = self.raining;
        self.raining = raining;
        self.thundering = raining && thundering;
        self.rain_time = duration;
        self.thunder_time = duration;
        let mut out = Vec::new();
        if was_raining != self.raining {
            out.push(if self.raining {
                WeatherEvent::StartRaining
            } else {
                WeatherEvent::StopRaining
            });
        }
        out
    }

    /// Advance the world by one 50ms tick (20 TPS) using the thread RNG.
    pub fn tick(&mut self) -> TickEvents {
        let mut rng = rand::thread_rng();
        self.tick_with_rng(&mut rng)
    }

    /// Advance the world by one 50ms tick (20 TPS) with an injected RNG.
    pub fn tick_with_rng(&mut self, rng: &mut impl Rng) -> TickEvents {
        self.world_age = self.world_age.wrapping_add(1);
        self.day_time = self.day_time.wrapping_add(1);

        let mut events = TickEvents::default();
        if self.world_age % TIME_SYNC_INTERVAL == 0 {
            events.time_sync = Some(self.time());
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
            events.weather.push(if self.raining {
                WeatherEvent::StartRaining
            } else {
                WeatherEvent::StopRaining
            });
        }
        if self.rain_steps != prev_rain_steps {
            events
                .weather
                .push(WeatherEvent::RainLevel(self.rain_level()));
        }
        if self.thunder_steps != prev_thunder_steps {
            events
                .weather
                .push(WeatherEvent::ThunderLevel(self.thunder_level()));
        }
        events
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
            let events = clock.tick_with_rng(&mut rng);
            assert!(events.time_sync.is_none(), "unexpected sync on tick {tick}");
            assert!(events.weather.is_empty());
        }
        let events = clock.tick_with_rng(&mut rng);
        assert_eq!(
            events.time_sync,
            Some(TimeSnapshot {
                world_age: 20,
                day_time: INITIAL_DAY_TIME + 20,
            })
        );
    }

    #[test]
    fn weather_cycle_transitions_smoothly_and_syncs_new_players() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut clock = WorldClock::with_rng(&mut rng);

        let start = clock.set_weather(true, true, 120);
        assert_eq!(start, vec![WeatherEvent::StartRaining]);

        for _ in 0..100 {
            let events = clock.tick_with_rng(&mut rng);
            assert!(
                events
                    .weather
                    .iter()
                    .any(|e| matches!(e, WeatherEvent::RainLevel(_)))
            );
            assert!(
                events
                    .weather
                    .iter()
                    .any(|e| matches!(e, WeatherEvent::ThunderLevel(_)))
            );
        }
        let snap = clock.weather();
        assert!(snap.raining && snap.thundering);
        assert_eq!(snap.rain_level, 1.0);
        assert_eq!(snap.thunder_level, 1.0);

        let (time, weather) = clock.initial_state();
        assert_eq!(time.world_age, 100);
        assert_eq!(
            weather,
            vec![
                WeatherEvent::StartRaining,
                WeatherEvent::RainLevel(1.0),
                WeatherEvent::ThunderLevel(1.0),
            ]
        );

        let steady = clock.tick_with_rng(&mut rng);
        assert!(steady.weather.is_empty());

        let mut saw_stop = false;
        for _ in 0..150 {
            let events = clock.tick_with_rng(&mut rng);
            if events.weather.contains(&WeatherEvent::StopRaining) {
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
