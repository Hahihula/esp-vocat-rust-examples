//! Gain steps of the ES7210 microphone preamplifier.

/// Microphone preamplifier gain: 0 to 33 dB in 3 dB steps, then 34.5, 36
/// and 37.5 dB (register codes 0 to 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gain(u8);

impl Gain {
    /// The lowest gain, 0 dB.
    pub const MIN: Gain = Gain(0);
    /// The highest gain, 37.5 dB.
    pub const MAX: Gain = Gain(14);

    /// The nearest gain at or below `db` (NaN and negative values give 0 dB).
    #[must_use]
    pub fn from_db(db: f32) -> Self {
        let code = match db {
            d if d >= 37.5 => 14,
            d if d >= 36.0 => 13,
            d if d >= 34.5 => 12,
            // `as u8` saturates, and the guard rules out NaN and negatives.
            d if d >= 0.0 => ((d / 3.0) as u8).min(11),
            _ => 0,
        };
        Gain(code)
    }

    /// The gain in dB.
    #[must_use]
    pub fn db(self) -> f32 {
        match self.0 {
            0..=11 => f32::from(self.0) * 3.0,
            12 => 34.5,
            13 => 36.0,
            _ => 37.5,
        }
    }

    /// The register code (0..=14).
    #[must_use]
    pub fn code(self) -> u8 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_round_down() {
        assert_eq!(Gain::from_db(0.0).db(), 0.0);
        assert_eq!(Gain::from_db(2.9).db(), 0.0);
        assert_eq!(Gain::from_db(30.0).db(), 30.0);
        assert_eq!(Gain::from_db(31.0).db(), 30.0);
        assert_eq!(Gain::from_db(33.9).db(), 33.0);
        assert_eq!(Gain::from_db(34.5).db(), 34.5);
        assert_eq!(Gain::from_db(37.0).db(), 36.0);
        assert_eq!(Gain::from_db(100.0), Gain::MAX);
    }

    #[test]
    fn out_of_range_is_zero() {
        assert_eq!(Gain::from_db(-5.0), Gain::MIN);
        assert_eq!(Gain::from_db(f32::NAN), Gain::MIN);
    }

    #[test]
    fn codes_round_trip() {
        for code in 0..=14 {
            let g = Gain(code);
            assert_eq!(Gain::from_db(g.db()), g);
        }
    }
}
