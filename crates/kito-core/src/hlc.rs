//! Hybrid logical clock (Kulkarni et al., 2014).
//!
//! Dà un ordine totale compatibile con la causalità anche quando l'orologio di
//! sistema torna indietro o due eventi cadono nello stesso millisecondo.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Timestamp HLC: millisecondi Unix più un contatore logico.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc {
    pub wall_ms: u64,
    pub logical: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc { wall_ms: 0, logical: 0 };

    pub const fn new(wall_ms: u64, logical: u32) -> Self {
        Hlc { wall_ms, logical }
    }
}

/// Generatore di timestamp strettamente crescenti.
#[derive(Debug, Default)]
pub struct HlcClock {
    last: Hlc,
}

impl HlcClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Riparte da un timestamp già emesso (per esempio l'ultimo nel log).
    pub fn starting_after(last: Hlc) -> Self {
        HlcClock { last }
    }

    pub fn last(&self) -> Hlc {
        self.last
    }

    /// Nuovo timestamp per un evento locale, con l'orologio di sistema.
    pub fn now(&mut self) -> Hlc {
        self.tick(system_ms())
    }

    /// Nuovo timestamp per un evento locale, dato l'orologio fisico.
    pub fn tick(&mut self, physical_ms: u64) -> Hlc {
        self.last =
            if physical_ms > self.last.wall_ms { Hlc::new(physical_ms, 0) } else { Self::bump(self.last) };
        self.last
    }

    /// Nuovo timestamp dopo aver visto un evento remoto: è maggiore sia
    /// dell'ultimo locale sia di quello remoto.
    pub fn observe(&mut self, remote: Hlc, physical_ms: u64) -> Hlc {
        let max = self.last.max(remote);
        self.last = if physical_ms > max.wall_ms { Hlc::new(physical_ms, 0) } else { Self::bump(max) };
        self.last
    }

    fn bump(t: Hlc) -> Hlc {
        match t.logical.checked_add(1) {
            Some(logical) => Hlc::new(t.wall_ms, logical),
            // Contatore esaurito nello stesso millisecondo: si avanza il tempo.
            None => Hlc::new(t.wall_ms + 1, 0),
        }
    }
}

fn system_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictly_increasing_even_if_wall_clock_goes_back() {
        let mut c = HlcClock::new();
        let a = c.tick(1000);
        let b = c.tick(1000);
        let d = c.tick(900); // orologio tornato indietro
        let e = c.tick(1001);
        assert!(a < b && b < d && d < e);
        assert_eq!(e, Hlc::new(1001, 0));
    }

    #[test]
    fn observe_dominates_remote() {
        let mut c = HlcClock::new();
        c.tick(10);
        let remote = Hlc::new(500, 7);
        let t = c.observe(remote, 20);
        assert!(t > remote);
        assert_eq!(t, Hlc::new(500, 8));
        assert!(c.tick(21) > t);
    }

    #[test]
    fn logical_overflow_advances_wall_time() {
        let mut c = HlcClock::starting_after(Hlc::new(5, u32::MAX));
        assert_eq!(c.tick(5), Hlc::new(6, 0));
    }
}
