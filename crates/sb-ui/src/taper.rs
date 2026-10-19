//! The fader taper: knob position (0..1) ↔ dB ↔ the linear gain the controller takes.
//!
//! dB = max − (max − min)·(1 − position)², so the top of the travel (where people mix) is
//! finer than the bottom. Position 0 is silence (−∞), whatever `min` says. The master and the
//! matrix go to +6 dB; app levels are Windows session volumes and stop at 0 dB.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Taper {
    pub min_db: f32,
    pub max_db: f32,
}

/// −60..+6 dB: the master, clips and matrix cells.
pub const BOOST: Taper = Taper {
    min_db: -60.0,
    max_db: 6.0,
};

/// −60..0 dB: app levels (a session volume can't go above 1).
pub const UNITY_MAX: Taper = Taper {
    min_db: -60.0,
    max_db: 0.0,
};

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * gain.log10()
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    if db == f32::NEG_INFINITY {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// "+6.0 dB", "−12.5 dB", "−∞ dB".
pub fn format_db(db: f32) -> String {
    if db == f32::NEG_INFINITY || db < -99.0 {
        "−∞ dB".into()
    } else if db.abs() < 0.05 {
        "0.0 dB".into()
    } else if db > 0.0 {
        format!("+{db:.1} dB")
    } else {
        format!("−{:.1} dB", -db)
    }
}

/// What someone typed into a dB box: "−6", "-6 dB", "+3", "0", "off", "-inf". Gains above
/// `max_db` are clamped; `None` if it isn't a number.
pub fn parse_db(text: &str, max_db: f32) -> Option<f32> {
    let t = text.trim().to_lowercase().replace('−', "-");
    let t = t.trim_end_matches("db").trim();
    if matches!(t, "off" | "-inf" | "-∞" | "mute") {
        return Some(f32::NEG_INFINITY);
    }
    let v: f32 = t.parse().ok()?;
    v.is_finite().then_some(v.min(max_db))
}

impl Taper {
    pub fn db_to_position(&self, db: f32) -> f32 {
        if db <= self.min_db {
            return 0.0;
        }
        let x = ((self.max_db - db.min(self.max_db)) / (self.max_db - self.min_db)).clamp(0.0, 1.0);
        1.0 - x.sqrt()
    }

    pub fn position_to_db(&self, position: f32) -> f32 {
        if position <= 0.0 {
            return f32::NEG_INFINITY;
        }
        let p = position.min(1.0);
        self.max_db - (self.max_db - self.min_db) * (1.0 - p) * (1.0 - p)
    }

    pub fn gain_to_position(&self, gain: f32) -> f32 {
        self.db_to_position(gain_to_db(gain))
    }

    pub fn position_to_gain(&self, position: f32) -> f32 {
        db_to_gain(self.position_to_db(position))
    }

    /// The position of 0 dB (double-click resets there).
    pub fn unity(&self) -> f32 {
        self.db_to_position(0.0)
    }

    /// `gain` moved by `steps` dB, as a mouse wheel notch or arrow key does (from silence,
    /// the first step lands on `min_db`).
    pub fn nudge(&self, gain: f32, steps_db: f32) -> f32 {
        let db = gain_to_db(gain);
        let next = if db == f32::NEG_INFINITY {
            if steps_db > 0.0 { self.min_db } else { db }
        } else if db + steps_db < self.min_db {
            f32::NEG_INFINITY
        } else {
            (db + steps_db).min(self.max_db)
        };
        db_to_gain(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_dbs_and_gains_agree() {
        for t in [BOOST, UNITY_MAX] {
            assert_eq!(t.position_to_db(1.0), t.max_db);
            assert_eq!(t.position_to_db(0.0), f32::NEG_INFINITY);
            assert_eq!(t.gain_to_position(0.0), 0.0);
            for p in [0.05, 0.3, 0.5, 0.77, 0.99] {
                let back = t.db_to_position(t.position_to_db(p));
                assert!((back - p).abs() < 1e-5, "{p} → {back}");
                let g = t.position_to_gain(p);
                assert!((t.gain_to_position(g) - p).abs() < 1e-4);
            }
            assert!((t.position_to_gain(t.unity()) - 1.0).abs() < 1e-5);
        }
        // Finer at the top: the upper half of the travel covers less than a quarter of the range.
        assert!(BOOST.position_to_db(0.5) < BOOST.max_db - 0.7 * 66.0 / 3.0);
        assert!(BOOST.gain_to_position(4.0) == 1.0, "above the top is the top");
    }

    #[test]
    fn nudges_step_in_db_and_stop_at_the_ends() {
        let g = BOOST.nudge(1.0, 1.0);
        assert!((gain_to_db(g) - 1.0).abs() < 1e-4);
        assert_eq!(gain_to_db(BOOST.nudge(db_to_gain(5.5), 1.0)), 6.0);
        assert_eq!(UNITY_MAX.nudge(1.0, 1.0), 1.0, "apps stop at 0 dB");
        assert_eq!(BOOST.nudge(db_to_gain(-59.5), -1.0), 0.0, "below the bottom is silence");
        assert!((gain_to_db(BOOST.nudge(0.0, 1.0)) + 60.0).abs() < 1e-4);
        assert_eq!(BOOST.nudge(0.0, -1.0), 0.0);
    }

    #[test]
    fn db_text_both_ways() {
        assert_eq!(format_db(f32::NEG_INFINITY), "−∞ dB");
        assert_eq!(format_db(0.01), "0.0 dB");
        assert_eq!(format_db(6.0), "+6.0 dB");
        assert_eq!(format_db(-12.46), "−12.5 dB");
        assert_eq!(parse_db("−6", 6.0), Some(-6.0));
        assert_eq!(parse_db(" -6 dB ", 6.0), Some(-6.0));
        assert_eq!(parse_db("+3dB", 6.0), Some(3.0));
        assert_eq!(parse_db("12", 6.0), Some(6.0), "clamped to the top");
        assert_eq!(parse_db("off", 6.0), Some(f32::NEG_INFINITY));
        assert_eq!(parse_db("loud", 6.0), None);
    }
}
